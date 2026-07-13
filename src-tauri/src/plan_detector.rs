//! Maps credentials + profile + usage payload → `Plan` enum.
//!
//! Priority order (§2.3):
//!   1. Profile endpoint (`has_claude_max`, `has_claude_pro`, `rate_limit_tier`)
//!   2. Credentials file (`subscriptionType`, `rateLimitTier`)
//!   3. Usage payload shape (presence of model-specific windows)
//!   4. Unknown

use crate::model::{Plan, Profile, QuotaWindow};

/// Map a known `rate_limit_tier` string to a `Plan`.
/// Strings are undocumented; we map what is observed and fall back to `Plan::Max`.
fn tier_to_plan(tier: &str) -> Plan {
    let lower = tier.to_lowercase();
    if lower.contains("20") || lower.contains("twenty") {
        return Plan::Max20x;
    }
    if lower.contains("5") || lower.contains("five") {
        return Plan::Max5x;
    }
    if lower.contains("max") {
        return Plan::Max;
    }
    if lower.contains("pro") {
        return Plan::Pro;
    }
    if lower.contains("free") {
        return Plan::Free;
    }
    Plan::Max // default for any tier we don't recognise
}

/// Detect the plan from the profile API response.
pub fn detect_from_profile(profile: &Profile) -> Plan {
    if profile.has_claude_max {
        if let Some(tier) = &profile.rate_limit_tier {
            let plan = tier_to_plan(tier);
            // Only trust tier refinement if we already know it's Max.
            match plan {
                Plan::Max5x | Plan::Max20x | Plan::Max => return plan,
                _ => {}
            }
        }
        return Plan::Max;
    }
    if profile.has_claude_pro {
        return Plan::Pro;
    }
    Plan::Free
}

/// Detect plan from credential hints (offline, used before first profile call).
pub fn detect_from_credentials(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<Plan> {
    if let Some(tier) = rate_limit_tier {
        let plan = tier_to_plan(tier);
        if plan != Plan::Max && plan != Plan::Unknown {
            return Some(plan);
        }
    }
    match subscription_type {
        Some(s) if s.to_lowercase().contains("max") => Some(Plan::Max),
        Some(s) if s.to_lowercase().contains("pro") => Some(Plan::Pro),
        Some(s) if s.to_lowercase().contains("free") => Some(Plan::Free),
        _ => None,
    }
}

/// Last-resort inference from which quota windows are present in the usage payload.
pub fn detect_from_windows(windows: &[QuotaWindow]) -> Plan {
    let has_sonnet = windows.iter().any(|w| w.key.contains("sonnet"));
    let has_opus = windows.iter().any(|w| w.key.contains("opus"));
    let has_seven_day = windows.iter().any(|w| w.key == "seven_day");

    if has_sonnet || has_opus {
        return Plan::Max;
    }
    if has_seven_day {
        return Plan::Pro;
    }
    Plan::Unknown
}

/// Determine the canonical plan label, incorporating an optional user override.
pub fn resolve_plan(
    profile: Option<&Profile>,
    cred_sub_type: Option<&str>,
    cred_rate_tier: Option<&str>,
    windows: &[QuotaWindow],
    override_plan: Option<&Plan>,
) -> Plan {
    // User override wins.
    if let Some(p) = override_plan {
        return p.clone();
    }
    // Profile endpoint is authoritative.
    if let Some(p) = profile {
        let plan = detect_from_profile(p);
        if plan != Plan::Unknown {
            return plan;
        }
    }
    // Credential hints (offline).
    if let Some(plan) = detect_from_credentials(cred_sub_type, cred_rate_tier) {
        return plan;
    }
    // Usage shape inference.
    detect_from_windows(windows)
}

/// Raw `/api/oauth/usage` top-level keys that are NOT real quota windows and
/// must be excluded at parse time so they never render as bars.
///
/// `"limits"` — an array, not an object with `.utilization`, so it can never
/// be parsed as a generic top-level window (it would render as a perpetual 0%
/// "Limits" bar). It is handled separately in `usage_client::fetch_usage`,
/// which pulls the `kind: "weekly_scoped"` entries out of it (per-model
/// weekly caps such as Fable) as their own `QuotaWindow`s; this exclusion
/// only stops it from *also* being treated as a naive top-level window.
///
/// `"spend"` — a spend-tracker object with no `.utilization` field; same
/// symptom: perpetual 0% "Spend" bar, unrelated to quota windows.
///
/// The list is intentionally a one-line-editable constant so it stays trivial
/// to extend when Anthropic adds more non-window keys to the response.
/// Keys are the exact raw API keys (case-sensitive); labels are NOT used here.
pub const EXCLUDED_WINDOW_KEYS: &[&str] = &["limits", "spend"];

/// Return `true` if `key` is a known non-window key that must not be parsed
/// into a `QuotaWindow`.  Applied at parse time in `usage_client.rs`.
pub fn is_excluded_window_key(key: &str) -> bool {
    EXCLUDED_WINDOW_KEYS.contains(&key)
}

/// Return a human-readable label for a raw quota window key.
pub fn label_for_key(key: &str) -> String {
    match key {
        "five_hour" => "5-hour session".to_string(),
        "seven_day" => "Weekly".to_string(),
        "seven_day_sonnet" => "Weekly (Sonnet)".to_string(),
        "seven_day_opus" => "Weekly (Opus)".to_string(),
        "seven_day_cowork" => "Weekly (Co-work)".to_string(),
        "seven_day_oauth_apps" => "Weekly (OAuth Apps)".to_string(),
        other => {
            // Humanize: replace underscores with spaces, title-case.
            let words: Vec<String> = other
                .split('_')
                .map(|w| {
                    let mut chars = w.chars();
                    match chars.next() {
                        None => String::new(),
                        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                    }
                })
                .collect();
            words.join(" ")
        }
    }
}

/// Canonical display ordering for quota windows.
/// Known keys first in priority order; unknowns appended after.
pub fn sort_windows(windows: &mut Vec<QuotaWindow>) {
    let priority: &[&str] = &[
        "five_hour",
        "seven_day",
        "seven_day_sonnet",
        "seven_day_opus",
        "seven_day_cowork",
        "seven_day_oauth_apps",
    ];
    windows.sort_by_key(|w| {
        priority
            .iter()
            .position(|&k| k == w.key)
            .unwrap_or(priority.len())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Profile;

    fn make_profile(has_max: bool, has_pro: bool, tier: Option<&str>) -> Profile {
        Profile {
            display_name: None,
            email: None,
            has_claude_max: has_max,
            has_claude_pro: has_pro,
            rate_limit_tier: tier.map(|s| s.to_string()),
            has_extra_usage_enabled: false,
            subscription_status: None,
        }
    }

    #[test]
    fn profile_max_20x() {
        let p = make_profile(true, false, Some("claude_max_20x"));
        assert_eq!(detect_from_profile(&p), Plan::Max20x);
    }

    #[test]
    fn profile_max_5x() {
        let p = make_profile(true, false, Some("claude_max_5x"));
        assert_eq!(detect_from_profile(&p), Plan::Max5x);
    }

    #[test]
    fn profile_max_no_tier() {
        let p = make_profile(true, false, None);
        assert_eq!(detect_from_profile(&p), Plan::Max);
    }

    #[test]
    fn profile_pro() {
        let p = make_profile(false, true, None);
        assert_eq!(detect_from_profile(&p), Plan::Pro);
    }

    #[test]
    fn profile_free() {
        let p = make_profile(false, false, None);
        assert_eq!(detect_from_profile(&p), Plan::Free);
    }

    #[test]
    fn cred_hints_max() {
        assert_eq!(
            detect_from_credentials(Some("max"), None),
            Some(Plan::Max)
        );
    }

    #[test]
    fn cred_hints_tier_overrides_sub() {
        assert_eq!(
            detect_from_credentials(Some("max"), Some("claude_max_20x")),
            Some(Plan::Max20x)
        );
    }

    #[test]
    fn label_known_keys() {
        assert_eq!(label_for_key("five_hour"), "5-hour session");
        assert_eq!(label_for_key("seven_day"), "Weekly");
        assert_eq!(label_for_key("seven_day_sonnet"), "Weekly (Sonnet)");
    }

    #[test]
    fn label_unknown_key_humanized() {
        assert_eq!(label_for_key("seven_day_new_model"), "Seven Day New Model");
    }

    #[test]
    fn excluded_keys_are_listed() {
        // The constant must contain exactly the two noise keys confirmed from
        // the Phase-1 live payload capture.
        assert!(EXCLUDED_WINDOW_KEYS.contains(&"limits"));
        assert!(EXCLUDED_WINDOW_KEYS.contains(&"spend"));
    }

    #[test]
    fn is_excluded_window_key_rejects_noise() {
        assert!(is_excluded_window_key("limits"));
        assert!(is_excluded_window_key("spend"));
    }

    #[test]
    fn is_excluded_window_key_passes_real_windows() {
        // Real quota windows must NOT be excluded.
        assert!(!is_excluded_window_key("five_hour"));
        assert!(!is_excluded_window_key("seven_day"));
        assert!(!is_excluded_window_key("seven_day_sonnet"));
        assert!(!is_excluded_window_key("seven_day_opus"));
        assert!(!is_excluded_window_key("extra_usage"));
    }

    #[test]
    fn sort_windows_canonical_order() {
        let mut windows = vec![
            QuotaWindow {
                key: "seven_day_sonnet".to_string(),
                label: "".to_string(),
                utilization: 0.0,
                resets_at: None,
            },
            QuotaWindow {
                key: "five_hour".to_string(),
                label: "".to_string(),
                utilization: 0.0,
                resets_at: None,
            },
            QuotaWindow {
                key: "seven_day".to_string(),
                label: "".to_string(),
                utilization: 0.0,
                resets_at: None,
            },
        ];
        sort_windows(&mut windows);
        assert_eq!(windows[0].key, "five_hour");
        assert_eq!(windows[1].key, "seven_day");
        assert_eq!(windows[2].key, "seven_day_sonnet");
    }
}
