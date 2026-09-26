//! BilibiliSponsorBlock-style sponsor cutting for Bilibili page downloads.
//!
//! Base: https://github.com/NeeYoonc/bili-sync-up (v3.1.0)
//! API / segment semantics: https://github.com/hanydd/BilibiliSponsorBlock
//! Wiki: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API
//!
//! MVP: after successful media merge, fetch skip segments and cut them out
//! with ffmpeg stream-copy + concat. Fail-open. Mutual exclusion with chapter split.

mod client;
mod ffmpeg_cut;
pub mod health;
mod model;
pub mod timeline;

use std::path::Path;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::SponsorBlockConfig;

use self::client::SponsorBlockClient;
use self::ffmpeg_cut::{atomic_replace_with_optional_backup, cut_out_segments_with_ffmpeg};
use self::timeline::compute_keep_ranges;

/// Orchestrate fetch → timeline → ffmpeg cut for one Bilibili page media file.
///
/// Fail-open: on any error when `fail_open` is true, logs a warning and leaves
/// the uncut file in place.
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
            "SponsorBlock: skip cut for {} cid={} because split_chapters_after_download is enabled (mutual exclusion MVP)",
            bvid, cid
        );
        return Ok(SponsorCutOutcome::SkippedForChapters);
    }

    if !page_path.exists() {
        warn!(
            "SponsorBlock: media file missing, skip cut: {}",
            page_path.display()
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

    let segments = match client.fetch_skip_segments(config, bvid, cid).await {
        Ok(s) => s,
        Err(e) => {
            return fail_open_or_err(config, e, "fetch segments");
        }
    };

    if segments.is_empty() {
        debug!("SponsorBlock: no segments for {} cid={}", bvid, cid);
        return Ok(SponsorCutOutcome::Noop);
    }

    let categories: std::collections::HashSet<&str> =
        config.categories.iter().map(|s| s.as_str()).collect();
    let action_types: std::collections::HashSet<&str> =
        config.action_types.iter().map(|s| s.as_str()).collect();

    let mut remove_intervals: Vec<(f64, f64)> = Vec::new();
    for seg in &segments {
        if !action_types.contains(seg.action_type.as_str()) {
            continue;
        }
        // MVP: only cut skip (never poi/full; mute not cut)
        if seg.action_type != "skip" {
            continue;
        }
        if !categories.contains(seg.category.as_str()) {
            continue;
        }
        if !seg.matches_cid(cid) {
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
        if let Some((start, end)) = seg.start_end() {
            remove_intervals.push((start, end));
        }
    }

    if remove_intervals.is_empty() {
        debug!(
            "SponsorBlock: no matching skip segments for {} cid={}",
            bvid, cid
        );
        return Ok(SponsorCutOutcome::Noop);
    }

    let keep = match compute_keep_ranges(
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
            return Ok(SponsorCutOutcome::WouldRemoveAll);
        }
        Some(k) if k.is_empty() => {
            return Ok(SponsorCutOutcome::Noop);
        }
        Some(k) => k,
    };

    let removed_secs: f64 = remove_intervals.iter().map(|(s, e)| e - s).sum();
    info!(
        "SponsorBlock: cutting {} cid={} — ~{:.1}s remove, {} keep parts → {}",
        bvid,
        cid,
        removed_secs,
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

    if let Err(e) = cut_out_segments_with_ffmpeg(page_path, &tmp_out, &keep).await {
        let _ = tokio::fs::remove_file(&tmp_out).await;
        return fail_open_or_err(config, e, "ffmpeg cut");
    }

    if let Err(e) = atomic_replace_with_optional_backup(
        page_path,
        &tmp_out,
        config.keep_original,
        &config.original_suffix,
    )
    .await
    {
        let _ = tokio::fs::remove_file(&tmp_out).await;
        return fail_open_or_err(config, e, "replace media");
    }

    Ok(SponsorCutOutcome::Cut {
        keep_parts: keep.len(),
        removed_approx_secs: removed_secs,
    })
}

fn fail_open_or_err(
    config: &SponsorBlockConfig,
    err: anyhow::Error,
    stage: &str,
) -> Result<SponsorCutOutcome> {
    if config.fail_open {
        warn!("SponsorBlock {} failed (fail_open, keep original): {:#}", stage, err);
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
        }
    }
}
