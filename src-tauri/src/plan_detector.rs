//! Maps credentials + profile + usage payload → `Plan` enum.
//!
//! Priority order (§2.3):
//!   1. Profile endpoint (`has_claude_max`, `has_claude_pro`, `rate_limit_tier`)
//!   2. Credentials file (`subscriptionType`, `rateLimitTier`)
//!   3. Usage payload shape (presence of model-specific windows)
//!   4. Unknown

use crate::model::{Plan, Profile, QuotaWindow};
use std::sync::OnceLock;

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

/// Captured at compile time from the build environment (release CI sets it
/// from the `EXCLUDED_USAGE_KEYS` repository variable; see
/// `cargo:rerun-if-env-changed=EXCLUDED_USAGE_KEYS` in `build.rs`).
const EXCLUDED_KEYS_ENV: Option<&str> = option_env!("EXCLUDED_USAGE_KEYS");

/// Compiled-in fallback used when the env var is unset or blank at build time
/// (local dev, `cargo test`, forks). A set, non-blank `EXCLUDED_USAGE_KEYS`
/// REPLACES this list entirely — it is not merged with it. Keep this in sync
/// with the repository variable's intended value when editing this file.
///
/// Raw `/api/oauth/usage` top-level keys, case-sensitive, comma-separated:
///
/// `"limits"` — an array, not an object with `.utilization`, so it can never
/// be parsed as a generic top-level window (it would render as a perpetual 0%
/// "Limits" bar). It is handled separately in `usage_client::fetch_usage`,
/// which pulls the `kind: "weekly_scoped"` entries out of it (per-model
/// weekly caps such as Fable) as their own `QuotaWindow`s; this exclusion
/// only stops it from *also* being treated as a naive top-level window.
/// Defensive: `usage_client.rs` already `continue`s this key before the
/// exclusion check runs.
///
/// `"spend"` — a spend-tracker object with no `.utilization` field; same
/// symptom: perpetual 0% "Spend" bar, unrelated to quota windows. Defensive:
/// `usage_client.rs` already `continue`s this key before the exclusion check.
///
/// `"member_dashboard_available"` — a bare boolean feature flag (whether the
/// account can see the per-member usage dashboard on claude.ai), not a quota
/// window; this is the only one of the three the exclusion list alone hides.
///
/// `"iguana_necktie"`, `"seven_day_breakdown"`, `"weekly_scoped_shares"` —
/// **NEW**, inferred from their humanized display labels (reverse of
/// `label_for_key`'s fallback: lowercase, spaces → underscores).
pub const DEFAULT_EXCLUDED_USAGE_KEYS: &str =
    "limits,spend,member_dashboard_available,iguana_necktie,seven_day_breakdown,weekly_scoped_shares";

/// Pick the raw excluded-keys string: the env value wins whenever it is
/// `Some` and non-blank after trim; otherwise fall back to `default`. GitHub
/// renders an unset `vars.*` as the empty string, so "not set" and "set
/// blank" both land in the fallback by design.
fn resolve_raw_excluded<'a>(env_value: Option<&'a str>, default: &'a str) -> &'a str {
    match env_value {
        Some(v) if !v.trim().is_empty() => v,
        _ => default,
    }
}

/// Split a raw comma-separated key list into trimmed, non-empty entries.
/// No case normalization — matching stays case-sensitive.
pub fn parse_excluded_keys(raw: &str) -> Vec<&str> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Return `true` if `key` is present in `excluded` (case-sensitive, exactly
/// like the old `EXCLUDED_WINDOW_KEYS.contains` behaviour).
fn is_excluded_in(key: &str, excluded: &[&str]) -> bool {
    excluded.contains(&key)
}

static EXCLUDED: OnceLock<Vec<&'static str>> = OnceLock::new();

/// The effective excluded-key list for this build: the `EXCLUDED_USAGE_KEYS`
/// build-time env var if set and non-blank, else `DEFAULT_EXCLUDED_USAGE_KEYS`.
/// Parsed once and cached.
fn excluded_window_keys() -> &'static [&'static str] {
    EXCLUDED
        .get_or_init(|| {
            let raw = resolve_raw_excluded(EXCLUDED_KEYS_ENV, DEFAULT_EXCLUDED_USAGE_KEYS);
            let keys = parse_excluded_keys(raw);
            log::debug!("active excluded usage keys: {:?}", keys);
            keys
        })
        .as_slice()
}

/// Return `true` if `key` is a known non-window key that must not be parsed
/// into a `QuotaWindow`.  Applied at parse time in `usage_client.rs`.
pub fn is_excluded_window_key(key: &str) -> bool {
    is_excluded_in(key, excluded_window_keys())
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

    // Exclusion tests below target the pure fns with explicit inputs, never
    // the `OnceLock` static or `is_excluded_window_key`: `option_env!` captures
    // the compile-time environment, so a static-based assertion would flip on
    // any machine that happens to export EXCLUDED_USAGE_KEYS.

    #[test]
    fn parse_splits_on_commas() {
        assert_eq!(parse_excluded_keys("a,b,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_trims_and_drops_empty() {
        assert_eq!(parse_excluded_keys(" a , ,b,,c ,"), vec!["a", "b", "c"]);
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert_eq!(parse_excluded_keys("Spend"), vec!["Spend"]);
        assert!(!is_excluded_in("spend", &["Spend"]));
    }

    #[test]
    fn default_list_is_exactly_the_six_keys() {
        let mut got = parse_excluded_keys(DEFAULT_EXCLUDED_USAGE_KEYS);
        let mut want = vec![
            "limits",
            "spend",
            "member_dashboard_available",
            "iguana_necktie",
            "seven_day_breakdown",
            "weekly_scoped_shares",
        ];
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn env_overrides_default_and_blank_falls_back() {
        let d = "d";
        assert_eq!(resolve_raw_excluded(Some("x,y"), d), "x,y");
        assert_eq!(resolve_raw_excluded(None, d), d);
        assert_eq!(resolve_raw_excluded(Some(""), d), d);
        assert_eq!(resolve_raw_excluded(Some("   "), d), d);
    }

    #[test]
    fn is_excluded_in_rejects_noise_and_passes_real_windows() {
        let excluded = parse_excluded_keys(DEFAULT_EXCLUDED_USAGE_KEYS);
        for key in [
            "limits",
            "spend",
            "member_dashboard_available",
            "iguana_necktie",
            "seven_day_breakdown",
            "weekly_scoped_shares",
        ] {
            assert!(is_excluded_in(key, &excluded), "{key} should be excluded");
        }
        for key in [
            "five_hour",
            "seven_day",
            "seven_day_sonnet",
            "extra_usage",
            "weekly_scoped_fable",
            "zebra_hat",
        ] {
            assert!(
                !is_excluded_in(key, &excluded),
                "{key} must render, not be excluded"
            );
        }
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
