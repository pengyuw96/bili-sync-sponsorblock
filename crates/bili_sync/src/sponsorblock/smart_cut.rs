//! BETA「智能裁剪」(smart cut) for SponsorBlock.
//!
//! Stable mode snaps every keep boundary to a keyframe and stream-copies.
//! Smart mode keeps the *exact* boundaries:
//!
//! * For each keep `[s, e)` the video is split into pieces:
//!   - `Encode [s, K1)` where `K1` is the first keyframe ≥ `s` (only when `s`
//!     is not already on a keyframe),
//!   - `Copy   [K1, K2)` where `K2` is the last keyframe ≤ `e`,
//!   - `Encode [K2, e)` (only when `e` is not on a keyframe / not the file end).
//!   A keep shorter than one GOP is re-encoded completely.
//! * Re-encoded pieces use libx264 (veryfast, `-threads 2`, `nice -n 10`)
//!   with the source profile / level / pix_fmt / resolution. Every piece is
//!   written as MPEG-TS (Annex B, SPS/PPS in-band) so concat of copied and
//!   re-encoded pieces decodes cleanly; the result is remuxed to the original
//!   container with the source video timescale.
//! * Audio is re-encoded once for all keeps with sample-accurate `atrim`.
//! * Only H.264 (8-bit) is supported. Anything else, or any validation error
//!   (ffprobe failure, concat failure, duration mismatch, decode errors at the
//!   joins) returns `Err` and the caller falls back to the stable cut.
//! * A global semaphore allows one smart cut at a time.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info};

/// Only one smart cut (CPU heavy) at a time across the whole process.
static SMART_CUT_GATE: once_cell::sync::Lazy<tokio::sync::Semaphore> =
    once_cell::sync::Lazy::new(|| tokio::sync::Semaphore::new(1));

/// Duration tolerance (seconds) between the expected keep total and the result.
const DURATION_TOLERANCE_SECS: f64 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PieceKind {
    Copy,
    Encode,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Piece {
    pub start: f64,
    pub end: f64,
    pub kind: PieceKind,
}

#[derive(Debug, Clone, Default)]
pub struct SmartCutStats {
    pub pieces: usize,
    pub encoded_secs: f64,
    pub copied_secs: f64,
    pub output_duration_secs: f64,
}

/// Plan copy / encode pieces for exact keep ranges (pure; unit tested).
///
/// `keyframes` are seconds on the same clock as `-ss` (format start_time
/// subtracted). `frame` is one frame duration; boundaries within half a frame
/// of a keyframe count as "on the keyframe".
pub fn plan_pieces(keep: &[(f64, f64)], keyframes: &[f64], frame: f64, duration: f64) -> Vec<Piece> {
    let eps = (frame * 0.5).max(1e-4);
    let mut out = Vec::new();
    fn push(out: &mut Vec<Piece>, start: f64, end: f64, kind: PieceKind, eps: f64) {
        if end - start >= eps {
            out.push(Piece { start, end, kind });
        }
    }
    for &(s, e) in keep {
        if e - s < eps {
            continue;
        }
        let at_file_end = e >= duration - eps;
        // First keyframe at/after s (a keyframe within half a frame before s counts).
        let k1 = keyframes.iter().copied().find(|k| *k >= s - eps);
        // Last keyframe at/before e.
        let k2 = keyframes.iter().rev().copied().find(|k| *k <= e + eps);
        let usable = match (k1, k2) {
            (Some(k1), Some(k2)) => k1 < e - eps && (k2 > k1 + eps || at_file_end),
            _ => false,
        };
        if !usable {
            push(&mut out, s, e, PieceKind::Encode, eps);
            continue;
        }
        let (k1, k2) = (k1.unwrap(), k2.unwrap());
        // Copy pieces must start exactly on a keyframe (input seek lands there).
        let copy_from = k1;
        if k1 - s > eps {
            push(&mut out, s, k1, PieceKind::Encode, eps);
        }
        if at_file_end {
            push(&mut out, copy_from, e, PieceKind::Copy, eps);
        } else if (e - k2).abs() <= eps {
            push(&mut out, copy_from, k2, PieceKind::Copy, eps);
        } else {
            push(&mut out, copy_from, k2, PieceKind::Copy, eps);
            push(&mut out, k2, e, PieceKind::Encode, eps);
        }
    }
    out
}

fn tool(name: &str) -> PathBuf {
    #[cfg(test)]
    {
        PathBuf::from(name)
    }
    #[cfg(not(test))]
    {
        crate::downloader::resolve_media_tool_path(name)
    }
}

/// ffmpeg wrapped in `nice -n 10` when a `nice` binary exists.
fn low_prio_ffmpeg() -> Command {
    let ffmpeg = tool("ffmpeg");
    for nice in ["/usr/bin/nice", "/bin/nice"] {
        if Path::new(nice).exists() {
            let mut c = Command::new(nice);
            c.arg("-n").arg("10").arg(ffmpeg);
            return c;
        }
    }
    Command::new(ffmpeg)
}

async fn run(mut cmd: Command, what: &str) -> Result<String> {
    let out = cmd.output().await.with_context(|| format!("spawn {}", what))?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        bail!("{} failed: {}", what, stderr.trim());
    }
    Ok(stderr)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VideoParams {
    codec: String,
    profile: String,
    level: i64,
    pix_fmt: String,
    width: i64,
    height: i64,
    frame: f64,
    timescale: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AudioParams {
    codec: String,
    bit_rate: Option<u64>,
    sample_rate: Option<u64>,
    channels: Option<u64>,
}

fn parse_rate(r: &str) -> Option<f64> {
    let (n, d) = r.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then_some(n / d)
}

async fn probe_streams(path: &Path) -> Result<(VideoParams, Option<AudioParams>, f64)> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-show_entries"])
        .arg("stream=codec_type,codec_name,profile,level,pix_fmt,width,height,r_frame_rate,avg_frame_rate,time_base,bit_rate,sample_rate,channels:format=start_time")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .await
        .context("spawn ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe streams: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe json")?;
    let start = v["format"]["start_time"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let vs = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .context("no video stream")?;
    let s = |k: &str| vs[k].as_str().unwrap_or("").to_string();
    let frame = parse_rate(&s("avg_frame_rate"))
        .or_else(|| parse_rate(&s("r_frame_rate")))
        .map(|fps| 1.0 / fps)
        .context("unknown frame rate")?;
    let timescale = s("time_base").split_once('/').and_then(|(_, d)| d.parse().ok());
    let video = VideoParams {
        codec: s("codec_name"),
        profile: s("profile"),
        level: vs["level"].as_i64().unwrap_or(0),
        pix_fmt: s("pix_fmt"),
        width: vs["width"].as_i64().unwrap_or(0),
        height: vs["height"].as_i64().unwrap_or(0),
        frame,
        timescale,
    };
    let audio = streams.iter().find(|s| s["codec_type"] == "audio").map(|a| AudioParams {
        codec: a["codec_name"].as_str().unwrap_or("").to_string(),
        bit_rate: a["bit_rate"].as_str().and_then(|x| x.parse().ok()),
        sample_rate: a["sample_rate"].as_str().and_then(|x| x.parse().ok()),
        channels: a["channels"].as_u64(),
    });
    Ok((video, audio, start))
}

fn x264_profile(p: &str) -> Option<&'static str> {
    match p.trim().to_ascii_lowercase().as_str() {
        "high" => Some("high"),
        "main" => Some("main"),
        "baseline" | "constrained baseline" => Some("baseline"),
        _ => None,
    }
}

fn audio_encoder(codec: &str) -> Option<&'static str> {
    match codec {
        "aac" => Some("aac"),
        "flac" => Some("flac"),
        "eac3" => Some("eac3"),
        "ac3" => Some("ac3"),
        "mp3" => Some("libmp3lame"),
        "opus" => Some("libopus"),
        _ => None,
    }
}

pub(crate) fn encode_args(v: &VideoParams) -> Result<Vec<String>> {
    if v.codec != "h264" {
        bail!("smart cut supports H.264 only (got {})", v.codec);
    }
    let profile = x264_profile(&v.profile)
        .with_context(|| format!("unsupported H.264 profile {:?}", v.profile))?;
    if !matches!(v.pix_fmt.as_str(), "yuv420p" | "yuvj420p") {
        bail!("unsupported pix_fmt {}", v.pix_fmt);
    }
    let mut a: Vec<String> = [
        "-c:v", "libx264", "-preset", "veryfast", "-threads", "2", "-crf", "18", "-profile:v", profile,
    ]
    .map(String::from)
    .to_vec();
    if v.level > 0 {
        a.push("-level".into());
        a.push(format!("{}.{}", v.level / 10, v.level % 10));
    }
    a.extend(["-pix_fmt".into(), v.pix_fmt.replace("yuvj", "yuv"), "-s".into(), format!("{}x{}", v.width, v.height)]);
    Ok(a)
}

/// Smart-cut `input` into `output` keeping exactly `keep`.
pub async fn smart_cut(
    input: &Path,
    output: &Path,
    keep: &[(f64, f64)],
    keyframes: &[f64],
    duration: f64,
) -> Result<SmartCutStats> {
    if keep.is_empty() {
        bail!("no keep ranges");
    }
    let (video, audio, start_time) = probe_streams(input).await?;
    let enc = encode_args(&video)?;
    let audio_enc = match &audio {
        Some(a) => Some(audio_encoder(&a.codec).with_context(|| format!("unsupported audio codec {}", a.codec))?),
        None => None,
    };
    if keyframes.is_empty() {
        bail!("no keyframes");
    }
    let pieces = plan_pieces(keep, keyframes, video.frame, duration);
    if pieces.is_empty() {
        bail!("empty plan");
    }
    let _gate = SMART_CUT_GATE.acquire().await.context("smart cut gate")?;

    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let tmp = parent.join(format!(".bili-sync-smartcut-{}-{}", std::process::id(), ts));
    fs::create_dir_all(&tmp).await?;
    let res = smart_cut_inner(input, output, keep, &pieces, &video, audio.as_ref(), audio_enc, &enc, start_time, &tmp).await;
    let _ = fs::remove_dir_all(&tmp).await;
    res
}

#[allow(clippy::too_many_arguments)]
async fn smart_cut_inner(
    input: &Path,
    output: &Path,
    keep: &[(f64, f64)],
    pieces: &[Piece],
    video: &VideoParams,
    audio: Option<&AudioParams>,
    audio_enc: Option<&str>,
    enc: &[String],
    start_time: f64,
    tmp: &Path,
) -> Result<SmartCutStats> {
    let mut stats = SmartCutStats { pieces: pieces.len(), ..Default::default() };
    let mut list = String::new();
    for (i, p) in pieces.iter().enumerate() {
        let part = tmp.join(format!("v{:03}.ts", i));
        let mut c = low_prio_ffmpeg();
        c.args(["-v", "error", "-y", "-ss", &format!("{:.6}", p.start)])
            .arg("-i")
            .arg(input)
            .args(["-t", &format!("{:.6}", p.end - p.start)])
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1"]);
        match p.kind {
            PieceKind::Copy => {
                c.args(["-c:v", "copy", "-bsf:v", "h264_mp4toannexb"]);
                stats.copied_secs += p.end - p.start;
            }
            PieceKind::Encode => {
                c.args(enc);
                stats.encoded_secs += p.end - p.start;
            }
        }
        c.args(["-f", "mpegts"]).arg(&part);
        run(c, &format!("smart cut piece {} {:?}", i, p.kind)).await?;
        list.push_str(&format!("file '{}'\n", part.to_string_lossy().replace('\'', "'\\''")));
    }
    let list_path = tmp.join("list.txt");
    fs::write(&list_path, list).await?;

    let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_ascii_lowercase();
    let mut inputs: Vec<std::ffi::OsString> = vec!["-f".into(), "concat".into(), "-safe".into(), "0".into(), "-i".into(), list_path.clone().into()];
    let mut maps: Vec<String> = vec!["-map".into(), "0:v:0".into()];
    if let (Some(a), Some(aenc)) = (audio, audio_enc) {
        let audio_path = tmp.join("audio.mka");
        let mut graph = String::new();
        for (i, (s, e)) in keep.iter().enumerate() {
            graph.push_str(&format!(
                "[0:a:0]atrim=start={:.6}:end={:.6},asetpts=PTS-STARTPTS[a{}];",
                s + start_time,
                e + start_time,
                i
            ));
        }
        for i in 0..keep.len() {
            graph.push_str(&format!("[a{}]", i));
        }
        graph.push_str(&format!("concat=n={}:v=0:a=1[aout]", keep.len()));
        let mut c = low_prio_ffmpeg();
        c.args(["-v", "error", "-y", "-copyts"]).arg("-i").arg(input)
            .args(["-filter_complex", &graph, "-map", "[aout]", "-c:a", aenc, "-threads", "2"]);
        if aenc != "flac" {
            c.args(["-b:a", &format!("{}", a.bit_rate.unwrap_or(192_000).max(64_000))]);
        }
        if let Some(sr) = a.sample_rate {
            c.args(["-ar", &sr.to_string()]);
        }
        if let Some(ch) = a.channels {
            c.args(["-ac", &ch.to_string()]);
        }
        c.arg(&audio_path);
        run(c, "smart cut audio").await?;
        inputs.extend(["-i".into(), audio_path.into()]);
        maps.extend(["-map".into(), "1:a:0".into()]);
    }
    let muxed = tmp.join(format!("out.{}", ext));
    let mut c = Command::new(tool("ffmpeg"));
    c.args(["-v", "error", "-y"]).args(&inputs).args(&maps)
        .args(["-c", "copy", "-map_metadata", "-1", "-map_chapters", "-1"]);
    if matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
        c.args(["-movflags", "+faststart"]);
        if let Some(tsc) = video.timescale {
            c.args(["-video_track_timescale", &tsc.to_string()]);
        }
    }
    c.arg(&muxed);
    run(c, "smart cut concat/mux").await?;

    // Validate: duration and clean decode around every join.
    let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
    let got = probe_video_stream_duration(&muxed).await?;
    let tol = DURATION_TOLERANCE_SECS.max(video.frame * 2.0);
    if (got - expected).abs() > tol {
        bail!("duration mismatch: expected {:.3}s got {:.3}s", expected, got);
    }
    let mut t = 0.0;
    let mut joins = Vec::new();
    for p in pieces.iter().take(pieces.len().saturating_sub(1)) {
        t += p.end - p.start;
        joins.push(t);
    }
    for j in joins {
        let mut c = Command::new(tool("ffmpeg"));
        c.args(["-v", "error", "-ss", &format!("{:.3}", (j - 1.0).max(0.0))]).arg("-i").arg(&muxed)
            .args(["-t", "2", "-map", "0:v:0", "-f", "null", "-"]);
        let err = run(c, "smart cut decode check").await?;
        if !err.trim().is_empty() {
            bail!("decode errors near join {:.3}s: {}", j, err.trim());
        }
    }
    stats.output_duration_secs = got;
    if let Some(p) = output.parent() {
        fs::create_dir_all(p).await.ok();
    }
    if fs::rename(&muxed, output).await.is_err() {
        fs::copy(&muxed, output).await.context("move smart cut output")?;
    }
    info!(
        "SponsorBlock smart cut (beta): {} pieces, re-encoded {:.2}s, copied {:.2}s → {:.3}s {}",
        stats.pieces, stats.encoded_secs, stats.copied_secs, got, output.display()
    );
    debug!("smart cut pieces: {:?}", pieces);
    Ok(stats)
}

async fn probe_video_stream_duration(path: &Path) -> Result<f64> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=duration:format=duration", "-of", "json"])
        .arg(path)
        .output()
        .await?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe json")?;
    let d = v["streams"][0]["duration"].as_str().and_then(|s| s.parse::<f64>().ok())
        .or_else(|| v["format"]["duration"].as_str().and_then(|s| s.parse().ok()))
        .context("no duration")?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: f64 = 1.0 / 30.0;
    fn kfs() -> Vec<f64> {
        (0..10).map(|i| i as f64 * 2.0).collect()
    }

    #[test]
    fn plan_head_copy_tail() {
        let p = plan_pieces(&[(3.1, 9.3)], &kfs(), F, 20.0);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].start, p[0].end, p[0].kind), (3.1, 4.0, PieceKind::Encode));
        assert_eq!((p[1].start, p[1].end, p[1].kind), (4.0, 8.0, PieceKind::Copy));
        assert_eq!((p[2].start, p[2].end, p[2].kind), (8.0, 9.3, PieceKind::Encode));
    }

    #[test]
    fn plan_boundaries_on_keyframes_are_pure_copy() {
        let p = plan_pieces(&[(0.0, 6.0), (10.0, 20.0)], &kfs(), F, 20.0);
        assert!(p.iter().all(|x| x.kind == PieceKind::Copy), "{:?}", p);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].end, 20.0);
    }

    #[test]
    fn plan_within_half_frame_counts_as_keyframe() {
        let p = plan_pieces(&[(4.01, 8.0)], &kfs(), F, 20.0);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, PieceKind::Copy);
    }

    #[test]
    fn plan_short_keep_inside_gop_is_fully_encoded() {
        let p = plan_pieces(&[(4.5, 5.5)], &kfs(), F, 20.0);
        assert_eq!(p, vec![Piece { start: 4.5, end: 5.5, kind: PieceKind::Encode }]);
    }

    #[test]
    fn plan_tail_to_file_end_is_copied() {
        let p = plan_pieces(&[(9.37, 19.97)], &kfs(), F, 19.97);
        assert_eq!(p[0], Piece { start: 9.37, end: 10.0, kind: PieceKind::Encode });
        assert_eq!(p[1], Piece { start: 10.0, end: 19.97, kind: PieceKind::Copy });
        let total: f64 = p.iter().map(|x| x.end - x.start).sum();
        assert!((total - (19.97 - 9.37)).abs() < 1e-9);
    }

    #[test]
    fn plan_preserves_exact_total() {
        let keep = [(0.0, 5.1), (9.37, 14.2), (15.05, 20.0)];
        let p = plan_pieces(&keep, &kfs(), F, 20.0);
        let total: f64 = p.iter().map(|x| x.end - x.start).sum();
        let want: f64 = keep.iter().map(|(s, e)| e - s).sum();
        assert!((total - want).abs() < 1e-9);
    }

    #[test]
    fn encode_args_reject_hevc_and_high10() {
        let mut v = VideoParams { codec: "hevc".into(), profile: "Main".into(), pix_fmt: "yuv420p".into(), ..Default::default() };
        assert!(encode_args(&v).is_err());
        v.codec = "h264".into();
        v.profile = "High 10".into();
        assert!(encode_args(&v).is_err());
        v.profile = "High".into();
        v.level = 40;
        v.width = 1920;
        v.height = 1080;
        let a = encode_args(&v).unwrap();
        assert!(a.windows(2).any(|w| w == ["-level", "4.0"]));
        assert!(a.windows(2).any(|w| w == ["-threads", "2"]));
        assert!(a.windows(2).any(|w| w == ["-preset", "veryfast"]));
    }

    // ---------- integration (needs ffmpeg with libx264 on PATH) ----------

    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx264"))
            .unwrap_or(false)
    }

    /// Children CPU time (user+sys, seconds) from /proc/self/stat.
    fn children_cpu_secs() -> f64 {
        let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let rest = s.rsplit_once(')').map(|x| x.1).unwrap_or("");
        let f: Vec<&str> = rest.split_whitespace().collect();
        // fields after ')' start at index 0 = state(3); cutime=16, cstime=17 (1-based) → 13,14
        let tick = 100.0;
        let g = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
        (g(13) + g(14)) / tick
    }

    fn gen(path: &Path, vcodec: &[&str], size: &str) {
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc2=size={}:rate=30", size)])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
            .args(["-t", "20"])
            .args(vcodec)
            .args(["-c:a", "aac", "-b:a", "128k", "-shortest"])
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
    }

    async fn stream_durations(p: &Path) -> (f64, f64, u64) {
        let o = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries", "stream=codec_type,duration,nb_read_frames", "-of", "json"])
            .arg(p)
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        let mut vd = 0.0;
        let mut ad = 0.0;
        let mut n = 0;
        for s in v["streams"].as_array().unwrap() {
            let d: f64 = s["duration"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0.0);
            if s["codec_type"] == "video" {
                vd = d;
                n = s["nb_read_frames"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0);
            } else if s["codec_type"] == "audio" {
                ad = d;
            }
        }
        (vd, ad, n)
    }

    #[tokio::test]
    async fn integration_smart_cut_h264_1080p_frame_accurate() {
        if !have_ffmpeg() {
            eprintln!("skip: ffmpeg/libx264 missing");
            return;
        }
        let dir = tempfile_dir("smartcut-h264");
        let input = dir.join("in.mp4");
        gen(&input, &["-c:v", "libx264", "-preset", "ultrafast", "-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-pix_fmt", "yuv420p"], "1920x1080");
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        assert!(kfs.len() >= 9, "{:?}", kfs);
        // boundaries deliberately off-keyframe (GOP = 2 s)
        let keep = vec![(0.0, 5.1), (9.4, 14.3), (15.5, 20.0)];
        let out = dir.join("out.mp4");
        let cpu0 = children_cpu_secs();
        let wall = std::time::Instant::now();
        let stats = smart_cut(&input, &out, &keep, &kfs, 20.0).await.expect("smart cut");
        let cpu = children_cpu_secs() - cpu0;
        eprintln!(
            "SMARTCUT_METRICS 1080p: cpu={:.2}s wall={:.2}s encoded={:.2}s copied={:.2}s pieces={}",
            cpu, wall.elapsed().as_secs_f64(), stats.encoded_secs, stats.copied_secs, stats.pieces
        );
        let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
        let (vd, ad, frames) = stream_durations(&out).await;
        assert!((vd - expected).abs() <= F + 1e-3, "video {} vs {}", vd, expected);
        let want_frames = (expected / F).round() as i64;
        assert!((frames as i64 - want_frames).abs() <= 1, "frames {} vs {}", frames, want_frames);
        assert!((ad - vd).abs() < 0.05, "A/V desync: audio {} video {}", ad, vd);
        let dec = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(dec.status.success());
        assert!(String::from_utf8_lossy(&dec.stderr).trim().is_empty(), "decode errors: {}", String::from_utf8_lossy(&dec.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn integration_non_h264_falls_back() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile_dir("smartcut-mpeg4");
        let input = dir.join("in.mp4");
        gen(&input, &["-c:v", "mpeg4", "-g", "60", "-q:v", "5"], "320x180");
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        let keep = vec![(0.0, 5.1), (9.4, 20.0)];
        let out = dir.join("out.mp4");
        let err = smart_cut(&input, &out, &keep, &kfs, 20.0).await.unwrap_err();
        assert!(format!("{:#}", err).contains("H.264 only"), "{:#}", err);
        assert!(!out.exists());
        // the stable path still works on this file (what the caller falls back to)
        let snapped = super::super::timeline::snap_keep_ranges_to_keyframes(&keep, &kfs);
        super::super::ffmpeg_cut::cut_out_segments_with_ffmpeg(&input, &out, &snapped, super::super::ffmpeg_cut::CutMode::Keyframe).await.expect("stable cut");
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn integration_hevc_falls_back() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile_dir("smartcut-hevc");
        let input = dir.join("in.mp4");
        gen(&input, &["-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "keyint=60:log-level=error"], "320x180");
        let err = smart_cut(&input, &dir.join("o.mp4"), &[(0.0, 5.1)], &[0.0, 2.0, 4.0], 20.0).await.unwrap_err();
        assert!(format!("{:#}", err).contains("H.264 only"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{}-{}-{}", tag, std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
