//! The commands the webviews invoke and the payloads of the events they
//! receive. Which window may call which command is declared in `build.rs`
//! and `capabilities/`.

use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, WebviewWindow};

use crate::{
    colors::AppColors,
    config::{lock_config, persist, save_config_values},
    elevation::{is_elevated, relaunch_elevated},
    hotkey::{
        app_shortcut, store_shortcuts, window_shortcut, DEFAULT_APP_SHORTCUT,
        DEFAULT_WINDOW_SHORTCUT,
    },
    settings_model::{
        apply_card_style, apply_hotkey, apply_layout, apply_monitor_mode,
        apply_settings_in_switcher, current_card_style, current_layout, current_monitor_mode,
        parse_toggle, RUN_AS_ADMIN, SETTINGS_IN_SWITCHER,
    },
    settings_window::{settings_hwnd, settings_is_foreground, RECORDING_HOTKEY},
    startup,
    switcher::{
        controller::{post_command, Command},
        session::payload_snapshot,
        GridCell, RUNTIME,
    },
    thumbnails, timing,
    tray::refresh_tray_tooltip,
    update,
};

#[derive(Clone, Serialize)]
pub(crate) struct SwitchItem {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) icon: Option<String>,
    pub(crate) accent: String,
    /// Theme colors from the app's icon; `None` for a neutral or missing icon.
    pub(crate) colors: Option<AppColors>,
    pub(crate) window_count: usize,
    /// Preview style only: the overlay shows a live thumbnail of window `id`
    /// in this card. False for minimized windows, which DWM cannot render.
    pub(crate) thumbnail: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct SwitcherPayload {
    pub(crate) mode: String,
    pub(crate) selected: usize,
    pub(crate) items: Vec<SwitchItem>,
    /// Label for the active mode, e.g. "Alt + `" or "Win + Tab".
    pub(crate) hint: String,
    /// "icon" or "preview".
    pub(crate) card_style: String,
    /// "horizontal" or "vertical".
    pub(crate) layout: String,
    /// Echoed back by `update_thumbnails` so late reports from an overlay
    /// that already closed are ignored.
    pub(crate) generation: u64,
    /// Latency logging is on (see `timing.rs`): the overlay reports when it
    /// rendered and painted each opening.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) timing: bool,
    /// Latency logging only: Unix time in ms when the event was sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sent_at: Option<f64>,
}

/// A key combination captured for the Settings window, which validates it.
#[derive(Clone, Serialize)]
pub(crate) struct RecordedHotkey {
    pub(crate) alt: bool,
    pub(crate) win: bool,
    pub(crate) ctrl: bool,
    pub(crate) shift: bool,
    pub(crate) escape: bool,
    /// The `config.json` key name ("Tab" or "Backtick"), or `None` when the
    /// hotkey format cannot express the pressed key.
    pub(crate) key: Option<String>,
    /// Readable name of the pressed key for error messages, e.g. "Q".
    pub(crate) key_name: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct SelectionPayload {
    pub(crate) generation: u64,
    pub(crate) selected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sent_at: Option<f64>,
}

/// Returns the payload currently on screen, if the overlay is open. The
/// frontend calls this right after registering its listener so a fast hotkey
/// press cannot be missed.
#[tauri::command]
pub(crate) fn switcher_snapshot() -> Option<SwitcherPayload> {
    RUNTIME.get().and_then(|runtime| {
        runtime
            .state
            .lock()
            .ok()
            .and_then(|state| payload_snapshot(&state))
    })
}

/// The overlay laid out opening `generation` with its cards on `cells`
/// (one per card, in card order): the arrows that move across lines (Up/Down,
/// or Left/Right in the vertical layout) go to the closest card on the next
/// line. Only the frontend knows, since preview cards shrink to fit and rows
/// are centered. Posted to the controller thread, which owns the session, so
/// the main thread never waits on the session lock.
#[tauri::command]
pub(crate) fn switcher_grid(generation: u64, cells: Vec<GridCell>) {
    post_command(Command::Grid { generation, cells });
}

/// The mouse clicked a card: commit that target immediately, even while the
/// switcher's modifier is still held. Posted to the controller thread like a
/// modifier release, so activating the window never runs on the main thread
/// and a click and the release that follows are handled one after the
/// other: whichever comes first commits, the other finds the session closed.
#[tauri::command]
pub(crate) fn switcher_click(generation: u64, index: u32) {
    post_command(Command::Click {
        generation,
        index: index as usize,
        at: std::time::Instant::now(),
    });
}

/// Every setting the Settings window shows, keyed like `config.json`.
#[derive(Clone, Serialize)]
pub(crate) struct SettingsPayload {
    show_switcher_on: String,
    card_style: String,
    layout: String,
    app_hotkey: String,
    window_hotkey: String,
    settings_in_switcher: bool,
    /// Read from the registry and Task Scheduler each time, not
    /// `config.json` (see `startup.rs`).
    start_with_windows: bool,
    run_as_admin: bool,
    check_for_updates: bool,
    /// Whether this process is elevated (read-only).
    running_as_admin: bool,
}

#[tauri::command]
pub(crate) fn get_settings() -> SettingsPayload {
    SettingsPayload {
        show_switcher_on: current_monitor_mode().as_str().to_string(),
        card_style: current_card_style().as_str().to_string(),
        layout: current_layout().as_str().to_string(),
        app_hotkey: app_shortcut().config_value(),
        window_hotkey: window_shortcut().config_value(),
        settings_in_switcher: SETTINGS_IN_SWITCHER.load(Ordering::SeqCst),
        start_with_windows: startup::is_enabled(),
        run_as_admin: RUN_AS_ADMIN.load(Ordering::SeqCst),
        check_for_updates: update::AUTO_CHECK.load(Ordering::SeqCst),
        running_as_admin: is_elevated(),
    }
}

/// Applies one setting immediately and saves it to `config.json`, then
/// returns every setting. Each key validates its own value; a new setting is
/// one more match arm. Async so it runs off the main thread: it may create or
/// destroy webviews.
///
/// Every setting is saved first and only then made live, both under the
/// config lock: a failed save changes nothing, and two quick changes can
/// never leave memory and the file holding different values.
#[tauri::command]
pub(crate) async fn set_setting(
    app: AppHandle,
    key: String,
    value: String,
) -> Result<SettingsPayload, String> {
    match key.as_str() {
        "show_switcher_on" => apply_monitor_mode(&app, &value)?,
        "card_style" => apply_card_style(&value)?,
        "layout" => apply_layout(&value)?,
        "app_hotkey" | "window_hotkey" => apply_hotkey(&app, &key, &value)?,
        "settings_in_switcher" => apply_settings_in_switcher(&value)?,
        "start_with_windows" => apply_start_with_windows(&value)?,
        "run_as_admin" => apply_run_as_admin(&app, &value)?,
        "check_for_updates" => apply_check_for_updates(&app, &value)?,
        _ => return Err(format!("unknown setting {key:?}")),
    }
    Ok(get_settings())
}

/// Adds or removes the sign-in startup entry. It lives in the registry or
/// Task Scheduler rather than `config.json`; the config lock still keeps two
/// quick toggles in order, and keeps "Run as administrator" from changing
/// meanwhile.
fn apply_start_with_windows(value: &str) -> Result<(), String> {
    let enabled = parse_toggle("start_with_windows", value)?;
    let _lock = lock_config();
    startup::set_enabled(enabled, RUN_AS_ADMIN.load(Ordering::SeqCst))?;
    log::info!("start with Windows: {enabled}");
    Ok(())
}

/// Saves "Run as administrator". Turned on without administrator rights, it
/// restarts the app elevated through the UAC prompt and this instance exits
/// (the reply never arrives; the new instance reopens Settings). If the
/// prompt is declined, the setting is turned back off and the error says so:
/// leaving it on would show a UAC prompt at every launch the user just
/// refused, while the checkbox claimed something that is not in effect.
///
/// Turned off while elevated, the app stays elevated until it restarts.
/// Either way, an existing startup entry moves to the matching mechanism
/// when this process is allowed to (see `startup::reconcile`).
fn apply_run_as_admin(app: &AppHandle, value: &str) -> Result<(), String> {
    let enabled = parse_toggle("run_as_admin", value)?;
    let lock = lock_config();
    save_config_values(&lock, &[("run_as_admin", enabled.into())])?;
    RUN_AS_ADMIN.store(enabled, Ordering::SeqCst);
    log::info!("run as administrator: {enabled}");

    if enabled && !is_elevated() {
        match relaunch_elevated(settings_hwnd(), true) {
            Ok(()) => {
                app.exit(0);
                return Ok(());
            }
            Err(error) => {
                if let Err(revert) = save_config_values(&lock, &[("run_as_admin", false.into())]) {
                    log::error!("unable to turn run_as_admin back off: {revert}");
                }
                RUN_AS_ADMIN.store(false, Ordering::SeqCst);
                return Err(error.message());
            }
        }
    }
    if let Err(error) = startup::reconcile(enabled) {
        // Retried at every launch; the next one with the rights fixes it.
        log::warn!("unable to update the startup entry: {error}");
    }
    Ok(())
}

/// Turns the automatic update checks on or off. Turned on, it checks now.
fn apply_check_for_updates(app: &AppHandle, value: &str) -> Result<(), String> {
    let enabled = parse_toggle("check_for_updates", value)?;
    persist(&[("check_for_updates", enabled.into())], || {
        update::AUTO_CHECK.store(enabled, Ordering::SeqCst)
    })?;
    log::info!("check for updates: {enabled}");
    let _ = app.emit(update::STATUS_EVENT, update::payload());
    if enabled {
        let app = app.clone();
        std::thread::spawn(move || update::check(&app, true));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn get_update_status() -> update::UpdatePayload {
    update::payload()
}

/// Checks GitHub now and returns the result. Async: it blocks on the network.
#[tauri::command]
pub(crate) async fn check_for_updates(app: AppHandle) -> update::UpdatePayload {
    update::check(&app, true);
    update::payload()
}

/// Downloads and installs the available update, then restarts (Settings
/// reopens). Progress and failures arrive as `update:status` events.
#[tauri::command]
pub(crate) fn install_update(app: AppHandle) {
    update::install_in_background(&app, true);
}

#[tauri::command]
pub(crate) fn open_release_page() {
    update::open_release_page();
}

/// Restores both hotkeys to their defaults in one write.
#[tauri::command]
pub(crate) fn reset_hotkeys(app: AppHandle) -> Result<SettingsPayload, String> {
    persist(
        &[
            (
                "window_hotkey",
                DEFAULT_WINDOW_SHORTCUT.config_value().into(),
            ),
            ("app_hotkey", DEFAULT_APP_SHORTCUT.config_value().into()),
        ],
        || store_shortcuts(DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT),
    )?;
    log::info!("hotkeys reset to defaults");
    refresh_tray_tooltip(&app);
    Ok(get_settings())
}

/// Starts or stops capturing key combinations for the Settings window. While
/// active and the window is in the foreground, the hook swallows every
/// non-modifier key press and sends it, with the modifiers held, as a
/// `settings:hotkey-recorded` event; the hotkeys themselves do not fire.
#[tauri::command]
pub(crate) fn set_hotkey_recording(active: bool) {
    log::debug!(
        "set_hotkey_recording({active}); settings foreground: {}",
        settings_is_foreground()
    );
    RECORDING_HOTKEY.store(active, Ordering::SeqCst);
}

/// A preview slot in the overlay, in CSS pixels relative to the viewport.
#[derive(Deserialize)]
pub(crate) struct ThumbnailSlot {
    pub(crate) id: u64,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
    /// Where the card's app icon sits over the preview's corner, when the
    /// card has an icon (see `icon_layer.rs`).
    #[serde(default)]
    pub(crate) icon: Option<SlotRect>,
}

/// A rectangle in CSS pixels relative to the viewport.
#[derive(Clone, Copy, Deserialize)]
pub(crate) struct SlotRect {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// Latency logging only: the overlay reports when it rendered or painted an
/// opening (`webview_ms` is measured from the event's arrival in the webview),
/// or, for stage "delivered", how long an event took to reach it.
/// Places the live previews on the calling overlay (see `thumbnails.rs`).
#[tauri::command]
pub(crate) fn update_thumbnails(
    window: WebviewWindow,
    generation: u64,
    scale: f64,
    slots: Vec<ThumbnailSlot>,
) -> Result<(), String> {
    thumbnails::place_thumbnails(&window, generation, scale, slots)
}

#[tauri::command]
pub(crate) fn overlay_timing(
    window: WebviewWindow,
    generation: u64,
    stage: String,
    webview_ms: f64,
) {
    if stage.starts_with("delivered") && stage.len() <= 32 && timing::enabled() {
        log::info!(
            "[timing] {stage} #{generation} in {webview_ms:.2} ms to {}",
            window.label()
        );
        return;
    }
    if stage.len() <= 32 && webview_ms.is_finite() {
        timing::since_open(
            generation,
            &stage,
            &format!("({webview_ms:.2} ms after arrival, {})", window.label()),
        );
    }
}
