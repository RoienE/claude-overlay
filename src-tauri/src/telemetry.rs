//! Anonymous operational telemetry — hand-rolled OTLP/HTTP JSON sender.
//!
//! ## Privacy guarantee (enforced by unit tests)
//! The ONLY fields ever transmitted are:
//! - `install_id`   — random UUIDv4, never machine-derived
//! - `event`        — event type string
//! - `app_version`  — semver string from Cargo.toml
//! - `os`           — `"windows"` | `"macos"` | `"linux"`
//! - `arch`         — `"x86_64"` | `"aarch64"`
//! - timestamp      — nanoseconds since epoch
//! - `endpoint`     — `"usage"` | `"profile"` (rate-limit events only)
//! - `backoff_secs` — u64 (rate-limit events only)
//!
//! NEVER sent: OAuth tokens, profile/account data, plan tier, usage numbers,
//! file paths, hostnames, usernames, IPs, or any machine-derived identifier.
//!
//! ## Design
//! `Telemetry` is cheaply cloneable (Arc internals) so it can be shared between
//! the heartbeat loop, the poller, and Tauri commands without lifetime friction.
//! All sends are fire-and-forget tokio tasks — telemetry can never block or
//! crash the app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::debug;
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::{HEARTBEAT_INTERVAL_SECS, TELEMETRY_TIMEOUT_SECS};

// ── Inner state (Arc-shared) ──────────────────────────────────────────────────

struct TelemetryInner {
    /// Base URL, e.g. `https://telemetry.example.com`.  `None` → no-op.
    endpoint: Option<String>,
    /// Raw API key stored for use with `reqwest::RequestBuilder::basic_auth`.
    api_key: Option<String>,
    /// Live toggle; updated atomically so opt-out takes effect immediately.
    enabled: AtomicBool,
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Cheaply cloneable telemetry handle.  All heavy state lives behind an `Arc`.
#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<TelemetryInner>,
}

impl Telemetry {
    /// Construct from build-time config constants + the persisted opt-out flag.
    ///
    /// When `endpoint` is `None` (dev builds without the secret injected)
    /// every method is a guaranteed no-op — no network traffic, no errors.
    pub fn new(endpoint: Option<&str>, api_key: Option<&str>, enabled: bool) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                endpoint: endpoint.map(|s| s.trim_end_matches('/').to_string()),
                api_key: api_key.map(|s| s.to_string()),
                enabled: AtomicBool::new(enabled),
            }),
        }
    }

    /// Flip the live opt-out flag.  Takes effect on the very next `record` call
    /// (including the heartbeat loop's next tick).
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
    }

    /// The single send gate.  Sends ONLY when enabled AND an endpoint is set.
    ///
    /// Spawns a fire-and-forget tokio task; all errors are swallowed at `debug`.
    /// Never blocks, never propagates failures.
    pub fn record(&self, event: &str, attrs: &[(&str, String)]) {
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return;
        }
        let endpoint = match &self.inner.endpoint {
            Some(ep) => ep.clone(),
            None => return,
        };

        let payload = build_payload(event, attrs);
        let url = format!("{endpoint}/v1/logs");
        let api_key = self.inner.api_key.clone();
        let event = event.to_string();

        // Use Tauri's managed runtime (not `tokio::spawn`) so this works even when
        // called from a non-async context such as the `setup` hook, where no Tokio
        // reactor is entered on the current thread.
        tauri::async_runtime::spawn(async move {
            match send_payload(&url, api_key.as_deref(), payload).await {
                Ok(status) => debug!("Telemetry sent '{event}' → HTTP {status} ({url})"),
                Err(e) => debug!("Telemetry send failed (non-fatal) for '{event}': {e}"),
            }
        });
    }

    // ── Convenience methods ───────────────────────────────────────────────────

    /// Emit an `install` event (first run only).
    pub fn record_install(&self, install_id: &str, app_version: &str, os: &str, arch: &str) {
        self.record(
            "install",
            &[
                ("install_id", install_id.to_string()),
                ("app_version", app_version.to_string()),
                ("os", os.to_string()),
                ("arch", arch.to_string()),
            ],
        );
    }

    /// Emit a `heartbeat` event.
    pub fn record_heartbeat(&self, install_id: &str, app_version: &str, os: &str, arch: &str) {
        self.record(
            "heartbeat",
            &[
                ("install_id", install_id.to_string()),
                ("app_version", app_version.to_string()),
                ("os", os.to_string()),
                ("arch", arch.to_string()),
            ],
        );
    }

    /// Emit a `rate_limit_hit` event.
    pub fn record_rate_limit_hit(&self, endpoint: &str, backoff_secs: u64) {
        self.record(
            "rate_limit_hit",
            &[
                ("endpoint", endpoint.to_string()),
                ("backoff_secs", backoff_secs.to_string()),
            ],
        );
    }

    /// Spawn a long-running heartbeat task.
    ///
    /// Emits one heartbeat immediately, then one every `HEARTBEAT_INTERVAL_SECS`.
    /// Each tick re-checks `enabled` via `record` so opt-out silences it live.
    pub fn spawn_heartbeat_loop(
        self,
        install_id: String,
        app_version: String,
        os: String,
        arch: String,
    ) {
        // Spawn onto Tauri's managed runtime so it is safe to call from `setup`.
        tauri::async_runtime::spawn(async move {
            loop {
                self.record_heartbeat(&install_id, &app_version, &os, &arch);
                tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
            }
        });
    }
}

// ── OTLP payload builder ──────────────────────────────────────────────────────

/// Build the OTLP/HTTP logs envelope as a `serde_json::Value`.
///
/// Pure function — no I/O, no side-effects.  Kept public so unit tests can
/// inspect the exact wire format.
pub fn build_payload(event: &str, attrs: &[(&str, String)]) -> Value {
    let time_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
        .to_string();

    // Build per-record attribute array: always start with "event".
    let mut attribute_entries: Vec<Value> = vec![json!({
        "key": "event",
        "value": { "stringValue": event }
    })];
    for (k, v) in attrs {
        attribute_entries.push(json!({
            "key": k,
            "value": { "stringValue": v }
        }));
    }

    json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [{
                    "key": "service.name",
                    "value": { "stringValue": "claude-overlay" }
                }]
            },
            "scopeLogs": [{
                "scope": { "name": "claude-overlay-telemetry" },
                "logRecords": [{
                    "timeUnixNano": time_ns,
                    "severityNumber": 9,
                    "severityText": "INFO",
                    "body": { "stringValue": event },
                    "attributes": attribute_entries
                }]
            }]
        }]
    })
}

// ── HTTP send ─────────────────────────────────────────────────────────────────

/// Returns the HTTP status on a 2xx response. Any non-2xx status is turned into an
/// `Err` (with a truncated response body) so misconfigured endpoints — wrong path
/// (404), missing/incorrect auth (401), etc. — surface instead of being silently
/// swallowed. `reqwest` only errors on transport failures, not on HTTP error codes.
async fn send_payload(
    url: &str,
    api_key: Option<&str>,
    payload: Value,
) -> Result<reqwest::StatusCode, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(TELEMETRY_TIMEOUT_SECS))
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(&payload);

    if let Some(key) = api_key {
        // Basic auth: Authorization: Basic base64("overlay:<key>")
        req = req.basic_auth("overlay", Some(key));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(status)
}

// ── OS/arch helpers ───────────────────────────────────────────────────────────

/// Normalize `std::env::consts::OS` to `"windows"` | `"macos"` | `"linux"` | raw value.
pub fn normalized_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    }
}

/// Normalize `std::env::consts::ARCH` to `"x86_64"` | `"aarch64"` | raw value.
pub fn normalized_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitelist of all attribute `key` values permitted in the payload.
    const ALLOWED_ATTR_KEYS: &[&str] = &[
        "event",
        "install_id",
        "app_version",
        "os",
        "arch",
        "endpoint",
        "backoff_secs",
    ];

    /// Keys that must NEVER appear in telemetry payloads (privacy check).
    const FORBIDDEN_KEYS: &[&str] = &[
        "token",
        "access_token",
        "bearer",
        "email",
        "display_name",
        "full_name",
        "account",
        "organization",
        "subscription",
        "credits",
        "utilization",
        "hostname",
        "username",
    ];

    /// Collect all string values of `"key"` fields within the logRecords attributes array.
    fn collect_record_attr_keys(payload: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(attrs) = payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]
            ["attributes"]
            .as_array()
        {
            for entry in attrs {
                if let Some(k) = entry["key"].as_str() {
                    out.push(k.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn build_payload_structure_is_valid_otlp() {
        let payload = build_payload(
            "heartbeat",
            &[
                ("install_id", "test-uuid".to_string()),
                ("app_version", "0.8.0".to_string()),
                ("os", "windows".to_string()),
                ("arch", "x86_64".to_string()),
            ],
        );

        assert!(payload.get("resourceLogs").is_some(), "must have resourceLogs");
        let resource_logs = payload["resourceLogs"].as_array().unwrap();
        assert_eq!(resource_logs.len(), 1);

        let scope_logs = resource_logs[0]["scopeLogs"].as_array().unwrap();
        assert_eq!(scope_logs.len(), 1);

        let log_records = scope_logs[0]["logRecords"].as_array().unwrap();
        assert_eq!(log_records.len(), 1);

        let record = &log_records[0];
        assert_eq!(record["severityNumber"], 9);
        assert_eq!(record["severityText"], "INFO");
        assert_eq!(record["body"]["stringValue"], "heartbeat");
    }

    #[test]
    fn build_payload_service_name_is_claude_overlay() {
        let payload = build_payload("install", &[]);
        let attrs = payload["resourceLogs"][0]["resource"]["attributes"]
            .as_array()
            .unwrap();
        let svc = attrs.iter().find(|a| a["key"] == "service.name").unwrap();
        assert_eq!(svc["value"]["stringValue"], "claude-overlay");
    }

    /// Privacy test: attribute keys must be a subset of the whitelist.
    #[test]
    fn build_payload_attrs_only_whitelisted_keys() {
        let payload = build_payload(
            "heartbeat",
            &[
                ("install_id", "aaaaaaaa-0000-0000-0000-000000000000".to_string()),
                ("app_version", "0.8.0".to_string()),
                ("os", "windows".to_string()),
                ("arch", "x86_64".to_string()),
            ],
        );

        let keys = collect_record_attr_keys(&payload);
        for key in &keys {
            assert!(
                ALLOWED_ATTR_KEYS.contains(&key.as_str()),
                "attribute key '{key}' is not in the privacy whitelist"
            );
        }
    }

    /// Privacy test: forbidden strings must never appear in the serialized payload.
    #[test]
    fn build_payload_contains_no_forbidden_keys() {
        let payload = build_payload(
            "rate_limit_hit",
            &[
                ("endpoint", "usage".to_string()),
                ("backoff_secs", "120".to_string()),
            ],
        );

        let json_str = serde_json::to_string(&payload).unwrap().to_lowercase();
        for forbidden in FORBIDDEN_KEYS {
            assert!(
                !json_str.contains(forbidden),
                "payload must not contain forbidden string '{forbidden}'"
            );
        }
    }

    #[test]
    fn build_payload_rate_limit_hit_has_endpoint_and_backoff() {
        let payload = build_payload(
            "rate_limit_hit",
            &[
                ("endpoint", "profile".to_string()),
                ("backoff_secs", "60".to_string()),
            ],
        );

        let keys = collect_record_attr_keys(&payload);
        assert!(keys.contains(&"endpoint".to_string()));
        assert!(keys.contains(&"backoff_secs".to_string()));
    }

    #[test]
    fn build_payload_time_is_nonzero() {
        let payload = build_payload("heartbeat", &[]);
        let time_ns = payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["timeUnixNano"]
            .as_str()
            .unwrap();
        let ns: u128 = time_ns.parse().unwrap();
        assert!(ns > 0, "timeUnixNano must be nonzero");
    }

    #[test]
    fn record_noop_when_disabled() {
        // enabled=false → record must return immediately without spawning.
        let t = Telemetry::new(Some("http://localhost:9999"), None, false);
        assert!(!t.inner.enabled.load(Ordering::Relaxed));
        // Must not panic.
        t.record("heartbeat", &[]);
    }

    #[test]
    fn record_noop_when_no_endpoint() {
        // endpoint=None → record must return immediately.
        let t = Telemetry::new(None, None, true);
        assert!(t.inner.endpoint.is_none());
        // Must not panic.
        t.record("heartbeat", &[]);
    }

    #[test]
    fn set_enabled_flips_atomicbool() {
        let t = Telemetry::new(None, None, true);
        assert!(t.inner.enabled.load(Ordering::Relaxed));
        t.set_enabled(false);
        assert!(!t.inner.enabled.load(Ordering::Relaxed));
        t.set_enabled(true);
        assert!(t.inner.enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn clone_shares_same_atomicbool() {
        let t1 = Telemetry::new(None, None, true);
        let t2 = t1.clone();
        t1.set_enabled(false);
        // t2 observes the change because they share the same Arc<TelemetryInner>.
        assert!(!t2.inner.enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn normalized_os_returns_known_values() {
        let os = normalized_os();
        assert!(!os.is_empty(), "normalized_os must return a non-empty string");
    }

    #[test]
    fn normalized_arch_returns_known_values() {
        let arch = normalized_arch();
        assert!(!arch.is_empty(), "normalized_arch must return a non-empty string");
    }

    #[test]
    fn endpoint_trailing_slash_is_stripped() {
        let t = Telemetry::new(Some("https://telemetry.example.com/"), None, true);
        assert_eq!(
            t.inner.endpoint.as_deref().unwrap(),
            "https://telemetry.example.com"
        );
    }
}
