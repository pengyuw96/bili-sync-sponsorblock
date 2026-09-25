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
const ORIGIN: &str = "bili-sync-up";

/// Local audit build tag; not an upstream release claim.
pub const CLIENT_VERSION: &str = "3.1.0+sponsorblock";

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
            HeaderValue::from_str(CLIENT_VERSION).unwrap_or_else(|_| HeaderValue::from_static("3.1.0")),
        );
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .default_headers(headers)
            .user_agent(format!("bili-sync-up/{}", CLIENT_VERSION))
            .build()
            .context("build SponsorBlock HTTP client")?;
        Ok(Self { http })
    }

    /// Fetch skip segments for `(bvid, cid)`, trying primary then mirrors.
    ///
    /// 404 / empty array → Ok(vec![]). Network/5xx → Err after all mirrors fail.
    pub async fn fetch_skip_segments(
        &self,
        config: &SponsorBlockConfig,
        bvid: &str,
        cid: i64,
    ) -> Result<Vec<SponsorSegment>> {
        let mut servers = Vec::with_capacity(1 + config.mirror_server_addresses.len());
        servers.push(config.server_address.trim().trim_end_matches('/').to_string());
        for mirror in &config.mirror_server_addresses {
            let m = mirror.trim().trim_end_matches('/').to_string();
            if !m.is_empty() && !servers.iter().any(|s| s == &m) {
                servers.push(m);
            }
        }

        let categories_json = serde_json::to_string(&config.categories).unwrap_or_else(|_| "[]".to_string());
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
