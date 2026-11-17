//! The low-level keyboard hook: reads each key event, applies `decide_key`,
//! and posts the resulting command to the controller.

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        OnceLock,
    },
    thread,
    time::Instant,
};

use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_LSHIFT, VK_RSHIFT, VK_SHIFT},
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, PostThreadMessageW,
            SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK,
            KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, MSG, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
        },
    },
};

use super::{
    alt_key_down,
    decide::{decide_key, Action, Foreground, KeyInput},
    inject::{send_mask_key, MASK_KEY_TAG},
    key_is_down,
    mouse::{install_mouse_hook, remove_mouse_hook, remove_mouse_hook_if_idle},
    win_key_down,
};
use crate::{
    hotkey::{app_shortcut, window_shortcut},
    settings_window::{settings_is_foreground, RECORDING_HOTKEY},
    show_error_message,
    switcher::{
        controller::{post_command, Command, RecordedKey},
        SwitchMode, SWITCHER_GENERATION,
    },
    targets::enumerate::is_internal_window,
    timing,
};

static HOOK_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// The open session as the hook sees it: `(generation << 2) | mode bits`, or
/// 0 when no switcher is open. Written by the hook; the controller only ever
/// clears it, and only for the generation it is closing.
pub(crate) static HOOK_SESSION: AtomicU64 = AtomicU64::new(0);

pub(super) fn hook_session() -> Option<(u64, SwitchMode)> {
    let packed = HOOK_SESSION.load(Ordering::SeqCst);
    let mode = match packed & 3 {
        1 => SwitchMode::Window,
        2 => SwitchMode::App,
        _ => return None,
    };
    Some((packed >> 2, mode))
}

fn set_hook_session(generation: u64, mode: SwitchMode) {
    HOOK_SESSION.store((generation << 2) | mode.bits(), Ordering::SeqCst);
}

/// Closes the hook's session if it is still `generation`. Returns false when
/// the hook already closed or replaced it (its own command is then queued).
pub(crate) fn close_hook_session(generation: u64) -> bool {
    let current = HOOK_SESSION.load(Ordering::SeqCst);
    current != 0
        && current >> 2 == generation
        && HOOK_SESSION
            .compare_exchange(current, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

/// Number of app cards Win+Tab would show, kept fresh by the controller. The
/// hook reads it to decide whether to swallow the app shortcut (with fewer
/// than two apps, Win+Tab falls through to Task View). It is a hint: when it
/// is stale the controller replays the shortcut (see `replay_shortcut`).
pub(crate) static APP_TARGETS_HINT: AtomicUsize = AtomicUsize::new(2);

/// Posted by the hook procedure to its own thread: send the mask key right
/// after the hook returns (see `queue_mask_key`).
const WM_HOOK_SEND_MASK: u32 = WM_APP + 2;
/// Posted to the hook thread when a switcher opens: (re)install the mouse
/// hook that closes it on a click outside (see `mouse.rs`).
const WM_HOOK_MOUSE_ON: u32 = WM_APP + 3;
/// Posted by the mouse hook once the switcher closed: remove it.
pub(super) const WM_HOOK_MOUSE_OFF: u32 = WM_APP + 4;
/// The hook thread's id, for the `WM_HOOK_*` messages.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Posts a `WM_HOOK_*` message to the hook thread. Returns false when it
/// could not be posted.
pub(super) fn post_to_hook_thread(message: u32) -> bool {
    let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
    thread_id != 0
        && unsafe { PostThreadMessageW(thread_id, message, WPARAM(0), LPARAM(0)) }.is_ok()
}

/// How often the hook thread considers reinstalling the hook, and how long
/// a hook lives before it is reinstalled at the next quiet moment.
const REHOOK_CHECK_MS: u32 = 5_000;
const REHOOK_AFTER: std::time::Duration = std::time::Duration::from_secs(30);
/// No key event for this long counts as a quiet moment.
const REHOOK_QUIET: std::time::Duration = std::time::Duration::from_secs(1);
/// Set on exit so the hook thread does not reinstall a removed hook.
static HOOK_EXITING: AtomicBool = AtomicBool::new(false);
/// Microseconds since `process_start()` of the last key event the hook saw.
static LAST_KEY_EVENT_US: AtomicU64 = AtomicU64::new(0);

fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn micros_since_start() -> u64 {
    process_start().elapsed().as_micros() as u64
}

pub(crate) fn install_keyboard_hook() {
    process_start();
    let spawned = thread::Builder::new()
        .name("switcher-hook".into())
        .spawn(|| unsafe {
            // This thread only runs the hook, so it is always free to take
            // the next key event; no COM or window work happens here.
            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                Ok(hook) => hook,
                Err(error) => {
                    let message =
                        format!("Window Switcher could not install its global hotkeys.\n\n{error}");
                    log::error!("{message}");
                    show_error_message(&message);
                    std::process::exit(1);
                }
            };
            log::info!("keyboard hook installed");
            HOOK_HANDLE.store(hook.0 as usize, Ordering::SeqCst);
            HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
            let mut installed_at = Instant::now();
            let rehook_timer = SetTimer(None, 0, REHOOK_CHECK_MS, None);
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                if message.hwnd.0.is_null() {
                    match message.message {
                        WM_HOOK_SEND_MASK => {
                            send_mask_key();
                            continue;
                        }
                        WM_HOOK_MOUSE_ON => {
                            install_mouse_hook();
                            continue;
                        }
                        WM_HOOK_MOUSE_OFF => {
                            remove_mouse_hook_if_idle();
                            continue;
                        }
                        _ => {}
                    }
                }
                if message.hwnd.0.is_null()
                    && message.message == WM_TIMER
                    && message.wParam.0 == rehook_timer
                {
                    if installed_at.elapsed() >= REHOOK_AFTER && hook_is_quiet() {
                        reinstall_keyboard_hook();
                        installed_at = Instant::now();
                    }
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            remove_keyboard_hook();
            remove_mouse_hook();
        });

    if let Err(error) = spawned {
        let message = format!("Window Switcher could not start its hotkey thread.\n\n{error}");
        log::error!("{message}");
        show_error_message(&message);
        std::process::exit(1);
    }
}

/// No switcher open, nothing recording, no modifier held and no key event
/// for a moment: reinstalling the hook now cannot split a key sequence.
fn hook_is_quiet() -> bool {
    let idle_us = micros_since_start().saturating_sub(LAST_KEY_EVENT_US.load(Ordering::SeqCst));
    HOOK_SESSION.load(Ordering::SeqCst) == 0
        && !RECORDING_HOTKEY.load(Ordering::SeqCst)
        && idle_us >= REHOOK_QUIET.as_micros() as u64
        && !alt_key_down()
        && !win_key_down()
        && !unsafe { key_is_down(VK_CONTROL.0 as i32) || key_is_down(VK_SHIFT.0 as i32) }
}

/// Windows removes a low-level hook without notice when one call exceeds
/// `LowLevelHooksTimeout` (a stall elsewhere in the system can cause it, not
/// only a slow hook), and there is no API to ask whether a hook is still
/// installed. So the hook thread periodically installs a fresh hook and then
/// removes the old one. Both calls run on this thread, which also runs the
/// hook procedure, so no key event is handled between them: there is never a
/// moment without a hook, and no event is seen twice.
fn reinstall_keyboard_hook() {
    if HOOK_EXITING.load(Ordering::SeqCst) {
        return;
    }
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) } {
        Ok(hook) => {
            let old = HOOK_HANDLE.swap(hook.0 as usize, Ordering::SeqCst);
            if old != 0 {
                unsafe {
                    let _ = UnhookWindowsHookEx(HHOOK(old as *mut c_void));
                }
            }
            log::debug!("keyboard hook reinstalled");
        }
        Err(error) => log::warn!("unable to reinstall the keyboard hook: {error}"),
    }
}

pub(crate) fn remove_keyboard_hook() {
    HOOK_EXITING.store(true, Ordering::SeqCst);
    remove_mouse_hook();
    let raw = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

/// Hook procedure timing (only with `WINDOW_SWITCHER_TIMING=1`), logged and
/// reset by the controller after each switch.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static HOOK_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static HOOK_MAX_NS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 {
        let started = Instant::now();
        // A panic must never unwind into Windows (that aborts the process):
        // on a panic the key simply passes through.
        let swallow = std::panic::catch_unwind(|| {
            let key = *(data.0 as *const KBDLLHOOKSTRUCT);
            handle_key_event(&key, message.0 as u32, started)
        })
        .unwrap_or_else(|_| {
            log::error!("the keyboard hook panicked; the key was passed through");
            false
        });
        if timing::enabled() {
            let elapsed = started.elapsed().as_nanos() as u64;
            HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
            HOOK_TOTAL_NS.fetch_add(elapsed, Ordering::Relaxed);
            HOOK_MAX_NS.fetch_max(elapsed, Ordering::Relaxed);
        }
        if swallow {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, message, data)
}

/// Reads the inputs for `decide_key`, applies its decision (session bit,
/// mask key, command) and returns whether to swallow the key. Only atomics,
/// `GetAsyncKeyState`, `GetForegroundWindow` and a channel push: no locks
/// that other threads hold for long, no COM, no window messages.
fn handle_key_event(key: &KBDLLHOOKSTRUCT, message: u32, at: Instant) -> bool {
    if key.dwExtraInfo == MASK_KEY_TAG {
        // Our own mask key or replayed shortcut: never treat it as a hotkey.
        return false;
    }
    LAST_KEY_EVENT_US.store(micros_since_start(), Ordering::SeqCst);
    let down = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return false,
    };
    let alt = key.flags.0 & LLKHF_ALTDOWN.0 != 0 || alt_key_down();
    let win = win_key_down();
    let shift = unsafe {
        key_is_down(VK_SHIFT.0 as i32)
            || key_is_down(VK_LSHIFT.0 as i32)
            || key_is_down(VK_RSHIFT.0 as i32)
    };
    let session = hook_session();
    let foreground = unsafe { GetForegroundWindow() };
    let input = KeyInput {
        vk: key.vkCode,
        scan_code: key.scanCode,
        down,
        alt,
        win,
        shift,
        session: session.map(|(_, mode)| mode),
        recording: RECORDING_HOTKEY.load(Ordering::SeqCst) && settings_is_foreground(),
        foreground: if foreground.0.is_null() {
            Foreground::None
        } else if is_internal_window(foreground) {
            Foreground::Own
        } else {
            Foreground::Other
        },
        app_targets: APP_TARGETS_HINT.load(Ordering::SeqCst),
        window_shortcut: window_shortcut(),
        app_shortcut: app_shortcut(),
    };
    let decision = decide_key(&input);

    if decision.close_stale {
        if let Some((generation, _)) = session {
            if close_hook_session(generation) {
                post_command(Command::Cancel {
                    generation,
                    reason: "switcher modifier is no longer held",
                });
            }
        }
    }
    match decision.action {
        Action::PassThrough => {}
        Action::Open { mode, reverse } => {
            let generation = SWITCHER_GENERATION
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            set_hook_session(generation, mode);
            post_command(Command::Open {
                mode,
                reverse,
                generation,
                vk: key.vkCode,
                scan_code: key.scanCode,
                at,
            });
            // Watch for a click outside the switcher while it is open.
            post_to_hook_thread(WM_HOOK_MOUSE_ON);
        }
        Action::Advance { reverse } => {
            if let Some((generation, _)) = session {
                post_command(Command::Advance {
                    generation,
                    reverse,
                    at,
                });
            }
        }
        Action::Navigate(direction) => {
            if let Some((generation, _)) = session {
                post_command(Command::Navigate {
                    generation,
                    direction,
                    at,
                });
            }
        }
        Action::Commit => {
            if let Some((generation, _)) = session {
                if close_hook_session(generation) {
                    post_command(Command::Commit { generation, at });
                }
            }
        }
        Action::Cancel => {
            if let Some((generation, _)) = session {
                if close_hook_session(generation) {
                    post_command(Command::Cancel {
                        generation,
                        reason: "Escape",
                    });
                }
            }
        }
        Action::RecordKey => {
            const LLKHF_EXTENDED: u32 = 0x01;
            post_command(Command::RecordKey(RecordedKey {
                vk: key.vkCode,
                scan_code: key.scanCode,
                extended: key.flags.0 & LLKHF_EXTENDED != 0,
                alt,
                win,
                ctrl: unsafe { key_is_down(VK_CONTROL.0 as i32) },
                shift,
            }));
        }
    }
    // After the command, so the controller starts on it first.
    if decision.mask {
        queue_mask_key();
    }
    decision.swallow
}

/// Sends the mask key from the hook thread as soon as the hook procedure
/// returns. `SendInput` called inside the hook procedure itself takes about
/// 2 ms (the input system is busy waiting for this very hook), which would
/// make every swallowed key that slow. The message is handled within
/// microseconds, while the modifier is still physically held, and injected
/// input is appended to the input queue either way, so the order of events
/// is the same as sending it inline.
fn queue_mask_key() {
    if !post_to_hook_thread(WM_HOOK_SEND_MASK) {
        send_mask_key();
    }
}

/// Logs how long the hook procedure took since the last switch.
pub(crate) fn log_hook_timing() {
    if !timing::enabled() {
        return;
    }
    let calls = HOOK_CALLS.swap(0, Ordering::Relaxed);
    let total = HOOK_TOTAL_NS.swap(0, Ordering::Relaxed);
    let max = HOOK_MAX_NS.swap(0, Ordering::Relaxed);
    if calls > 0 {
        log::info!(
            "[timing] hook: {calls} events, avg {:.1} us, max {:.1} us",
            total as f64 / calls as f64 / 1000.0,
            max as f64 / 1000.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_bits_round_trip_and_close_only_their_generation() {
        set_hook_session(41, SwitchMode::Window);
        assert_eq!(hook_session(), Some((41, SwitchMode::Window)));
        assert!(!close_hook_session(40));
        assert_eq!(hook_session(), Some((41, SwitchMode::Window)));
        assert!(close_hook_session(41));
        assert_eq!(hook_session(), None);
        assert!(!close_hook_session(41));
    }
}
