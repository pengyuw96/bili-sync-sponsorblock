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
//! * Re-encoded pieces match the source codec:
//!   - H.264: `libx264` veryfast, `-threads 2`
//!   - H.265: prefer `hevc_qsv` → `hevc_vaapi` → `libx265` veryfast (`-threads 2`
//!     on software only). All under `nice -n 10` when available.
//!   Profile / level / pix_fmt / resolution are taken from the source. Pieces
//!   are written as MPEG-TS (Annex B + in-band VPS/SPS/PPS) then remuxed.
//! * Audio is re-encoded once for all keeps with sample-accurate `atrim`.
//! * Supported: H.264 8-bit (High/Main/Baseline yuv420p) and H.265 8-bit Main
//!   yuv420p. Main10 / 10-bit / other codecs → `Err` → stable fallback.
//! * Open-GOP HEVC: mid stream-copy often leaves dangling refs; the join
//!   decode check catches that and the caller falls back to stable. Closed-GOP
//!   HEVC (common after x265 `open-gop=0`, and many remuxes) works.
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
    /// Encoder used for re-encode pieces, e.g. libx264 / libx265 / hevc_qsv.
    pub encoder: String,
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

fn hevc_profile_ok(p: &str) -> bool {
    matches!(p.trim().to_ascii_lowercase().as_str(), "main" | "main still picture")
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

fn annexb_bsf(codec: &str) -> &'static str {
    if codec == "hevc" {
        "hevc_mp4toannexb"
    } else {
        "h264_mp4toannexb"
    }
}

#[derive(Debug, Clone)]
struct EncodePlan {
    name: String,
    /// Args inserted after `-map …` and before output path (codec + opts).
    args: Vec<String>,
    /// Extra args that must appear *before* `-i` (VAAPI device).
    pre_input: Vec<String>,
}

fn dri_render_nodes() -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/dri") {
        for e in rd.flatten() {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if s.starts_with("renderD") {
                nodes.push(e.path());
            }
        }
    }
    nodes.sort();
    nodes
}

fn ffmpeg_lists_encoder(name: &str) -> bool {
    std::process::Command::new(tool("ffmpeg"))
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(name))
        .unwrap_or(false)
}

/// Tiny probe encode; returns true only when ffmpeg exits 0 and writes a file.
async fn probe_encoder(plan: &EncodePlan, size: (i64, i64)) -> bool {
    let dir = std::env::temp_dir().join(format!(
        "bili-sync-encprobe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("t.ts");
    let mut c = low_prio_ffmpeg();
    c.args(["-v", "error", "-y"]);
    for a in &plan.pre_input {
        c.arg(a);
    }
    c.args([
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={}x{}:rate=30", size.0.max(16), size.1.max(16)),
        "-t",
        "0.2",
        "-an",
    ]);
    for a in &plan.args {
        c.arg(a);
    }
    c.args(["-f", "mpegts"]).arg(&out);
    let ok = c
        .output()
        .await
        .map(|o| o.status.success() && out.exists() && out.metadata().map(|m| m.len() > 0).unwrap_or(false))
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

fn h264_software_plan(v: &VideoParams) -> Result<EncodePlan> {
    let profile = x264_profile(&v.profile)
        .with_context(|| format!("unsupported H.264 profile {:?}", v.profile))?;
    if !matches!(v.pix_fmt.as_str(), "yuv420p" | "yuvj420p") {
        bail!("unsupported pix_fmt {}", v.pix_fmt);
    }
    let mut args: Vec<String> = [
        "-c:v", "libx264", "-preset", "veryfast", "-threads", "2", "-crf", "18", "-profile:v", profile,
    ]
    .map(String::from)
    .to_vec();
    if v.level > 0 {
        args.push("-level".into());
        args.push(format!("{}.{}", v.level / 10, v.level % 10));
    }
    args.extend([
        "-pix_fmt".into(),
        v.pix_fmt.replace("yuvj", "yuv"),
        "-s".into(),
        format!("{}x{}", v.width, v.height),
    ]);
    Ok(EncodePlan {
        name: "libx264".into(),
        args,
        pre_input: Vec::new(),
    })
}

fn hevc_software_plan(v: &VideoParams) -> Result<EncodePlan> {
    if !hevc_profile_ok(&v.profile) {
        bail!("unsupported H.265 profile {:?} (need Main 8-bit)", v.profile);
    }
    if v.pix_fmt != "yuv420p" {
        bail!("unsupported H.265 pix_fmt {} (need yuv420p)", v.pix_fmt);
    }
    // Closed GOP + repeat headers so MPEG-TS concat of encode+copy pieces can
    // re-sync VPS/SPS/PPS at each join. open-gop=0 on *our* pieces only; the
    // stream-copied mid still comes from the source.
    // Do not forward ffprobe's `level` as x265 level-idc: HEVC level_idc values
    // (e.g. 120 = Level 4.0) are not the same numbers ffprobe always exposes, and
    // a bad value makes the encode fail hard. Resolution + profile are enough.
    let x265 = "open-gop=0:repeat-headers=1:log-level=error".to_string();
    let args: Vec<String> = [
        "-c:v",
        "libx265",
        "-preset",
        "veryfast",
        "-threads",
        "2",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-s",
        &format!("{}x{}", v.width, v.height),
        "-x265-params",
        &x265,
        "-tag:v",
        "hvc1",
    ]
    .map(String::from)
    .to_vec();
    Ok(EncodePlan {
        name: "libx265".into(),
        args,
        pre_input: Vec::new(),
    })
}

fn hevc_qsv_plan(v: &VideoParams) -> EncodePlan {
    // QSV: no software -threads; look_ahead off for short GOP slivers.
    let mut args: Vec<String> = [
        "-c:v", "hevc_qsv", "-preset", "veryfast", "-global_quality", "23", "-look_ahead", "0",
        "-profile:v", "main",
    ]
    .map(String::from)
    .to_vec();
    args.extend(["-pix_fmt".into(), "nv12".into(), "-s".into(), format!("{}x{}", v.width, v.height)]);
    EncodePlan {
        name: "hevc_qsv".into(),
        args,
        pre_input: Vec::new(),
    }
}

fn hevc_vaapi_plan(v: &VideoParams, render: &Path) -> EncodePlan {
    EncodePlan {
        name: "hevc_vaapi".into(),
        pre_input: vec![
            "-init_hw_device".into(),
            format!("vaapi=va:{}", render.display()),
            "-filter_hw_device".into(),
            "va".into(),
        ],
        args: [
            "-vf".into(),
            "format=nv12,hwupload".into(),
            "-c:v".into(),
            "hevc_vaapi".into(),
            "-qp".into(),
            "23".into(),
            "-s".into(),
            format!("{}x{}", v.width, v.height),
        ]
        .into(),
    }
}

/// Pick encoder: H.264 → libx264; H.265 → QSV → VAAPI → libx265.
pub(crate) async fn select_encode_plan(v: &VideoParams) -> Result<EncodePlan> {
    match v.codec.as_str() {
        "h264" => h264_software_plan(v),
        "hevc" => {
            if !hevc_profile_ok(&v.profile) || v.pix_fmt != "yuv420p" {
                bail!(
                    "smart cut H.265 requires Main 8-bit yuv420p (got profile={:?} pix_fmt={})",
                    v.profile, v.pix_fmt
                );
            }
            let dri = dri_render_nodes();
            if !dri.is_empty() && ffmpeg_lists_encoder("hevc_qsv") {
                let plan = hevc_qsv_plan(v);
                if probe_encoder(&plan, (v.width.min(320), v.height.min(180))).await {
                    info!("SponsorBlock smart cut: using hevc_qsv");
                    return Ok(plan);
                }
                debug!("hevc_qsv listed but probe encode failed; trying vaapi/libx265");
            }
            if !dri.is_empty() && ffmpeg_lists_encoder("hevc_vaapi") {
                for node in &dri {
                    let plan = hevc_vaapi_plan(v, node);
                    if probe_encoder(&plan, (v.width.min(320), v.height.min(180))).await {
                        info!("SponsorBlock smart cut: using hevc_vaapi on {}", node.display());
                        return Ok(plan);
                    }
                }
                debug!("hevc_vaapi probe failed on all render nodes; trying libx265");
            }
            if ffmpeg_lists_encoder("libx265") {
                let plan = hevc_software_plan(v)?;
                // Soft probe — skip on CI without x265 by checking list only;
                // actual encode failure still bubbles up mid-cut → stable.
                return Ok(plan);
            }
            bail!("no H.265 encoder available (need hevc_qsv, hevc_vaapi, or libx265)");
        }
        other => bail!("smart cut supports H.264/H.265 only (got {})", other),
    }
}

/// Unit-test helper: software encode args only (no HW probe).
pub(crate) fn encode_args_software(v: &VideoParams) -> Result<Vec<String>> {
    Ok(match v.codec.as_str() {
        "h264" => h264_software_plan(v)?.args,
        "hevc" => hevc_software_plan(v)?.args,
        other => bail!("smart cut supports H.264/H.265 only (got {})", other),
    })
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
    let plan = select_encode_plan(&video).await?;
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
    let res = smart_cut_inner(input, output, keep, &pieces, keyframes, &video, audio.as_ref(), audio_enc, &plan, start_time, &tmp).await;
    let _ = fs::remove_dir_all(&tmp).await;
    res
}

#[allow(clippy::too_many_arguments)]
async fn smart_cut_inner(
    input: &Path,
    output: &Path,
    keep: &[(f64, f64)],
    pieces: &[Piece],
    keyframes: &[f64],
    video: &VideoParams,
    audio: Option<&AudioParams>,
    audio_enc: Option<&str>,
    plan: &EncodePlan,
    start_time: f64,
    tmp: &Path,
) -> Result<SmartCutStats> {
    let mut stats = SmartCutStats {
        pieces: pieces.len(),
        encoder: plan.name.clone(),
        ..Default::default()
    };
    let bsf = annexb_bsf(&video.codec);
    let mut list = String::new();
    for (i, p) in pieces.iter().enumerate() {
        let part = tmp.join(format!("v{:03}.ts", i));
        let mut c = low_prio_ffmpeg();
        c.args(["-v", "error", "-y"]);
        match p.kind {
            PieceKind::Copy => {
                // `-t`/`-to` on HEVC stream-copy routinely keeps one extra packet
                // (~0.1–0.2 s). Frame count from the source fps is exact.
                let nframes = ((p.end - p.start) / video.frame).round().max(1.0) as i64;
                c.args(["-ss", &format!("{:.6}", p.start)])
                    .arg("-i")
                    .arg(input)
                    .args(["-frames:v", &nframes.to_string()])
                    .args([
                        "-map", "0:v:0", "-an", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1",
                        "-c:v", "copy", "-bsf:v", bsf,
                    ]);
                stats.copied_secs += p.end - p.start;
            }
            PieceKind::Encode => {
                // Coarse input seek to the previous known keyframe (exact list), then
                // trim to [start,end) so short HEVC slivers don't overshoot.
                for a in &plan.pre_input {
                    c.arg(a);
                }
                let coarse = keyframes
                    .iter()
                    .copied()
                    .rev()
                    .find(|k| *k <= p.start + 1e-6)
                    .unwrap_or(0.0)
                    .min(p.start);
                let trim = if plan.name == "hevc_vaapi" {
                    format!(
                        "trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS,format=nv12,hwupload",
                        p.start - coarse,
                        p.end - coarse
                    )
                } else {
                    format!(
                        "trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS",
                        p.start - coarse,
                        p.end - coarse
                    )
                };
                if coarse > 0.0 {
                    c.args(["-ss", &format!("{:.6}", coarse)]);
                }
                c.arg("-i").arg(input).args([
                    "-map", "0:v:0", "-an", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1",
                ]);
                // Drop plan -vf (VAAPI) — replaced by trim pipeline above.
                let mut args = Vec::new();
                let mut it = plan.args.iter();
                while let Some(a) = it.next() {
                    if a == "-vf" {
                        let _ = it.next();
                        continue;
                    }
                    args.push(a.clone());
                }
                c.args(["-vf", &trim]);
                for a in &args {
                    c.arg(a);
                }
                stats.encoded_secs += p.end - p.start;
            }
        }
        c.args(["-f", "mpegts"]).arg(&part);
        run(c, &format!("smart cut piece {} {:?} via {}", i, p.kind, plan.name)).await?;
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
    let tol = DURATION_TOLERANCE_SECS.max(video.frame * 3.0);
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
        "SponsorBlock smart cut (beta): encoder={} {} pieces, re-encoded {:.2}s, copied {:.2}s → {:.3}s {}",
        stats.encoder, stats.pieces, stats.encoded_secs, stats.copied_secs, got, output.display()
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
    fn encode_args_h264_and_hevc_software() {
        let mut v = VideoParams {
            codec: "h264".into(),
            profile: "High 10".into(),
            pix_fmt: "yuv420p".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        assert!(encode_args_software(&v).is_err());
        v.profile = "High".into();
        v.level = 40;
        let a = encode_args_software(&v).unwrap();
        assert!(a.windows(2).any(|w| w == ["-level", "4.0"]));
        assert!(a.windows(2).any(|w| w == ["-c:v", "libx264"]));

        v.codec = "hevc".into();
        v.profile = "Main".into();
        let a = encode_args_software(&v).unwrap();
        assert!(a.windows(2).any(|w| w == ["-c:v", "libx265"]));
        assert!(a.iter().any(|x| x.contains("open-gop=0")));

        v.profile = "Main 10".into();
        assert!(encode_args_software(&v).is_err());
        v.profile = "Main".into();
        v.pix_fmt = "yuv420p10le".into();
        assert!(encode_args_software(&v).is_err());
        assert_eq!(annexb_bsf("hevc"), "hevc_mp4toannexb");
        assert_eq!(annexb_bsf("h264"), "h264_mp4toannexb");
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
        assert!(
            format!("{:#}", err).contains("H.264/H.265 only") || format!("{:#}", err).contains("mpeg4"),
            "{:#}",
            err
        );
        assert!(!out.exists());
        let snapped = super::super::timeline::snap_keep_ranges_to_keyframes(&keep, &kfs);
        super::super::ffmpeg_cut::cut_out_segments_with_ffmpeg(&input, &out, &snapped, super::super::ffmpeg_cut::CutMode::Keyframe).await.expect("stable cut");
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn integration_smart_cut_hevc_1080p_closed_gop() {
        if !have_ffmpeg()
            || !std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-encoders"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
                .unwrap_or(false)
        {
            eprintln!("skip: ffmpeg/libx265 missing");
            return;
        }
        let dir = tempfile_dir("smartcut-hevc");
        let input = dir.join("in.mp4");
        // Closed GOP — open-GOP mid-copy fails decode checks and must fall back to stable.
        gen(
            &input,
            &[
                "-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                "-x265-params", "keyint=60:min-keyint=60:scenecut=0:open-gop=0:log-level=error",
            ],
            "1920x1080",
        );
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        assert!(kfs.len() >= 9, "{:?}", kfs);
        let keep = vec![(0.0, 5.1), (9.4, 14.3), (15.5, 20.0)];
        let out = dir.join("out.mp4");
        let cpu0 = children_cpu_secs();
        let wall = std::time::Instant::now();
        let stats = smart_cut(&input, &out, &keep, &kfs, 20.0).await.expect("hevc smart cut");
        let cpu = children_cpu_secs() - cpu0;
        eprintln!(
            "SMARTCUT_METRICS hevc1080p encoder={}: cpu={:.2}s wall={:.2}s encoded={:.2}s copied={:.2}s pieces={}",
            stats.encoder, cpu, wall.elapsed().as_secs_f64(), stats.encoded_secs, stats.copied_secs, stats.pieces
        );
        assert_eq!(stats.encoder, "libx265"); // no /dev/dri on the build box
        let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
        let (vd, ad, frames) = stream_durations(&out).await;
        assert!((vd - expected).abs() <= F + 0.05, "video {} vs {}", vd, expected);
        let want_frames = (expected / F).round() as i64;
        assert!((frames as i64 - want_frames).abs() <= 2, "frames {} vs {}", frames, want_frames);
        assert!((ad - vd).abs() < 0.08, "A/V desync: audio {} video {}", ad, vd);
        let dec = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(dec.status.success());
        assert!(
            String::from_utf8_lossy(&dec.stderr).trim().is_empty(),
            "decode errors: {}",
            String::from_utf8_lossy(&dec.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn integration_hevc_open_gop_fails_cleanly_for_stable_fallback() {
        if !have_ffmpeg()
            || !std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-encoders"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
                .unwrap_or(false)
        {
            return;
        }
        let dir = tempfile_dir("smartcut-hevc-opengop");
        let input = dir.join("in.mp4");
        gen(
            &input,
            &[
                "-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                // default open-gop=1
                "-x265-params", "keyint=60:min-keyint=60:scenecut=0:log-level=error",
            ],
            "640x360",
        );
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        let keep = vec![(0.0, 5.1), (9.4, 14.3), (15.5, 20.0)];
        let out = dir.join("out.mp4");
        let err = smart_cut(&input, &out, &keep, &kfs, 20.0).await;
        // Either the cut fails validation (preferred) or somehow succeeds; if it
        // fails, caller falls back to stable. Accept either Err or Ok with clean file.
        if let Err(e) = &err {
            eprintln!("open-gop hevc smart cut erred as expected: {:#}", e);
            assert!(!out.exists() || out.metadata().map(|m| m.len()).unwrap_or(0) == 0);
        } else {
            eprintln!("open-gop hevc unexpectedly succeeded; decode must still be clean");
            let dec = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&out)
                .args(["-f", "null", "-"])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&dec.stderr).trim().is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hw_encoder_plans_exist_without_dri() {
        let v = VideoParams {
            codec: "hevc".into(),
            profile: "Main".into(),
            pix_fmt: "yuv420p".into(),
            width: 1920,
            height: 1080,
            ..Default::default()
        };
        let q = hevc_qsv_plan(&v);
        assert_eq!(q.name, "hevc_qsv");
        assert!(q.args.windows(2).any(|w| w == ["-c:v", "hevc_qsv"]));
        let va = hevc_vaapi_plan(&v, Path::new("/dev/dri/renderD128"));
        assert_eq!(va.name, "hevc_vaapi");
        assert!(!va.pre_input.is_empty());
        // No /dev/dri on this box → dri_render_nodes empty (HW path skipped at runtime).
        if !Path::new("/dev/dri").exists() {
            assert!(dri_render_nodes().is_empty());
        }
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{}-{}-{}", tag, std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
