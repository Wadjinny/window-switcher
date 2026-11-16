//! Recognising the shell's own surfaces (Task View, Start, search, the
//! desktop, the taskbar) when they are the foreground window. They are never
//! switch targets, and Task View refuses to hand the foreground to a window
//! until it is closed.

use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetClassNameW};

use super::enumerate::process_path_for_window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellSurface {
    /// Task View (Win+Tab), and the shell's own full-screen switchers.
    TaskView,
    /// Start, search, the notification centre, quick settings, the tray
    /// overflow: flyouts that close on Escape or when they lose focus.
    Flyout,
    /// The desktop (`Progman` / `WorkerW`).
    Desktop,
    /// The taskbar on any monitor.
    Taskbar,
}

impl ShellSurface {
    /// Escape closes it and returns the foreground to the window under it.
    pub(crate) fn closes_on_escape(self) -> bool {
        matches!(self, Self::TaskView | Self::Flyout)
    }
}

/// Packaged shell hosts whose `CoreWindow` is a flyout, not an app.
const FLYOUT_HOSTS: &[&str] = &[
    "startmenuexperiencehost.exe",
    "searchhost.exe",
    "searchapp.exe",
    "searchui.exe",
    "shellexperiencehost.exe",
    "shellhost.exe",
];

/// Classifies a window from its class name and its process's file name.
/// Pure, so the table is unit tested.
pub(crate) fn classify(class: &str, exe: &str) -> Option<ShellSurface> {
    let explorer = exe.eq_ignore_ascii_case("explorer.exe");
    match class {
        // Windows 11 hosts Task View in a XAML island inside explorer;
        // Windows 10 used `MultitaskingViewFrame`.
        "XamlExplorerHostIslandWindow" | "MultitaskingViewFrame" | "TaskSwitcherWnd"
            if explorer =>
        {
            Some(ShellSurface::TaskView)
        }
        "Windows.UI.Core.CoreWindow"
            if FLYOUT_HOSTS
                .iter()
                .any(|host| exe.eq_ignore_ascii_case(host)) =>
        {
            Some(ShellSurface::Flyout)
        }
        "NotifyIconOverflowWindow" | "TopLevelWindowForOverflowXamlIsland" if explorer => {
            Some(ShellSurface::Flyout)
        }
        "Progman" | "WorkerW" if explorer => Some(ShellSurface::Desktop),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" if explorer => Some(ShellSurface::Taskbar),
        _ => None,
    }
}

pub(crate) fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

/// The shell surface `hwnd` is, if any.
pub(crate) fn shell_surface(hwnd: HWND) -> Option<ShellSurface> {
    if hwnd.0.is_null() {
        return None;
    }
    let class = window_class(hwnd);
    // Only the classes above need the (slower) process lookup.
    if !matches!(
        class.as_str(),
        "XamlExplorerHostIslandWindow"
            | "MultitaskingViewFrame"
            | "TaskSwitcherWnd"
            | "Windows.UI.Core.CoreWindow"
            | "NotifyIconOverflowWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
    ) {
        return None;
    }
    let path = process_path_for_window(hwnd)?;
    let exe = path.file_name()?.to_string_lossy();
    classify(&class, &exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_view_is_recognised_on_windows_10_and_11() {
        for class in ["XamlExplorerHostIslandWindow", "MultitaskingViewFrame"] {
            assert_eq!(
                classify(class, "explorer.exe"),
                Some(ShellSurface::TaskView)
            );
            assert_eq!(
                classify(class, "EXPLORER.EXE"),
                Some(ShellSurface::TaskView)
            );
        }
    }

    #[test]
    fn shell_flyouts_are_recognised_by_their_host() {
        for host in [
            "StartMenuExperienceHost.exe",
            "SearchHost.exe",
            "ShellExperienceHost.exe",
            "ShellHost.exe",
        ] {
            assert_eq!(
                classify("Windows.UI.Core.CoreWindow", host),
                Some(ShellSurface::Flyout),
                "{host}"
            );
        }
        assert_eq!(
            classify("TopLevelWindowForOverflowXamlIsland", "explorer.exe"),
            Some(ShellSurface::Flyout)
        );
    }

    #[test]
    fn desktop_and_taskbar_are_recognised() {
        assert_eq!(
            classify("Progman", "explorer.exe"),
            Some(ShellSurface::Desktop)
        );
        assert_eq!(
            classify("WorkerW", "explorer.exe"),
            Some(ShellSurface::Desktop)
        );
        assert_eq!(
            classify("Shell_TrayWnd", "explorer.exe"),
            Some(ShellSurface::Taskbar)
        );
        assert_eq!(
            classify("Shell_SecondaryTrayWnd", "explorer.exe"),
            Some(ShellSurface::Taskbar)
        );
    }

    #[test]
    fn app_windows_are_not_shell_surfaces() {
        // A File Explorer window, a packaged app and look-alike classes in
        // other processes are ordinary switch targets.
        assert_eq!(classify("CabinetWClass", "explorer.exe"), None);
        assert_eq!(
            classify("ApplicationFrameWindow", "ApplicationFrameHost.exe"),
            None
        );
        assert_eq!(
            classify("Windows.UI.Core.CoreWindow", "Calculator.exe"),
            None
        );
        assert_eq!(classify("Progman", "notepad.exe"), None);
        assert_eq!(classify("XamlExplorerHostIslandWindow", "myapp.exe"), None);
    }

    #[test]
    fn only_overlays_close_on_escape() {
        assert!(ShellSurface::TaskView.closes_on_escape());
        assert!(ShellSurface::Flyout.closes_on_escape());
        assert!(!ShellSurface::Desktop.closes_on_escape());
        assert!(!ShellSurface::Taskbar.closes_on_escape());
    }
}
