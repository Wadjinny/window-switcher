#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod hotkey;
mod icons;
mod input;
mod ipc;
mod overlay;
mod settings_model;
mod settings_window;
mod switcher;
mod targets;
mod thumbnails;
mod timing;
mod tray;

use std::{
    env,
    sync::{atomic::Ordering, Arc, Mutex, MutexGuard},
};

use tauri::Manager;
use windows::{
    core::PCWSTR,
    Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
};

use crate::{
    config::load_config,
    hotkey::{startup_shortcuts, store_shortcuts},
    icons::warm_item_caches,
    input::hook::{install_keyboard_hook, remove_keyboard_hook},
    overlay::{all_monitor_areas, prepare_overlay_window, spawn_overlay_windows, OVERLAY_LABEL},
    settings_model::{
        CardStyle, Layout, MonitorMode, MONITOR_MODE, PREVIEW_CARDS, VERTICAL_LAYOUT,
    },
    switcher::{controller::start_controller, HookRuntime, SwitcherState, RUNTIME},
    tray::install_tray,
};

fn main() {
    // `WINDOW_SWITCHER_DEBUG=1` adds debug lines (e.g. hotkey recording).
    let debug = env::var_os("WINDOW_SWITCHER_DEBUG").is_some_and(|value| value != "0");
    let log_plugin = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .level_for(
            "window_switcher",
            if debug {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::LogDir {
                file_name: Some("window-switcher".to_string()),
            },
        ))
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Stdout,
        ))
        .build();

    let built = tauri::Builder::default()
        // The single-instance guard must come first: a second launch has to
        // exit before setup installs a second hook and tray icon.
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            log::warn!("another instance asked to start; keeping the running one");
        }))
        .plugin(log_plugin)
        .invoke_handler(tauri::generate_handler![
            ipc::switcher_snapshot,
            ipc::switcher_grid,
            ipc::switcher_click,
            ipc::get_settings,
            ipc::set_setting,
            ipc::reset_hotkeys,
            ipc::set_hotkey_recording,
            ipc::update_thumbnails,
            ipc::overlay_timing
        ])
        .setup(|app| setup_app(app))
        .build(tauri::generate_context!());

    let app = match built {
        Ok(app) => app,
        Err(error) => fatal(&format!("Window Switcher could not start: {error}")),
    };

    app.run(|_app, event| {
        if let tauri::RunEvent::Exit = event {
            // Leave nothing behind: remove the global hook before the process goes away.
            remove_keyboard_hook();
        }
    });
}

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Window Switcher {} starting", env!("CARGO_PKG_VERSION"));

    let config = load_config();
    let monitor_mode = MonitorMode::parse(&config.show_switcher_on).unwrap_or_else(|| {
        log::warn!(
            "unknown show_switcher_on value {:?}; using primary",
            config.show_switcher_on
        );
        MonitorMode::Primary
    });
    MONITOR_MODE.store(monitor_mode.to_u8(), Ordering::SeqCst);
    log::info!("switcher shows on: {}", monitor_mode.as_str());
    let card_style = CardStyle::parse(&config.card_style).unwrap_or_else(|| {
        log::warn!(
            "unknown card_style value {:?}; using icon",
            config.card_style
        );
        CardStyle::Icon
    });
    PREVIEW_CARDS.store(card_style == CardStyle::Preview, Ordering::SeqCst);
    log::info!("card style: {}", card_style.as_str());
    let layout = Layout::parse(&config.layout).unwrap_or_else(|| {
        log::warn!("unknown layout value {:?}; using horizontal", config.layout);
        Layout::Horizontal
    });
    VERTICAL_LAYOUT.store(layout == Layout::Vertical, Ordering::SeqCst);
    log::info!("layout: {}", layout.as_str());
    let (window_shortcut, app_shortcut) =
        startup_shortcuts(&config.window_hotkey, &config.app_hotkey);
    log::info!(
        "window shortcut: {} | app shortcut: {}",
        window_shortcut.hint(),
        app_shortcut.hint()
    );
    store_shortcuts(window_shortcut, app_shortcut);

    let runtime = HookRuntime {
        app: app.handle().clone(),
        state: Arc::new(Mutex::new(SwitcherState::default())),
    };
    let _ = RUNTIME.set(runtime);
    // Read the timing flag now rather than on the first key press.
    timing::enabled();
    start_controller();
    install_keyboard_hook();

    if let Err(error) = install_tray(app) {
        log::error!("unable to install the tray icon: {error}");
        return Err(error.into());
    }
    log::info!("tray icon installed");

    if let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) {
        prepare_overlay_window(&overlay);
    }
    // Fill the icon and name caches now, so even the first opening draws
    // every card complete.
    warm_item_caches();
    if monitor_mode == MonitorMode::Every {
        // Create the other monitors' overlays now so the first hotkey press
        // does not wait for their webviews.
        spawn_overlay_windows(app.handle().clone(), all_monitor_areas().len());
    }
    Ok(())
}

/// Surfaces a fatal startup problem to the user, records it, and stops the
/// process. A message box is the only channel available before the webview
/// exists.
fn fatal(message: &str) -> ! {
    log::error!("{message}");
    show_error_message(message);
    std::process::exit(1);
}

pub(crate) fn show_error_message(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Window Switcher".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Locks a mutex that only orders work (it guards no data of its own), so a
/// panic while it was held leaves nothing half-updated to worry about.
pub(crate) fn lock_unpoisoned<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
