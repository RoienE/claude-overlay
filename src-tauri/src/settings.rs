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

use crate::config::{OPACITY_MAX, OPACITY_MIN};

/// All persisted user preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}

fn default_opacity() -> f32 {
    0.92
}

impl Default for Settings {
    fn default() -> Self {
        Self { opacity: default_opacity() }
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

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let original = Settings { opacity: 0.75 };
        let json = serde_json::to_string(&original).unwrap();
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert!((restored.opacity - 0.75).abs() < f32::EPSILON, "round-trip must preserve value");
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
            let s = Settings { opacity: v };
            let json = serde_json::to_string(&s).unwrap();
            let restored: Settings = serde_json::from_str(&json).unwrap();
            assert!((restored.opacity - v).abs() < f32::EPSILON, "value {v} must survive round-trip");
        }
    }
}
