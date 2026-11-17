//! The Settings window and the hotkey recording it drives.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread;

use tauri::{
    window::Color, AppHandle, Emitter, Manager, Theme, WebviewUrl, WebviewWindowBuilder,
    WindowEvent,
};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::{GetKeyNameTextW, VK_ESCAPE},
        WindowsAndMessaging::GetForegroundWindow,
    },
};

use crate::{
    hotkey::config_key_name,
    ipc::RecordedHotkey,
    overlay::{cursor_monitor_area, primary_monitor_area},
    settings_model::SETTINGS_IN_SWITCHER,
    switcher::{controller::RecordedKey, HookRuntime},
    targets::activate::activate_window,
};

const SETTINGS_LABEL: &str = "settings";

/// Set while the worker thread builds the Settings window, so a second quick
/// click on "Settings…" does not start building another one.
static SETTINGS_OPENING: AtomicBool = AtomicBool::new(false);

/// Settings window size in logical pixels. The height is capped to the work
/// area of the monitor it opens on; the content scrolls when it is shorter.
const SETTINGS_WIDTH: f64 = 460.0;
const SETTINGS_HEIGHT: f64 = 660.0;
const SETTINGS_MIN_WIDTH: f64 = 400.0;
const SETTINGS_MIN_HEIGHT: f64 = 320.0;
/// Room kept for the title bar and some breathing space above and below.
const SETTINGS_VERTICAL_MARGIN: f64 = 72.0;

/// Shows the settings window, creating it on first use. Building a webview
/// from the menu event handler (main thread) can deadlock on Windows, so a
/// new window is built from a worker thread.
pub(crate) fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    if SETTINGS_OPENING.swap(true, Ordering::SeqCst) {
        // Already being built by an earlier click.
        return;
    }
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-settings".into())
        .spawn(move || {
            // `center()` uses the monitor the window is created on; size it
            // for the same one so it never extends past the work area.
            let height = cursor_monitor_area()
                .or_else(primary_monitor_area)
                .map(|area| area.work.height as f64 / area.scale - SETTINGS_VERTICAL_MARGIN)
                .map_or(SETTINGS_HEIGHT, |available| {
                    SETTINGS_HEIGHT.min(available).max(SETTINGS_MIN_HEIGHT)
                });
            let built = WebviewWindowBuilder::new(
                &app,
                SETTINGS_LABEL,
                WebviewUrl::App("settings.html".into()),
            )
            .title("Window Switcher Settings")
            .inner_size(SETTINGS_WIDTH, height)
            .min_inner_size(SETTINGS_MIN_WIDTH, SETTINGS_MIN_HEIGHT)
            .resizable(true)
            .maximizable(false)
            .minimizable(false)
            .center()
            .theme(Some(Theme::Dark))
            .background_color(Color(36, 36, 36, 255))
            .focused(true)
            .build();
            match built {
                // Windows' foreground lock leaves a window created from this
                // worker thread behind the active app; raise it explicitly.
                Ok(window) => {
                    window.on_window_event(|event| match event {
                        // Never leave the hook capturing keys for a window
                        // that is in the background or no longer exists.
                        WindowEvent::Focused(false) => {
                            RECORDING_HOTKEY.store(false, Ordering::SeqCst);
                        }
                        WindowEvent::Destroyed => {
                            RECORDING_HOTKEY.store(false, Ordering::SeqCst);
                            SETTINGS_HWND.store(0, Ordering::SeqCst);
                        }
                        _ => {}
                    });
                    if let Ok(hwnd) = window.hwnd() {
                        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
                        activate_window(HWND(hwnd.0));
                    }
                }
                Err(error) => log::error!("unable to open the settings window: {error}"),
            }
            SETTINGS_OPENING.store(false, Ordering::SeqCst);
        });
    if let Err(error) = spawned {
        SETTINGS_OPENING.store(false, Ordering::SeqCst);
        log::error!("unable to start the settings thread: {error}");
    }
}

/// Set while a hotkey field in the Settings window is recording.
pub(crate) static RECORDING_HOTKEY: AtomicBool = AtomicBool::new(false);
/// HWND of the Settings window (0 when closed). Recording only captures keys
/// while that window is in the foreground.
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

/// Whether `hwnd` is the Settings window and `settings_in_switcher` is on, so
/// the switcher treats it like any other app window.
pub(crate) fn settings_is_switchable(hwnd: HWND) -> bool {
    let settings = SETTINGS_HWND.load(Ordering::SeqCst);
    SETTINGS_IN_SWITCHER.load(Ordering::SeqCst) && settings != 0 && hwnd.0 as isize == settings
}

/// The Settings window, while it exists.
pub(crate) fn settings_hwnd() -> Option<HWND> {
    let settings = SETTINGS_HWND.load(Ordering::SeqCst);
    (settings != 0).then_some(HWND(settings as *mut _))
}

pub(crate) fn settings_is_foreground() -> bool {
    let settings = SETTINGS_HWND.load(Ordering::SeqCst);
    settings != 0 && unsafe { GetForegroundWindow() }.0 as isize == settings
}

pub(crate) fn emit_recorded_hotkey(runtime: &HookRuntime, key: &RecordedKey) {
    log::debug!(
        "recording: vk={:#x} scan={:#x} alt={} win={}",
        key.vk,
        key.scan_code,
        key.alt,
        key.win
    );
    // Same physical-key rule as `Shortcut::matches`.
    let name = config_key_name(key.vk, key.scan_code);
    let recorded = RecordedHotkey {
        alt: key.alt,
        win: key.win,
        ctrl: key.ctrl,
        shift: key.shift,
        escape: key.vk == VK_ESCAPE.0 as u32,
        key: name.map(str::to_string),
        key_name: key_name(key),
    };
    if let Err(error) = runtime
        .app
        .emit_to(SETTINGS_LABEL, "settings:hotkey-recorded", recorded)
    {
        log::warn!("unable to send the recorded hotkey: {error}");
    }
}

/// The key's name in the current layout ("Q", "F5", "Page Down").
pub(crate) fn key_name(key: &RecordedKey) -> String {
    let mut lparam = ((key.scan_code & 0xFF) as i32) << 16;
    if key.extended {
        lparam |= 1 << 24;
    }
    let mut buffer = [0u16; 64];
    let length = unsafe { GetKeyNameTextW(lparam, &mut buffer) };
    if length > 0 {
        String::from_utf16_lossy(&buffer[..length as usize])
    } else {
        format!("key {:#04x}", key.vk)
    }
}
