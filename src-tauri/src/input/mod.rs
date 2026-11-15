//! Keyboard input: the low-level hook, its pure decision, and the keys we
//! inject.

pub(crate) mod decide;
pub(crate) mod hook;
pub(crate) mod inject;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LMENU, VK_LWIN, VK_RMENU, VK_RWIN,
};

// ---------------------------------------------------------------------------
// Keyboard hook and switcher controller
// ---------------------------------------------------------------------------
//
// While a low-level keyboard hook runs, keyboard input for the whole session
// waits for it, and Windows silently removes a hook that takes longer than
// `LowLevelHooksTimeout`. So the hook (`keyboard_hook`) only makes a pure,
// in-memory decision (`decide_key`): swallow or pass the key, and which
// `Command` to post. Everything slow (enumerating windows, COM, building the
// payload, emitting events, showing and hiding overlays, activating windows)
// runs in order on the controller thread (`run_controller`).
//
// The hook owns the "is a switcher session open" bit (`HOOK_SESSION`), so it
// can decide the next key without waiting for the controller. The controller
// clears that bit (compare-and-swap on the generation) when it closes a
// session on its own: no targets, or the watchdog found the modifier up.

pub(crate) unsafe fn key_is_down(vk: i32) -> bool {
    GetAsyncKeyState(vk) as u16 & 0x8000 != 0
}

pub(crate) fn alt_key_down() -> bool {
    unsafe { key_is_down(VK_LMENU.0 as i32) || key_is_down(VK_RMENU.0 as i32) }
}

pub(crate) fn win_key_down() -> bool {
    unsafe { key_is_down(VK_LWIN.0 as i32) || key_is_down(VK_RWIN.0 as i32) }
}
