//! The low-level mouse hook: a click outside the switcher closes it, like
//! Escape.
//!
//! The overlay is sized to the panel and never takes focus, so a click
//! outside it lands on another app and the overlay never hears of it. A
//! low-level mouse hook sees it first. The hook is only installed while a
//! switcher is open (every mouse move in the session waits for a low-level
//! hook), on the keyboard hook's thread, which pumps the messages it needs.
//!
//! The click that closes the switcher is swallowed, together with its button
//! release, so it does nothing else: like Escape, the window that was in
//! front stays in front.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
};

use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetAncestor, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint,
        GA_ROOT, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
        WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    },
};

use super::hook::{close_hook_session, hook_session, post_to_hook_thread, WM_HOOK_MOUSE_OFF};
use crate::{
    overlay::is_overlay_window,
    switcher::controller::{post_command, Command},
};

static MOUSE_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Buttons (`button_bit`) whose press was swallowed: their release is
/// swallowed too, so no app sees a release without its press.
static SWALLOWED_BUTTONS: AtomicU8 = AtomicU8::new(0);
/// Set once the hook asked its thread to remove it, so it asks only once.
static REMOVAL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A button press or release, as one bit per button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ButtonEvent {
    pub(crate) button: u8,
    pub(crate) down: bool,
}

/// Maps a low-level mouse message to a button event; moves and the wheel are
/// `None`.
fn button_event(message: u32, mouse_data: u32) -> Option<ButtonEvent> {
    let (button, down) = match message {
        WM_LBUTTONDOWN => (1, true),
        WM_LBUTTONUP => (1, false),
        WM_RBUTTONDOWN => (2, true),
        WM_RBUTTONUP => (2, false),
        WM_MBUTTONDOWN => (4, true),
        WM_MBUTTONUP => (4, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // The high word names the X button: 1 or 2.
            let bit = match mouse_data >> 16 {
                1 => 8,
                2 => 16,
                _ => return None,
            };
            (bit, message == WM_XBUTTONDOWN)
        }
        _ => return None,
    };
    Some(ButtonEvent { button, down })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MouseDecision {
    pub(crate) swallow: bool,
    /// Close the open switcher, like Escape.
    pub(crate) cancel: bool,
    /// The new `SWALLOWED_BUTTONS`.
    pub(crate) swallowed: u8,
}

/// The hook's whole decision for one button event. Pure, so it is unit
/// tested. `outside` is only asked for a press while a switcher is open.
pub(crate) fn decide_button(
    event: ButtonEvent,
    session_open: bool,
    swallowed: u8,
    outside: impl FnOnce() -> bool,
) -> MouseDecision {
    let pass = |swallowed| MouseDecision {
        swallow: false,
        cancel: false,
        swallowed,
    };
    if !event.down {
        if swallowed & event.button != 0 {
            return MouseDecision {
                swallow: true,
                cancel: false,
                swallowed: swallowed & !event.button,
            };
        }
        return pass(swallowed);
    }
    if session_open && outside() {
        return MouseDecision {
            swallow: true,
            cancel: true,
            swallowed: swallowed | event.button,
        };
    }
    // A press that goes through: its release must go through as well.
    pass(swallowed & !event.button)
}

/// Installs a fresh mouse hook, replacing any earlier one: Windows removes a
/// low-level hook without notice when it is too slow once, so a hook from an
/// earlier opening may be gone. Runs on the keyboard hook's thread.
pub(super) fn install_mouse_hook() {
    REMOVAL_REQUESTED.store(false, Ordering::SeqCst);
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) } {
        Ok(hook) => {
            let old = MOUSE_HOOK.swap(hook.0 as usize, Ordering::SeqCst);
            unhook(old);
        }
        Err(error) => log::warn!("unable to install the mouse hook: {error}"),
    }
}

/// Removes the mouse hook once no switcher is open and no swallowed button
/// is still waiting for its release.
pub(super) fn remove_mouse_hook_if_idle() {
    if mouse_hook_is_idle() {
        remove_mouse_hook();
    } else {
        REMOVAL_REQUESTED.store(false, Ordering::SeqCst);
    }
}

pub(super) fn remove_mouse_hook() {
    unhook(MOUSE_HOOK.swap(0, Ordering::SeqCst));
}

fn unhook(raw: usize) {
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

fn mouse_hook_is_idle() -> bool {
    hook_session().is_none() && SWALLOWED_BUTTONS.load(Ordering::SeqCst) == 0
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 {
        // A panic must never unwind into Windows: on a panic the event passes.
        let swallow = std::panic::catch_unwind(|| {
            let event = *(data.0 as *const MSLLHOOKSTRUCT);
            handle_mouse_event(&event, message.0 as u32)
        })
        .unwrap_or(false);
        if swallow {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, message, data)
}

/// Applies `decide_button` and returns whether to swallow the event. Like the
/// keyboard hook: atomics, a hit test and a channel push, nothing slow.
fn handle_mouse_event(event: &MSLLHOOKSTRUCT, message: u32) -> bool {
    let session = hook_session();
    let swallowed = SWALLOWED_BUTTONS.load(Ordering::SeqCst);
    if session.is_none() && swallowed == 0 {
        // The switcher closed: this hook has nothing left to do.
        if !REMOVAL_REQUESTED.swap(true, Ordering::SeqCst)
            && !post_to_hook_thread(WM_HOOK_MOUSE_OFF)
        {
            REMOVAL_REQUESTED.store(false, Ordering::SeqCst);
        }
        return false;
    }
    let Some(button) = button_event(message, event.mouseData) else {
        return false;
    };
    let decision = decide_button(button, session.is_some(), swallowed, || {
        // The window under the pointer, by its top-level window: the
        // overlay's webview is a child window of it.
        let target = unsafe { GetAncestor(WindowFromPoint(event.pt), GA_ROOT) };
        // When the overlay registry is busy, let the click through rather
        // than close the switcher on a click that may be on a card.
        !is_overlay_window(target).unwrap_or(true)
    });
    SWALLOWED_BUTTONS.store(decision.swallowed, Ordering::SeqCst);
    if decision.cancel {
        if let Some((generation, _)) = session {
            if close_hook_session(generation) {
                post_command(Command::Cancel {
                    generation,
                    reason: "click outside the switcher",
                });
            }
        }
    }
    decision.swallow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(button: u8) -> ButtonEvent {
        ButtonEvent { button, down: true }
    }

    fn release(button: u8) -> ButtonEvent {
        ButtonEvent {
            button,
            down: false,
        }
    }

    #[test]
    fn maps_button_messages_to_one_bit_per_button() {
        assert_eq!(button_event(WM_LBUTTONDOWN, 0), Some(press(1)));
        assert_eq!(button_event(WM_RBUTTONUP, 0), Some(release(2)));
        assert_eq!(button_event(WM_MBUTTONDOWN, 0), Some(press(4)));
        assert_eq!(button_event(WM_XBUTTONDOWN, 1 << 16), Some(press(8)));
        assert_eq!(button_event(WM_XBUTTONUP, 2 << 16), Some(release(16)));
        assert_eq!(button_event(WM_XBUTTONDOWN, 3 << 16), None);
        // Moves and the wheel are not buttons.
        assert_eq!(button_event(0x0200, 0), None);
        assert_eq!(button_event(0x020A, 120 << 16), None);
    }

    #[test]
    fn a_press_outside_closes_the_switcher_and_swallows_the_whole_click() {
        let down = decide_button(press(1), true, 0, || true);
        assert_eq!(
            down,
            MouseDecision {
                swallow: true,
                cancel: true,
                swallowed: 1
            }
        );
        // The switcher is closed by the time the button is released.
        let up = decide_button(release(1), false, down.swallowed, || unreachable!());
        assert_eq!(
            up,
            MouseDecision {
                swallow: true,
                cancel: false,
                swallowed: 0
            }
        );
    }

    #[test]
    fn a_press_on_the_overlay_goes_through() {
        // Clicking a card is handled by the overlay itself.
        let down = decide_button(press(1), true, 0, || false);
        assert!(!down.swallow && !down.cancel && down.swallowed == 0);
        let up = decide_button(release(1), true, 0, || unreachable!());
        assert!(!up.swallow && !up.cancel);
    }

    #[test]
    fn without_a_switcher_the_pointer_is_not_even_hit_tested() {
        let down = decide_button(press(2), false, 0, || unreachable!());
        assert!(!down.swallow && !down.cancel);
    }

    #[test]
    fn only_the_swallowed_button_has_its_release_swallowed() {
        // Right button swallowed; the left one is released normally.
        let up = decide_button(release(1), false, 2, || unreachable!());
        assert!(!up.swallow && up.swallowed == 2);
        let up = decide_button(release(2), false, 2, || unreachable!());
        assert!(up.swallow && up.swallowed == 0);
    }

    #[test]
    fn a_press_that_goes_through_clears_a_stale_swallowed_release() {
        // The release of a swallowed press was never seen: the next press of
        // that button goes through, so its release must too.
        let down = decide_button(press(1), false, 1 | 4, || unreachable!());
        assert!(!down.swallow && down.swallowed == 4);
    }
}
