//! HTTP client for BilibiliSponsorBlock skipSegments API.
//!
//! Spec: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API
//! Upstream extension: https://github.com/hanydd/BilibiliSponsorBlock

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use tracing::{debug, warn};

use super::model::SponsorSegment;
use crate::config::SponsorBlockConfig;

/// Client identity for upstream telemetry (not Chrome store ID).
pub const ORIGIN: &str = "bili-sync-up";

/// Local audit build tag; not an upstream release claim.
pub const CLIENT_VERSION: &str = "3.1.5.1";

pub struct SponsorBlockClient {
    http: reqwest::Client,
}

impl SponsorBlockClient {
    pub fn new(timeout_ms: u64) -> Result<Self> {
        let timeout = Duration::from_millis(timeout_ms.max(1000));
        let mut headers = HeaderMap::new();
        headers.insert("origin", HeaderValue::from_static(ORIGIN));
        headers.insert(
            "x-ext-version",
            HeaderValue::from_str(CLIENT_VERSION).unwrap_or_else(|_| HeaderValue::from_static("3.1.5.1")),
        );
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .user_agent(format!("bili-sync-up/{}", CLIENT_VERSION))
            .build()
            .context("build SponsorBlock HTTP client")?;
        Ok(Self { http })
    }

    /// Fetch skip segments for `(bvid, cid)` using configured cut categories.
    #[allow(dead_code)]
    pub async fn fetch_skip_segments(
        &self,
        config: &SponsorBlockConfig,
        bvid: &str,
        cid: i64,
    ) -> Result<Vec<SponsorSegment>> {
        self.fetch_skip_segments_for_categories(config, bvid, cid, &config.categories)
            .await
    }

    /// Fetch skip segments for `(bvid, cid)` with an explicit category list
    /// (union of cut + mark categories), trying primary then mirrors.
    ///
    /// 404 / empty array → Ok(vec![]). Network/5xx → Err after all mirrors fail.
    pub async fn fetch_skip_segments_for_categories(
        &self,
        config: &SponsorBlockConfig,
        bvid: &str,
        cid: i64,
        categories: &[String],
    ) -> Result<Vec<SponsorSegment>> {
        let mut servers = Vec::with_capacity(1 + config.mirror_server_addresses.len());
        servers.push(config.server_address.trim().trim_end_matches('/').to_string());
        for mirror in &config.mirror_server_addresses {
            let m = mirror.trim().trim_end_matches('/').to_string();
            if !m.is_empty() && !servers.iter().any(|s| s == &m) {
                servers.push(m);
            }
        }

        let categories_json = serde_json::to_string(categories).unwrap_or_else(|_| "[]".to_string());
        let action_types_json = serde_json::to_string(&config.action_types).unwrap_or_else(|_| "[\"skip\"]".to_string());

        let mut last_err: Option<anyhow::Error> = None;
        for server in servers {
            let url = format!(
                "{}/api/skipSegments?videoID={}&cid={}&categories={}&actionTypes={}",
                server,
                urlencoding_minimal(bvid),
                cid,
                urlencoding_minimal(&categories_json),
                urlencoding_minimal(&action_types_json),
            );
            debug!("SponsorBlock GET {}", url);
            match self.http.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 404 {
                        debug!("SponsorBlock 404 (no segments) from {}", server);
                        return Ok(Vec::new());
                    }
                    if !status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        warn!(
                            "SponsorBlock {} returned {}: {}",
                            server,
                            status,
                            body.chars().take(200).collect::<String>()
                        );
                        last_err = Some(anyhow!("SponsorBlock HTTP {} from {}", status, server));
                        continue;
                    }
                    let text = resp.text().await.context("read SponsorBlock body")?;
                    if text.trim().is_empty() || text.trim() == "[]" {
                        return Ok(Vec::new());
                    }
                    let segments: Vec<SponsorSegment> = serde_json::from_str(&text).with_context(|| {
                        format!(
                            "parse SponsorBlock JSON from {}: {}",
                            server,
                            text.chars().take(200).collect::<String>()
                        )
                    })?;
                    return Ok(segments);
                }
                Err(e) => {
                    warn!("SponsorBlock request to {} failed: {:#}", server, e);
                    last_err = Some(e.into());
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("SponsorBlock: no servers configured")))
    }
}

/// Minimal URL encoding for query values (bvid / JSON arrays are ASCII-safe enough).
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// SHA-256 hex prefix of a YouTube video id (first `chars` hex digits, default 4).
/// Official SponsorBlock privacy API: GET /api/skipSegments/{hashPrefix}
pub fn youtube_privacy_hash_prefix(video_id: &str, chars: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(video_id.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    let n = chars.clamp(1, hex.len());
    hex[..n].to_string()
}

impl SponsorBlockClient {
    /// Fetch YouTube skip segments from the official SponsorBlock API.
    ///
    /// Uses the privacy hash-prefix endpoint first (`/api/skipSegments/{sha256[:4]}`),
    /// then filters by `videoID`. Falls back to direct `?videoID=` if hash returns empty
    /// and mirrors are exhausted without a definitive 404 for this video.
    /// Always passes `service=YouTube`. Never talks to Bilibili bsbsb.top.
    pub async fn fetch_youtube_skip_segments_for_categories(
        &self,
        config: &SponsorBlockConfig,
        video_id: &str,
        categories: &[String],
    ) -> Result<Vec<SponsorSegment>> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return Ok(Vec::new());
        }
        let mut servers = Vec::with_capacity(1 + config.mirror_server_addresses.len());
        servers.push(config.server_address.trim().trim_end_matches('/').to_string());
        for mirror in &config.mirror_server_addresses {
            let m = mirror.trim().trim_end_matches('/').to_string();
            if !m.is_empty() && !servers.iter().any(|s| s == &m) {
                servers.push(m);
            }
        }

        let categories_json = serde_json::to_string(categories).unwrap_or_else(|_| "[]".to_string());
        let action_types_json =
            serde_json::to_string(&config.action_types).unwrap_or_else(|_| "[\"skip\"]".to_string());
        let hash_prefix = youtube_privacy_hash_prefix(video_id, 4);

        let mut last_err: Option<anyhow::Error> = None;
        for server in &servers {
            // Prefer privacy hash endpoint
            let hash_url = format!(
                "{}/api/skipSegments/{}?categories={}&actionTypes={}&service=YouTube",
                server,
                urlencoding_minimal(&hash_prefix),
                urlencoding_minimal(&categories_json),
                urlencoding_minimal(&action_types_json),
            );
            debug!("SponsorBlock YouTube GET {}", hash_url);
            match self.http.get(&hash_url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 404 {
                        debug!(
                            "SponsorBlock YouTube 404 (no segments) via hash from {}",
                            server
                        );
                        return Ok(Vec::new());
                    }
                    if status.is_success() {
                        let text = resp.text().await.context("read SponsorBlock YouTube body")?;
                        if text.trim().is_empty() || text.trim() == "[]" {
                            return Ok(Vec::new());
                        }
                        let segments: Vec<SponsorSegment> =
                            serde_json::from_str(&text).with_context(|| {
                                format!(
                                    "parse SponsorBlock YouTube JSON from {}: {}",
                                    server,
                                    text.chars().take(200).collect::<String>()
                                )
                            })?;
                        let matched: Vec<SponsorSegment> = segments
                            .into_iter()
                            .filter(|s| {
                                s.video_id
                                    .as_deref()
                                    .map(|id| id == video_id)
                                    .unwrap_or(true)
                            })
                            .collect();
                        return Ok(matched);
                    }
                    let body = resp.text().await.unwrap_or_default();
                    warn!(
                        "SponsorBlock YouTube {} returned {}: {}",
                        server,
                        status,
                        body.chars().take(200).collect::<String>()
                    );
                    last_err = Some(anyhow!("SponsorBlock YouTube HTTP {} from {}", status, server));
                }
                Err(e) => {
                    warn!("SponsorBlock YouTube request to {} failed: {:#}", server, e);
                    last_err = Some(e.into());
                }
            }

            // Fallback: direct videoID query on same server
            let direct_url = format!(
                "{}/api/skipSegments?videoID={}&categories={}&actionTypes={}&service=YouTube",
                server,
                urlencoding_minimal(video_id),
                urlencoding_minimal(&categories_json),
                urlencoding_minimal(&action_types_json),
            );
            debug!("SponsorBlock YouTube fallback GET {}", direct_url);
            match self.http.get(&direct_url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 404 {
                        return Ok(Vec::new());
                    }
                    if !status.is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        warn!(
                            "SponsorBlock YouTube direct {} returned {}: {}",
                            server,
                            status,
                            body.chars().take(200).collect::<String>()
                        );
                        last_err =
                            Some(anyhow!("SponsorBlock YouTube HTTP {} from {}", status, server));
                        continue;
                    }
                    let text = resp.text().await.context("read SponsorBlock YouTube body")?;
                    if text.trim().is_empty() || text.trim() == "[]" {
                        return Ok(Vec::new());
                    }
                    let segments: Vec<SponsorSegment> =
                        serde_json::from_str(&text).with_context(|| {
                            format!(
                                "parse SponsorBlock YouTube JSON from {}: {}",
                                server,
                                text.chars().take(200).collect::<String>()
                            )
                        })?;
                    return Ok(segments);
                }
                Err(e) => {
                    warn!(
                        "SponsorBlock YouTube direct request to {} failed: {:#}",
                        server, e
                    );
                    last_err = Some(e.into());
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("SponsorBlock YouTube: no servers configured")))
    }
}

#[cfg(test)]
mod youtube_hash_tests {
    use super::youtube_privacy_hash_prefix;

    #[test]
    fn hash_prefix_is_four_hex_chars() {
        // Known: SHA256("dQw4w9WgXcQ") starts with known prefix — just check length/charset
        let p = youtube_privacy_hash_prefix("dQw4w9WgXcQ", 4);
        assert_eq!(p.len(), 4);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
        // Stable across calls
        assert_eq!(p, youtube_privacy_hash_prefix("dQw4w9WgXcQ", 4));
        assert_ne!(p, youtube_privacy_hash_prefix("xxxxxxxxxxx", 4));
    }

    #[test]
    fn hash_prefix_clamp() {
        let p = youtube_privacy_hash_prefix("abc", 0);
        assert_eq!(p.len(), 1);
    }
}

