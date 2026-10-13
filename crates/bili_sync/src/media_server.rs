//! Notify Emby / Jellyfin to rescan after a page finishes downloading
//! (after SponsorBlock cut + sidecar processing).
//!
//! Logic follows MoviePilot's Emby module / MediaServerRefresh plugin and Sonarr's
//! MediaBrowser notifier:
//! - targeted: `POST /Library/Media/Updated` with `{"Updates":[{"Path", "UpdateType":"Created"}]}`
//!   (Sonarr MediaBrowserProxy.Update; Jellyfin LibraryController.PostUpdatedMedia)
//! - fallback: `POST /Library/Refresh` full scan (MoviePilot Emby.refresh_root_library)
//!
//! Paths are mapped from this container's view (e.g. `/bili-sync/...`) to the media
//! server's view with longest-prefix mappings. Changed folders are collected for a whole
//! scan/download round; one request is sent `delay_seconds` after the last round ends
//! (a round starting during the delay postpones it).
//!
//! The API key is only sent in the `X-Emby-Token` header and is never logged.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::{MediaServerConfig, PathMapping};

/// Map a local (container) path to the media server's path via the longest matching prefix.
/// Matching is on path-component boundaries; separators in the remainder follow the target
/// style (`\` when the target looks like a Windows/UNC path). Unmapped paths are returned as-is.
pub fn map_path(local: &str, mappings: &[PathMapping]) -> String {
    let norm = |s: &str| s.replace('\\', "/");
    let local_n = norm(local);
    let mut best: Option<(&PathMapping, usize)> = None;
    for m in mappings {
        let from = norm(m.from.trim());
        let from = from.trim_end_matches('/');
        if from.is_empty() || m.to.trim().is_empty() {
            continue;
        }
        let hit = local_n == from || local_n.starts_with(&format!("{from}/"));
        if hit && best.is_none_or(|(_, len)| from.len() > len) {
            best = Some((m, from.len()));
        }
    }
    let Some((m, len)) = best else {
        return local.to_string();
    };
    let rest = local_n[len..].trim_start_matches('/');
    let to = m.to.trim();
    let windows = to.contains('\\') || (to.len() >= 2 && to.as_bytes()[1] == b':');
    let sep = if windows { '\\' } else { '/' };
    let to = to.trim_end_matches(['/', '\\']);
    if rest.is_empty() {
        return to.to_string();
    }
    let rest = if windows { rest.replace('/', "\\") } else { rest.to_string() };
    format!("{to}{sep}{rest}")
}

/// Round-aware debouncer.
///
/// Changed folders are collected while scan/download rounds are active. Only when the
/// last active round finishes (queue idle) is a flush armed; it fires after `delay`.
/// A round that starts during the delay disarms it (the generation changes) and the
/// next round end re-arms it, so one refresh is sent per quiet period.
#[derive(Default)]
pub struct Debouncer {
    paths: BTreeSet<String>,
    active_rounds: usize,
    generation: u64,
}

impl Debouncer {
    fn arm(&mut self) -> Option<u64> {
        if self.active_rounds == 0 && !self.paths.is_empty() {
            self.generation += 1;
            Some(self.generation)
        } else {
            None
        }
    }

    /// Queue a folder. Returns a generation to schedule when no round is active
    /// (e.g. a manual download outside the scheduled loop).
    pub fn push(&mut self, path: String) -> Option<u64> {
        let new = self.paths.insert(path);
        if new { self.arm() } else { None }
    }

    pub fn round_started(&mut self) {
        self.active_rounds += 1;
        self.generation += 1; // cancel any pending flush
    }

    /// Returns a generation to schedule when this was the last active round.
    pub fn round_finished(&mut self) -> Option<u64> {
        self.active_rounds = self.active_rounds.saturating_sub(1);
        self.arm()
    }

    /// Called when a timer for `generation` expires: returns the batch to send,
    /// or None if the timer is stale or a round is running.
    pub fn fire(&mut self, generation: u64) -> Option<Vec<String>> {
        if generation != self.generation || self.active_rounds > 0 || self.paths.is_empty() {
            return None;
        }
        Some(prune_nested(std::mem::take(&mut self.paths)))
    }
}

/// Drop paths covered by an ancestor in the same set.
fn prune_nested(all: BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in all {
        let covered = out.iter().any(|a| {
            let a = a.trim_end_matches(['/', '\\']);
            p.len() > a.len() && p.starts_with(a) && matches!(p.as_bytes()[a.len()], b'/' | b'\\')
        });
        if !covered {
            out.push(p);
        }
    }
    out
}

static STATE: Lazy<Mutex<Debouncer>> = Lazy::new(|| Mutex::new(Debouncer::default()));

fn schedule(generation: u64) {
    let delay = crate::config::reload_config().media_server.delay_seconds;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        let Some(batch) = STATE.lock().fire(generation) else {
            return;
        };
        let cfg = crate::config::reload_config().media_server.clone();
        if !cfg.enabled {
            return;
        }
        if let Err(e) = flush(&cfg, &batch).await {
            warn!("媒体服务器刷新失败: {:#}", e);
        }
    });
}

/// RAII marker for one scan/download round; dropping it ends the round.
pub struct RoundGuard(());

impl RoundGuard {
    pub fn start() -> Self {
        STATE.lock().round_started();
        RoundGuard(())
    }
}

impl Drop for RoundGuard {
    fn drop(&mut self) {
        let armed = STATE.lock().round_finished();
        if let Some(g) = armed {
            let delay = crate::config::reload_config().media_server.delay_seconds;
            info!("本轮下载已全部完成，{} 秒后通知媒体服务器扫描", delay);
            schedule(g);
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MediaUpdate<'a> {
    path: &'a str,
    update_type: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MediaUpdatedBody<'a> {
    updates: Vec<MediaUpdate<'a>>,
}


/// Record that `file` (a finished media file) changed. Its parent folder is queued so
/// the media server also picks up NFO / subtitles / danmaku sidecars next to it.
pub fn notify_file_changed(file: &std::path::Path) {
    let cfg = crate::config::reload_config().media_server.clone();
    if !cfg.enabled || cfg.url.trim().is_empty() || cfg.api_key.as_deref().unwrap_or("").is_empty() {
        return;
    }
    let dir = file.parent().unwrap_or(file).to_string_lossy().to_string();
    let mapped = map_path(&dir, &cfg.path_mappings);
    debug!("媒体服务器刷新排队: {} -> {}", dir, mapped);
    let armed = STATE.lock().push(mapped);
    if let Some(g) = armed {
        schedule(g);
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?)
}

async fn post(cfg: &MediaServerConfig, path: &str, body: Option<String>) -> Result<()> {
    let base = cfg.url.trim().trim_end_matches('/');
    let key = cfg.api_key.as_deref().unwrap_or("");
    let mut req = client()?
        .post(format!("{base}{path}"))
        .header("X-Emby-Token", key)
        .header("Authorization", format!("MediaBrowser Token=\"{key}\""));
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b);
    }
    let resp = req.send().await.with_context(|| format!("POST {path}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("POST {path} -> HTTP {status}");
    }
    Ok(())
}

pub async fn flush(cfg: &MediaServerConfig, paths: &[String]) -> Result<()> {
    let body = MediaUpdatedBody {
        updates: paths.iter().map(|p| MediaUpdate { path: p, update_type: "Created" }).collect(),
    };
    let body = serde_json::to_string(&body)?;
    match post(cfg, "/Library/Media/Updated", Some(body)).await {
        Ok(()) => {
            info!("已通知{}定向扫描 {} 个目录: {:?}", cfg.server_type, paths.len(), paths);
            Ok(())
        }
        Err(e) if cfg.fallback_full_refresh => {
            warn!("定向扫描失败({:#})，回退为全库扫描", e);
            post(cfg, "/Library/Refresh", None).await?;
            info!("已通知{}全库扫描", cfg.server_type);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Connectivity check used by the settings page: `GET /System/Info`.
pub async fn test_connection(cfg: &MediaServerConfig) -> Result<String> {
    let base = cfg.url.trim().trim_end_matches('/');
    let key = cfg.api_key.as_deref().unwrap_or("");
    let resp = client()?
        .get(format!("{base}/System/Info"))
        .header("X-Emby-Token", key)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(format!(
        "{} {}",
        v.get("ServerName").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("Version").and_then(|x| x.as_str()).unwrap_or("")
    ))
}

/// Mask the API key inside a stored `media_server` config value (history / raw item API).
pub fn redact_config_value(key: &str, value: &mut serde_json::Value) {
    if key == "media_server" {
        if let Some(k) = value.get_mut("api_key") {
            if k.as_str().is_some_and(|s| !s.is_empty()) {
                *k = serde_json::Value::String("***".to_string());
            }
        }
    }
}

/// String variant of [`redact_config_value`]; non-JSON input is returned unchanged.
pub fn redact_config_json(key: &str, json: &str) -> String {
    if key != "media_server" {
        return json.to_string();
    }
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(mut v) => {
            redact_config_value(key, &mut v);
            v.to_string()
        }
        Err(_) => json.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(from: &str, to: &str) -> PathMapping {
        PathMapping { from: from.into(), to: to.into() }
    }

    #[test]
    fn maps_longest_prefix_on_component_boundary() {
        let maps = vec![m("/bili-sync", "/media/bili"), m("/bili-sync/番剧", "/anime")];
        assert_eq!(map_path("/bili-sync/UP/视频", &maps), "/media/bili/UP/视频");
        assert_eq!(map_path("/bili-sync/番剧/A/Season 1", &maps), "/anime/A/Season 1");
        assert_eq!(map_path("/bili-sync", &maps), "/media/bili");
        // not a component boundary
        assert_eq!(map_path("/bili-sync2/x", &maps), "/bili-sync2/x");
    }

    #[test]
    fn trailing_slashes_and_unmapped() {
        let maps = vec![m("/bili-sync/", "/data/bili/")];
        assert_eq!(map_path("/bili-sync/a/b", &maps), "/data/bili/a/b");
        assert_eq!(map_path("/other/a", &maps), "/other/a");
        assert_eq!(map_path("/bili-sync/a", &[]), "/bili-sync/a");
        assert_eq!(map_path("/bili-sync/a", &[m("", "/x"), m("/bili-sync", "")]), "/bili-sync/a");
    }

    #[test]
    fn maps_to_windows_and_unc_targets() {
        let maps = vec![m("/bili-sync", "\\\\nas\\videos\\bili-sync")];
        assert_eq!(map_path("/bili-sync/UP/P1", &maps), "\\\\nas\\videos\\bili-sync\\UP\\P1");
        let maps = vec![m("/bili-sync", "D:/media/")];
        assert_eq!(map_path("/bili-sync/a/b", &maps), "D:/media\\a\\b");
    }

    #[test]
    fn collects_during_round_and_fires_once_after_it() {
        let mut d = Debouncer::default();
        d.round_started();
        assert_eq!(d.push("/m/a".into()), None);
        assert_eq!(d.push("/m/b".into()), None);
        assert_eq!(d.push("/m/a".into()), None);
        let g = d.round_finished().expect("armed after last round");
        assert_eq!(d.fire(g), Some(vec!["/m/a".to_string(), "/m/b".to_string()]));
        assert_eq!(d.fire(g), None);
    }

    #[test]
    fn new_round_during_delay_postpones_flush() {
        let mut d = Debouncer::default();
        d.round_started();
        d.push("/m/a".into());
        let g1 = d.round_finished().unwrap();
        d.round_started(); // next round begins before the delay expires
        assert_eq!(d.fire(g1), None);
        d.push("/m/c".into());
        let g2 = d.round_finished().unwrap();
        assert_eq!(d.fire(g1), None, "stale timer");
        assert_eq!(d.fire(g2), Some(vec!["/m/a".to_string(), "/m/c".to_string()]));
    }

    #[test]
    fn empty_round_does_not_arm_and_idle_push_arms() {
        let mut d = Debouncer::default();
        d.round_started();
        assert_eq!(d.round_finished(), None);
        let g = d.push("/m/x".into()).expect("idle push arms");
        assert_eq!(d.fire(g), Some(vec!["/m/x".to_string()]));
    }

    #[test]
    fn prunes_paths_under_queued_ancestor() {
        let mut d = Debouncer::default();
        d.round_started();
        for p in ["/m/show/Season 1", "/m/show", "/m/showcase", "\\\\nas\\v\\x", "\\\\nas\\v\\x\\y"] {
            d.push(p.into());
        }
        let g = d.round_finished().unwrap();
        assert_eq!(d.fire(g).unwrap(), vec!["/m/show", "/m/showcase", "\\\\nas\\v\\x"]);
    }

    #[test]
    fn redacts_api_key_only_for_media_server() {
        let j = r#"{"enabled":true,"api_key":"secret123"}"#;
        assert!(!redact_config_json("media_server", j).contains("secret123"));
        assert!(redact_config_json("media_server", j).contains("***"));
        assert_eq!(redact_config_json("media_server", r#"{"api_key":null}"#), r#"{"api_key":null}"#);
        assert_eq!(redact_config_json("other", j), j);
    }

    #[test]
    fn media_updated_body_shape() {
        let body = MediaUpdatedBody { updates: vec![MediaUpdate { path: "/a", update_type: "Created" }] };
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"Updates":[{"Path":"/a","UpdateType":"Created"}]}"#
        );
    }
}
