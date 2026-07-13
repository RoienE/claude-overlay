//! HTTP client for /api/oauth/usage and /api/oauth/profile.
//!
//! Handles the exact four required headers, 10s timeout, and HTTP status
//! classification. Never touches credentials or UI state.

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use std::time::Duration;

use crate::config::{
    ANTHROPIC_BETA, DEFAULT_USER_AGENT, PROFILE_URL, REQUEST_TIMEOUT_SECS, USAGE_URL,
};
use crate::model::{
    ApiResult, ExtraUsage, Profile, QuotaWindow, RawExtraUsage, RawLimitEntry, RawProfile,
    RawSpend,
};
use crate::plan_detector::label_for_key;

/// Build a configured reqwest client for the Anthropic API.
/// Reuse this per-poll (not per-request) for connection pooling.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(Into::into)
}

/// Return the User-Agent string: try to read installed claude-code version, else default.
pub fn user_agent() -> String {
    // Attempt to get the version from the tauri app metadata; fall back to the pinned default.
    DEFAULT_USER_AGENT.to_string()
}

fn build_headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
    let mut map = HeaderMap::new();
    map.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    );
    map.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    map.insert(
        USER_AGENT,
        HeaderValue::from_str(&user_agent()).unwrap_or_else(|_| {
            HeaderValue::from_static(DEFAULT_USER_AGENT)
        }),
    );
    map.insert(
        "anthropic-beta",
        HeaderValue::from_static(ANTHROPIC_BETA),
    );
    map
}

/// Normalize a money amount to "cents" (2-decimal minor units) so the frontend,
/// which divides by 100 for display, renders the correct value regardless of the
/// currency's exponent. e.g. amount_minor=2035 exponent=2 → 2035.0 (→ €20.35);
/// amount_minor=2000 exponent=0 (e.g. JPY) → 200000.0 (→ ¥2000.00).
fn to_display_cents(amount_minor: f64, exponent: i32) -> f64 {
    amount_minor * 10f64.powi(2 - exponent)
}

/// Extract a "cents" value from the `spend.limit` field, which may arrive as a
/// money object (`{ amount_minor, exponent }`), a bare number (assumed minor
/// units), or null.
fn money_value_to_cents(v: &serde_json::Value) -> Option<f64> {
    if let Some(obj) = v.as_object() {
        let amount_minor = obj.get("amount_minor").and_then(|x| x.as_f64())?;
        let exponent = obj.get("exponent").and_then(|x| x.as_i64()).unwrap_or(2) as i32;
        Some(to_display_cents(amount_minor, exponent))
    } else {
        // Bare number → assume it is already in minor units (cents).
        v.as_f64()
    }
}

/// Build an `ExtraUsage` from the `spend` block. Used as the overage source on
/// accounts where the legacy `extra_usage` block is absent or empty.
fn extra_usage_from_spend(sp: &RawSpend) -> ExtraUsage {
    let used_credits = sp.used.as_ref().and_then(|u| {
        let amount_minor = u.amount_minor?;
        Some(to_display_cents(amount_minor, u.exponent.unwrap_or(2)))
    });
    let monthly_limit = sp.limit.as_ref().and_then(money_value_to_cents);
    ExtraUsage {
        enabled: sp.enabled.unwrap_or(false),
        used_credits,
        monthly_limit,
        utilization: sp.percent,
    }
}

/// Extract per-model weekly-scoped windows (e.g. a "Fable" weekly cap) from
/// the raw `limits` array of /api/oauth/usage. Entries whose `kind` is not
/// `"weekly_scoped"`, or that are missing `percent`/`scope.model.display_name`,
/// are silently skipped — they duplicate windows parsed elsewhere or are not
/// yet in a shape we can render.
fn scoped_weekly_windows_from_limits(entries: &[serde_json::Value]) -> Vec<QuotaWindow> {
    entries
        .iter()
        .filter_map(|entry| {
            let raw_limit = serde_json::from_value::<RawLimitEntry>(entry.clone()).ok()?;
            if raw_limit.kind.as_deref() != Some("weekly_scoped") {
                return None;
            }
            let percent = raw_limit.percent?;
            let model_name = raw_limit.scope?.model?.display_name?;
            let slug = model_name.to_lowercase().replace(' ', "_");
            let resets_at: Option<DateTime<Utc>> =
                raw_limit.resets_at.as_deref().and_then(|s| s.parse().ok());
            Some(QuotaWindow {
                key: format!("weekly_scoped_{slug}"),
                label: format!("Weekly ({model_name})"),
                utilization: percent,
                resets_at,
            })
        })
        .collect()
}

/// Fetch and parse /api/oauth/usage.
pub async fn fetch_usage(
    client: &Client,
    token: &str,
) -> ApiResult<(Vec<QuotaWindow>, Option<ExtraUsage>)> {
    let headers = build_headers(token);

    let response = match client.get(USAGE_URL).headers(headers).send().await {
        Ok(r) => r,
        Err(e) => return ApiResult::NetworkError(e.to_string()),
    };

    let status = response.status();
    match status {
        StatusCode::UNAUTHORIZED => return ApiResult::Unauthorized,
        StatusCode::TOO_MANY_REQUESTS => return ApiResult::RateLimited,
        s if s.is_server_error() => {
            return ApiResult::NetworkError(format!("Server error: {}", s));
        }
        s if !s.is_success() => {
            return ApiResult::NetworkError(format!("Unexpected status: {}", s));
        }
        _ => {}
    }

    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => return ApiResult::NetworkError(e.to_string()),
    };

    // Parse the usage response as a flat JSON object. We handle extra_usage
    // separately since it's nested differently.
    let raw: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return ApiResult::ParseError(format!("JSON parse error: {} — body: {}", e, &body[..body.len().min(200)])),
    };

    let obj = match raw.as_object() {
        Some(o) => o,
        None => return ApiResult::ParseError("Expected JSON object from usage endpoint".to_string()),
    };

    let mut windows: Vec<QuotaWindow> = Vec::new();
    let mut extra_usage: Option<ExtraUsage> = None;
    let mut spend: Option<RawSpend> = None;

    for (key, value) in obj {
        if key == "extra_usage" {
            // Parse extra_usage separately
            if !value.is_null() {
                if let Ok(raw_extra) = serde_json::from_value::<RawExtraUsage>(value.clone()) {
                    extra_usage = Some(ExtraUsage {
                        enabled: raw_extra.is_enabled.unwrap_or(false),
                        used_credits: raw_extra.used_credits,
                        monthly_limit: raw_extra.monthly_limit,
                        utilization: raw_extra.utilization,
                    });
                }
            }
            continue;
        }

        if key == "spend" {
            // The `spend` block is the authoritative overage source on accounts
            // where `extra_usage` is empty; capture it for synthesis below.
            // (Still excluded from quota windows — it is not a rate window.)
            if !value.is_null() {
                if let Ok(raw_spend) = serde_json::from_value::<RawSpend>(value.clone()) {
                    spend = Some(raw_spend);
                }
            }
            continue;
        }

        if key == "limits" {
            // Most entries (`kind: "session"` / `"weekly_all"`) duplicate the
            // five_hour/seven_day windows already parsed above. But
            // `kind: "weekly_scoped"` carries per-model weekly caps (e.g. a
            // "Fable" weekly limit) that appear nowhere else in the payload,
            // so pull those out as their own windows.
            if let Some(arr) = value.as_array() {
                windows.extend(scoped_weekly_windows_from_limits(arr));
            }
            continue;
        }

        // Skip null quota windows
        if value.is_null() {
            continue;
        }

        // Skip non-window keys returned by the API (e.g. "limits" duplicates
        // five_hour/seven_day; "spend" is a spend-tracker).  Both lack a
        // .utilization field and would render as perpetual 0% bars.
        if crate::plan_detector::is_excluded_window_key(key) {
            continue;
        }

        // Parse the quota window
        let utilization = value
            .get("utilization")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32)
            .unwrap_or(0.0);

        let resets_at: Option<DateTime<Utc>> = value
            .get("resets_at")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok());

        windows.push(QuotaWindow {
            label: label_for_key(key),
            key: key.clone(),
            utilization,
            resets_at,
        });
    }

    // Sort windows in canonical display order
    crate::plan_detector::sort_windows(&mut windows);

    // Prefer the `spend` block as the overage source when `extra_usage` carries
    // no usable numbers (true on accounts where the legacy block is empty but
    // the user has a real monthly spend limit, e.g. the EUR €20 cap case).
    let extra_usage = match (extra_usage, spend) {
        (None, Some(sp)) => Some(extra_usage_from_spend(&sp)),
        (Some(eu), Some(sp)) if eu.used_credits.is_none() && eu.monthly_limit.is_none() => {
            Some(extra_usage_from_spend(&sp))
        }
        (eu, _) => eu,
    };

    ApiResult::Ok((windows, extra_usage))
}

/// Fetch and parse /api/oauth/profile.
pub async fn fetch_profile(client: &Client, token: &str) -> ApiResult<Profile> {
    let headers = build_headers(token);

    let response = match client.get(PROFILE_URL).headers(headers).send().await {
        Ok(r) => r,
        Err(e) => return ApiResult::NetworkError(e.to_string()),
    };

    let status = response.status();
    match status {
        StatusCode::UNAUTHORIZED => return ApiResult::Unauthorized,
        StatusCode::TOO_MANY_REQUESTS => return ApiResult::RateLimited,
        s if s.is_server_error() => {
            return ApiResult::NetworkError(format!("Server error: {}", s));
        }
        s if !s.is_success() => {
            return ApiResult::NetworkError(format!("Unexpected status: {}", s));
        }
        _ => {}
    }

    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => return ApiResult::NetworkError(e.to_string()),
    };

    let raw: RawProfile = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return ApiResult::ParseError(format!("Profile JSON parse error: {}", e)),
    };

    let account = raw.account.unwrap_or_default();
    let org = raw.organization.unwrap_or_default();

    ApiResult::Ok(Profile {
        display_name: account.display_name.or(account.full_name),
        email: account.email,
        has_claude_max: account.has_claude_max.unwrap_or(false),
        has_claude_pro: account.has_claude_pro.unwrap_or(false),
        rate_limit_tier: org.rate_limit_tier,
        has_extra_usage_enabled: org.has_extra_usage_enabled.unwrap_or(false),
        subscription_status: org.subscription_status,
    })
}

// Implement Default for raw profile subfields used in unwrap_or_default above
impl Default for crate::model::RawProfileAccount {
    fn default() -> Self {
        Self {
            uuid: None,
            full_name: None,
            display_name: None,
            email: None,
            has_claude_max: None,
            has_claude_pro: None,
            created_at: None,
        }
    }
}

impl Default for crate::model::RawProfileOrganization {
    fn default() -> Self {
        Self {
            uuid: None,
            name: None,
            rate_limit_tier: None,
            has_extra_usage_enabled: None,
            subscription_status: None,
            billing_type: None,
            organization_type: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape observed live from /api/oauth/usage once a per-model weekly cap
    /// ("Fable") is active: a "weekly_scoped" entry alongside the "session"
    /// and "weekly_all" entries that duplicate five_hour/seven_day.
    fn sample_limits() -> Vec<serde_json::Value> {
        serde_json::from_str(
            r#"[
                { "kind": "session", "group": "session", "percent": 51,
                  "resets_at": "2026-07-13T16:00:00.230399+02:00" },
                { "kind": "weekly_all", "group": "weekly", "percent": 59,
                  "resets_at": "2026-07-13T23:00:00.230425+02:00" },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 81,
                  "resets_at": "2026-07-13T23:00:00.230755+02:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" } } }
            ]"#,
        )
        .unwrap()
    }

    #[test]
    fn scoped_weekly_window_extracted_from_limits() {
        let windows = scoped_weekly_windows_from_limits(&sample_limits());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].key, "weekly_scoped_fable");
        assert_eq!(windows[0].label, "Weekly (Fable)");
        assert_eq!(windows[0].utilization, 81.0);
        assert!(windows[0].resets_at.is_some());
    }

    #[test]
    fn session_and_weekly_all_entries_are_not_duplicated() {
        let windows = scoped_weekly_windows_from_limits(&sample_limits());
        assert!(!windows.iter().any(|w| w.key == "five_hour" || w.key == "seven_day"));
    }

    #[test]
    fn entry_missing_model_display_name_is_skipped() {
        let entries: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{ "kind": "weekly_scoped", "group": "weekly", "percent": 10,
                  "scope": { "model": { "id": null, "display_name": null } } }]"#,
        )
        .unwrap();
        assert!(scoped_weekly_windows_from_limits(&entries).is_empty());
    }

    #[test]
    fn empty_limits_array_yields_no_windows() {
        assert!(scoped_weekly_windows_from_limits(&[]).is_empty());
    }
}
