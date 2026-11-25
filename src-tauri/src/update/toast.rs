//! The "new version available" Windows notification.
//!
//! A portable exe has no Start menu shortcut to name it in notifications, so
//! the app registers its AppUserModelID under
//! `HKCU\Software\Classes\AppUserModelId` (a display name and an icon), the
//! way unpackaged apps do. Clicking the notification is handled in-process
//! through its `Activated` event, while the app runs.

use std::{path::PathBuf, sync::Mutex};

use tauri::AppHandle;
use windows::{
    core::{Interface, HSTRING, PCWSTR},
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::{
            Com::{CoInitializeEx, COINIT_MULTITHREADED},
            Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ},
        },
    },
    UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager},
};

use crate::{config::app_data_path, settings_window::open_settings};

/// The app's identifier (tauri.conf.json), used as its AppUserModelID.
const APP_ID: &str = "com.ilyaswadjinny.window-switcher";
/// The notification's "Update now" button.
const UPDATE_ARGUMENT: &str = "update";

/// The notification on screen (or in the notification center), kept alive
/// so its `Activated` handler still runs when it is clicked later.
static SHOWN: Mutex<Option<ToastNotification>> = Mutex::new(None);

/// Shows "Window Switcher `version` is available" with an "Update now"
/// button; clicking the notification itself opens Settings.
pub(super) fn show_update_toast(app: &AppHandle, version: &str) -> Result<(), String> {
    register(app).map_err(|error| format!("unable to register for notifications: {error}"))?;
    show(app, version).map_err(|error| error.message())
}

fn show(app: &AppHandle, version: &str) -> windows::core::Result<()> {
    // WinRT needs COM on this thread; the updater thread is otherwise idle.
    // SAFETY: an already-initialized thread just reports it.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(toast_xml(
        version,
        env!("CARGO_PKG_VERSION"),
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    let app = app.clone();
    toast.Activated(&TypedEventHandler::new(
        move |_, args: windows::core::Ref<windows::core::IInspectable>| {
            let argument = args
                .ok()
                .and_then(|args| args.cast::<ToastActivatedEventArgs>())
                .and_then(|args| args.Arguments())
                .map(|argument| argument.to_string())
                .unwrap_or_default();
            if argument == UPDATE_ARGUMENT {
                super::install_in_background(&app, false);
            } else {
                open_settings(&app);
            }
            Ok(())
        },
    ))?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    if let Ok(mut shown) = SHOWN.lock() {
        *shown = Some(toast);
    }
    Ok(())
}

fn toast_xml(version: &str, current: &str) -> String {
    format!(
        r#"<toast launch="open"><visual><binding template="ToastGeneric"><text>Window Switcher {version} is available</text><text>You have {current}. Update now and Window Switcher restarts by itself.</text></binding></visual><actions><action content="Update now" arguments="{UPDATE_ARGUMENT}" activationType="foreground"/></actions></toast>"#,
        version = xml_escape(version),
        current = xml_escape(current),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Names the app and gives it an icon in notifications (idempotent).
fn register(app: &AppHandle) -> Result<(), String> {
    let icon = write_icon(app);
    let key = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
    set_value(&key, "DisplayName", "Window Switcher")?;
    if let Some(icon) = icon {
        set_value(&key, "IconUri", &icon.to_string_lossy())?;
    }
    Ok(())
}

/// The app icon as a PNG in the app data folder, for the notification.
fn write_icon(app: &AppHandle) -> Option<PathBuf> {
    let icon = app.default_window_icon()?;
    let directory = app_data_path();
    let path = directory.join("notification-icon.png");
    let saved = std::fs::create_dir_all(&directory).is_ok()
        && image::save_buffer_with_format(
            &path,
            icon.rgba(),
            icon.width(),
            icon.height(),
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .is_ok();
    saved.then_some(path)
}

fn set_value(key: &str, name: &str, value: &str) -> Result<(), String> {
    let key_w: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name_w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let data: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    // SAFETY: every buffer outlives the call; the size is in bytes.
    let result = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(result.to_hresult().message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toast_names_both_versions_and_offers_the_update() {
        let xml = toast_xml("0.4.0", "0.3.0");
        assert!(xml.contains("Window Switcher 0.4.0 is available"));
        assert!(xml.contains("You have 0.3.0."));
        assert!(xml.contains(r#"arguments="update""#));
        assert_eq!(xml_escape(r#"<a & "b">"#), "&lt;a &amp; &quot;b&quot;&gt;");
    }
}
