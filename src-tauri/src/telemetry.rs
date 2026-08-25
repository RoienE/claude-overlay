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
//! - `stage`        — credential source that failed, closed vocabulary
//! - `reason`       — why it failed, closed vocabulary
//! - `os_status`    — macOS `OSStatus` integer (error events only)
//! - `exit_code`    — `security` CLI exit code (error events only)
//! - `component`    — subsystem that raised an app error, closed vocabulary
//! - `location`     — `src/<file>:<line>` of a panic, sanitised of build paths
//!
//! NEVER sent: OAuth tokens, profile/account data, plan tier, usage numbers,
//! file paths, hostnames, usernames, IPs, panic messages, OS error strings, or
//! any machine-derived identifier. Failures are transmitted as *codes only* —
//! the raw error text stays in the local log, because it embeds home-directory
//! paths and account names.
//!
//! ## Design
//! `Telemetry` is cheaply cloneable (Arc internals) so it can be shared between
//! the heartbeat loop, the poller, and Tauri commands without lifetime friction.
//! All sends are fire-and-forget tokio tasks — telemetry can never block or
//! crash the app.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::debug;
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::{HEARTBEAT_INTERVAL_SECS, TELEMETRY_TIMEOUT_SECS};

/// How long an unchanged error fingerprint stays silent after being reported.
const ERROR_REPEAT_SECS: u64 = 3_600;

/// Handle used by the panic hook. Set once, at startup.
static PANIC_REPORTER: OnceLock<Telemetry> = OnceLock::new();

/// Install a panic hook that reports panics as telemetry, then defers to the
/// previous hook so the normal abort/backtrace behaviour is unchanged.
///
/// Only the sanitised source location is transmitted: a panic *message* can embed
/// values from whatever was being processed, so it is logged locally and never sent.
pub fn install_panic_hook(telemetry: Telemetry) {
    if PANIC_REPORTER.set(telemetry).is_err() {
        return; // already installed
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| sanitize_location(l.file(), l.line()))
            .unwrap_or_else(|| "unknown".to_string());
        log::error!("Panic at {location}: {info}");
        if let Some(telemetry) = PANIC_REPORTER.get() {
            telemetry.record_panic(&location);
        }
        previous(info);
    }));
}

// ── Inner state (Arc-shared) ──────────────────────────────────────────────────

struct TelemetryInner {
    /// Base URL, e.g. `https://telemetry.example.com`.  `None` → no-op.
    endpoint: Option<String>,
    /// Raw API key stored for use with `reqwest::RequestBuilder::basic_auth`.
    api_key: Option<String>,
    /// Per-install identity attached to every event, so an error can be traced to
    /// one install without any machine-derived identifier.
    identity: Identity,
    /// Last error fingerprint per event name, for throttling. See `should_emit_error`.
    error_state: Mutex<HashMap<&'static str, (String, Instant)>>,
    /// Live toggle; updated atomically so opt-out takes effect immediately.
    enabled: AtomicBool,
}

// ── Identity & severity ──────────────────────────────────────────────────

/// The anonymous identity stamped onto every event.
///
/// Held by the handle rather than passed per call, so an event added anywhere in
/// the app is correlatable to one install without the caller having to remember.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Random UUIDv4 from settings — never machine-derived.
    pub install_id: String,
    pub app_version: String,
    pub os: String,
    pub arch: String,
}

impl Identity {
    /// Build from the persisted install id; everything else is known at build time.
    pub fn new(install_id: impl Into<String>) -> Self {
        Self {
            install_id: install_id.into(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            os: normalized_os().to_string(),
            arch: normalized_arch().to_string(),
        }
    }

    fn attrs(&self) -> Vec<(&'static str, String)> {
        vec![
            ("install_id", self.install_id.clone()),
            ("app_version", self.app_version.clone()),
            ("os", self.os.clone()),
            ("arch", self.arch.clone()),
        ]
    }
}

/// OTLP severity. Only the two levels this app emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Error,
}

impl Severity {
    fn number(self) -> u8 {
        match self {
            Self::Info => 9,
            Self::Error => 17,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Error => "ERROR",
        }
    }
}

/// A panic location is turned into `src/<file>:<line>`.
///
/// A dependency panic carries the *build machine's* cargo registry path, which is
/// noise at best; keep only the part that identifies the code.
pub fn sanitize_location(file: &str, line: u32) -> String {
    let short = match file.rsplit_once("src/") {
        Some((_, tail)) => format!("src/{tail}"),
        None => file
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("unknown")
            .to_string(),
    };
    format!("{short}:{line}")
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
    pub fn new(
        endpoint: Option<&str>,
        api_key: Option<&str>,
        enabled: bool,
        identity: Identity,
    ) -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                endpoint: endpoint.map(|s| s.trim_end_matches('/').to_string()),
                api_key: api_key.map(|s| s.to_string()),
                identity,
                error_state: Mutex::new(HashMap::new()),
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
        self.record_with_severity(event, Severity::Info, attrs);
    }

    /// As `record`, but stamps an explicit OTLP severity so error events can be
    /// filtered apart from the routine install/heartbeat traffic.
    pub fn record_with_severity(&self, event: &str, severity: Severity, attrs: &[(&str, String)]) {
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return;
        }
        let endpoint = match &self.inner.endpoint {
            Some(ep) => ep.clone(),
            None => return,
        };

        // Identity first, then the event's own attributes.
        let mut all: Vec<(&str, String)> = self.inner.identity.attrs();
        all.extend(attrs.iter().map(|(k, v)| (*k, v.clone())));

        let payload = build_payload_with_severity(event, &all, severity);
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
    pub fn record_install(&self) {
        self.record("install", &[]);
    }

    /// Emit a `heartbeat` event.
    pub fn record_heartbeat(&self) {
        self.record("heartbeat", &[]);
    }

    /// Emit a `credential_error` event: the app could not resolve a usable token.
    ///
    /// `stage` and `reason` are the closed-vocabulary codes from `credential_source`;
    /// no path, account name or OS error string is ever passed here.
    pub fn record_credential_error(
        &self,
        stage: &str,
        reason: &str,
        os_status: Option<i32>,
        exit_code: Option<i32>,
    ) {
        let fingerprint = format!("{stage}/{reason}/{os_status:?}/{exit_code:?}");
        if !self.should_emit_error("credential_error", &fingerprint) {
            return;
        }

        let mut attrs = vec![("stage", stage.to_string()), ("reason", reason.to_string())];
        if let Some(code) = os_status {
            attrs.push(("os_status", code.to_string()));
        }
        if let Some(code) = exit_code {
            attrs.push(("exit_code", code.to_string()));
        }
        self.record_with_severity("credential_error", Severity::Error, &attrs);
    }

    /// Emit an `app_error` event for a non-credential subsystem failure.
    pub fn record_app_error(&self, component: &str, reason: &str) {
        let fingerprint = format!("{component}/{reason}");
        if !self.should_emit_error("app_error", &fingerprint) {
            return;
        }
        self.record_with_severity(
            "app_error",
            Severity::Error,
            &[
                ("component", component.to_string()),
                ("reason", reason.to_string()),
            ],
        );
    }

    /// Emit a `panic` event. `location` must already be sanitised.
    ///
    /// Best-effort: the send is a spawned task, so a panic that immediately aborts
    /// the process may outrun it. Panics inside a Tokio task normally do report.
    pub fn record_panic(&self, location: &str) {
        self.record_with_severity("panic", Severity::Error, &[("location", location.to_string())]);
    }

    /// Throttle gate for error events.
    ///
    /// The poller retries every `AUTH_RETRY_SECS`, so an unresolvable failure would
    /// otherwise emit ~1400 events per install per day. Emit when the fingerprint
    /// changes, and at most once per `ERROR_REPEAT_SECS` while it does not.
    fn should_emit_error(&self, event: &'static str, fingerprint: &str) -> bool {
        let mut state = match self.inner.error_state.lock() {
            Ok(state) => state,
            // A poisoned lock must not silence error reporting.
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        match state.get(event) {
            Some((seen, at))
                if seen == fingerprint && now.duration_since(*at).as_secs() < ERROR_REPEAT_SECS =>
            {
                false
            }
            _ => {
                state.insert(event, (fingerprint.to_string(), now));
                true
            }
        }
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
    pub fn spawn_heartbeat_loop(self) {
        // Spawn onto Tauri's managed runtime so it is safe to call from `setup`.
        tauri::async_runtime::spawn(async move {
            loop {
                self.record_heartbeat();
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
    build_payload_with_severity(event, attrs, Severity::Info)
}

/// As `build_payload`, with an explicit severity.
pub fn build_payload_with_severity(
    event: &str,
    attrs: &[(&str, String)],
    severity: Severity,
) -> Value {
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
                    "severityNumber": severity.number(),
                    "severityText": severity.text(),
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
        // error events
        "stage",
        "reason",
        "os_status",
        "exit_code",
        "component",
        "location",
    ];

    /// Identity used by handle tests. Never a real install id.
    fn test_identity() -> Identity {
        Identity {
            install_id: "test-uuid".to_string(),
            app_version: "0.0.0-test".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
        }
    }

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
        let t = Telemetry::new(Some("http://localhost:9999"), None, false, test_identity());
        assert!(!t.inner.enabled.load(Ordering::Relaxed));
        // Must not panic.
        t.record("heartbeat", &[]);
    }

    #[test]
    fn record_noop_when_no_endpoint() {
        // endpoint=None → record must return immediately.
        let t = Telemetry::new(None, None, true, test_identity());
        assert!(t.inner.endpoint.is_none());
        // Must not panic.
        t.record("heartbeat", &[]);
    }

    #[test]
    fn set_enabled_flips_atomicbool() {
        let t = Telemetry::new(None, None, true, test_identity());
        assert!(t.inner.enabled.load(Ordering::Relaxed));
        t.set_enabled(false);
        assert!(!t.inner.enabled.load(Ordering::Relaxed));
        t.set_enabled(true);
        assert!(t.inner.enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn clone_shares_same_atomicbool() {
        let t1 = Telemetry::new(None, None, true, test_identity());
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
        let t = Telemetry::new(Some("https://telemetry.example.com/"), None, true, test_identity());
        assert_eq!(
            t.inner.endpoint.as_deref().unwrap(),
            "https://telemetry.example.com"
        );
    }

    // ── identity, severity, throttling ───────────────────────────────────

    #[test]
    fn build_payload_with_severity_marks_errors() {
        let payload = build_payload_with_severity("credential_error", &[], Severity::Error);
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["severityNumber"], 17);
        assert_eq!(record["severityText"], "ERROR");
    }

    #[test]
    fn build_payload_defaults_to_info_severity() {
        let payload = build_payload("heartbeat", &[]);
        let record = &payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["severityNumber"], 9);
        assert_eq!(record["severityText"], "INFO");
    }

    #[test]
    fn identity_supplies_the_correlation_attributes() {
        let attrs = test_identity().attrs();
        let keys: Vec<&str> = attrs.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["install_id", "app_version", "os", "arch"]);
        for (key, _) in &attrs {
            assert!(
                ALLOWED_ATTR_KEYS.contains(key),
                "identity key '{key}' is not whitelisted"
            );
        }
    }

    /// An unresolvable credential failure retries every minute; without throttling
    /// that is ~1400 events per install per day.
    #[test]
    fn repeat_error_fingerprint_is_throttled() {
        let t = Telemetry::new(None, None, true, test_identity());
        assert!(t.should_emit_error("credential_error", "keychain_cli/timeout"));
        assert!(
            !t.should_emit_error("credential_error", "keychain_cli/timeout"),
            "the same fingerprint must not be re-sent inside the repeat window"
        );
    }

    #[test]
    fn changed_error_fingerprint_is_emitted_immediately() {
        let t = Telemetry::new(None, None, true, test_identity());
        assert!(t.should_emit_error("credential_error", "keychain_cli/timeout"));
        assert!(
            t.should_emit_error("credential_error", "keychain_legacy/permission_denied"),
            "a different failure must report even inside the repeat window"
        );
    }

    #[test]
    fn error_events_are_throttled_per_event_name() {
        let t = Telemetry::new(None, None, true, test_identity());
        assert!(t.should_emit_error("credential_error", "same"));
        assert!(
            t.should_emit_error("app_error", "same"),
            "throttling one event must not silence another"
        );
    }

    /// Privacy: a panic location must never carry the build machine's paths.
    #[test]
    fn sanitize_location_strips_build_paths() {
        assert_eq!(
            sanitize_location("/Users/somebody/.cargo/registry/src/x-1.0/src/lib.rs", 42),
            "src/lib.rs:42"
        );
        assert_eq!(sanitize_location("src/poller.rs", 7), "src/poller.rs:7");
        assert_eq!(sanitize_location("weird.rs", 1), "weird.rs:1");
    }

    /// Privacy: the credential error payload carries codes only — no path, no token.
    #[test]
    fn credential_error_payload_is_codes_only() {
        let payload = build_payload_with_severity(
            "credential_error",
            &[
                ("install_id", "test-uuid".to_string()),
                ("stage", "keychain_legacy".to_string()),
                ("reason", "permission_denied".to_string()),
                ("os_status", "-25293".to_string()),
            ],
            Severity::Error,
        );

        for key in collect_record_attr_keys(&payload) {
            assert!(
                ALLOWED_ATTR_KEYS.contains(&key.as_str()),
                "attribute key '{key}' is not in the privacy whitelist"
            );
        }

        let json_str = payload.to_string();
        for forbidden in FORBIDDEN_KEYS {
            assert!(
                !json_str.contains(forbidden),
                "payload must not contain forbidden string '{forbidden}'"
            );
        }
        assert!(
            !json_str.contains("/Users/"),
            "payload must not contain a home-directory path"
        );
    }
}
