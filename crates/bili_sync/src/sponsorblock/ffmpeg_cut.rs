//! Cut keep-ranges from a media file via ffmpeg stream-copy + concat.
//!
//! Base merge/split patterns from bili-sync-up v3.1.3 downloader;
//! segment semantics from BilibiliSponsorBlock.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tokio::fs;
use tracing::{debug, info};

use crate::downloader::resolve_media_tool_path;

/// Extract each keep interval with stream copy, then concat into `output`.
pub async fn cut_out_segments_with_ffmpeg(
    input: &Path,
    output: &Path,
    keep: &[(f64, f64)],
) -> Result<()> {
    ensure_nonempty_keeps(keep)?;
    if keep.len() == 1 {
        // Single contiguous keep: one ffmpeg extract is enough (no concat).
        extract_part(input, output, keep[0].0, keep[0].1).await?;
        return Ok(());
    }

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let temp_dir = parent.join(format!(
        ".bili-sync-sponsorblock-{}-{}",
        std::process::id(),
        ts
    ));
    fs::create_dir_all(&temp_dir).await?;

    let ext = output
        .extension()
        .and_then(|v| v.to_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("mp4");

    let cleanup = |dir: &PathBuf| {
        let dir = dir.clone();
        async move {
            let _ = fs::remove_dir_all(&dir).await;
        }
    };

    let mut part_paths: Vec<PathBuf> = Vec::with_capacity(keep.len());
    for (i, (start, end)) in keep.iter().enumerate() {
        let part = temp_dir.join(format!("part-{:03}.{}", i, ext));
        if let Err(e) = extract_part(input, &part, *start, *end).await {
            cleanup(&temp_dir).await;
            return Err(e).with_context(|| format!("extract keep part {} [{}, {})", i, start, end));
        }
        part_paths.push(part);
    }

    let list_path = temp_dir.join("parts.txt");
    let mut list_body = String::new();
    for part in &part_paths {
        list_body.push_str(&format!("file '{}'\n", escape_concat_path(part)));
    }
    if let Err(e) = fs::write(&list_path, list_body.as_bytes()).await {
        cleanup(&temp_dir).await;
        return Err(e).context("write ffmpeg concat list");
    }

    let tmp_out = temp_dir.join(format!("concat-out.{}", ext));
    let list_str = list_path.to_string_lossy().to_string();
    let tmp_out_str = tmp_out.to_string_lossy().to_string();

    let output_cmd = tokio::process::Command::new(resolve_media_tool_path("ffmpeg"))
        .args([
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &list_str,
            "-map",
            "0",
            "-c",
            "copy",
            &tmp_out_str,
        ])
        .output()
        .await;

    let output_cmd = match output_cmd {
        Ok(o) => o,
        Err(e) => {
            cleanup(&temp_dir).await;
            return Err(e).context("spawn ffmpeg concat");
        }
    };

    if !output_cmd.status.success() {
        let stderr = String::from_utf8_lossy(&output_cmd.stderr);
        cleanup(&temp_dir).await;
        bail!("ffmpeg concat error: {}", stderr.trim());
    }

    if let Some(out_parent) = output.parent() {
        fs::create_dir_all(out_parent).await?;
    }
    if output.exists() {
        fs::remove_file(output).await.ok();
    }
    if let Err(e) = fs::rename(&tmp_out, output).await {
        // Cross-device fallback
        if let Err(copy_err) = fs::copy(&tmp_out, output).await {
            cleanup(&temp_dir).await;
            return Err(copy_err).context(format!("move concat output failed (rename: {:#})", e));
        }
        let _ = fs::remove_file(&tmp_out).await;
    }

    cleanup(&temp_dir).await;
    info!(
        "SponsorBlock cut wrote {} keep parts → {}",
        keep.len(),
        output.display()
    );
    Ok(())
}

async fn extract_part(input: &Path, part: &Path, start: f64, end: f64) -> Result<()> {
    if end <= start {
        bail!("invalid keep range [{}, {})", start, end);
    }
    let duration = end - start;
    let input_str = input.to_string_lossy().to_string();
    let part_str = part.to_string_lossy().to_string();
    let ss = format!("{:.3}", start);
    let t = format!("{:.3}", duration);

    debug!(
        "ffmpeg extract part ss={} t={} → {}",
        ss,
        t,
        part.display()
    );

    // -ss before -i for fast seek; -t after for duration; stream copy + break non-keyframes
    // matches existing chapter-split style (keyframe-accurate, not frame-accurate).
    let output = tokio::process::Command::new(resolve_media_tool_path("ffmpeg"))
        .args([
            "-y",
            "-ss",
            &ss,
            "-i",
            &input_str,
            "-t",
            &t,
            "-map",
            "0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            "-break_non_keyframes",
            "1",
            &part_str,
        ])
        .output()
        .await
        .context("spawn ffmpeg extract")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg extract error: {}", stderr.trim());
    }
    if !part.exists() {
        bail!("ffmpeg extract produced no file: {}", part.display());
    }
    Ok(())
}

fn ensure_nonempty_keeps(keep: &[(f64, f64)]) -> Result<()> {
    if keep.is_empty() {
        return Err(anyhow!("no keep ranges to cut"));
    }
    Ok(())
}

fn escape_concat_path(path: &Path) -> String {
    // concat demuxer: escape single quotes
    path.to_string_lossy().replace('\'', "'\\''")
}

/// Atomically replace `target` with `new_file`, optionally keeping original beside it.
pub async fn atomic_replace_with_optional_backup(
    target: &Path,
    new_file: &Path,
    keep_original: bool,
    original_suffix: &str,
) -> Result<()> {
    if keep_original && target.exists() {
        let backup = sibling_with_suffix(target, original_suffix);
        if backup.exists() {
            fs::remove_file(&backup).await.ok();
        }
        fs::rename(target, &backup)
            .await
            .with_context(|| format!("backup original to {}", backup.display()))?;
        info!("SponsorBlock kept original as {}", backup.display());
    } else if target.exists() {
        fs::remove_file(target).await.ok();
    }

    if let Err(e) = fs::rename(new_file, target).await {
        fs::copy(new_file, target)
            .await
            .with_context(|| format!("replace target {} (rename failed: {:#})", target.display(), e))?;
        let _ = fs::remove_file(new_file).await;
    }
    Ok(())
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty());
    match ext {
        Some(ext) => parent.join(format!("{}{}.{}", stem, suffix, ext)),
        None => parent.join(format!("{}{}", stem, suffix)),
    }
}
