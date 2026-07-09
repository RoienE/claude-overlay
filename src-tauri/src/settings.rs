//! Persisted application settings — hand-rolled JSON file in the app config dir.
//!
//! Settings file: `{app_config_dir}/settings.json`
//! (e.g. `%APPDATA%\com.claude-overlay.app\settings.json` on Windows)
//!
//! Design rules:
//! - `load()` is **infallible** — on any error it returns `Settings::default()`.
//!   The app must start correctly on first launch when no file exists.
//! - `save()` returns `Result` so callers can log failures without panicking.
//! - `#[serde(default)]` on every field means future fields added here won't
//!   break parsing of older settings files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::config::{OPACITY_MAX, OPACITY_MIN};

/// All persisted user preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_opacity")]
    pub opacity: f32,

    /// Window size preset last chosen by the user.
    /// Valid values: `"small"`, `"medium"`, `"large"`, `"default"`.
    /// Defaults to `"default"` so new installs open at the configured window size.
    #[serde(default = "default_size_preset")]
    pub size_preset: String,

    /// User-pinned plan override. `None` means auto-detect; otherwise one of
    /// `"free"`, `"pro"`, `"max5x"`, `"max20x"`, `"max"`.
    /// Uses `skip_serializing_if` so `null` round-trips cleanly as an absent key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_override: Option<String>,

    /// Recency window for the active sessions tracker (minutes).
    /// Valid range: 5–720. Defaults to 30 min.
    #[serde(default = "default_history_threshold_mins")]
    pub history_threshold_mins: u32,

    /// Whether anonymous telemetry is enabled (opt-out; default `true`).
    /// Absent in older settings files → defaults to `true` (backward-compat).
    #[serde(default = "default_telemetry_enabled")]
    pub telemetry_enabled: bool,

    /// Randomly-generated anonymous install ID (UUIDv4). `None` until first run.
    /// Generated once by `ensure_install_id` and persisted thereafter.
    #[serde(default)]
    pub install_id: Option<String>,
}

fn default_opacity() -> f32 {
    0.92
}

fn default_size_preset() -> String {
    "default".to_string()
}

fn default_history_threshold_mins() -> u32 {
    30
}

fn default_telemetry_enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            opacity: default_opacity(),
            size_preset: default_size_preset(),
            plan_override: None,
            history_threshold_mins: default_history_threshold_mins(),
            telemetry_enabled: default_telemetry_enabled(),
            install_id: None,
        }
    }
}

/// Ensure the settings have an `install_id`.
///
/// If `install_id` is `None`, generates a new random UUIDv4, stores it in
/// `settings.install_id`, and returns `true` (signals "first run").
/// If `install_id` is already set, returns `false` (no-op).
pub fn ensure_install_id(settings: &mut Settings) -> bool {
    if settings.install_id.is_none() {
        settings.install_id = Some(Uuid::new_v4().to_string());
        true
    } else {
        false
    }
}

/// Resolve the path to `settings.json` inside the app's own config dir.
/// Creates the directory if it does not exist.
pub fn settings_path(app: &AppHandle) -> PathBuf {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."));

    // Create the directory if it doesn't exist; ignore errors (handled in load/save).
    let _ = std::fs::create_dir_all(&dir);

    dir.join("settings.json")
}

/// Load settings from disk. Returns `Settings::default()` on any error (missing
/// file, bad JSON, permission denied — all treated as "use defaults").
pub fn load(app: &AppHandle) -> Settings {
    let path = settings_path(app);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Settings::default(),
    };
    serde_json::from_str::<Settings>(&raw).unwrap_or_default()
}

/// Persist settings to disk. Returns `Err` on failure; callers should log and
/// continue — a save failure must not affect the live opacity already applied.
pub fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app);
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create settings dir: {e}"))?;
    }
    let json =
        serde_json::to_string_pretty(settings).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("Failed to write settings: {e}"))
}

/// Clamp an opacity value to the allowed range `[OPACITY_MIN, OPACITY_MAX]`.
#[inline]
pub fn clamp_opacity(v: f32) -> f32 {
    v.clamp(OPACITY_MIN, OPACITY_MAX)
}

/// Clamp a history threshold value to the allowed range `[5, 720]` minutes.
#[inline]
pub fn clamp_history_threshold(v: u32) -> u32 {
    v.clamp(5, 720)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing opacity tests (preserved) ───────────────────────────────────

    #[test]
    fn default_opacity_is_092() {
        let s = Settings::default();
        assert!((s.opacity - 0.92).abs() < f32::EPSILON, "default opacity should be 0.92");
    }

    #[test]
    fn deserialize_missing_opacity_uses_default() {
        // An empty JSON object should give us the default (via #[serde(default)]).
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!((s.opacity - 0.92).abs() < f32::EPSILON);
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let original = Settings { opacity: 0.75, size_preset: "large".to_string(), plan_override: Some("pro".to_string()), history_threshold_mins: 30, ..Settings::default() };
        let json = serde_json::to_string(&original).unwrap();
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert!((restored.opacity - 0.75).abs() < f32::EPSILON, "round-trip must preserve opacity");
        assert_eq!(restored.size_preset, "large", "round-trip must preserve size_preset");
        assert_eq!(restored.plan_override.as_deref(), Some("pro"), "round-trip must preserve plan_override");
    }

    #[test]
    fn clamp_keeps_value_within_range() {
        // Below minimum → clamped to OPACITY_MIN
        assert!((clamp_opacity(0.0) - OPACITY_MIN).abs() < f32::EPSILON);
        assert!((clamp_opacity(-1.0) - OPACITY_MIN).abs() < f32::EPSILON);

        // Above maximum → clamped to OPACITY_MAX
        assert!((clamp_opacity(2.0) - OPACITY_MAX).abs() < f32::EPSILON);
        assert!((clamp_opacity(1.5) - OPACITY_MAX).abs() < f32::EPSILON);

        // Within range → unchanged
        let mid = (OPACITY_MIN + OPACITY_MAX) / 2.0;
        assert!((clamp_opacity(mid) - mid).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_file_returns_default() {
        // Simulate reading a non-existent file.
        let result: Result<Settings, _> =
            serde_json::from_str::<Settings>("this is not json");
        // If the file can't be parsed, we fall back to default.
        assert!(result.is_err(), "bad JSON should produce an error so we fall back");
        let fallback = result.unwrap_or_default();
        assert!((fallback.opacity - 0.92).abs() < f32::EPSILON);
    }

    #[test]
    fn opacity_boundary_values_survive_round_trip() {
        for &v in &[OPACITY_MIN, OPACITY_MAX, 0.5, 0.92] {
            let s = Settings { opacity: v, size_preset: "default".to_string(), plan_override: None, history_threshold_mins: 30, ..Settings::default() };
            let json = serde_json::to_string(&s).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert!((restored.opacity - v).abs() < f32::EPSILON, "value {v} must survive round-trip");
        }
    }

    // ── New fields: default values ────────────────────────────────────────────

    #[test]
    fn default_size_preset_is_default() {
        let s = Settings::default();
        assert_eq!(s.size_preset, "default", "default size_preset should be \"default\"");
    }

    #[test]
    fn default_plan_override_is_none() {
        let s = Settings::default();
        assert!(s.plan_override.is_none(), "default plan_override should be None");
    }

    // ── New fields: #[serde(default)] coverage ────────────────────────────────

    #[test]
    fn empty_json_gives_new_field_defaults() {
        // An empty object must produce defaults for the new fields.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.size_preset, "default");
        assert!(s.plan_override.is_none());
    }

    #[test]
    fn opacity_only_json_gives_new_field_defaults() {
        // Old settings files that contain only "opacity" must load without error
        // and gain the new defaults — backward compatibility guarantee.
        let s: Settings = serde_json::from_str(r#"{"opacity":0.5}"#).unwrap();
        assert!((s.opacity - 0.5).abs() < f32::EPSILON);
        assert_eq!(s.size_preset, "default", "old file must gain default size_preset");
        assert!(s.plan_override.is_none(), "old file must gain default plan_override");
    }

    // ── New fields: round-trips ───────────────────────────────────────────────

    #[test]
    fn size_preset_round_trip() {
        for preset in &["small", "medium", "large", "default"] {
            let s = Settings { opacity: 0.92, size_preset: preset.to_string(), plan_override: None, history_threshold_mins: 30, ..Settings::default() };
            let json = serde_json::to_string(&s).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(&restored.size_preset, preset, "size_preset '{preset}' must survive round-trip");
        }
    }

    #[test]
    fn plan_override_some_round_trip() {
        for plan in &["free", "pro", "max5x", "max20x", "max"] {
            let s = Settings {
                opacity: 0.92,
                size_preset: "default".to_string(),
                plan_override: Some(plan.to_string()),
                history_threshold_mins: 30,
                ..Settings::default()
            };
            let json = serde_json::to_string(&s).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.plan_override.as_deref(), Some(*plan), "plan '{plan}' must survive round-trip");
        }
    }

    #[test]
    fn plan_override_none_round_trip() {
        let s = Settings { opacity: 0.92, size_preset: "default".to_string(), plan_override: None, history_threshold_mins: 30, ..Settings::default() };
        let json = serde_json::to_string(&s).unwrap();
        // None → skip_serializing_if, so "plan_override" key must not appear.
        assert!(!json.contains("plan_override"), "None plan_override must not be serialized");
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert!(restored.plan_override.is_none(), "absent plan_override key must deserialize to None");
    }

    // ── history_threshold_mins ────────────────────────────────────────────────

    #[test]
    fn default_history_threshold_mins_is_30() {
        let s = Settings::default();
        assert_eq!(s.history_threshold_mins, 30, "default history_threshold_mins should be 30");
    }

    #[test]
    fn empty_json_gives_history_threshold_default() {
        // An empty object must produce 30 for the new field (backward compat).
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.history_threshold_mins, 30);
    }

    #[test]
    fn opacity_only_json_gives_history_threshold_default() {
        // Old settings files with only "opacity" must gain the default — backward compat.
        let s: Settings = serde_json::from_str(r#"{"opacity":0.5}"#).unwrap();
        assert!((s.opacity - 0.5).abs() < f32::EPSILON);
        assert_eq!(s.history_threshold_mins, 30, "old file must gain default history_threshold_mins");
    }

    #[test]
    fn history_threshold_mins_round_trip() {
        for &mins in &[5u32, 30, 60, 120, 720] {
            let s = Settings {
                opacity: 0.92,
                size_preset: "default".to_string(),
                plan_override: None,
                history_threshold_mins: mins,
                ..Settings::default()
            };
            let json = serde_json::to_string(&s).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert_eq!(
                restored.history_threshold_mins, mins,
                "history_threshold_mins {mins} must survive round-trip"
            );
        }
    }

    #[test]
    fn clamp_history_threshold_enforces_bounds() {
        // Below minimum → clamped to 5.
        assert_eq!(clamp_history_threshold(0), 5);
        assert_eq!(clamp_history_threshold(4), 5);
        // At minimum → unchanged.
        assert_eq!(clamp_history_threshold(5), 5);
        // Within range → unchanged.
        assert_eq!(clamp_history_threshold(30), 30);
        assert_eq!(clamp_history_threshold(720), 720);
        // Above maximum → clamped to 720.
        assert_eq!(clamp_history_threshold(721), 720);
        assert_eq!(clamp_history_threshold(9999), 720);
    }

    // ── telemetry_enabled ─────────────────────────────────────────────────────

    #[test]
    fn default_telemetry_enabled_is_true() {
        let s = Settings::default();
        assert!(s.telemetry_enabled, "default telemetry_enabled should be true (opt-out)");
    }

    #[test]
    fn empty_json_gives_telemetry_enabled_true() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.telemetry_enabled, "missing key must default to true (backward compat)");
    }

    #[test]
    fn telemetry_enabled_false_round_trip() {
        let s = Settings { telemetry_enabled: false, ..Settings::default() };
        let json = serde_json::to_string(&s).unwrap();
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert!(!restored.telemetry_enabled, "false must survive round-trip");
    }

    #[test]
    fn telemetry_enabled_true_round_trip() {
        let s = Settings { telemetry_enabled: true, ..Settings::default() };
        let json = serde_json::to_string(&s).unwrap();
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert!(restored.telemetry_enabled, "true must survive round-trip");
    }

    // ── install_id ────────────────────────────────────────────────────────────

    #[test]
    fn default_install_id_is_none() {
        let s = Settings::default();
        assert!(s.install_id.is_none(), "default install_id should be None");
    }

    #[test]
    fn empty_json_gives_install_id_none() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.install_id.is_none(), "missing key must default to None");
    }

    #[test]
    fn ensure_install_id_generates_on_first_call() {
        let mut s = Settings::default();
        assert!(s.install_id.is_none());
        let first_run = ensure_install_id(&mut s);
        assert!(first_run, "should return true when generating a new ID");
        assert!(s.install_id.is_some(), "install_id must be set after ensure_install_id");
        // Verify it looks like a UUID (36 chars with hyphens).
        let id = s.install_id.as_deref().unwrap();
        assert_eq!(id.len(), 36, "UUID must be 36 characters");
        assert_eq!(id.chars().filter(|&c| c == '-').count(), 4, "UUID must have 4 hyphens");
    }

    #[test]
    fn ensure_install_id_is_stable_on_second_call() {
        let mut s = Settings::default();
        ensure_install_id(&mut s);
        let first_id = s.install_id.clone().unwrap();

        let second_run = ensure_install_id(&mut s);
        assert!(!second_run, "should return false when ID already exists");
        assert_eq!(
            s.install_id.as_deref().unwrap(),
            first_id,
            "install_id must not change on subsequent calls"
        );
    }

    #[test]
    fn install_id_round_trip() {
        let mut s = Settings::default();
        ensure_install_id(&mut s);
        let original_id = s.install_id.clone().unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.install_id.as_deref().unwrap(),
            original_id,
            "install_id must survive round-trip"
        );
    }

    #[test]
    fn ensure_install_id_generates_unique_ids() {
        let mut s1 = Settings::default();
        let mut s2 = Settings::default();
        ensure_install_id(&mut s1);
        ensure_install_id(&mut s2);
        assert_ne!(
            s1.install_id, s2.install_id,
            "two separate installs must get distinct IDs"
        );
    }
}
