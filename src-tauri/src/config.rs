//! All compile-time constants and configuration defaults.
//! Change interval values here; no need to touch any other module.

// ── API endpoints ────────────────────────────────────────────────────────────
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
pub const DEFAULT_USER_AGENT: &str = "claude-code/2.1.85";
pub const REQUEST_TIMEOUT_SECS: u64 = 10;

// ── Credentials ──────────────────────────────────────────────────────────────
pub const CREDENTIALS_FILENAME: &str = ".credentials.json";
pub const CLAUDE_DIR_NAME: &str = ".claude";
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

// ── Adaptive polling intervals (all in seconds) ──────────────────────────────
/// Standard cadence when usage is steady.
pub const POLL_INTERVAL: u64 = 60;
/// Cadence while utilization is actively increasing.
pub const POLL_FAST: u64 = 35;
/// A few rapid follow-up checks right after activity stops.
pub const POLL_FAST_EXTRA: u64 = 25;
/// Retry cadence after transient 5xx / network errors.
pub const POLL_ERROR: u64 = 30;
/// Hard cap for 429 exponential backoff (15 min).
pub const MAX_BACKOFF: u64 = 900;
/// After this much OS idle (or when workstation is locked), pause polling entirely.
/// Set to 0 to disable idle detection.
pub const IDLE_PAUSE: u64 = 300;
/// How many rapid `poll_fast_extra` shots to fire after usage stops rising.
pub const FAST_EXTRA_COUNT: u32 = 1;
/// How long to wait between credential re-checks when auth is expired (seconds).
pub const AUTH_RETRY_SECS: u64 = 60;
/// Profile endpoint slow-poll cadence (seconds, ~1 hour).
pub const PROFILE_POLL_SECS: u64 = 3600;

// ── Telemetry ────────────────────────────────────────────────────────────────
// The OTLP/HTTP endpoint base URL and the API key are injected at BUILD time via
// the `TELEMETRY_ENDPOINT` / `TELEMETRY_API_KEY` env vars (GitHub Secrets in release
// CI) and XOR-obfuscated by `build.rs` into `$OUT_DIR/telemetry_secrets.rs`, so no
// plaintext endpoint/key appears in the shipped binary. When the env vars are unset
// (local/dev builds) both accessors return `None` → telemetry is a guaranteed no-op.
//
// Obfuscation defeats `strings`/static scraping only — it is NOT real secrecy. A
// determined reverse engineer, or a MITM of the app's own HTTPS request, can still
// recover the values. The key is deliberately a write-only, rate-limited, rotatable
// ingest token (see docs/telemetry.md), so a leaked key cannot do meaningful harm.
include!(concat!(env!("OUT_DIR"), "/telemetry_secrets.rs"));

/// Deobfuscated OTLP/HTTP endpoint base URL (e.g. `https://telemetry.example.com`),
/// or `None` in dev builds where no endpoint was injected.
pub fn telemetry_endpoint() -> Option<String> {
    deobfuscate(TELEMETRY_ENDPOINT_SET, TELEMETRY_ENDPOINT_OBF, TELEMETRY_ENDPOINT_KEY)
}

/// Deobfuscated API key (the basicAuth password), or `None` in dev builds.
pub fn telemetry_api_key() -> Option<String> {
    deobfuscate(TELEMETRY_API_KEY_SET, TELEMETRY_API_KEY_OBF, TELEMETRY_API_KEY_KEY)
}

/// Reverse the build-time XOR obfuscation. Returns `None` when unset or non-UTF8.
fn deobfuscate(set: bool, obf: &[u8], key: &[u8]) -> Option<String> {
    if !set || key.is_empty() {
        return None;
    }
    let bytes: Vec<u8> = obf.iter().zip(key.iter().cycle()).map(|(b, k)| b ^ k).collect();
    String::from_utf8(bytes).ok()
}

/// Heartbeat emission interval: 6 hours.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 21_600;

/// HTTP request timeout for telemetry POSTs.
pub const TELEMETRY_TIMEOUT_SECS: u64 = 5;

// ── Opacity range ─────────────────────────────────────────────────────────────
/// Minimum allowed opacity (20 % — matches the slider lower bound).
pub const OPACITY_MIN: f32 = 0.2;
/// Maximum allowed opacity (100 % — fully opaque).
pub const OPACITY_MAX: f32 = 1.0;

// ── Window defaults ───────────────────────────────────────────────────────────
pub const WINDOW_DEFAULT_WIDTH: f64 = 260.0;
pub const WINDOW_DEFAULT_HEIGHT: f64 = 200.0;
pub const WINDOW_SMALL_WIDTH: f64 = 220.0;
pub const WINDOW_SMALL_HEIGHT: f64 = 160.0;
pub const WINDOW_MEDIUM_WIDTH: f64 = 280.0;
pub const WINDOW_MEDIUM_HEIGHT: f64 = 220.0;
pub const WINDOW_LARGE_WIDTH: f64 = 340.0;
pub const WINDOW_LARGE_HEIGHT: f64 = 280.0;
