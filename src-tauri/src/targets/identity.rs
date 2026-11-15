//! Which app a window belongs to, and that app's name.

use std::{ffi::c_void, path::Path};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HWND,
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::{CoTaskMemFree, StructuredStorage::PropVariantToStringAlloc},
        UI::Shell::{
            IShellItem,
            PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY,
        },
    },
};

use super::Target;

/// Identity used to group windows into one switcher card. Packaged apps and
/// browser PWAs expose an AppUserModelID, which is what the taskbar groups by;
/// everything else groups by executable path.
pub(crate) fn target_key(target: &Target) -> String {
    match &target.app_id {
        Some(id) => format!("aumid:{id}"),
        None => format!("path:{}", path_key(&target.process_path)),
    }
}

pub(crate) fn window_key(hwnd: HWND, path: &Path) -> String {
    match app_user_model_id(hwnd) {
        Some(id) => format!("aumid:{id}"),
        None => format!("path:{}", path_key(path)),
    }
}

pub(crate) fn app_user_model_id(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok()?;
        let id = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const c_void));
        id.filter(|value| !value.is_empty())
    }
}

pub(crate) fn app_display_name(app_id: &str) -> Option<String> {
    unsafe {
        let path = format!("shell:AppsFolder\\{app_id}");
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        text.filter(|value| !value.is_empty())
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

pub(crate) fn fallback_name(target: &Target) -> String {
    target
        .process_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Application")
        .to_string()
}
