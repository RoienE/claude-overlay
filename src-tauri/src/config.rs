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
pub const POLL_FAST: u64 = 30;
/// A few rapid follow-up checks right after activity stops.
pub const POLL_FAST_EXTRA: u64 = 5;
/// Retry cadence after transient 5xx / network errors.
pub const POLL_ERROR: u64 = 30;
/// Hard cap for 429 exponential backoff (15 min).
pub const MAX_BACKOFF: u64 = 900;
/// After this much OS idle (or when workstation is locked), pause polling entirely.
/// Set to 0 to disable idle detection.
pub const IDLE_PAUSE: u64 = 300;
/// How many rapid `poll_fast_extra` shots to fire after usage stops rising.
pub const FAST_EXTRA_COUNT: u32 = 3;
/// How long to wait between credential re-checks when auth is expired (seconds).
pub const AUTH_RETRY_SECS: u64 = 60;
/// Profile endpoint slow-poll cadence (seconds, ~1 hour).
pub const PROFILE_POLL_SECS: u64 = 3600;

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
