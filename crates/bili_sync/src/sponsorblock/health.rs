//! SponsorBlock server heartbeat / health detection.
//!
//! Probes `GET {server}/api/status` (BilibiliSponsorBlock status API).
//! Spec: https://github.com/hanydd/BilibiliSponsorBlock/wiki/API
//! Upstream: https://github.com/hanydd/BilibiliSponsorBlock
//! Base project: https://github.com/NeeYoonc/bili-sync-up (v3.1.3)
//!
//! Heartbeats are observational only — downloads already fail-open and are
//! never blocked on a failed health check.

use std::sync::RwLock;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use utoipa::ToSchema;

use super::client::{CLIENT_VERSION, ORIGIN};
use crate::config::SponsorBlockConfig;

/// Cached summary of the most recent health check (any trigger).
static LAST_HEALTH: Lazy<RwLock<Option<LastHealthSummary>>> = Lazy::new(|| RwLock::new(None));

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerHealth {
    pub url: String,
    pub ok: bool,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LastHealthSummary {
    pub last_ok: bool,
    /// Human-readable timestamp (Beijing-standard via project helper)
    pub checked_at: String,
    pub any_ok: bool,
    pub primary_ok: Option<bool>,
    pub primary_latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SponsorBlockHealthResponse {
    pub servers: Vec<ServerHealth>,
    pub last_ok: bool,
    pub checked_at: String,
}

/// Loose `/api/status` body — only uptime / hostname are consumed.
#[derive(Debug, Deserialize)]
struct StatusBody {
    #[serde(default)]
    uptime: Option<f64>,
    #[serde(default)]
    hostname: Option<String>,
}

/// Parse loose JSON from `/api/status` (pure; unit-tested).
pub fn parse_status_json(text: &str) -> (Option<f64>, Option<String>) {
    match serde_json::from_str::<StatusBody>(text) {
        Ok(body) => (body.uptime, body.hostname),
        Err(_) => (None, None),
    }
}

fn server_list(config: &SponsorBlockConfig) -> Vec<String> {
    let mut servers = Vec::with_capacity(1 + config.mirror_server_addresses.len());
    let primary = config.server_address.trim().trim_end_matches('/').to_string();
    if !primary.is_empty() {
        servers.push(primary);
    }
    for mirror in &config.mirror_server_addresses {
        let m = mirror.trim().trim_end_matches('/').to_string();
        if !m.is_empty() && !servers.iter().any(|s| s == &m) {
            servers.push(m);
        }
    }
    servers
}

fn build_http_client(timeout_ms: u64) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "origin",
        reqwest::header::HeaderValue::from_static(ORIGIN),
    );
    headers.insert(
        "x-ext-version",
        reqwest::header::HeaderValue::from_str(CLIENT_VERSION)
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("3.1.3")),
    );
    reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.max(1000)))
        .default_headers(headers)
        .user_agent(format!("bili-sync-up/{}", CLIENT_VERSION))
        .build()
        .map_err(|e| format!("build http: {e}"))
}

/// Ping one SponsorBlock server via `GET {server}/api/status`.
pub async fn ping_server(server: &str) -> ServerHealth {
    let base = server.trim().trim_end_matches('/');
    let url = format!("{}/api/status", base);
    let started = Instant::now();

    let http = match build_http_client(10_000) {
        Ok(c) => c,
        Err(e) => {
            return ServerHealth {
                url,
                ok: false,
                latency_ms: started.elapsed().as_millis() as u64,
                status_code: None,
                error: Some(e),
                uptime: None,
                hostname: None,
            };
        }
    };

    match http.get(&url).send().await {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let latency_ms = started.elapsed().as_millis() as u64;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return ServerHealth {
                    url,
                    ok: false,
                    latency_ms,
                    status_code: Some(status_code),
                    error: Some(format!(
                        "HTTP {}: {}",
                        status_code,
                        body.chars().take(120).collect::<String>()
                    )),
                    uptime: None,
                    hostname: None,
                };
            }
            let text = resp.text().await.unwrap_or_default();
            let (uptime, hostname) = parse_status_json(&text);
            ServerHealth {
                url,
                ok: true,
                latency_ms,
                status_code: Some(status_code),
                error: None,
                uptime,
                hostname,
            }
        }
        Err(e) => ServerHealth {
            url,
            ok: false,
            latency_ms: started.elapsed().as_millis() as u64,
            status_code: None,
            error: Some(format!("{e:#}")),
            uptime: None,
            hostname: None,
        },
    }
}

/// Check primary + all mirrors.
pub async fn check_all(config: &SponsorBlockConfig) -> Vec<ServerHealth> {
    let servers = server_list(config);
    let mut out = Vec::with_capacity(servers.len());
    for server in servers {
        out.push(ping_server(&server).await);
    }
    store_last_summary(&out);
    out
}

fn store_last_summary(results: &[ServerHealth]) {
    let any_ok = results.iter().any(|r| r.ok);
    let primary_ok = results.first().map(|r| r.ok);
    let primary_latency_ms = results.first().map(|r| r.latency_ms);
    let checked_at = crate::utils::time_format::now_standard_string();
    let summary = LastHealthSummary {
        last_ok: any_ok,
        checked_at,
        any_ok,
        primary_ok,
        primary_latency_ms,
    };
    if let Ok(mut guard) = LAST_HEALTH.write() {
        *guard = Some(summary);
    }
}

pub fn last_health_summary() -> Option<LastHealthSummary> {
    LAST_HEALTH.read().ok().and_then(|g| g.clone())
}

fn log_results(results: &[ServerHealth]) {
    if results.is_empty() {
        warn!("SponsorBlock heartbeat: no servers configured");
        return;
    }
    for r in results {
        if r.ok {
            info!(
                "SponsorBlock heartbeat OK {} latency={}ms hostname={:?} uptime={:?}",
                r.url, r.latency_ms, r.hostname, r.uptime
            );
        } else {
            warn!(
                "SponsorBlock heartbeat FAIL {} latency={}ms err={:?}",
                r.url, r.latency_ms, r.error
            );
        }
    }
}

/// One-shot + optional periodic heartbeat. Never blocks downloads.
///
/// Runs an initial probe when `sponsor_block.enabled` is true.
/// Periodic probes use `heartbeat_interval_secs` (default 300; 0 disables periodic).
pub async fn run_heartbeat_loop() {
    // Small delay so config DB init / HTTP server can settle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    loop {
        let config = {
            let config = crate::config::reload_config();
            config.sponsor_block.clone()
        };

        if !config.enabled {
            debug!("SponsorBlock heartbeat: skipped (sponsor_block.enabled=false)");
        } else {
            let results = check_all(&config).await;
            log_results(&results);
        }

        let interval = config.heartbeat_interval_secs;
        if interval == 0 || !config.enabled {
            // No periodic: sleep and re-check config in case user enables later.
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_live_like_status_json() {
        let sample = r#"{
            "uptime": 651327.733,
            "commit": "50bc188a",
            "hostname": "bsbsb.top",
            "role": "primary"
        }"#;
        let (uptime, hostname) = parse_status_json(sample);
        assert!((uptime.unwrap() - 651327.733).abs() < 0.01);
        assert_eq!(hostname.as_deref(), Some("bsbsb.top"));
    }

    #[test]
    fn parse_empty_object() {
        let (uptime, hostname) = parse_status_json("{}");
        assert!(uptime.is_none());
        assert!(hostname.is_none());
    }

    #[test]
    fn parse_invalid_json_is_loose() {
        let (uptime, hostname) = parse_status_json("not-json");
        assert!(uptime.is_none());
        assert!(hostname.is_none());
    }
}
