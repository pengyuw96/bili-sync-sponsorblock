//! BilibiliSponsorBlock-style sponsor cutting + chapter marking for Bilibili page downloads.
//!
//! Base: https://github.com/NeeYoonc/bili-sync-up (v3.1.2)
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
use self::ffmpeg_cut::{atomic_replace_with_optional_backup, cut_out_segments_with_ffmpeg};
use self::timeline::{
    compute_keep_ranges, duration_after_removes, merge_remove_intervals, remap_mark_interval,
};

/// Orchestrate fetch → cut → mark for one Bilibili page media file.
///
/// Fail-open: on any error when `fail_open` is true, logs a warning and leaves
/// the media file in place (uncut / unmarked as applicable).
pub async fn maybe_cut_sponsor_segments(
    page_path: &Path,
    bvid: &str,
    cid: i64,
    duration_secs: u32,
    config: &SponsorBlockConfig,
    skip_because_chapter_split: bool,
) -> Result<SponsorCutOutcome> {
    if !config.enabled {
        return Ok(SponsorCutOutcome::Disabled);
    }

    if skip_because_chapter_split {
        info!(
            "SponsorBlock: skip cut/mark for {} cid={} because split_chapters_after_download is enabled (mutual exclusion)",
            bvid, cid
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
            "SponsorBlock: enabled but no cut/mark categories for {} cid={}",
            bvid, cid
        );
        return Ok(SponsorCutOutcome::Noop);
    }

    let duration = duration_secs as f64;
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

    let segments = match client
        .fetch_skip_segments_for_categories(config, bvid, cid, &fetch_categories)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return fail_open_or_err(config, e, "fetch segments");
        }
    };

    if segments.is_empty() {
        debug!("SponsorBlock: no segments for {} cid={}", bvid, cid);
        return Ok(SponsorCutOutcome::Noop);
    }

    let action_types: HashSet<&str> = config.action_types.iter().map(|s| s.as_str()).collect();

    let mut remove_intervals: Vec<(f64, f64)> = Vec::new();
    let mut mark_raw: Vec<(f64, f64, String)> = Vec::new(); // start, end, category

    for seg in &segments {
        if !seg.matches_cid(cid) {
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
            if vd > 0.0 && duration > 0.0 && (vd - duration).abs() > 2.0 {
                debug!(
                    "SponsorBlock: drop segment {:?} due to videoDuration mismatch (seg={}, page={})",
                    seg.uuid, vd, duration
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

    if !remove_intervals.is_empty() {
        match compute_keep_ranges(
            &remove_intervals,
            duration,
            config.min_segment_seconds,
            config.min_keep_gap_seconds,
        ) {
            None => {
                warn!(
                    "SponsorBlock: cutting would remove ~100% of {} cid={}, keeping original (fail_open)",
                    bvid, cid
                );
                would_remove_all = true;
            }
            Some(k) if k.is_empty() => {
                debug!(
                    "SponsorBlock: no effective cut ranges for {} cid={}",
                    bvid, cid
                );
            }
            Some(keep) => {
                let merged_removes = merge_remove_intervals(&remove_intervals, 1e-3);
                removed_approx_secs = merged_removes.iter().map(|(s, e)| e - s).sum();
                info!(
                    "SponsorBlock: cutting {} cid={} — ~{:.1}s remove, {} keep parts → {}",
                    bvid,
                    cid,
                    removed_approx_secs,
                    keep.len(),
                    page_path.display()
                );

                let parent = page_path.parent().unwrap_or_else(|| Path::new("."));
                let ext = page_path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("mp4");
                let tmp_out = parent.join(format!(
                    ".bili-sync-sponsor-cut-{}.{}",
                    std::process::id(),
                    ext
                ));

                match cut_out_segments_with_ffmpeg(page_path, &tmp_out, &keep).await {
                    Ok(()) => {
                        match atomic_replace_with_optional_backup(
                            page_path,
                            &tmp_out,
                            config.keep_original,
                            &config.original_suffix,
                        )
                        .await
                        {
                            Ok(()) => {
                                did_cut = true;
                                keep_parts = keep.len();
                                applied_removes = merged_removes;
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
            }
        }
    }

    // Remap mark timestamps if a cut was applied; otherwise use original timeline.
    let new_duration = if did_cut {
        duration_after_removes(duration, &applied_removes)
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
            "SponsorBlock: marking {} chapters on {} cid={} → {}",
            chapters.len(),
            bvid,
            cid,
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

    // Compose outcome
    if did_cut && did_mark {
        return Ok(SponsorCutOutcome::CutAndMarked {
            keep_parts,
            removed_approx_secs,
            chapter_count: chapters.len(),
        });
    }
    if did_cut {
        return Ok(SponsorCutOutcome::Cut {
            keep_parts,
            removed_approx_secs,
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
            "SponsorBlock: no matching skip segments for {} cid={}",
            bvid, cid
        );
    }
    Ok(SponsorCutOutcome::Noop)
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
    },
    Marked {
        chapter_count: usize,
    },
    CutAndMarked {
        keep_parts: usize,
        removed_approx_secs: f64,
        chapter_count: usize,
    },
}

impl SponsorCutOutcome {
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

