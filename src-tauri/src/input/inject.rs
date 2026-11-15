//! Keys we inject: the mask key and replayed shortcuts.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LMENU, VK_LWIN,
};

use super::{alt_key_down, win_key_down};
use crate::hotkey::Modifier;

/// `dwExtraInfo` stamped on the mask key so the hook can recognise it.
pub(crate) const MASK_KEY_TAG: usize = 0x5753_4D4B; // "WSMK"
/// Unassigned virtual key (the same one AutoHotkey recommends as its menu
/// mask key): no application or shell shortcut reacts to it.
const MASK_KEY: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

fn tagged_key(vk: u16, scan_code: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan_code,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: MASK_KEY_TAG,
            },
        },
    }
}

fn send_tagged(inputs: &[INPUT]) -> bool {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    sent == inputs.len() as u32
}

/// The hook swallows the hotkey's second key (Tab, backtick, Escape), so the
/// shell only sees the modifier go down and come back up with nothing in
/// between. A lone Win press/release opens Start, and a lone Alt
/// press/release activates the focused window's menu bar. Injecting a
/// harmless key while the modifier is still held tells Windows a combination
/// was used, so the release does nothing. Must be called while the modifier
/// is down, i.e. from the key-down that is being swallowed.
pub(crate) fn send_mask_key() {
    let inputs = [
        tagged_key(MASK_KEY.0, 0, false),
        tagged_key(MASK_KEY.0, 0, true),
    ];
    if !send_tagged(&inputs) {
        log::warn!("unable to send the mask key");
    }
}

/// The hook swallowed a shortcut (from a stale hint) but there turned out to
/// be nothing to switch to: send it on to Windows after all, so Win+Tab still
/// opens Task View. Tagged, so our hook lets it through. If the modifier was
/// already released, the whole combination is replayed.
pub(crate) fn replay_shortcut(modifier: Modifier, vk: u32, scan_code: u32) {
    let (vk, scan) = (vk as u16, scan_code as u16);
    let key = [tagged_key(vk, scan, false), tagged_key(vk, scan, true)];
    let held = modifier.is_down(alt_key_down(), win_key_down());
    let sent = if held {
        send_tagged(&key)
    } else {
        let modifier_vk = match modifier {
            Modifier::Alt => VK_LMENU.0,
            Modifier::Win => VK_LWIN.0,
        };
        send_tagged(&[
            tagged_key(modifier_vk, 0, false),
            key[0],
            key[1],
            tagged_key(modifier_vk, 0, true),
        ])
    };
    if !sent {
        log::warn!("unable to replay the shortcut");
    }
}
