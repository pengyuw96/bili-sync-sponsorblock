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


/// Probe video keyframe timestamps (seconds) via ffprobe packet flags.
///
/// Reads packet `flags` (K = keyframe) instead of decoding with `-skip_frame nokey`.
/// The old frame/CSV approach broke on ffmpeg 6.1 + Bilibili HEVC mp4: frames with
/// side data print extra CSV columns (`0.000000,`) so the float parse dropped them.
/// Uses `compact` key=value output so extra fields/side data never shift columns.
/// Falls back to `dts_time` when `pts_time` is N/A. Empty Vec = "do not snap".
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
            "-show_entries",
            "packet=pts_time,dts_time,flags",
            "-of",
            "compact=p=1:nk=0",
            &path_str,
        ])
        .output()
        .await
        .context("spawn ffprobe keyframe probe")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe keyframe probe error: {}", stderr.trim());
    }
    let times = parse_keyframe_packets(&String::from_utf8_lossy(&output.stdout));
    debug!("ffprobe keyframes: {} entries for {}", times.len(), path.display());
    Ok(times)
}

/// Keyframes plus the earliest PTS of their open-GOP *leading* pictures
/// (packets after the keyframe in decode order but displayed before it — H.264
/// open-gop B-frames, HEVC RASL after CRA). `lead == key` for closed GOP.
pub async fn probe_keyframe_index(path: &Path) -> Result<Vec<(f64, f64)>> {
    let output = tokio::process::Command::new(resolve_media_tool_path("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,dts_time,flags", "-of", "compact=p=1:nk=0"])
        .arg(path)
        .output()
        .await
        .context("spawn ffprobe keyframe index")?;
    if !output.status.success() {
        bail!("ffprobe keyframe index error: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(parse_keyframe_index(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_packet_line(line: &str) -> Option<(f64, bool)> {
    let (mut pts, mut dts, mut key) = (None, None, false);
    for kv in line.trim().split('|') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "pts_time" => pts = v.trim().parse::<f64>().ok(),
            "dts_time" => dts = v.trim().parse::<f64>().ok(),
            "flags" => key = v.starts_with('K'),
            _ => {}
        }
    }
    let t = pts.or(dts)?;
    t.is_finite().then_some((t, key))
}

/// Decode-order packet list → `(key_pts, lead_start)` sorted by key_pts.
pub(crate) fn parse_keyframe_index(out: &str) -> Vec<(f64, f64)> {
    let mut idx: Vec<(f64, f64)> = Vec::new();
    for (t, key) in out.lines().filter_map(parse_packet_line) {
        if key {
            idx.push((t, t));
        } else if let Some(last) = idx.last_mut() {
            if t < last.0 - 1e-6 && t < last.1 {
                last.1 = t;
            }
        }
    }
    idx.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    idx.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-6);
    idx
}

/// Parse `ffprobe -show_entries packet=pts_time,dts_time,flags -of compact` output.
pub(crate) fn parse_keyframe_packets(out: &str) -> Vec<f64> {
    let mut times: Vec<f64> = out
        .lines()
        .filter_map(parse_packet_line)
        .filter(|(_, k)| *k)
        .map(|(t, _)| t.max(0.0))
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    times
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
    fn keyframe_packet_parse_compact_and_na() {
        let out = "packet|pts_time=0.000000|dts_time=-0.066733|flags=K__\n\
                   packet|pts_time=0.033367|dts_time=0.000000|flags=___\n\
                   packet|pts_time=N/A|dts_time=4.004000|flags=K__|side_data|\n\
                   packet|pts_time=8.008000|dts_time=7.941267|flags=K_D\n\
                   garbage line,\n";
        assert_eq!(parse_keyframe_packets(out), vec![0.0, 4.004, 8.008]);
    }

    #[test]
    fn keyframe_index_finds_open_gop_leading_pictures() {
        let out = "packet|pts_time=0.000000|dts_time=-0.03|flags=K__\n\
                   packet|pts_time=0.066733|dts_time=-0.01|flags=___\n\
                   packet|pts_time=7.507500|dts_time=7.42|flags=K__\n\
                   packet|pts_time=7.474133|dts_time=7.44|flags=___\n\
                   packet|pts_time=7.457450|dts_time=7.45|flags=___\n\
                   packet|pts_time=7.574233|dts_time=7.49|flags=___\n";
        let idx = parse_keyframe_index(out);
        assert_eq!(idx, vec![(0.0, 0.0), (7.5075, 7.45745)]);
    }

    /// Bilibili-style HEVC mp4 (hvc1, faststart, B-frames, AAC): keyframes must be
    /// found (was empty on ffmpeg 6.1.2 with the old `-skip_frame nokey` CSV parse)
    /// and the stable path must snap cut-ins onto them.
    #[tokio::test]
    async fn integration_keyframes_hevc_bilibili_style_and_stable_snaps() {
        let has_x265 = std::process::Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx265")).unwrap_or(false);
        if !has_x265 { eprintln!("skip: no libx265"); return; }
        let dir = std::env::temp_dir().join(format!("kf-hevc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("bili.mp4");
        let st = std::process::Command::new("ffmpeg").args(["-v", "error", "-y",
            "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30000/1001:d=20",
            "-f", "lavfi", "-i", "sine=d=20",
            "-c:v", "libx265", "-x265-params", "keyint=120:min-keyint=120:bframes=4:open-gop=0:log-level=error",
            "-tag:v", "hvc1", "-c:a", "aac", "-movflags", "+faststart"]).arg(&f).status().unwrap();
        assert!(st.success());
        let kfs = probe_video_keyframe_times(&f).await.unwrap();
        assert_eq!(kfs.len(), 5, "keyframes {:?}", kfs);
        assert!((kfs[1] - 4.004).abs() < 0.01);
        let snapped = crate::sponsorblock::timeline::snap_keep_ranges_to_keyframes(&[(0.0, 5.0), (9.3, 20.0)], &kfs);
        assert!(snapped.iter().any(|(s, _)| (s - 8.008).abs() < 0.01), "not snapped: {:?}", snapped);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
