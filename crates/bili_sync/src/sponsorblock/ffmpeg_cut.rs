//! Cut keep-ranges from a media file via ffmpeg (stream-copy or re-encode) + concat.
//!
//! Base merge/split patterns from bili-sync-up v3.1.3 downloader;
//! segment semantics from BilibiliSponsorBlock.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tokio::fs;
use tracing::{debug, info};

fn resolve_media_tool_path(name: &str) -> std::path::PathBuf {
    #[cfg(test)]
    {
        std::path::PathBuf::from(name)
    }
    #[cfg(not(test))]
    {
        crate::downloader::resolve_media_tool_path(name)
    }
}

/// How each keep interval is extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutMode {
    /// `-c copy`; keep starts must already sit on keyframes (see timeline snap).
    Keyframe,
    /// Re-encode video (libx264) + audio (aac); frame-accurate cut points.
    Accurate,
}

impl CutMode {
    pub fn from_config(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("accurate") {
            Self::Accurate
        } else {
            Self::Keyframe
        }
    }
}

/// Extract each keep interval, then concat into `output`.
pub async fn cut_out_segments_with_ffmpeg(
    input: &Path,
    output: &Path,
    keep: &[(f64, f64)],
    mode: CutMode,
) -> Result<()> {
    ensure_nonempty_keeps(keep)?;
    if keep.len() == 1 {
        // Single contiguous keep: one ffmpeg extract is enough (no concat).
        extract_part(input, output, keep[0].0, keep[0].1, mode).await?;
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
        if let Err(e) = extract_part(input, &part, *start, *end, mode).await {
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
            "-map_chapters",
            "-1",
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

/// `-ss` value for an input seek. Six decimals: rounding a keyframe pts such as
/// 20.0213 to "20.021" would land the copy seek on the *previous* keyframe and
/// silently keep a whole extra GOP (the old ~2 s drift).
pub(crate) fn format_seek(t: f64) -> String {
    format!("{:.6}", t.max(0.0))
}

fn extract_args(input: &str, part: &str, start: f64, end: f64, mode: CutMode) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-ss".into(),
        format_seek(start),
        "-i".into(),
        input.into(),
        "-t".into(),
        format!("{:.6}", (end - start).max(0.0)),
        "-map_chapters".into(),
        "-1".into(),
    ];
    match mode {
        CutMode::Keyframe => args.extend(
            [
                "-map", "0", "-c", "copy", "-avoid_negative_ts", "make_zero",
            ]
            .map(String::from),
        ),
        CutMode::Accurate => args.extend(
            [
                "-map", "0:v:0", "-map", "0:a?", "-c:v", "libx264", "-preset", "veryfast",
                "-crf", "18", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k",
                "-avoid_negative_ts", "make_zero",
            ]
            .map(String::from),
        ),
    }
    args.push(part.into());
    args
}

async fn extract_part(input: &Path, part: &Path, start: f64, end: f64, mode: CutMode) -> Result<()> {
    if end <= start {
        bail!("invalid keep range [{}, {})", start, end);
    }
    let args = extract_args(
        &input.to_string_lossy(),
        &part.to_string_lossy(),
        start,
        end,
        mode,
    );
    debug!("ffmpeg extract part ({:?}) {:?}", mode, args);

    let output = tokio::process::Command::new(resolve_media_tool_path("ffmpeg"))
        .args(&args)
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


/// Probe video keyframe timestamps (seconds) via ffprobe.
///
/// Uses `-skip_frame nokey` so only keyframes are decoded/listed. Empty Vec on
/// soft failure is fine — callers treat that as "do not snap".
pub async fn probe_video_keyframe_times(path: &Path) -> Result<Vec<f64>> {
    if !path.exists() {
        bail!("media missing for keyframe probe: {}", path.display());
    }
    let path_str = path.to_string_lossy().to_string();
    let output = tokio::process::Command::new(resolve_media_tool_path("ffprobe"))
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
            &path_str,
        ])
        .output()
        .await
        .context("spawn ffprobe keyframe probe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe keyframe probe error: {}", stderr.trim());
    }

    let times: Vec<f64> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let t = line.trim().parse::<f64>().ok()?;
            (t.is_finite() && t >= 0.0).then_some(t)
        })
        .collect();
    // ffmpeg input `-ss` is relative to the container start_time, while frame
    // pts_time is absolute. Bilibili DASH merges often start at 0.02–1.5 s, so
    // without this shift every snapped seek was off by start_time.
    let start_time = probe_format_start_time(path).await.unwrap_or(0.0);
    let mut times: Vec<f64> = times
        .into_iter()
        .map(|t| t - start_time)
        .filter(|t| *t > -1e-3)
        .map(|t| t.max(0.0))
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    debug!(
        "ffprobe keyframes: {} entries for {}",
        times.len(),
        path.display()
    );
    Ok(times)
}

/// Container `format=start_time` in seconds (0 when absent).
async fn probe_format_start_time(path: &Path) -> Result<f64> {
    let output = tokio::process::Command::new(resolve_media_tool_path("ffprobe"))
        .args(["-v", "error", "-show_entries", "format=start_time", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .output()
        .await
        .context("spawn ffprobe start_time probe")?;
    let raw = String::from_utf8_lossy(&output.stdout);
    Ok(raw.trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0))
}

/// Probe container duration in seconds (`format=duration`).
pub async fn probe_media_duration_secs(path: &Path) -> Result<f64> {
    if !path.exists() {
        bail!("media missing for duration probe: {}", path.display());
    }
    let path_str = path.to_string_lossy().to_string();
    let output = tokio::process::Command::new(resolve_media_tool_path("ffprobe"))
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            &path_str,
        ])
        .output()
        .await
        .context("spawn ffprobe duration probe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe duration probe error: {}", stderr.trim());
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let dur: f64 = raw
        .trim()
        .parse()
        .with_context(|| format!("parse ffprobe duration {:?}", raw.trim()))?;
    if !(dur.is_finite() && dur > 0.0) {
        bail!("invalid ffprobe duration: {}", dur);
    }
    Ok(dur)
}

/// Atomically replace `target` with `new_file`. The uncut original is not kept:
/// a second video beside the cut one made Emby show an empty item.
pub async fn atomic_replace(target: &Path, new_file: &Path) -> Result<()> {
    if target.exists() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_keeps_sub_millisecond_precision() {
        assert_eq!(format_seek(20.021333), "20.021333");
        assert_eq!(format_seek(-1.0), "0.000000");
    }

    #[test]
    fn keyframe_mode_copies_streams() {
        let a = extract_args("in.mp4", "out.mp4", 20.021333, 80.0, CutMode::Keyframe);
        assert!(a.windows(2).any(|w| w == ["-ss", "20.021333"]));
        assert!(a.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(!a.iter().any(|x| x == "-break_non_keyframes"));
    }

    #[test]
    fn accurate_mode_reencodes() {
        let a = extract_args("in.mp4", "out.mp4", 1.0, 2.0, CutMode::Accurate);
        assert!(a.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert_eq!(CutMode::from_config("Accurate"), CutMode::Accurate);
        assert_eq!(CutMode::from_config("whatever"), CutMode::Keyframe);
    }
}
