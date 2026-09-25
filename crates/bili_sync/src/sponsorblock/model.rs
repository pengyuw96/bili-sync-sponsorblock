//! BilibiliSponsorBlock segment models.
//!
//! API / segment semantics:
//! https://github.com/hanydd/BilibiliSponsorBlock
//! https://github.com/hanydd/BilibiliSponsorBlock/wiki/API

use serde::Deserialize;

/// One skip/mute/poi/full segment from `/api/skipSegments`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SponsorSegment {
    /// `[start, end]` in seconds. POI may be length-1; we only cut length-2 skip segments.
    pub segment: Vec<f64>,
    /// Page cid as string (multi-P filter).
    #[serde(default)]
    pub cid: Option<String>,
    #[serde(rename = "UUID", default)]
    pub uuid: Option<String>,
    pub category: String,
    pub action_type: String,
    #[serde(default)]
    pub locked: Option<i32>,
    #[serde(default)]
    pub votes: Option<i32>,
    /// 0 = unknown; otherwise should roughly match page duration (±2s).
    #[serde(default)]
    pub video_duration: Option<f64>,
    #[serde(default)]
    pub description: Option<String>,
}

impl SponsorSegment {
    pub fn start_end(&self) -> Option<(f64, f64)> {
        if self.segment.len() < 2 {
            return None;
        }
        let start = self.segment[0];
        let end = self.segment[1];
        if !start.is_finite() || !end.is_finite() || end <= start {
            return None;
        }
        Some((start, end))
    }

    pub fn matches_cid(&self, page_cid: i64) -> bool {
        match self.cid.as_deref() {
            None | Some("") => true, // some responses omit cid for single-P
            Some(cid) => cid.trim() == page_cid.to_string(),
        }
    }
}
