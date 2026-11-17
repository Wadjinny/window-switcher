//! The live settings: what each one can be, its current value (atomics the
//! hook and controller read without locks), and applying a change from the
//! Settings window.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tauri::{AppHandle, Manager};

use crate::{
    config::{lock_config, persist, save_config_values},
    hotkey::{app_shortcut, assign_shortcut, store_shortcuts, window_shortcut, Shortcut},
    overlay::{all_monitor_areas, extra_overlay_index, spawn_overlay_windows},
    tray::refresh_tray_tooltip,
};

/// Where the overlay appears. Stored in `config.json` as `show_switcher_on`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MonitorMode {
    Primary,
    Every,
    Cursor,
}

impl MonitorMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" => Some(Self::Primary),
            "every" | "all" => Some(Self::Every),
            "cursor" | "mouse" => Some(Self::Cursor),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Every => "every",
            Self::Cursor => "cursor",
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Every,
            2 => Self::Cursor,
            _ => Self::Primary,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Primary => 0,
            Self::Every => 1,
            Self::Cursor => 2,
        }
    }
}

/// How the cards are arranged. Stored in `config.json` as `layout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// Cards side by side in rows (the original layout).
    Horizontal,
    /// Cards stacked as a list, wrapping into extra columns when needed.
    Vertical,
}

impl Layout {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "horizontal" | "row" => Some(Self::Horizontal),
            "vertical" | "column" | "list" => Some(Self::Vertical),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// What a switcher card shows. Stored in `config.json` as `card_style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardStyle {
    /// The app icon (the original compact cards).
    Icon,
    /// A live DWM thumbnail of the window, like the built-in Alt+Tab.
    Preview,
}

impl CardStyle {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "icon" => Some(Self::Icon),
            "preview" | "thumbnail" => Some(Self::Preview),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Icon => "icon",
            Self::Preview => "preview",
        }
    }
}

/// Read on the keyboard hook thread, so it is an atomic rather than a lock.
pub(crate) static MONITOR_MODE: AtomicU8 = AtomicU8::new(0);
/// `true` while cards show window previews instead of app icons.
pub(crate) static PREVIEW_CARDS: AtomicBool = AtomicBool::new(false);
/// `true` while cards are stacked vertically instead of side by side.
pub(crate) static VERTICAL_LAYOUT: AtomicBool = AtomicBool::new(false);
/// `true` while the Settings window is listed and switched from like any
/// other window. Stored in `config.json` as `settings_in_switcher`.
pub(crate) static SETTINGS_IN_SWITCHER: AtomicBool = AtomicBool::new(false);
/// `true` while "Run as administrator" is on. Stored in `config.json` as
/// `run_as_admin`; whether this process actually is elevated is
/// `elevation::is_elevated`.
pub(crate) static RUN_AS_ADMIN: AtomicBool = AtomicBool::new(false);

pub(crate) fn current_monitor_mode() -> MonitorMode {
    MonitorMode::from_u8(MONITOR_MODE.load(Ordering::SeqCst))
}

pub(crate) fn current_layout() -> Layout {
    if VERTICAL_LAYOUT.load(Ordering::SeqCst) {
        Layout::Vertical
    } else {
        Layout::Horizontal
    }
}

pub(crate) fn current_card_style() -> CardStyle {
    if PREVIEW_CARDS.load(Ordering::SeqCst) {
        CardStyle::Preview
    } else {
        CardStyle::Icon
    }
}

pub(crate) fn apply_monitor_mode(app: &AppHandle, value: &str) -> Result<(), String> {
    let mode =
        MonitorMode::parse(value).ok_or_else(|| format!("unknown monitor mode {value:?}"))?;
    persist(&[("show_switcher_on", mode.as_str().into())], || {
        MONITOR_MODE.store(mode.to_u8(), Ordering::SeqCst)
    })?;
    log::info!("switcher shows on: {}", mode.as_str());

    if mode == MonitorMode::Every {
        spawn_overlay_windows(app.clone(), all_monitor_areas().len());
    } else {
        // The extra overlays are only used in every-monitor mode; free them.
        for (label, window) in app.webview_windows() {
            if extra_overlay_index(&label).is_some() {
                let _ = window.destroy();
            }
        }
    }
    Ok(())
}

/// Swaps one hotkey live: the keyboard hook reads it on the next key press,
/// and the overlay hint follows because it is derived when the overlay opens.
/// Choosing the other switcher's shortcut swaps the two, saved in one write.
pub(crate) fn apply_hotkey(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let shortcut = Shortcut::parse(value).ok_or_else(|| {
        format!("unsupported shortcut {value:?}: use Alt or Win with Tab or Backtick")
    })?;
    let (window, apps) = {
        let lock = lock_config();
        let (window, apps) = assign_shortcut(
            (window_shortcut(), app_shortcut()),
            key == "window_hotkey",
            shortcut,
        );
        save_config_values(
            &lock,
            &[
                ("window_hotkey", window.config_value().into()),
                ("app_hotkey", apps.config_value().into()),
            ],
        )?;
        store_shortcuts(window, apps);
        (window, apps)
    };
    log::info!(
        "window shortcut: {} | app shortcut: {}",
        window.hint(),
        apps.hint()
    );
    refresh_tray_tooltip(app);
    Ok(())
}

/// Takes effect the next time the switcher opens.
pub(crate) fn apply_card_style(value: &str) -> Result<(), String> {
    let style = CardStyle::parse(value).ok_or_else(|| format!("unknown card style {value:?}"))?;
    persist(&[("card_style", style.as_str().into())], || {
        PREVIEW_CARDS.store(style == CardStyle::Preview, Ordering::SeqCst)
    })?;
    log::info!("card style: {}", style.as_str());
    Ok(())
}

/// Takes effect the next time the switcher opens.
pub(crate) fn apply_layout(value: &str) -> Result<(), String> {
    let layout = Layout::parse(value).ok_or_else(|| format!("unknown layout {value:?}"))?;
    persist(&[("layout", layout.as_str().into())], || {
        VERTICAL_LAYOUT.store(layout == Layout::Vertical, Ordering::SeqCst)
    })?;
    log::info!("layout: {}", layout.as_str());
    Ok(())
}

/// Takes effect on the next key press: the hook and the window list read it
/// each time.
pub(crate) fn apply_settings_in_switcher(value: &str) -> Result<(), String> {
    let enabled = parse_toggle("settings_in_switcher", value)?;
    persist(&[("settings_in_switcher", enabled.into())], || {
        SETTINGS_IN_SWITCHER.store(enabled, Ordering::SeqCst)
    })?;
    log::info!("settings in switcher: {enabled}");
    Ok(())
}

/// On/off settings arrive from the Settings window as "true" or "false".
pub(crate) fn parse_toggle(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} must be true or false, not {value:?}")),
    }
}
