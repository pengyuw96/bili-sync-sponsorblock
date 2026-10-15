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
    plan_pieces_open_gop(keep, keyframes, &[], frame, duration)
}

/// Like [`plan_pieces`], with `leads[i]` = earliest PTS of keyframe i's open-GOP
/// leading pictures (`== keyframes[i]` when closed; missing ⇒ closed). A copy
/// piece that ends at keyframe k2 stops at `lead(k2)` instead, because the frames
/// displayed in `[lead(k2), k2)` are decoded *after* k2 and depend on it; that
/// short span is re-encoded together with the tail. Leading pictures of the copy
/// start keyframe are dropped from the copy (they sit in the preceding encode).
pub fn plan_pieces_open_gop(keep: &[(f64, f64)], keyframes: &[f64], leads: &[f64], frame: f64, duration: f64) -> Vec<Piece> {
    let lead_of = |k: f64| -> f64 {
        keyframes
            .iter()
            .position(|x| (*x - k).abs() < 1e-9)
            .and_then(|i| leads.get(i).copied())
            .map(|l| l.min(k))
            .unwrap_or(k)
    };
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
        // First *clean* keyframe at/after s (a keyframe within half a frame before s
        // counts). Open-GOP keyframes (with leading pictures) are skipped as copy
        // starts: they are non-IDR (x264 open-gop I / HEVC CRA), so the decoder does
        // not reset POC/reorder state after our IDR-started encode piece and outputs
        // frames out of order across the join.
        let k1 = keyframes.iter().copied().find(|k| *k >= s - eps && lead_of(*k) >= *k - 1e-9);
        // Last keyframe at/before e.
        let k2 = keyframes.iter().rev().copied().find(|k| *k <= e + eps);
        let usable = match (k1, k2) {
            (Some(k1), Some(k2)) => k1 < e - eps && (lead_of(k2) > k1 + eps || at_file_end),
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
        } else {
            let stop = lead_of(k2);
            push(&mut out, copy_from, stop, PieceKind::Copy, eps);
            if (e - stop).abs() > eps {
                push(&mut out, stop, e, PieceKind::Encode, eps);
            }
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
        "hevc_mp4toannexb,dump_extra=freq=keyframe"
    } else {
        // mp4toannexb only prepends SPS/PPS before IDR; an open-GOP I (non-IDR)
        // copy start would be undecodable without dump_extra.
        "h264_mp4toannexb,dump_extra=freq=keyframe"
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
    let ok = match c.output().await {
        Ok(o) => {
            let ok = o.status.success() && out.exists() && out.metadata().map(|m| m.len() > 0).unwrap_or(false);
            if !ok {
                let err = String::from_utf8_lossy(&o.stderr);
                let tail: Vec<&str> = err.lines().rev().take(3).collect();
                info!("SponsorBlock smart cut: {} probe encode failed: {}", plan.name, tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
            }
            ok
        }
        Err(e) => {
            info!("SponsorBlock smart cut: {} probe spawn failed: {}", plan.name, e);
            false
        }
    };
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
    // Minimal options: on Jasper Lake + ffmpeg 6.1 `-preset`/`-global_quality`
    // made the encoder open fail ("Function not implemented"), while a plain
    // nv12 -> hevc_qsv encode works. Bitrate scaled to resolution (~6 Mb/s @1080p).
    let br = ((v.width * v.height) as f64 * 3.0).max(1_000_000.0) as u64;
    let args: Vec<String> = vec![
        "-c:v".into(), "hevc_qsv".into(), "-profile:v".into(), "main".into(),
        "-pix_fmt".into(), "nv12".into(), "-b:v".into(), br.to_string(),
        "-maxrate".into(), (br * 2).to_string(),
    ];
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
            // No `-s`: a size change after hwupload would need a software scaler
            // on VAAPI surfaces and fails; pieces keep the source size anyway.
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
            // VAAPI first: verified on the NAS (N5095); QSV is the second choice.
            if !dri.is_empty() && ffmpeg_lists_encoder("hevc_vaapi") {
                for node in &dri {
                    let plan = hevc_vaapi_plan(v, node);
                    if probe_encoder(&plan, (v.width.min(320), v.height.min(180))).await {
                        info!("SponsorBlock smart cut: using hevc_vaapi on {}", node.display());
                        return Ok(plan);
                    }
                }
                debug!("hevc_vaapi probe failed on all render nodes; trying hevc_qsv");
            }
            if !dri.is_empty() && ffmpeg_lists_encoder("hevc_qsv") {
                let plan = hevc_qsv_plan(v);
                if probe_encoder(&plan, (v.width.min(320), v.height.min(180))).await {
                    info!("SponsorBlock smart cut: using hevc_qsv");
                    return Ok(plan);
                }
                debug!("hevc_qsv listed but probe encode failed; trying libx265");
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
    // Open-GOP leading pictures per keyframe (same -ss clock as `keyframes`).
    let index = super::ffmpeg_cut::probe_keyframe_index(input).await.unwrap_or_default();
    let leads: Vec<f64> = keyframes
        .iter()
        .map(|k| {
            index
                .iter()
                .find(|(key, _)| ((key - start_time) - k).abs() < 1e-3)
                .map(|(key, lead)| k - (key - lead))
                .unwrap_or(*k)
        })
        .collect();
    // Non-IDR keyframes (x264 open-gop I, HEVC CRA) cannot start a copy piece
    // even without leading pictures: POC/reorder state is not reset after our
    // IDR-started encode piece. Mark them "open" (lead just below the key).
    let leads: Vec<f64> = match probe_keyframe_clean(input, &video.codec).await {
        Ok(clean) if clean.len() == index.len() => leads
            .iter()
            .zip(keyframes)
            .map(|(l, k)| {
                let i = index.iter().position(|(key, _)| ((key - start_time) - k).abs() < 1e-3);
                match i.and_then(|i| clean.get(i)) {
                    Some(false) => l.min(k - 1e-7),
                    _ => *l,
                }
            })
            .collect(),
        Ok(clean) => {
            debug!("smart cut: IRAP type count {} != keyframes {}; trusting leading-picture scan", clean.len(), index.len());
            leads
        }
        Err(e) => {
            debug!("smart cut: IRAP type probe failed: {:#}", e);
            leads
        }
    };
    let open_gop = leads.iter().zip(keyframes).filter(|(l, k)| *l < *k).count();
    if open_gop > 0 {
        debug!("smart cut: {} of {} keyframes have open-GOP leading pictures", open_gop, keyframes.len());
    }
    let pieces = plan_pieces_open_gop(keep, keyframes, &leads, video.frame, duration);
    let keep_total: f64 = keep.iter().map(|(s, e)| e - s).sum();
    let enc_total: f64 = pieces.iter().filter(|p| p.kind == PieceKind::Encode).map(|p| p.end - p.start).sum();
    if open_gop > 0 && enc_total > keep_total * 0.5 {
        bail!(
            "open-GOP source ({} of {} keyframes are non-IDR/CRA or have leading pictures): smart cut would re-encode {:.0}% of the video",
            open_gop,
            keyframes.len(),
            enc_total / keep_total.max(1e-6) * 100.0
        );
    }
    if pieces.is_empty() {
        bail!("empty plan");
    }
    let _gate = SMART_CUT_GATE.acquire().await.context("smart cut gate")?;

    let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let tmp = parent.join(format!(".bili-sync-smartcut-{}-{}", std::process::id(), ts));
    fs::create_dir_all(&tmp).await?;
    // Absolute: the concat list resolves relative entries against its own dir.
    let tmp = fs::canonicalize(&tmp).await.unwrap_or(tmp);
    let res = smart_cut_inner(input, output, keep, &pieces, keyframes, &video, audio.as_ref(), audio_enc, &plan, start_time, &tmp).await;
    if std::env::var_os("BILI_SYNC_SMARTCUT_KEEP_TMP").is_none() {
        let _ = fs::remove_dir_all(&tmp).await;
    }
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
    let mut parts: Vec<String> = Vec::new();
    // Output timeline position of each piece. Written into the piece itself
    // (-output_ts_offset, no mpegts mux delay) and joined with the concat
    // *protocol*, so PTS are exact by construction. The concat demuxer instead
    // re-derived offsets from each file's start_time, which for a copy piece is
    // skewed by the source's deeper B-frame DTS lead (open-GOP x264: 5 frames vs
    // our 2) and made pieces overlap by 1-3 frames ("non monotonically
    // increasing dts" in the join check).
    // +1 s base: mpegts cannot hold negative DTS (B-frame lead of the first
    // piece) and would otherwise shift that piece alone (avoid_negative_ts).
    // The final mux normalises the input start back to 0.
    let mut t_out = 1.0f64;
    for (i, p) in pieces.iter().enumerate() {
        let part = tmp.join(format!("v{:03}.ts", i));
        let mut c = low_prio_ffmpeg();
        c.args(["-v", "error", "-y"]);
        match p.kind {
            PieceKind::Copy => {
                // Keep exactly the packets displayed in [start, end): drop open-GOP
                // leading pictures (pts < keyframe pts) and anything at/after end.
                // `-frames:v` counted decode order and broke on B-frames/open GOP.
                let span = (p.end - p.start - video.frame * 0.5).max(video.frame * 0.5);
                let bsf_chain = format!(
                    "{},noise=amount=0:drop=lt(pts\\,startpts)+gte((pts-startpts)*tb\\,{:.6})",
                    bsf, span
                );
                c.args(["-ss", &format!("{:.6}", p.start)])
                    .arg("-i")
                    .arg(input)
                    .args([
                        "-map", "0:v:0", "-an", "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1",
                        "-c:v", "copy", "-bsf:v", &bsf_chain,
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
                // passthrough: never duplicate/drop frames to make CFR. With the
                // default (auto → cfr for some muxer/encoder combos) a VFR source
                // (Bilibili avg 51206400/1706881) gained duplicated frames per
                // encode piece and the output came out ~0.3 s long.
                c.args(["-vf", &trim, "-fps_mode", "passthrough"]);
                // Frame-exact cap from the source packets in [start, end): guards
                // against any encoder/filter (e.g. VAAPI hwupload) emitting extras.
                if let Ok(n) = count_source_frames(input, p.start + start_time, p.end + start_time).await {
                    if n > 0 {
                        c.args(["-frames:v", &n.to_string()]);
                    }
                }
                for a in &args {
                    c.arg(a);
                }
                stats.encoded_secs += p.end - p.start;
            }
        }
        c.args(["-output_ts_offset", &format!("{:.6}", t_out), "-muxdelay", "0", "-muxpreload", "0"]);
        c.args(["-f", "mpegts"]).arg(&part);
        run(c, &format!("smart cut piece {} {:?} via {}", i, p.kind, plan.name)).await?;
        // Next piece starts one frame after this piece's last displayed frame.
        // Piece lengths are not whole frames (e.g. 2.366 s = 70.9 frames → 71), so
        // advancing by the nominal length would overlap the next piece by a few ms.
        let last = match max_packet_pts(&part).await {
            Ok(t) => t,
            // e.g. keep starts at 0 but the first video frame is at 0.033 (audio
            // starts first): the [0, first frame) sliver has no frames. Skip it.
            Err(_) if p.kind == PieceKind::Encode && p.end - p.start <= video.frame * 2.5 => {
                let _ = fs::remove_file(&part).await;
                continue;
            }
            Err(e) => return Err(e.context(format!("smart cut piece {}", i))),
        };
        if last + 1e-6 < t_out {
            bail!("smart cut piece {} produced no frames", i);
        }
        t_out = last + video.frame;
        list.push_str(&format!("file '{}'\n", part.to_string_lossy().replace('\'', "'\\''")));
        parts.push(part.to_string_lossy().to_string());
    }
let list_path = tmp.join("list.txt");
    fs::write(&list_path, list).await?;

    let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_ascii_lowercase();
    if parts.iter().any(|p| p.contains('|')) {
        bail!("temp path contains '|'");
    }
    let mut inputs: Vec<std::ffi::OsString> = vec!["-i".into(), format!("concat:{}", parts.join("|")).into()];
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
        // H.264 open GOP: dropping the leading (B-pyramid ref) pictures of the copy
        // start keyframe leaves MMCO ops pointing at absent refs. ffmpeg warns
        // "mmco: unref short failure" but output frames are unaffected.
        let err: String = err
            .lines()
            .filter(|l| !l.contains("mmco: unref short failure") && !l.trim_start().starts_with("Last message repeated"))
            .collect::<Vec<_>>()
            .join("\n");
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

/// For each keyframe packet (decode order = PTS order for keyframes) whether it
/// is a clean random access point that resets decoding: H.264 IDR (5), HEVC
/// BLA/IDR (16-20). x264 open-gop I (1) and HEVC CRA (21) are not.
async fn probe_keyframe_clean(input: &Path, codec: &str) -> Result<Vec<bool>> {
    let types = if codec == "hevc" { "16-21" } else { "1-5" };
    let out = Command::new(tool("ffmpeg"))
        .args(["-hide_banner", "-nostats", "-v", "info", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-c:v", "copy", "-bsf:v"])
        .arg(format!("noise=amount=0:drop=not(key),filter_units=pass_types={},trace_headers", types))
        .args(["-f", "null", "-"])
        .output()
        .await
        .context("spawn ffmpeg trace_headers")?;
    Ok(parse_irap_trace(&String::from_utf8_lossy(&out.stderr), codec))
}

pub(crate) fn parse_irap_trace(log: &str, codec: &str) -> Vec<bool> {
    let mut res = Vec::new();
    let mut want = false;
    for line in log.lines() {
        if line.contains("Packet:") && line.contains("key frame") {
            want = true;
            continue;
        }
        if want && line.contains("nal_unit_type") {
            if let Some(t) = line.rsplit('=').next().and_then(|v| v.trim().parse::<u32>().ok()) {
                res.push(if codec == "hevc" { (16..=20).contains(&t) } else { t == 5 });
                want = false;
            }
        }
    }
    res
}

/// Video packets of `input` whose PTS lies in [start, end) (absolute seconds).
async fn count_source_frames(input: &Path, start: f64, end: f64) -> Result<usize> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-read_intervals"])
        .arg(format!("{:.6}%{:.6}", (start - 10.0).max(0.0), end + 2.0))
        .args(["-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(input)
        .output()
        .await
        .context("spawn ffprobe frame count")?;
    let eps = 1e-4;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split(',').next()?.trim().parse::<f64>().ok())
        .filter(|t| *t >= start - eps && *t < end - eps)
        .count())
}

async fn max_packet_pts(path: &Path) -> Result<f64> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .await
        .context("spawn ffprobe piece pts")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split(',').next()?.trim().parse::<f64>().ok())
        .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))))
        .context("piece has no video packets")
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
        assert_eq!(annexb_bsf("hevc"), "hevc_mp4toannexb,dump_extra=freq=keyframe");
        assert_eq!(annexb_bsf("h264"), "h264_mp4toannexb,dump_extra=freq=keyframe");
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
        (g(13) + g(14) + g(15) + g(16)) / tick
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

    #[test]
    fn plan_open_gop_copy_stops_before_leading_pictures_and_skips_unclean_starts() {
        // keyframes 0,4,8,12 ; 8 is open-GOP with leading pictures from 7.9
        let k = vec![0.0, 4.0, 8.0, 12.0];
        let leads = vec![0.0, 4.0, 7.9, 12.0];
        let p = plan_pieces_open_gop(&[(1.0, 9.0)], &k, &leads, F, 20.0);
        // copy 4 → 7.9 (not 8: frames in [7.9,8) decode after key 8), encode the rest
        assert_eq!(p[0].kind, PieceKind::Encode);
        assert!((p[0].end - 4.0).abs() < 1e-9);
        assert_eq!(p[1].kind, PieceKind::Copy);
        assert!((p[1].end - 7.9).abs() < 1e-9, "{:?}", p);
        assert_eq!(p[2].kind, PieceKind::Encode);
        assert!((p[2].start - 7.9).abs() < 1e-9 && (p[2].end - 9.0).abs() < 1e-9);
        // a keep starting at 7.5: key 8 is unclean → never a copy start
        let p = plan_pieces_open_gop(&[(7.5, 11.0)], &k, &leads, F, 20.0);
        assert!(p.iter().all(|x| x.kind == PieceKind::Encode), "{:?}", p);
        // empty leads == closed GOP == plan_pieces
        let a = plan_pieces_open_gop(&[(1.0, 9.0)], &k, &[], F, 20.0);
        assert_eq!(a, plan_pieces(&[(1.0, 9.0)], &k, F, 20.0));
    }

    #[test]
    fn irap_trace_parse() {
        let log = "[trace_headers @ 0x1] Packet: 21330 bytes, key frame, pts 0, dts -2002.\n\
                   [trace_headers @ 0x1] 3           nal_unit_type          00101 = 5\n\
                   size=0kB [trace_headers @ 0x1] Packet: 28536 bytes, key frame, pts 450450.\n\
                   [trace_headers @ 0x1] 3           nal_unit_type          00001 = 1\n";
        assert_eq!(parse_irap_trace(log, "h264"), vec![true, false]);
        let log = "Packet: 1 bytes, key frame\n x nal_unit_type 010100 = 20\nPacket: 1 bytes, key frame\n x nal_unit_type 010101 = 21\n";
        assert_eq!(parse_irap_trace(log, "hevc"), vec![true, false]);
    }

    fn decode_errors(p: &Path) -> String {
        let dec = std::process::Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(p).args(["-f", "null", "-"]).output().unwrap();
        String::from_utf8_lossy(&dec.stderr).trim().to_string()
    }

    fn gen60(path: &Path, x264: &[&str]) {
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=60000/1001", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "30"])
            .args(["-c:v", "libx264", "-preset", "medium", "-pix_fmt", "yuv420p"])
            .args(x264)
            .args(["-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart", "-shortest"])
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// 3b1b-style source: 59.94 fps, x264 medium with B-frames (bf=3, B-pyramid →
    /// DTS lead 2 frames), long-ish GOP, piece lengths that are not whole frames.
    /// Used to overlap pieces at the joins ("non monotonically increasing dts").
    #[tokio::test]
    async fn integration_smart_cut_h264_bframes_joins_monotonic() {
        if !have_ffmpeg() {
            eprintln!("skip: ffmpeg/libx264 missing");
            return;
        }
        let dir = tempfile_dir("smartcut-h264-bf3");
        let input = dir.join("in.mp4");
        gen60(&input, &["-bf", "3", "-g", "300", "-keyint_min", "300", "-sc_threshold", "0"]);
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        assert!(kfs.len() >= 5 && kfs[0] == 0.0, "{:?}", kfs);
        let keep = vec![(0.0, 6.37), (12.61, 18.13), (21.5, 29.9)];
        let out = dir.join("out.mp4");
        let stats = smart_cut(&input, &out, &keep, &kfs, 30.0).await.expect("smart cut");
        assert!(stats.copied_secs > 4.0, "should copy whole GOPs: {:?}", stats);
        let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
        let (vd, ad, _) = stream_durations(&out).await;
        assert!((vd - expected).abs() <= 3.0 / 59.94, "video {} vs {}", vd, expected);
        assert!((ad - vd).abs() < 0.06, "A/V desync: audio {} video {}", ad, vd);
        let err = decode_errors(&out);
        assert!(err.is_empty(), "decode errors: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// x264 open-gop: keyframes are non-IDR I with leading B-frames. Copying from
    /// them broke the join (PPS missing, POC reorder). Must refuse cleanly so the
    /// caller falls back to stable — never emit a broken file.
    #[tokio::test]
    async fn integration_h264_open_gop_refused_cleanly() {
        if !have_ffmpeg() {
            eprintln!("skip: ffmpeg/libx264 missing");
            return;
        }
        let dir = tempfile_dir("smartcut-h264-open");
        let input = dir.join("in.mp4");
        gen60(&input, &["-bf", "3", "-g", "450", "-x264-params", "open-gop=1"]);
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        let out = dir.join("out.mp4");
        match smart_cut(&input, &out, &[(0.0, 8.37), (16.61, 24.13), (27.5, 29.9)], &kfs, 30.0).await {
            Ok(_) => assert!(decode_errors(&out).is_empty(), "accepted but broken"),
            Err(e) => assert!(format!("{:#}", e).contains("open-GOP"), "unexpected error: {:#}", e),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bilibili-like VFR HEVC: odd average fps (every 37th frame missing, first
    /// video frame at 0.033 s while audio starts at 0). Encode pieces must be
    /// frame-exact (passthrough + source frame cap) and an empty [0, first frame)
    /// sliver must be skipped instead of failing the cut.
    #[tokio::test]
    async fn integration_smart_cut_hevc_vfr_frame_exact() {
        let has_x265 = std::process::Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx265")).unwrap_or(false);
        if !has_x265 { eprintln!("skip: no libx265"); return; }
        let dir = tempfile_dir("smartcut-hevc-vfr");
        let input = dir.join("in.mp4");
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=40", "-f", "lavfi", "-i", "sine=d=40",
                "-vf", "select='not(eq(mod(n\\,37)\\,0))'", "-fps_mode", "vfr",
                "-c:v", "libx265", "-x265-params", "keyint=150:min-keyint=150:open-gop=0:log-level=error",
                "-tag:v", "hvc1", "-c:a", "aac", "-movflags", "+faststart"])
            .arg(&input).status().unwrap();
        assert!(st.success());
        let kfs = super::super::ffmpeg_cut::probe_video_keyframe_times(&input).await.unwrap();
        let keep = vec![(0.0, 8.37), (16.61, 24.13), (27.5, 40.0)];
        let out = dir.join("out.mp4");
        smart_cut(&input, &out, &keep, &kfs, 40.0).await.expect("smart cut");
        let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
        let (vd, ad, _) = stream_durations(&out).await;
        assert!((vd - expected).abs() <= 0.1, "video {} vs {}", vd, expected);
        assert!((ad - vd).abs() < 0.12, "A/V desync: audio {} video {}", ad, vd);
        let err = decode_errors(&out);
        assert!(err.is_empty(), "decode errors: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
