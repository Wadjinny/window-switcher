//! Bringing the chosen window to the front.

use windows::Win32::{
    Foundation::HWND,
    System::Threading::{AttachThreadInput, GetCurrentThreadId},
    UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
        SetForegroundWindow, ShowWindowAsync, SW_RESTORE,
    },
};

use crate::input::inject::send_mask_key;

/// Brings `hwnd` to the front from a background thread. Never sleeps or
/// waits on the target: a hung app cannot stall the controller.
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
        // The shell can win a race for the foreground right after a key
        // release. Input we inject counts as user activity for the
        // foreground lock, so send the harmless mask key and try once more.
        send_mask_key();
        if set_foreground(hwnd) {
            log::info!("activated the window on the second attempt");
            return true;
        }
        log::warn!("SetForegroundWindow failed for the selected window");
        false
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
    if attached {
        let _ = AttachThreadInput(current_thread, foreground_thread, false);
    }
    raised
}
