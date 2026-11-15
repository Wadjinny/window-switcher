//! The commands the webviews invoke and the payloads of the events they
//! receive. Which window may call which command is declared in `build.rs`
//! and `capabilities/`.

use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, WebviewWindow};

use crate::{
    config::persist,
    hotkey::{
        app_shortcut, store_shortcuts, window_shortcut, DEFAULT_APP_SHORTCUT,
        DEFAULT_WINDOW_SHORTCUT,
    },
    settings_model::{
        apply_card_style, apply_hotkey, apply_layout, apply_monitor_mode, current_card_style,
        current_layout, current_monitor_mode,
    },
    settings_window::{settings_is_foreground, RECORDING_HOTKEY},
    switcher::{session::payload_snapshot, RUNTIME},
    thumbnails, timing,
    tray::refresh_tray_tooltip,
};

#[derive(Clone, Serialize)]
pub(crate) struct SwitchItem {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) icon: Option<String>,
    pub(crate) accent: String,
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

/// Every setting the Settings window shows, keyed like `config.json`.
#[derive(Clone, Serialize)]
pub(crate) struct SettingsPayload {
    show_switcher_on: String,
    card_style: String,
    layout: String,
    app_hotkey: String,
    window_hotkey: String,
}

#[tauri::command]
pub(crate) fn get_settings() -> SettingsPayload {
    SettingsPayload {
        show_switcher_on: current_monitor_mode().as_str().to_string(),
        card_style: current_card_style().as_str().to_string(),
        layout: current_layout().as_str().to_string(),
        app_hotkey: app_shortcut().config_value(),
        window_hotkey: window_shortcut().config_value(),
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
        _ => return Err(format!("unknown setting {key:?}")),
    }
    Ok(get_settings())
}

/// Restores both hotkeys to their defaults in one write.
#[tauri::command]
pub(crate) fn reset_hotkeys(app: AppHandle) -> Result<SettingsPayload, String> {
    persist(
        &[
            ("window_hotkey", DEFAULT_WINDOW_SHORTCUT.config_value()),
            ("app_hotkey", DEFAULT_APP_SHORTCUT.config_value()),
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
