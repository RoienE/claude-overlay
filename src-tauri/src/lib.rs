//! Library crate root. Wires up Tauri app setup for both the binary and tests.

pub mod config;
pub mod credential_source;
pub mod desktop_credentials;
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

/// Reveal the log directory in the OS file manager.
///
/// Hands the user the log file itself rather than copying a one-line summary — the
/// file has the whole attempt trail. Uses the platform opener directly: the shell
/// plugin's `open` is deprecated, and this needs no extra dependency.
fn open_log_dir(app: &AppHandle) {
    let dir = match app.path().app_log_dir() {
        Ok(dir) => dir,
        Err(e) => {
            log::warn!("Could not resolve the log directory: {e}");
            return;
        }
    };

    // The directory only exists once the logger has written to it.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Could not create the log directory: {e}");
        return;
    }

    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let opener = "xdg-open";

    // explorer.exe reports a nonzero exit even on success, so the status is ignored
    // everywhere; a spawn failure is the only thing worth logging.
    if let Err(e) = std::process::Command::new(opener).arg(&dir).spawn() {
        log::warn!("Could not open the log directory: {e}");
    }
}

/// Log level for the file/stdout logger: `RUST_LOG` when it names a level, else Info.
///
/// Deliberately not the full RUST_LOG filter grammar — the plugin takes a single
/// level, and the only thing anyone needs here is `RUST_LOG=debug`.
fn log_level_from_env() -> log::LevelFilter {
    std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| raw.trim().parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info)
}

/// Notice shown before macOS asks to let the overlay read `Claude Safe Storage`.
///
/// Returns the consent gate the credential reader calls right before that Keychain
/// read — which only happens when a Claude Desktop login actually exists and its key
/// is not already cached, so a CLI-only user never sees it.
///
/// * **Continue** persists `desktop_keychain_notice_ack` (so later launches go straight
///   to the system prompt) and proceeds.
/// * **Not now** skips Claude Desktop for the rest of this run (in memory only), so the
///   user is not nagged every poll; restarting the overlay asks again.
///
/// Runs on the poller's blocking thread, never the main thread, as the dialog requires.
#[cfg(target_os = "macos")]
fn desktop_keychain_gate(app: AppHandle) -> credential_source::ConsentGate {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let declined = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if declined.load(Ordering::Relaxed) {
            return false;
        }
        let mut saved = settings::load(&app);
        if saved.desktop_keychain_notice_ack {
            return true;
        }

        let accepted = app
            .dialog()
            .message(
                "macOS will next ask whether to let Claude Overlay access \"Claude Safe \
                 Storage\".\n\nThat is how the overlay reads your Claude Desktop sign-in to \
                 show your usage. Access is read-only: the overlay never changes your login.\n\n\
                 Choose \"Always Allow\" in the macOS prompt to avoid being asked again.",
            )
            .title("Read Claude Desktop login")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Continue".to_owned(),
                "Not now".to_owned(),
            ))
            .blocking_show();

        if accepted {
            saved.desktop_keychain_notice_ack = true;
            if let Err(e) = settings::save(&app, &saved) {
                log::warn!("Failed to persist the Keychain notice acknowledgement: {e}");
            }
        } else {
            log::info!("Claude Desktop Keychain access postponed by the user");
            declined.store(true, Ordering::Relaxed);
        }
        accepted
    })
}

pub fn run() {
    // Must be called before any window or COM object is created so Windows can
    // associate the process with the Start Menu shortcut's AppUserModelID.
    #[cfg(windows)]
    set_aumid();


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
        // Registered directly after single-instance, so everything from here on —
        // every later plugin and the whole setup hook — is captured.
        //
        // A GUI .app has nowhere visible for stderr, which is why a credential
        // failure used to be invisible: the reason was written to a stream nobody
        // could read. The LogDir target puts it in a file the user can open
        // (macOS: ~/Library/Logs/<bundle id>/) via the tray's "Open log folder".
        // Default level is Info so warnings land without anyone knowing RUST_LOG;
        // RUST_LOG still overrides. No log statement in this app prints a token or
        // an API key.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log_level_from_env())
                .max_file_size(2 * 1024 * 1024)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("claude-overlay".into()),
                    }),
                ])
                .build(),
        )
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

            let logs_item = MenuItem::with_id(app, "logs", "Open log folder", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &show_item,
                    &refresh_item,
                    &update_item,
                    &logs_item,
                    &quit_item,
                ],
            )?;

            // Use the app's bundled icon for the tray, or a fallback pixel if unavailable.
            let tray_builder = TrayIconBuilder::new()
                .menu(&menu)
                // Left click must ONLY toggle the window.  `tray-icon` defaults
                // `menu_on_left_click` to true, which makes a single left click both
                // pop the menu and emit TrayIconEvent::Click — so the window toggled
                // *and* the menu appeared.  Right click is governed by a separate
                // `menu_on_right_click` flag that stays true, so the context menu is
                // unaffected.  Unsupported on Linux (appindicator owns the left click).
                .show_menu_on_left_click(false)
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
                    "logs" => open_log_dir(app),
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

            // Build the shared telemetry handle. The identity lives in the handle so
            // every event — including errors raised deep in the poller — is
            // attributable to one install without threading it through call sites.
            let telemetry = Telemetry::new(
                crate::config::telemetry_endpoint().as_deref(),
                crate::config::telemetry_api_key().as_deref(),
                saved.telemetry_enabled,
                crate::telemetry::Identity::new(saved.install_id.clone().unwrap_or_default()),
            );

            // Report panics (sanitised location only) before deferring to the
            // default hook.
            crate::telemetry::install_panic_hook(telemetry.clone());

            // Emit install event on first run.
            if is_first_run {
                telemetry.record_install();
            }

            // Start the heartbeat loop (runs for the lifetime of the app).
            telemetry.clone().spawn_heartbeat_loop();

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

            // Only macOS needs the notice (its Keychain prompt); elsewhere the gate
            // is never consulted and always agrees.
            #[cfg(target_os = "macos")]
            let consent_gate = desktop_keychain_gate(app.handle().clone());
            #[cfg(not(target_os = "macos"))]
            let consent_gate: credential_source::ConsentGate = Arc::new(|| true);

            tauri::async_runtime::spawn(async move {
                crate::poller::run(
                    tx,
                    state_for_poller,
                    notify_for_poller,
                    telemetry_for_poller,
                    consent_gate,
                )
                .await;
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
