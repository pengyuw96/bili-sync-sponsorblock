//! BilibiliSponsorBlock-style sponsor cutting + chapter marking for Bilibili page downloads.
//!
//! Base: https://github.com/NeeYoonc/bili-sync-up (v3.1.3)
//! API / segment semantics: https://github.com/hanydd/BilibiliSponsorBlock
//! Wiki: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API
//!
//! Dual modes:
//! - **Cut**: ffmpeg stream-copy cut matching categories out.
//! - **Mark**: embed Matroska/MP4 chapters for Emby/Jellyfin timeline manual skip.
//!
//! Fail-open. Mutual exclusion with chapter split (both cut and mark skip).

mod client;
mod ffmpeg_chapters;
mod ffmpeg_cut;
pub mod health;
mod model;
pub mod smart_cut;
pub mod sidecars;
pub mod timeline;

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::SponsorBlockConfig;

use self::client::SponsorBlockClient;
use self::ffmpeg_chapters::{
    category_chapter_title, embed_chapters_in_place, ChapterMark,
};
use self::ffmpeg_cut::{
    atomic_replace, cut_out_segments_with_ffmpeg, probe_media_duration_secs,
    probe_video_keyframe_times, CutMode,
};
use self::timeline::{compute_keep_ranges, duration_after_removes, remap_mark_interval, removes_from_keeps, snap_keep_ranges_to_keyframes};

static CUT_GATE: once_cell::sync::Lazy<tokio::sync::Semaphore> =
    once_cell::sync::Lazy::new(|| tokio::sync::Semaphore::new(crate::config::MAX_MEDIA_CONCURRENCY));

/// Permits per cut = MAX / limit, so `limit` cuts can run at once (limit 1 → serial).
fn cut_permits_for_limit(limit: usize) -> u32 {
    let limit = crate::config::clamp_media_concurrency(limit);
    (crate::config::MAX_MEDIA_CONCURRENCY / limit) as u32
}

async fn acquire_cut_permit() -> Option<tokio::sync::SemaphorePermit<'static>> {
    let limit = crate::config::reload_config().concurrent_limit.video;
    CUT_GATE.acquire_many(cut_permits_for_limit(limit)).await.ok()
}

#[cfg(test)]
mod cut_gate_tests {
    #[test]
    fn permits_make_cuts_serial_at_one() {
        assert_eq!(super::cut_permits_for_limit(1), 2);
        assert_eq!(super::cut_permits_for_limit(2), 1);
        assert_eq!(super::cut_permits_for_limit(9), 1);
        assert_eq!(super::cut_permits_for_limit(0), 2);
    }
}

/// Which platform's SponsorBlock API / id semantics to use.
#[derive(Debug, Clone, Copy)]
pub enum SponsorPlatformId<'a> {
    /// Bilibili: videoID=bvid + cid filter; server = bsbsb.top family.
    Bilibili { bvid: &'a str, cid: i64 },
    /// YouTube: videoID=youtube id; server = sponsor.ajay.app; no cid.
    YouTube { video_id: &'a str },
}

impl<'a> SponsorPlatformId<'a> {
    fn log_id(&self) -> String {
        match self {
            Self::Bilibili { bvid, cid } => format!("{} cid={}", bvid, cid),
            Self::YouTube { video_id } => (*video_id).to_string(),
        }
    }

    fn matches_segment(&self, seg: &model::SponsorSegment) -> bool {
        match self {
            Self::Bilibili { cid, .. } => seg.matches_cid(*cid),
            Self::YouTube { video_id } => match seg.video_id.as_deref() {
                None | Some("") => true,
                Some(id) => id == *video_id,
            },
        }
    }
}

/// YouTube entry: official SponsorBlock API + existing cut/mark/sidecar pipeline.
pub async fn maybe_cut_youtube_sponsor_segments(
    page_path: &Path,
    danmaku_path: &Path,
    subtitle_stem: &Path,
    youtube_id: &str,
    duration_secs: u32,
    config: &SponsorBlockConfig,
) -> Result<SponsorCutOutcome> {
    maybe_cut_sponsor_segments_for(
        page_path,
        danmaku_path,
        subtitle_stem,
        SponsorPlatformId::YouTube {
            video_id: youtube_id,
        },
        duration_secs,
        config,
        false,
    )
    .await
}

/// Orchestrate fetch → cut → mark for one Bilibili page media file.
///
/// Fail-open: on any error when `fail_open` is true, logs a warning and leaves
/// the media file in place (uncut / unmarked as applicable).
pub async fn maybe_cut_sponsor_segments(
    page_path: &Path,
    danmaku_path: &Path,
    subtitle_stem: &Path,
    bvid: &str,
    cid: i64,
    duration_secs: u32,
    config: &SponsorBlockConfig,
    skip_because_chapter_split: bool,
) -> Result<SponsorCutOutcome> {
    maybe_cut_sponsor_segments_for(
        page_path,
        danmaku_path,
        subtitle_stem,
        SponsorPlatformId::Bilibili { bvid, cid },
        duration_secs,
        config,
        skip_because_chapter_split,
    )
    .await
}

/// Platform-agnostic fetch → cut → mark.
pub async fn maybe_cut_sponsor_segments_for(
    page_path: &Path,
    danmaku_path: &Path,
    subtitle_stem: &Path,
    platform: SponsorPlatformId<'_>,
    duration_secs: u32,
    config: &SponsorBlockConfig,
    skip_because_chapter_split: bool,
) -> Result<SponsorCutOutcome> {
    if !config.enabled {
        return Ok(SponsorCutOutcome::Disabled);
    }

    if skip_because_chapter_split {
        info!(
            "SponsorBlock: skip cut/mark for {} because split_chapters_after_download is enabled (mutual exclusion)",
            platform.log_id()
        );
        return Ok(SponsorCutOutcome::SkippedForChapters);
    }

    if !page_path.exists() {
        warn!(
            "SponsorBlock: media file missing, skip: {}",
            page_path.display()
        );
        return Ok(SponsorCutOutcome::Noop);
    }

    // Cut wins over mark for the same category.
    let cut_categories: HashSet<&str> = config.categories.iter().map(|s| s.as_str()).collect();
    let mark_categories: HashSet<&str> = config
        .mark_categories
        .iter()
        .map(|s| s.as_str())
        .filter(|c| !cut_categories.contains(c))
        .collect();

    if cut_categories.is_empty() && mark_categories.is_empty() {
        debug!(
            "SponsorBlock: enabled but no cut/mark categories for {}",
            platform.log_id()
        );
        return Ok(SponsorCutOutcome::Noop);
    }

    // The API page duration is whole seconds; the real file is usually a bit
    // longer/shorter. Use the probed file duration for the cut timeline so the
    // tail keep and post-cut remaps use the media's own clock.
    let api_duration = duration_secs as f64;
    let duration = match probe_media_duration_secs(page_path).await {
        Ok(d) => d,
        Err(e) => {
            debug!("SponsorBlock: source duration probe failed, using API duration: {:#}", e);
            api_duration
        }
    };
    let cut_mode = CutMode::from_config(&config.cut_mode);
    let client = match SponsorBlockClient::new(config.api_timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            return fail_open_or_err(config, e, "build client");
        }
    };

    let fetch_categories: Vec<String> = {
        let mut cats: Vec<String> = cut_categories.iter().map(|s| (*s).to_string()).collect();
        for c in &mark_categories {
            if !cats.iter().any(|x| x == *c) {
                cats.push((*c).to_string());
            }
        }
        cats
    };

    let segments = match platform {
        SponsorPlatformId::Bilibili { bvid, cid } => {
            client
                .fetch_skip_segments_for_categories(config, bvid, cid, &fetch_categories)
                .await
        }
        SponsorPlatformId::YouTube { video_id } => {
            client
                .fetch_youtube_skip_segments_for_categories(config, video_id, &fetch_categories)
                .await
        }
    };
    let segments = match segments {
        Ok(s) => s,
        Err(e) => {
            return fail_open_or_err(config, e, "fetch segments");
        }
    };

    if segments.is_empty() {
        debug!("SponsorBlock: no segments for {}", platform.log_id());
        return Ok(SponsorCutOutcome::Noop);
    }

    let action_types: HashSet<&str> = config.action_types.iter().map(|s| s.as_str()).collect();

    let mut remove_intervals: Vec<(f64, f64)> = Vec::new();
    let mut mark_raw: Vec<(f64, f64, String)> = Vec::new(); // start, end, category

    for seg in &segments {
        if !platform.matches_segment(seg) {
            continue;
        }
        // MVP: only process skip action type for both cut and mark.
        if seg.action_type != "skip" {
            continue;
        }
        if !action_types.is_empty() && !action_types.contains(seg.action_type.as_str()) {
            continue;
        }
        if let Some(vd) = seg.video_duration {
            if vd > 0.0 && api_duration > 0.0 && (vd - api_duration).abs() > 2.0 {
                debug!(
                    "SponsorBlock: drop segment {:?} due to videoDuration mismatch (seg={}, page={}) for {}",
                    seg.uuid, vd, api_duration, platform.log_id()
                );
                continue;
            }
        }
        let Some((start, end)) = seg.start_end() else {
            continue;
        };
        let cat = seg.category.as_str();
        if cut_categories.contains(cat) {
            remove_intervals.push((start, end));
        } else if mark_categories.contains(cat) {
            mark_raw.push((start, end, seg.category.clone()));
        }
    }

    let mut did_cut = false;
    let mut keep_parts = 0usize;
    let mut removed_approx_secs = 0.0_f64;
    let mut applied_removes: Vec<(f64, f64)> = Vec::new();
    let mut would_remove_all = false;
    let mut cut_failed_open = false;
    let mut measured_duration_secs: Option<u32> = None;

    if !remove_intervals.is_empty() {
        match compute_keep_ranges(
            &remove_intervals,
            duration,
            config.min_segment_seconds,
            config.min_keep_gap_seconds,
        ) {
            None => {
                warn!(
                    "SponsorBlock: cutting would remove ~100% of {}, keeping original (fail_open)",
                    platform.log_id()
                );
                would_remove_all = true;
            }
            Some(k) if k.is_empty() => {
                debug!(
                    "SponsorBlock: no effective cut ranges for {}",
                    platform.log_id()
                );
            }
            Some(keep) => {
                // Snap cut-ins to previous keyframes so stream-copy `-ss` before `-i`
                // and sidecar remapping share one timeline (see timeline::snap_keep_ranges_to_keyframes).
                let keyframes = if cut_mode == CutMode::Accurate {
                    Vec::new()
                } else {
                    match probe_video_keyframe_times(page_path).await {
                    Ok(kfs) => kfs,
                    Err(e) => {
                        warn!(
                            "SponsorBlock keyframe probe failed for {} (cut without snap): {:#}",
                            platform.log_id(), e
                        );
                        Vec::new()
                    }
                    }
                };
                // BETA smart cut: exact boundaries, re-encode only the GOP slivers at each cut.
                // Any failure falls back to the stable keyframe-snap cut below.
                let mut smart_done = false;
                if cut_mode == CutMode::Keyframe
                    && config.cut_strategy.trim() == "beta_smart"
                    && !keyframes.is_empty()
                {
                    let exact_removes = removes_from_keeps(&keep, duration);
                    if !exact_removes.is_empty() {
                        let parent = page_path.parent().unwrap_or_else(|| Path::new("."));
                        let ext = page_path.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
                        let file_key = match platform {
                            SponsorPlatformId::Bilibili { cid, .. } => cid,
                            SponsorPlatformId::YouTube { .. } => 0,
                        };
                        let tmp_out = parent.join(format!(
                            ".bili-sync-sponsor-smartcut-{}-{}.{}",
                            std::process::id(),
                            file_key,
                            ext
                        ));
                        let _cut_permit = acquire_cut_permit().await;
                        match smart_cut::smart_cut(page_path, &tmp_out, &keep, &keyframes, duration).await {
                            Ok(stats) => match atomic_replace(page_path, &tmp_out).await {
                                Ok(()) => {
                                    info!(
                                        "SponsorBlock: beta smart cut {} — exact keep {} parts, re-encoded {:.2}s",
                                        platform.log_id(), keep.len(), stats.encoded_secs
                                    );
                                    removed_approx_secs = exact_removes.iter().map(|(s, e)| (e - s).max(0.0)).sum();
                                    did_cut = true;
                                    keep_parts = keep.len();
                                    applied_removes = exact_removes;
                                    smart_done = true;
                                }
                                Err(e) => {
                                    let _ = tokio::fs::remove_file(&tmp_out).await;
                                    warn!("SponsorBlock beta smart cut replace failed, falling back to stable cut: {:#}", e);
                                }
                            },
                            Err(e) => {
                                let _ = tokio::fs::remove_file(&tmp_out).await;
                                warn!(
                                    "SponsorBlock beta smart cut failed for {}, falling back to stable keyframe cut: {:#}",
                                    platform.log_id(), e
                                );
                            }
                        }
                    }
                }
                if !smart_done {
                let mut keep = snap_keep_ranges_to_keyframes(&keep, &keyframes);
                // Use the complement of the ranges ffmpeg will keep. Short gaps that
                // compute_keep_ranges absorbed (and GOP-sized removes collapsed by snap)
                // are part of the real cut.
                let mut cut_removes = removes_from_keeps(&keep, duration);
                if cut_removes.is_empty() || keep.is_empty() {
                    debug!(
                        "SponsorBlock: after keyframe snap, nothing to cut for {}",
                        platform.log_id()
                    );
                } else {
                removed_approx_secs = cut_removes.iter().map(|(s, e)| (e - s).max(0.0)).sum();
                info!(
                    "SponsorBlock: cutting {} — ~{:.1}s remove, {} keep parts (keyframes={}) → {}",
                    platform.log_id(),
                    removed_approx_secs,
                    keep.len(),
                    keyframes.len(),
                    page_path.display()
                );

                let parent = page_path.parent().unwrap_or_else(|| Path::new("."));
                let ext = page_path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("mp4");
                let file_key = match platform {
                    SponsorPlatformId::Bilibili { cid, .. } => cid,
                    SponsorPlatformId::YouTube { .. } => 0,
                };
                let tmp_out = parent.join(format!(
                    ".bili-sync-sponsor-cut-{}-{}.{}",
                    std::process::id(),
                    file_key,
                    ext
                ));

                // 全局 ffmpeg 裁剪闸门：同时裁剪数不超过视频并发上限（1-2）
                let _cut_permit = acquire_cut_permit().await;
                let mut cut_result = cut_out_segments_with_ffmpeg(page_path, &tmp_out, &keep, cut_mode).await;
                if cut_result.is_err() && cut_mode == CutMode::Accurate {
                    // e.g. no libx264 in this ffmpeg build: fall back to keyframe copy
                    // with a freshly snapped plan so sidecars match the media.
                    warn!(
                        "SponsorBlock accurate cut failed, falling back to keyframe copy: {:#}",
                        cut_result.as_ref().err().unwrap()
                    );
                    let _ = tokio::fs::remove_file(&tmp_out).await;
                    let kfs = probe_video_keyframe_times(page_path).await.unwrap_or_default();
                    let snapped = snap_keep_ranges_to_keyframes(&keep, &kfs);
                    cut_removes = removes_from_keeps(&snapped, duration);
                    removed_approx_secs = cut_removes.iter().map(|(s, e)| (e - s).max(0.0)).sum();
                    keep = snapped;
                    cut_result = if keep.is_empty() || cut_removes.is_empty() {
                        Err(anyhow::anyhow!("nothing left to cut after keyframe snap"))
                    } else {
                        cut_out_segments_with_ffmpeg(page_path, &tmp_out, &keep, CutMode::Keyframe).await
                    };
                }
                match cut_result {
                    Ok(()) => {
                        match atomic_replace(page_path, &tmp_out).await
                        {
                            Ok(()) => {
                                did_cut = true;
                                keep_parts = keep.len();
                                applied_removes = cut_removes;
                            }
                            Err(e) => {
                                let _ = tokio::fs::remove_file(&tmp_out).await;
                                if config.fail_open {
                                    warn!(
                                        "SponsorBlock replace media failed (fail_open, keep original): {:#}",
                                        e
                                    );
                                    cut_failed_open = true;
                                } else {
                                    return Err(e.context("SponsorBlock replace media"));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tokio::fs::remove_file(&tmp_out).await;
                        if config.fail_open {
                            warn!(
                                "SponsorBlock ffmpeg cut failed (fail_open, keep original): {:#}",
                                e
                            );
                            cut_failed_open = true;
                        } else {
                            return Err(e.context("SponsorBlock ffmpeg cut"));
                        }
                    }
                }
                } // end non-empty after snap
                } // end stable path (!smart_done)
            }
        }
    }

    // Remap mark timestamps if a cut was applied; otherwise use original timeline.
    // Prefer ffprobe-measured duration so chapter END / NFO runtime match the file.
    let new_duration = if did_cut {
        match probe_media_duration_secs(page_path).await {
            Ok(d) => {
                measured_duration_secs = Some(d.round().max(0.0) as u32);
                if let Err(e) = rewrite_nfo_runtime(page_path, d).await {
                    warn!(
                        "SponsorBlock NFO runtime rewrite failed (media kept): {:#}",
                        e
                    );
                }
                d
            }
            Err(e) => {
                warn!(
                    "SponsorBlock post-cut duration probe failed, using theoretical: {:#}",
                    e
                );
                let approx = duration_after_removes(duration, &applied_removes);
                measured_duration_secs = Some(approx.round().max(0.0) as u32);
                approx
            }
        }
    } else {
        duration
    };
    let removes_for_remap: &[(f64, f64)] = if did_cut {
        &applied_removes
    } else {
        &[]
    };

    let mut chapters: Vec<ChapterMark> = Vec::new();
    for (start, end, cat) in &mark_raw {
        if let Some((rs, re)) = remap_mark_interval(*start, *end, removes_for_remap, new_duration) {
            chapters.push(ChapterMark {
                start: rs,
                end: re,
                title: category_chapter_title(cat).to_string(),
            });
        }
    }
    // Sort by start for stable chapter order
    chapters.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut did_mark = false;
    let mut mark_failed_open = false;
    if !chapters.is_empty() {
        info!(
            "SponsorBlock: marking {} chapters on {} → {}",
            chapters.len(),
            platform.log_id(),
            page_path.display()
        );
        match embed_chapters_in_place(page_path, &chapters, new_duration).await {
            Ok(()) => {
                did_mark = true;
            }
            Err(e) => {
                if config.fail_open {
                    warn!(
                        "SponsorBlock chapter mark failed (fail_open, keep media): {:#}",
                        e
                    );
                    mark_failed_open = true;
                } else {
                    return Err(e.context("SponsorBlock chapter mark"));
                }
            }
        }
    }

    // Sidecar failure must not fail the page: a retry could cut the media twice.
    if did_cut || did_mark {
        let plan_removes: &[(f64, f64)] = if did_cut { &applied_removes } else { &[] };
        let plan_marks: Vec<(f64, f64, String)> = mark_raw
            .iter()
            .map(|(start, end, cat)| (*start, *end, category_chapter_title(cat).to_string()))
            .collect();
        if let Err(err) = sidecars::save_and_apply(
            page_path,
            danmaku_path,
            subtitle_stem,
            plan_removes,
            &plan_marks,
        )
        .await
        {
            warn!(
                "SponsorBlock sidecar rewrite failed (media kept, page still succeeds): {:#}",
                err
            );
        }
    }

    // Compose outcome
    if did_cut && did_mark {
        return Ok(SponsorCutOutcome::CutAndMarked {
            keep_parts,
            removed_approx_secs,
            chapter_count: chapters.len(),
            measured_duration_secs,
        });
    }
    if did_cut {
        return Ok(SponsorCutOutcome::Cut {
            keep_parts,
            removed_approx_secs,
            measured_duration_secs,
        });
    }
    if did_mark {
        return Ok(SponsorCutOutcome::Marked {
            chapter_count: chapters.len(),
        });
    }
    if would_remove_all && mark_raw.is_empty() {
        return Ok(SponsorCutOutcome::WouldRemoveAll);
    }
    if would_remove_all && !did_mark {
        // Cut aborted; marks may have been attempted and failed, or none left after remap.
        if mark_failed_open || cut_failed_open {
            return Ok(SponsorCutOutcome::FailedOpen);
        }
        return Ok(SponsorCutOutcome::WouldRemoveAll);
    }
    if cut_failed_open || mark_failed_open {
        return Ok(SponsorCutOutcome::FailedOpen);
    }
    if remove_intervals.is_empty() && mark_raw.is_empty() {
        debug!(
            "SponsorBlock: no matching skip segments for {}",
            platform.log_id()
        );
    }
    Ok(SponsorCutOutcome::Noop)
}


/// Rewrite `<runtime>` in the sibling `{stem}.nfo` (minutes, truncating like Episode NFO).
async fn rewrite_nfo_runtime(media_path: &Path, duration_secs: f64) -> Result<()> {
    let nfo_path = media_path.with_extension("nfo");
    if !nfo_path.exists() {
        return Ok(());
    }
    let runtime_min = (duration_secs.max(0.0) as i32 / 60).max(0);
    let text = tokio::fs::read_to_string(&nfo_path).await?;
    let updated = rewrite_runtime_xml(&text, runtime_min);
    if updated == text {
        return Ok(());
    }
    tokio::fs::write(&nfo_path, updated.as_bytes()).await?;
    info!(
        "SponsorBlock updated NFO runtime={}min → {}",
        runtime_min,
        nfo_path.display()
    );
    Ok(())
}

fn rewrite_runtime_xml(text: &str, runtime_min: i32) -> String {
    // Prefer replacing an existing <runtime>…</runtime>; otherwise leave untouched
    // (generate_page_nfo may omit runtime when duration is unknown).
    let open = "<runtime>";
    let close = "</runtime>";
    if let Some(start) = text.find(open) {
        let after_open = start + open.len();
        if let Some(rel_end) = text[after_open..].find(close) {
            let end = after_open + rel_end;
            let mut out = String::with_capacity(text.len() + 8);
            out.push_str(&text[..after_open]);
            out.push_str(&runtime_min.to_string());
            out.push_str(&text[end..]);
            return out;
        }
    }
    text.to_string()
}

fn fail_open_or_err(
    config: &SponsorBlockConfig,
    err: anyhow::Error,
    stage: &str,
) -> Result<SponsorCutOutcome> {
    if config.fail_open {
        warn!(
            "SponsorBlock {} failed (fail_open, keep original): {:#}",
            stage, err
        );
        Ok(SponsorCutOutcome::FailedOpen)
    } else {
        Err(err.context(format!("SponsorBlock {}", stage)))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SponsorCutOutcome {
    Disabled,
    SkippedForChapters,
    Noop,
    WouldRemoveAll,
    FailedOpen,
    Cut {
        keep_parts: usize,
        removed_approx_secs: f64,
        /// ffprobe (or theoretical fallback) duration after cut, seconds.
        measured_duration_secs: Option<u32>,
    },
    Marked {
        chapter_count: usize,
    },
    CutAndMarked {
        keep_parts: usize,
        removed_approx_secs: f64,
        chapter_count: usize,
        measured_duration_secs: Option<u32>,
    },
}

impl SponsorCutOutcome {
    /// Post-cut media duration in seconds when a cut was applied.
    pub fn measured_duration_secs(&self) -> Option<u32> {
        match self {
            Self::Cut {
                measured_duration_secs,
                ..
            }
            | Self::CutAndMarked {
                measured_duration_secs,
                ..
            } => *measured_duration_secs,
            _ => None,
        }
    }

    /// Short English code persisted on `page.sponsor_cut_result` for the UI.
    pub fn result_code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::SkippedForChapters => "skipped_chapters",
            Self::Noop => "none",
            Self::WouldRemoveAll => "would_remove_all",
            Self::FailedOpen => "failed_open",
            Self::Cut { .. } => "cut",
            Self::Marked { .. } => "marked",
            Self::CutAndMarked { .. } => "cut_and_marked",
        }
    }
}


#[cfg(test)]
mod nfo_runtime_tests {
    use super::rewrite_runtime_xml;

    #[test]
    fn rewrites_existing_runtime() {
        let xml = "<episodedetails><runtime>10</runtime><title>x</title></episodedetails>";
        let out = rewrite_runtime_xml(xml, 7);
        assert!(out.contains("<runtime>7</runtime>"));
        assert!(out.contains("<title>x</title>"));
    }

    #[test]
    fn leaves_untouched_without_runtime() {
        let xml = "<episodedetails><title>x</title></episodedetails>";
        assert_eq!(rewrite_runtime_xml(xml, 7), xml);
    }
}
