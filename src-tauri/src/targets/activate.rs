//! Bringing the chosen window to the front.

use std::time::{Duration, Instant};

use windows::Win32::{
    Foundation::{GetLastError, HWND},
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
        SetForegroundWindow, ShowWindowAsync, SwitchToThisWindow, SW_RESTORE,
    },
};

use super::shell::{shell_surface, window_class, ShellSurface};
use crate::input::inject::{send_escape, send_mask_key};

/// How long to wait for Task View (or a flyout) to close after Escape.
const DISMISS_TIMEOUT: Duration = Duration::from_millis(600);
/// How long the activated window must keep the foreground after Task View
/// closed, so its closing cannot hand the foreground back to the old window.
const SETTLE_TIME: Duration = Duration::from_millis(120);
/// How long to wait for the asynchronous `SwitchToThisWindow`.
const SWITCH_TIMEOUT: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(5);

/// Brings `hwnd` to the front from the controller thread. Never waits on the
/// target itself (a hung app cannot stall the controller); the only waits
/// are short, bounded polls of the foreground window while the shell closes.
pub(crate) fn activate_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            log::warn!("cannot activate a window that no longer exists");
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            // Asynchronous: `ShowWindow` on another process's window waits
            // for that process to handle the restore.
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        if set_foreground(hwnd) {
            return true;
        }
        let foreground = GetForegroundWindow();
        let surface = shell_surface(foreground);
        log::debug!(
            "foreground class={:?} surface={surface:?}",
            window_class(foreground)
        );

        // Task View keeps the foreground until it is closed. Close it the
        // way the user would; it hands the foreground back to the window
        // under it, and then ours can take it.
        if let Some(surface) = surface.filter(|surface| surface.closes_on_escape()) {
            if dismiss(foreground, surface) && set_foreground(hwnd) && settle(hwnd) {
                log::info!("activated the window after closing {surface:?}");
                return true;
            }
        }

        // The shell can win a race for the foreground right after a key
        // release. Input we inject counts as user activity for the
        // foreground lock, so send the harmless mask key and try once more.
        send_mask_key();
        if set_foreground(hwnd) {
            log::info!("activated the window on the second attempt");
            return true;
        }

        // Last resort: the shell's own switching path (what Alt+Tab uses).
        SwitchToThisWindow(hwnd, true);
        if wait_for(SWITCH_TIMEOUT, || GetForegroundWindow() == hwnd) {
            log::info!("activated the window with SwitchToThisWindow");
            return true;
        }
        log::warn!(
            "SetForegroundWindow failed for the selected window (foreground class={:?})",
            window_class(GetForegroundWindow())
        );
        false
    }
}

/// Sends Escape to the shell `surface` (window `foreground`) and waits until
/// it has given up the foreground.
fn dismiss(foreground: HWND, surface: ShellSurface) -> bool {
    let started = Instant::now();
    send_escape();
    let closed = wait_for(DISMISS_TIMEOUT, || unsafe {
        let now = GetForegroundWindow();
        now != foreground && !now.0.is_null()
    });
    log::debug!(
        "closing {surface:?} with Escape: {} after {} ms",
        if closed { "closed" } else { "still open" },
        started.elapsed().as_millis()
    );
    closed
}

/// The window keeps the foreground for a moment; if the shell took it back
/// while closing, takes it once more.
fn settle(hwnd: HWND) -> bool {
    let lost = wait_for(SETTLE_TIME, || unsafe { GetForegroundWindow() != hwnd });
    !lost || unsafe { set_foreground(hwnd) }
}

/// Polls `condition` until it holds or `timeout` passes.
fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if condition() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// The overlay never takes focus, so Windows' foreground lock rejects
/// `SetForegroundWindow` from this background thread. Sharing input state
/// with the current foreground thread lifts that restriction.
unsafe fn set_foreground(hwnd: HWND) -> bool {
    let foreground = GetForegroundWindow();
    let foreground_thread = GetWindowThreadProcessId(foreground, None);
    let current_thread = GetCurrentThreadId();
    let attached = foreground_thread != 0
        && foreground_thread != current_thread
        && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
    let _ = BringWindowToTop(hwnd);
    let raised = SetForegroundWindow(hwnd).as_bool();
    if !raised {
        log::debug!(
            "SetForegroundWindow refused ({:?}, attached to the foreground thread: {attached})",
            GetLastError()
        );
    }
    if attached {
        let _ = AttachThreadInput(current_thread, foreground_thread, false);
    }
    raised
}
