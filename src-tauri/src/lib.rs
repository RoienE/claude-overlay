//! Library crate root. Wires up Tauri app setup for both the binary and tests.

pub mod config;
pub mod credential_source;
pub mod fallback_logs;
pub mod model;
pub mod plan_detector;
pub mod poller;
pub mod sessions;
pub mod settings;
pub mod telemetry;
pub mod usage_client;
pub mod window_ctl;

use std::sync::{Arc, Mutex};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager,
};
use tokio::sync::mpsc;

use poller::{PollerState, RefreshNotify, SharedPollerState};
use telemetry::Telemetry;

/// Set the Windows AppUserModelID at process start.
///
/// Tauri v2 does not call SetCurrentProcessExplicitAppUserModelID itself.
/// The NSIS installer sets `System.AppUserModel.ID = com.claude-overlay.application`
/// on the Start Menu .lnk via SetLnkAppUserModelId, but without the running
/// process registering the same AUMID, Windows treats the shortcut as a bare
/// .lnk (shortcut-arrow overlay, generic icon).  Setting it here makes the
/// process AUMID match the shortcut AUMID, which suppresses the arrow and
/// restores the correct icon.
#[cfg(windows)]
fn set_aumid() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    extern "system" {
        fn SetCurrentProcessExplicitAppUserModelID(appid: *const u16) -> i32;
    }

    let wide: Vec<u16> = OsStr::new("com.claude-overlay.application")
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect();

    // SAFETY: wide is null-terminated and lives for the duration of this call.
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
}

pub fn run() {
    // Must be called before any window or COM object is created so Windows can
    // associate the process with the Start Menu shortcut's AppUserModelID.
    #[cfg(windows)]
    set_aumid();

    env_logger::init();

    let poller_state: SharedPollerState = Arc::new(Mutex::new(PollerState::default()));
    let refresh_notify: RefreshNotify = Arc::new(tokio::sync::Notify::new());

    tauri::Builder::default()
        // single-instance MUST be registered first so it can intercept a second
        // launch before the rest of the app initialises.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // A second instance was launched — bring the existing window to front.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(poller_state.clone())
        .manage(refresh_notify.clone())
        .setup(move |app| {
            // ── Build tray icon ───────────────────────────────────────────────
            let show_item = MenuItem::with_id(app, "show", "Show / Hide", true, None::<&str>)?;
            let refresh_item =
                MenuItem::with_id(app, "refresh", "Refresh Now", true, None::<&str>)?;
            let update_item =
                MenuItem::with_id(app, "update", "Check for Updates", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let menu =
                Menu::with_items(app, &[&show_item, &refresh_item, &update_item, &quit_item])?;

            // Use the app's bundled icon for the tray, or a fallback pixel if unavailable.
            let tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Claude Usage Overlay")
                .on_menu_event(|app: &AppHandle, event| match event.id.as_ref() {
                    "show" => toggle_window(app),
                    "refresh" => {
                        let state: tauri::State<SharedPollerState> = app.state();
                        {
                            let mut s = state.lock().unwrap();
                            s.refresh_requested = true;
                        } // MutexGuard dropped here before any await
                        let notify: tauri::State<RefreshNotify> = app.state();
                        notify.notify_one();
                    }
                    "update" => {
                        let _ = app.emit("updater://check-requested", ());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                });

            // Attach icon if one is available.
            let tray_builder = if let Some(icon) = app.default_window_icon() {
                tray_builder.icon(icon.clone())
            } else {
                tray_builder
            };

            let _tray = tray_builder.build(app)?;

            // ── Load settings + telemetry setup ──────────────────────────────
            let mut saved = crate::settings::load(app.handle());

            // Generate a random install ID on first run; persist immediately.
            let is_first_run = crate::settings::ensure_install_id(&mut saved);
            if is_first_run {
                if let Err(e) = crate::settings::save(app.handle(), &saved) {
                    log::warn!("Failed to persist new install_id: {e}");
                }
            }

            // Build the shared telemetry handle.
            let telemetry = Telemetry::new(
                crate::config::telemetry_endpoint().as_deref(),
                crate::config::telemetry_api_key().as_deref(),
                saved.telemetry_enabled,
            );

            // Emit install event on first run.
            if is_first_run {
                let install_id = saved.install_id.as_deref().unwrap_or("");
                let app_version = env!("CARGO_PKG_VERSION");
                telemetry.record_install(
                    install_id,
                    app_version,
                    crate::telemetry::normalized_os(),
                    crate::telemetry::normalized_arch(),
                );
            }

            // Start the heartbeat loop (runs for the lifetime of the app).
            telemetry.clone().spawn_heartbeat_loop(
                saved.install_id.clone().unwrap_or_default(),
                env!("CARGO_PKG_VERSION").to_string(),
                crate::telemetry::normalized_os().to_string(),
                crate::telemetry::normalized_arch().to_string(),
            );

            // Register the Telemetry handle so window_ctl commands can access it.
            app.manage(telemetry.clone());

            // ── Apply persisted settings to the window ────────────────────────
            {
                // Restore CSS opacity.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval(&format!(
                        "document.getElementById('app').style.opacity = '{}'",
                        saved.opacity
                    ));

                    // Restore saved window size preset.  Unknown values fall back
                    // to the default dimensions via `preset_size`.
                    let (w, h) = crate::window_ctl::preset_size(&saved.size_preset);
                    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
                        width: w,
                        height: h,
                    }));
                }

                // Hydrate plan_override into the poller state BEFORE the poller
                // is spawned so the very first poll uses the persisted override.
                if let Some(plan) = crate::window_ctl::parse_plan_override(
                    saved.plan_override.as_deref(),
                ) {
                    let mut s = poller_state.lock().unwrap();
                    s.plan_override = Some(plan);
                }
            }

            // ── Spawn the polling loop ────────────────────────────────────────
            let (tx, mut rx) = mpsc::unbounded_channel::<crate::model::UsageSnapshot>();
            let state_for_poller = poller_state.clone();
            let notify_for_poller = refresh_notify.clone();
            let telemetry_for_poller = telemetry.clone();

            tauri::async_runtime::spawn(async move {
                crate::poller::run(tx, state_for_poller, notify_for_poller, telemetry_for_poller).await;
            });

            // ── Forward snapshots to the WebView as Tauri events ─────────────
            let app_handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(snapshot) = rx.recv().await {
                    let _ = app_handle.emit("usage://snapshot", &snapshot);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            window_ctl::set_opacity,
            window_ctl::set_size_preset,
            window_ctl::set_always_on_top,
            window_ctl::toggle_visibility,
            window_ctl::set_plan_override,
            window_ctl::request_refresh,
            window_ctl::quit_app,
            window_ctl::get_settings,
            window_ctl::get_window_size,
            window_ctl::set_window_size,
            window_ctl::get_sessions,
            window_ctl::set_history_threshold,
            window_ctl::set_telemetry_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}
