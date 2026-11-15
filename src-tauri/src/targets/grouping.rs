//! Grouping windows into the cards of each switcher.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;

use super::{
    enumerate::{eligible_windows, process_path_for_window},
    identity::{target_key, window_key},
    Target,
};

pub(crate) fn windows_for_current_app(active: HWND) -> Vec<Target> {
    let Some(path) = process_path_for_window(active) else {
        return Vec::new();
    };
    windows_of_app(eligible_windows(), &window_key(active, &path))
}

/// The windows whose app identity is `key`, in their original order.
fn windows_of_app(windows: Vec<Target>, key: &str) -> Vec<Target> {
    windows
        .into_iter()
        .filter(|target| target_key(target) == key)
        .collect()
}

pub(crate) fn application_targets(active: HWND) -> Vec<Target> {
    let active_path = process_path_for_window(active);
    let active_key = active_path.as_ref().map(|path| window_key(active, path));
    group_by_app(eligible_windows(), active, active_key.as_deref())
}

/// One card per app for windows in z-order: the app's frontmost window (or
/// `active`, if it is one of them) carrying the app's window count. The
/// active app comes first, the others keep their z-order.
fn group_by_app(windows: Vec<Target>, active: HWND, active_key: Option<&str>) -> Vec<Target> {
    // Preserve the EnumWindows z-order: the first window seen for an app
    // decides where that app appears, so the order is stable between invocations.
    let mut order = Vec::<String>::new();
    let mut groups = HashMap::<String, Vec<Target>>::new();
    for target in windows {
        let key = target_key(&target);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(target);
    }
    let mut apps = order
        .into_iter()
        .filter_map(|key| {
            let mut windows = groups.remove(&key)?;
            windows.sort_by_key(|target| if target.hwnd == active { 0 } else { 1 });
            let count = windows.len();
            let mut first = windows.into_iter().next()?;
            first.window_count = count;
            Some(first)
        })
        .collect::<Vec<_>>();
    if let Some(index) = apps
        .iter()
        .position(|target| Some(target_key(target).as_str()) == active_key)
    {
        let active_app = apps.remove(index);
        apps.insert(0, active_app);
    }
    apps
}

#[cfg(test)]
mod tests {
    use std::{ffi::c_void, path::PathBuf};

    use super::*;

    fn hwnd(id: usize) -> HWND {
        HWND(id as *mut c_void)
    }

    fn window(id: usize, exe: &str, app_id: Option<&str>) -> Target {
        Target {
            hwnd: hwnd(id),
            process_path: PathBuf::from(format!(r"C:\Apps\{exe}")),
            app_id: app_id.map(str::to_string),
            title: format!("window {id}"),
            window_count: 1,
        }
    }

    /// z-order: editor, browser, editor (path in another case), a PWA hosted
    /// by the browser process, browser.
    fn desktop() -> Vec<Target> {
        vec![
            window(1, "Editor.exe", None),
            window(2, "browser.exe", None),
            window(3, "editor.EXE", None),
            window(4, "browser.exe", Some("Pwa.Mail")),
            window(5, "browser.exe", None),
        ]
    }

    /// (hwnd, window count) per card.
    fn cards(targets: &[Target]) -> Vec<(usize, usize)> {
        targets
            .iter()
            .map(|target| (target.hwnd.0 as usize, target.window_count))
            .collect()
    }

    #[test]
    fn identity_is_the_aumid_or_the_case_folded_path() {
        assert_eq!(
            target_key(&window(1, "Editor.exe", None)),
            r"path:c:\apps\editor.exe"
        );
        assert_eq!(
            target_key(&window(4, "browser.exe", Some("Pwa.Mail"))),
            "aumid:Pwa.Mail"
        );
    }

    #[test]
    fn apps_group_in_z_order_with_the_active_app_first() {
        let browser = target_key(&window(0, "browser.exe", None));
        let apps = group_by_app(desktop(), hwnd(5), Some(&browser));
        // The browser first, shown by the active window 5; then the editor
        // (two windows, frontmost first) and the PWA as an app of its own.
        assert_eq!(cards(&apps), vec![(5, 2), (1, 2), (4, 1)]);
    }

    #[test]
    fn without_a_known_active_app_the_z_order_stands() {
        let apps = group_by_app(desktop(), hwnd(99), None);
        assert_eq!(cards(&apps), vec![(1, 2), (2, 2), (4, 1)]);
        assert!(group_by_app(Vec::new(), hwnd(1), None).is_empty());
    }

    #[test]
    fn the_window_switcher_keeps_one_apps_windows_in_order() {
        let editor = target_key(&window(0, "EDITOR.exe", None));
        assert_eq!(
            cards(&windows_of_app(desktop(), &editor)),
            vec![(1, 1), (3, 1)]
        );
        assert_eq!(
            cards(&windows_of_app(desktop(), "aumid:Pwa.Mail")),
            vec![(4, 1)]
        );
        assert!(windows_of_app(desktop(), "path:nothing").is_empty());
    }
}
