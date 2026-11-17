//! Enumerating the top-level windows the switcher may offer.

use std::{ffi::c_void, path::PathBuf};

use windows::{
    core::{BOOL, PWSTR},
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM},
        Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
        System::Threading::{
            GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetForegroundWindow, GetWindow, GetWindowLongW, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE,
            GW_OWNER, WS_CHILD, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
        },
    },
};

use super::{identity::app_user_model_id, shell::shell_surface, Target};
use crate::settings_window::settings_is_switchable;

/// The window the switcher works relative to: the foreground window, or,
/// when one of ours (Settings, unless `settings_in_switcher` is on; see
/// `is_internal_window`) or a shell surface (Task View, Start, the
/// desktop, the taskbar) is in front, the topmost app window, i.e. the one
/// the user came from (neither is ever a switch target). The flag says that
/// window is not focused, so its card is a real move.
pub(crate) fn switch_origin() -> Option<(HWND, bool)> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    if !is_internal_window(foreground) && shell_surface(foreground).is_none() {
        return Some((foreground, false));
    }
    eligible_windows()
        .into_iter()
        .next()
        .map(|target| (target.hwnd, true))
}

/// Top-level windows the switcher could offer, in z-order, without the
/// per-window process and COM lookups of `eligible_windows`.
pub(crate) fn switchable_windows() -> Vec<HWND> {
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = &mut *(lparam.0 as *mut Vec<HWND>);
        if is_switchable(hwnd) {
            windows.push(hwnd);
        }
        BOOL(1)
    }
    let mut windows = Vec::<HWND>::new();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut windows as *mut Vec<HWND> as isize));
    }
    windows
}

pub(crate) fn eligible_windows() -> Vec<Target> {
    #[derive(Default)]
    struct Context {
        windows: Vec<Target>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let context = &mut *(lparam.0 as *mut Context);
        if is_switchable(hwnd) {
            if let Some(process_path) = process_path_for_window(hwnd) {
                context.windows.push(Target {
                    hwnd,
                    process_path,
                    app_id: app_user_model_id(hwnd),
                    title: window_title(hwnd),
                    window_count: 1,
                });
            }
        }
        BOOL(1)
    }
    let mut context = Context::default();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut context as *mut Context as isize));
    }
    context.windows
}

/// What the switch filter asks about a window. A trait so the filter is
/// tested with fake windows and still reads each fact only when needed:
/// most enumerated windows are invisible and fail the first check.
trait WindowFacts {
    fn visible(&self) -> bool;
    fn own(&self) -> bool;
    fn style(&self) -> u32;
    fn ex_style(&self) -> u32;
    fn has_owner(&self) -> bool;
    fn cloaked(&self) -> bool;
    fn has_title(&self) -> bool;
}

/// The windows Alt+Tab would show: visible, not ours (`own` already allows
/// for the Settings option), not a child or tool
/// window, unowned (or explicitly an app window), not cloaked, and titled.
fn passes_switch_filter(window: &impl WindowFacts) -> bool {
    // Our own overlay must never be offered as a target.
    if !window.visible() || window.own() {
        return false;
    }
    let style = window.style();
    let ex_style = window.ex_style();
    if style & WS_CHILD.0 != 0 || ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    if window.has_owner() && ex_style & WS_EX_APPWINDOW.0 == 0 {
        return false;
    }
    if window.cloaked() {
        return false;
    }
    window.has_title()
}

/// A real window, read through Win32.
struct LiveWindow(HWND);

impl WindowFacts for LiveWindow {
    fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.0) }.as_bool()
    }

    fn own(&self) -> bool {
        is_internal_window(self.0)
    }

    fn style(&self) -> u32 {
        unsafe { GetWindowLongW(self.0, GWL_STYLE) as u32 }
    }

    fn ex_style(&self) -> u32 {
        unsafe { GetWindowLongW(self.0, GWL_EXSTYLE) as u32 }
    }

    fn has_owner(&self) -> bool {
        unsafe { GetWindow(self.0, GW_OWNER) }
            .map(|owner| !owner.0.is_null())
            .unwrap_or(false)
    }

    fn cloaked(&self) -> bool {
        let mut cloaked = 0u32;
        unsafe {
            DwmGetWindowAttribute(
                self.0,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut c_void,
                std::mem::size_of::<u32>() as u32,
            )
        }
        .is_ok()
            && cloaked != 0
    }

    fn has_title(&self) -> bool {
        !window_title(self.0).is_empty()
    }
}

fn is_switchable(hwnd: HWND) -> bool {
    passes_switch_filter(&LiveWindow(hwnd))
}

/// One of ours that the switcher never lists or switches relative to: the
/// overlays always, and the Settings window unless `settings_in_switcher` is
/// on, in which case it is an ordinary window.
pub(crate) fn is_internal_window(hwnd: HWND) -> bool {
    internal(is_own_window(hwnd), || settings_is_switchable(hwnd))
}

/// `is_internal_window` without Win32, so it is unit tested. The Settings
/// check is only made for our own windows.
fn internal(own: bool, listed_settings: impl FnOnce() -> bool) -> bool {
    own && !listed_settings()
}

fn is_own_window(hwnd: HWND) -> bool {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..copied as usize])
    }
}

pub(crate) fn process_path_for_window(hwnd: HWND) -> Option<PathBuf> {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(process);
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window that passes every check; each test breaks one.
    #[derive(Clone, Copy)]
    struct Fake {
        visible: bool,
        own: bool,
        style: u32,
        ex_style: u32,
        has_owner: bool,
        cloaked: bool,
        has_title: bool,
    }

    const APP: Fake = Fake {
        visible: true,
        own: false,
        style: 0,
        ex_style: 0,
        has_owner: false,
        cloaked: false,
        has_title: true,
    };

    impl WindowFacts for Fake {
        fn visible(&self) -> bool {
            self.visible
        }
        fn own(&self) -> bool {
            self.own
        }
        fn style(&self) -> u32 {
            self.style
        }
        fn ex_style(&self) -> u32 {
            self.ex_style
        }
        fn has_owner(&self) -> bool {
            self.has_owner
        }
        fn cloaked(&self) -> bool {
            self.cloaked
        }
        fn has_title(&self) -> bool {
            self.has_title
        }
    }

    #[test]
    fn an_ordinary_app_window_is_offered() {
        assert!(passes_switch_filter(&APP));
    }

    #[test]
    fn hidden_own_child_tool_owned_cloaked_and_untitled_windows_are_not() {
        let rejected = [
            Fake {
                visible: false,
                ..APP
            },
            Fake { own: true, ..APP },
            Fake {
                style: WS_CHILD.0,
                ..APP
            },
            Fake {
                ex_style: WS_EX_TOOLWINDOW.0,
                ..APP
            },
            Fake {
                has_owner: true,
                ..APP
            },
            Fake {
                cloaked: true,
                ..APP
            },
            Fake {
                has_title: false,
                ..APP
            },
            // The app-window flag does not rescue a tool window.
            Fake {
                ex_style: WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0,
                ..APP
            },
        ];
        for (index, window) in rejected.iter().enumerate() {
            assert!(!passes_switch_filter(window), "case {index}");
        }
    }

    #[test]
    fn an_owned_window_needs_the_app_window_flag() {
        assert!(passes_switch_filter(&Fake {
            has_owner: true,
            ex_style: WS_EX_APPWINDOW.0,
            ..APP
        }));
    }

    #[test]
    fn our_windows_are_internal_unless_they_are_the_listed_settings_window() {
        // Another app's window: never internal, and the Settings check is
        // not even made.
        assert!(!internal(false, || panic!(
            "only asked for our own windows"
        )));
        // The overlay, or Settings with the option off.
        assert!(internal(true, || false));
        // Settings with `settings_in_switcher` on: an ordinary window.
        assert!(!internal(true, || true));
    }
}
