//! Canonical data model shared across the Rust modules.
//! The UI receives a serialised `UsageSnapshot` via Tauri events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Plan classification ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    Free,
    Pro,
    /// Max with 5× quota multiplier
    Max5x,
    /// Max with 20× quota multiplier
    Max20x,
    /// Max but exact tier not determined
    Max,
    /// Could not detect
    Unknown,
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Plan::Free => write!(f, "Free"),
            Plan::Pro => write!(f, "Pro"),
            Plan::Max5x => write!(f, "Max 5×"),
            Plan::Max20x => write!(f, "Max 20×"),
            Plan::Max => write!(f, "Max"),
            Plan::Unknown => write!(f, "Unknown"),
        }
    }
}

// ── Raw API response types ────────────────────────────────────────────────────

/// Raw quota window as returned by /api/oauth/usage (may be null → None).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawQuotaWindow {
    pub utilization: Option<f32>,
    pub resets_at: Option<String>,
}

/// Raw extra_usage block.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawExtraUsage {
    pub is_enabled: Option<bool>,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f32>,
}

/// Raw money amount as returned inside the `spend` block, e.g.
/// `{ "amount_minor": 2035, "currency": "EUR", "exponent": 2 }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawMoney {
    pub amount_minor: Option<f64>,
    pub currency: Option<String>,
    pub exponent: Option<i32>,
}

/// Raw `spend` block from /api/oauth/usage — the authoritative overage/spend
/// source on accounts where `extra_usage` is empty. `limit` may arrive as a
/// money object, a bare number, or null, so it is kept as a generic Value and
/// normalized defensively (see `usage_client::money_value_to_cents`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawSpend {
    pub used: Option<RawMoney>,
    pub limit: Option<serde_json::Value>,
    pub percent: Option<f32>,
    pub enabled: Option<bool>,
}

/// The full /api/oauth/usage response parsed as a generic map.
/// NOTE: In practice, we parse this via serde_json::Value in usage_client.rs
/// for more control. This struct is kept for documentation purposes.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawUsage {
    /// All dynamic quota windows (five_hour, seven_day, seven_day_sonnet, …)
    #[serde(flatten)]
    pub windows: HashMap<String, Option<RawQuotaWindow>>,
    /// The special extra_usage field, separated to avoid flatten conflicts.
    pub extra_usage: Option<RawExtraUsage>,
}

// ── Profile API response ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawProfileAccount {
    pub uuid: Option<String>,
    pub full_name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub has_claude_max: Option<bool>,
    pub has_claude_pro: Option<bool>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawProfileOrganization {
    pub uuid: Option<String>,
    pub name: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub has_extra_usage_enabled: Option<bool>,
    pub subscription_status: Option<String>,
    pub billing_type: Option<String>,
    pub organization_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawProfile {
    pub account: Option<RawProfileAccount>,
    pub organization: Option<RawProfileOrganization>,
}

// ── Normalized model emitted to the UI ───────────────────────────────────────

/// One quota window with a human-readable label and live countdown data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Raw API key, e.g. "five_hour", "seven_day_sonnet"
    pub key: String,
    /// Display label, e.g. "5-hour session", "Weekly (Sonnet)"
    pub label: String,
    /// 0–100 (may exceed 100 in overage scenarios)
    pub utilization: f32,
    /// ISO-8601 UTC timestamp when this window resets
    pub resets_at: Option<DateTime<Utc>>,
}

/// Normalized extra-usage block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub enabled: bool,
    pub used_credits: Option<f64>,
    pub monthly_limit: Option<f64>,
    pub utilization: Option<f32>,
}

/// Normalized profile / account info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub has_claude_max: bool,
    pub has_claude_pro: bool,
    pub rate_limit_tier: Option<String>,
    pub has_extra_usage_enabled: bool,
    pub subscription_status: Option<String>,
}

/// Overall status of the data source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "detail", rename_all = "snake_case")]
pub enum SourceStatus {
    /// Fresh data from the API.
    Live,
    /// Last-known data; API unreachable or rate-limited.
    Stale(String),
    /// Data sourced from local JSONL transcripts.
    Degraded,
    /// OAuth token has expired; user must re-authenticate in Claude Code.
    AuthExpired,
    /// Fatal error state.
    Error(String),
    /// No data yet (startup).
    Loading,
}

/// The normalized snapshot emitted to the WebView UI after each poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub plan: Plan,
    pub profile: Option<Profile>,
    /// Ordered list of quota windows to render (dynamic).
    pub windows: Vec<QuotaWindow>,
    pub extra_usage: Option<ExtraUsage>,
    pub status: SourceStatus,
    pub fetched_at: DateTime<Utc>,
    /// Seconds until next scheduled poll (for UI countdown).
    pub next_poll_in: u64,
}

impl UsageSnapshot {
    pub fn loading() -> Self {
        Self {
            plan: Plan::Unknown,
            profile: None,
            windows: vec![],
            extra_usage: None,
            status: SourceStatus::Loading,
            fetched_at: Utc::now(),
            next_poll_in: 0,
        }
    }

    pub fn auth_expired() -> Self {
        Self {
            plan: Plan::Unknown,
            profile: None,
            windows: vec![],
            extra_usage: None,
            status: SourceStatus::AuthExpired,
            fetched_at: Utc::now(),
            next_poll_in: 60,
        }
    }
}

// ── Fallback / local aggregate ────────────────────────────────────────────────

/// Token-based usage from local JSONL transcripts (no authoritative caps).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackUsage {
    pub input_tokens_5h: u64,
    pub output_tokens_5h: u64,
    pub input_tokens_7d: u64,
    pub output_tokens_7d: u64,
    pub cache_creation_5h: u64,
    pub cache_read_5h: u64,
    pub cache_creation_7d: u64,
    pub cache_read_7d: u64,
}

// ── Per-session summary ───────────────────────────────────────────────────────

/// Per-session token usage summary derived from local JSONL transcripts.
///
/// Serialises with camelCase keys so the frontend receives `sessionId`,
/// `lastActive`, `inputTokens`, etc.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    /// Sub-agent type (e.g. "Explore", "developer-backend") for sub-agent
    /// transcripts; `None` for ordinary top-level sessions.
    pub agent_name: Option<String>,
    pub model: Option<String>,
    /// ISO 8601 UTC timestamp of the last assistant message seen.
    pub last_active: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub total_tokens: u64,
    /// `true` when the session file was modified within the active threshold.
    pub active: bool,
}

// ── HTTP outcome classification ───────────────────────────────────────────────

#[derive(Debug)]
pub enum ApiResult<T> {
    Ok(T),
    RateLimited,
    Unauthorized,
    NetworkError(String),
    ParseError(String),
}

// ── Plan override (user-controlled via context menu) ──────────────────────────

/// Optional user override; None means "auto-detect".
pub type PlanOverride = Option<Plan>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_display() {
        assert_eq!(Plan::Free.to_string(), "Free");
        assert_eq!(Plan::Pro.to_string(), "Pro");
        assert_eq!(Plan::Max5x.to_string(), "Max 5×");
        assert_eq!(Plan::Max20x.to_string(), "Max 20×");
        assert_eq!(Plan::Max.to_string(), "Max");
        assert_eq!(Plan::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn source_status_serializes() {
        let s = SourceStatus::Stale("rate limited".to_string());
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("stale"));
    }
}
