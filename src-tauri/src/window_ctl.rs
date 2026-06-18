//! Tauri commands for window management: opacity, size presets, show/hide.
//! Invoked from the UI context menu and tray handler.

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::config::{
    WINDOW_DEFAULT_HEIGHT, WINDOW_DEFAULT_WIDTH, WINDOW_LARGE_HEIGHT, WINDOW_LARGE_WIDTH,
    WINDOW_MEDIUM_HEIGHT, WINDOW_MEDIUM_WIDTH, WINDOW_SMALL_HEIGHT, WINDOW_SMALL_WIDTH,
};
use crate::model::Plan;
use crate::poller::{RefreshNotify, SharedPollerState};
use crate::settings::{self, Settings};

/// Get the main application window.
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window("main")
}

/// Set CSS-level opacity by emitting a JS command to the WebView, then persist it.
/// The UI applies the eval result instantly; persistence is a best-effort side-effect.
#[tauri::command]
pub async fn set_opacity(app: AppHandle, opacity: f32) -> Result<(), String> {
    let opacity = settings::clamp_opacity(opacity);
    let window = main_window(&app).ok_or("Main window not found")?;
    window
        .eval(&format!(
            "document.getElementById('app').style.opacity = '{}'",
            opacity
        ))
        .map_err(|e| e.to_string())?;

    // Persist the new opacity.  Load current settings first so any future fields
    // are preserved, then update only opacity and write back.
    let mut s = settings::load(&app);
    s.opacity = opacity;
    if let Err(e) = settings::save(&app, &s) {
        // Non-fatal: live opacity already applied; log and continue.
        log::warn!("Failed to save opacity setting: {e}");
    }

    Ok(())
}

/// Return the persisted settings so the frontend can apply them at startup.
#[tauri::command]
pub async fn get_settings(app: AppHandle) -> Result<Settings, String> {
    Ok(settings::load(&app))
}

/// Window size in logical pixels, returned by `get_window_size`.
#[derive(serde::Serialize)]
pub struct WindowSize {
    pub width: f64,
    pub height: f64,
}

/// Return the current inner size of the main window in **logical** pixels.
/// Reads the physical size and divides by the scale factor to match the
/// Logical units that `set_size_preset` and `set_window_size` use.
#[tauri::command]
pub async fn get_window_size(app: AppHandle) -> Result<WindowSize, String> {
    let window = main_window(&app).ok_or("Main window not found")?;
    let physical = window.inner_size().map_err(|e| e.to_string())?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    Ok(WindowSize {
        width: physical.width as f64 / scale,
        height: physical.height as f64 / scale,
    })
}

/// Resize the main window to the given **logical** pixel dimensions.
/// Matches the Logical-size pattern used by `set_size_preset`.
#[tauri::command]
pub async fn set_window_size(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let window = main_window(&app).ok_or("Main window not found")?;
    window
        .set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }))
        .map_err(|e| e.to_string())
}

/// Resolve a preset name to `(width, height)` in logical pixels.
///
/// Unknown preset names fall back to the default dimensions so callers never
/// need to special-case an invalid stored value.
pub fn preset_size(preset: &str) -> (f64, f64) {
    match preset {
        "small" => (WINDOW_SMALL_WIDTH, WINDOW_SMALL_HEIGHT),
        "medium" => (WINDOW_MEDIUM_WIDTH, WINDOW_MEDIUM_HEIGHT),
        "large" => (WINDOW_LARGE_WIDTH, WINDOW_LARGE_HEIGHT),
        _ => (WINDOW_DEFAULT_WIDTH, WINDOW_DEFAULT_HEIGHT),
    }
}

/// Apply a size preset to the window, then persist the choice to settings.
///
/// Persistence is best-effort: a write failure is logged but does **not** cause
/// the command to fail, because the live resize already succeeded.
#[tauri::command]
pub async fn set_size_preset(app: AppHandle, preset: String) -> Result<(), String> {
    let (w, h) = preset_size(&preset);

    let window = main_window(&app).ok_or("Main window not found")?;
    window
        .set_size(tauri::Size::Logical(tauri::LogicalSize { width: w, height: h }))
        .map_err(|e| e.to_string())?;

    // Persist the chosen preset so it survives a restart.
    let mut s = settings::load(&app);
    s.size_preset = preset;
    if let Err(e) = settings::save(&app, &s) {
        log::warn!("Failed to save size_preset setting: {e}");
    }

    Ok(())
}

/// Toggle always-on-top state.
#[tauri::command]
pub async fn set_always_on_top(app: AppHandle, enabled: bool) -> Result<(), String> {
    let window = main_window(&app).ok_or("Main window not found")?;
    window
        .set_always_on_top(enabled)
        .map_err(|e| e.to_string())
}

/// Show or hide the main window.
#[tauri::command]
pub async fn toggle_visibility(app: AppHandle) -> Result<(), String> {
    let window = main_window(&app).ok_or("Main window not found")?;
    if window.is_visible().unwrap_or(false) {
        window.hide().map_err(|e| e.to_string())
    } else {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())
    }
}

/// Map an optional plan string to a `Plan` enum value.
///
/// `None`, `Some("auto")`, or any unrecognised string maps to `None` (auto-detect).
/// This helper is shared by `set_plan_override` and the startup hydration in `lib.rs`.
pub fn parse_plan_override(plan: Option<&str>) -> Option<Plan> {
    match plan {
        Some("free") => Some(Plan::Free),
        Some("pro") => Some(Plan::Pro),
        Some("max5x") => Some(Plan::Max5x),
        Some("max20x") => Some(Plan::Max20x),
        Some("max") => Some(Plan::Max),
        _ => None, // None, "auto", or any unknown string → clear override
    }
}

/// Set a plan override in the poller state (user picks from context menu or settings panel),
/// and persist the choice to settings.
///
/// `plan` is `None` (or `"auto"`) to clear the override.
/// Persistence is best-effort: a write failure is logged but does not fail the command.
///
/// Unlike `request_refresh`, this always wakes the poller regardless of rate-limit state:
/// the override itself is the data that changed, and the next snapshot must reflect it
/// promptly without waiting up to `current_interval` seconds.
#[tauri::command]
pub async fn set_plan_override(
    app: AppHandle,
    plan: Option<String>,
    state: tauri::State<'_, SharedPollerState>,
    notify: tauri::State<'_, RefreshNotify>,
) -> Result<(), String> {
    // Update live poller state and set the refresh flag while holding the lock,
    // so the woken loop always sees the new plan_override before it runs.
    let parsed = parse_plan_override(plan.as_deref());
    {
        let mut s = state.lock().unwrap();
        s.plan_override = parsed;
        s.refresh_requested = true;
    }

    // Wake the poller immediately.  The refresh_requested flag bypasses both
    // the idle-pause check (line ~129 in poller.rs) and the rate-limit backoff
    // check (line ~141), so the new snapshot is emitted even during backoff.
    notify.notify_one();

    // Persist the raw string (or None) so it survives a restart.
    let mut cfg = settings::load(&app);
    cfg.plan_override = match plan.as_deref() {
        Some("auto") | None => None,
        Some(s) => Some(s.to_string()),
    };
    if let Err(e) = settings::save(&app, &cfg) {
        log::warn!("Failed to save plan_override setting: {e}");
    }

    Ok(())
}

/// Request an immediate refresh (bypasses the wait interval; still respects rate-limit).
#[tauri::command]
pub async fn request_refresh(
    _app: AppHandle,
    state: tauri::State<'_, SharedPollerState>,
    notify: tauri::State<'_, RefreshNotify>,
) -> Result<(), String> {
    let not_rate_limited = {
        let mut s = state.lock().unwrap();
        if s.rate_limited_until
            .map_or(true, |until| std::time::Instant::now() >= until)
        {
            // Set the flag while holding the lock so the woken loop always sees it.
            s.refresh_requested = true;
            true
        } else {
            false
        }
    }; // MutexGuard dropped here

    if not_rate_limited {
        // Wake the poller so it acts on the flag immediately.
        notify.notify_one();
        Ok(())
    } else {
        // Rate-limited: do NOT notify; return error as before.
        Err("Rate limited — cannot refresh right now".to_string())
    }
}

/// Quit the application.
#[tauri::command]
pub async fn quit_app(app: AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}
