//! 容器以 root 运行（为了 iGPU）时，保持新建文件归属 PUID:PGID（如 2601:1113），
//! 使 Emby 等仍可读取。未设置 PUID/PGID 或非 root 运行时全部为 no-op。
//!
//! 策略：启动时设置 umask 022；每个视频源处理结束后、每轮扫描结束后，对该源目录 / 配置目录 /
//! UP 主头像目录做一次 lstat 遍历，仅对归属不符的条目 lchown（不跟随符号链接）。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

fn parse_owner(puid: Option<&str>, pgid: Option<&str>) -> Option<Owner> {
    let uid = puid?.trim().parse::<u32>().ok()?;
    let gid = pgid.and_then(|g| g.trim().parse::<u32>().ok()).unwrap_or(uid);
    Some(Owner { uid, gid })
}

/// 目标归属；仅当 PUID 已设置且当前为 root 时生效。
pub fn target_owner() -> Option<Owner> {
    static OWNER: OnceLock<Option<Owner>> = OnceLock::new();
    *OWNER.get_or_init(|| {
        #[cfg(unix)]
        {
            if unsafe { libc::geteuid() } != 0 {
                return None;
            }
            parse_owner(std::env::var("PUID").ok().as_deref(), std::env::var("PGID").ok().as_deref())
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

/// 启动早期调用：设置 umask 022（新文件 644 / 目录 755）。
pub fn init_process_umask() {
    #[cfg(unix)]
    if target_owner().is_some() {
        unsafe {
            libc::umask(0o022);
        }
    }
}

/// 遍历 root（含自身），把归属不符的条目改为 owner。返回修改数量。
pub fn fix_tree_with(root: &Path, owner: Owner) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut changed = 0usize;
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
            if (md.uid() != owner.uid || md.gid() != owner.gid)
                && std::os::unix::fs::lchown(&p, Some(owner.uid), Some(owner.gid)).is_ok()
            {
                changed += 1;
            }
            if md.file_type().is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
            }
        }
        changed
    }
    #[cfg(not(unix))]
    {
        let _ = (root, owner);
        0
    }
}

/// 同步版本；未启用时直接返回 0。
pub fn fix_tree(root: &Path) -> usize {
    match target_owner() {
        Some(o) if root.exists() => fix_tree_with(root, o),
        _ => 0,
    }
}

/// 异步（spawn_blocking）修复多个目录树。
pub async fn fix_trees(label: &str, roots: Vec<PathBuf>) {
    if target_owner().is_none() {
        return;
    }
    let label = label.to_string();
    let res = tokio::task::spawn_blocking(move || roots.iter().map(|r| fix_tree(r)).sum::<usize>()).await;
    match res {
        Ok(n) if n > 0 => tracing::info!("归属修正({}): {} 个条目已改为 PUID:PGID", label, n),
        Ok(_) => {}
        Err(e) => tracing::warn!("归属修正({})失败: {}", label, e),
    }
}

/// 配置目录 + UP 主头像目录。
pub async fn fix_config_and_upper() {
    if target_owner().is_none() {
        return;
    }
    let mut roots = vec![crate::config::CONFIG_DIR.clone()];
    let upper = crate::config::reload_config().upper_path.clone();
    roots.push(upper);
    fix_trees("config", roots).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_cases() {
        assert_eq!(parse_owner(Some("2601"), Some("1113")), Some(Owner { uid: 2601, gid: 1113 }));
        assert_eq!(parse_owner(Some("2601"), None), Some(Owner { uid: 2601, gid: 2601 }));
        assert_eq!(parse_owner(None, Some("1113")), None);
        assert_eq!(parse_owner(Some("x"), Some("1")), None);
    }

    #[cfg(unix)]
    #[test]
    fn fix_tree_noop_when_already_owned_and_walks_tree() {
        use std::os::unix::fs::MetadataExt;
        let base = std::env::temp_dir().join(format!("own-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        struct D(PathBuf);
        impl D { fn path(&self) -> &Path { &self.0 } }
        impl Drop for D { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
        let d = D(base);
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f.nfo"), b"x").unwrap();
        std::os::unix::fs::symlink("/nonexistent", d.path().join("a/link")).unwrap();
        let md = std::fs::metadata(d.path()).unwrap();
        let me = Owner { uid: md.uid(), gid: md.gid() };
        assert_eq!(fix_tree_with(d.path(), me), 0);
        if unsafe { libc::geteuid() } == 0 {
            let o = Owner { uid: 2601, gid: 1113 };
            assert_eq!(fix_tree_with(d.path(), o), 5);
            for p in ["", "a", "a/b", "a/b/f.nfo"] {
                let m = std::fs::symlink_metadata(d.path().join(p)).unwrap();
                assert_eq!((m.uid(), m.gid()), (2601, 1113));
            }
            let l = std::fs::symlink_metadata(d.path().join("a/link")).unwrap();
            assert_eq!((l.uid(), l.gid()), (2601, 1113));
            assert_eq!(fix_tree_with(d.path(), o), 0);
        }
    }
}
