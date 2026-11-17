//! The hook's decision for one key event, kept pure so it is unit tested.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_UP,
};

use crate::{
    hotkey::{Modifier, Shortcut},
    switcher::{Direction, SwitchMode},
};

/// What the foreground window is, for the swallow decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Foreground {
    /// No foreground window (e.g. during a desktop switch).
    None,
    /// One of ours (Settings, unless `settings_in_switcher` lists it): the
    /// switcher works relative to the last app.
    Own,
    Other,
}

/// Everything the hook decision needs, read from the key event and memory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyInput {
    pub(crate) vk: u32,
    pub(crate) scan_code: u32,
    pub(crate) down: bool,
    pub(crate) alt: bool,
    pub(crate) win: bool,
    pub(crate) shift: bool,
    /// The open session's mode, if any.
    pub(crate) session: Option<SwitchMode>,
    /// Settings is in front and one of its hotkey fields is recording.
    pub(crate) recording: bool,
    pub(crate) foreground: Foreground,
    pub(crate) app_targets: usize,
    pub(crate) window_shortcut: Shortcut,
    pub(crate) app_shortcut: Shortcut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    PassThrough,
    Open { mode: SwitchMode, reverse: bool },
    Advance { reverse: bool },
    Navigate(Direction),
    Commit,
    Cancel,
    RecordKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The open session's modifier is no longer held (its release was lost,
    /// e.g. to the secure desktop): close it before acting on this key.
    pub(crate) close_stale: bool,
    pub(crate) action: Action,
    /// Swallow the key instead of passing it on.
    pub(crate) swallow: bool,
    /// Inject the mask key (see `send_mask_key`) before returning.
    pub(crate) mask: bool,
}

impl Decision {
    const PASS: Self = Self {
        close_stale: false,
        action: Action::PassThrough,
        swallow: false,
        mask: false,
    };
}

impl KeyInput {
    fn shortcut(&self, mode: SwitchMode) -> Shortcut {
        match mode {
            SwitchMode::Window => self.window_shortcut,
            SwitchMode::App => self.app_shortcut,
        }
    }

    /// Whether pressing `mode`'s shortcut now opens a switcher. The window
    /// switcher opens even for a single window (releasing just re-activates
    /// it); the app switcher needs two apps, or one when Settings is in
    /// front. An empty result is handled by the controller.
    fn opens(&self, mode: SwitchMode) -> bool {
        match (self.foreground, mode) {
            (Foreground::None, _) => false,
            (_, SwitchMode::Window) => true,
            (Foreground::Own, SwitchMode::App) => self.app_targets >= 1,
            (Foreground::Other, SwitchMode::App) => self.app_targets >= 2,
        }
    }
}

fn is_release_of(modifier: Modifier, vk: u32) -> bool {
    match modifier {
        Modifier::Alt => {
            vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 || vk == VK_MENU.0 as u32
        }
        Modifier::Win => vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32,
    }
}

/// The hook's whole decision for one key event. Pure, so it is unit tested.
pub(crate) fn decide_key(input: &KeyInput) -> Decision {
    if input.recording {
        // A Settings field is recording a new shortcut: hand it the
        // combination instead of acting on it, so neither the current hotkeys
        // nor Windows' own Win+Tab / Alt+Tab fire while the user types it.
        // Modifiers pass through so the webview and the shell stay in sync.
        if input.down && !is_modifier_key(input.vk) {
            return Decision {
                action: Action::RecordKey,
                swallow: true,
                mask: input.alt || input.win,
                ..Decision::PASS
            };
        }
        return Decision::PASS;
    }

    // A modifier release can go missing, e.g. when Ctrl+Alt+Del or a UAC
    // prompt switches to the secure desktop mid-switch. Any key event with
    // the mode's modifier no longer held closes the session. The release
    // itself still reports the key as down here, so it commits normally.
    let mut session = input.session;
    let mut close_stale = false;
    if let Some(mode) = session {
        if !input.shortcut(mode).modifier.is_down(input.alt, input.win) {
            session = None;
            close_stale = true;
        }
    }
    let pass = Decision {
        close_stale,
        ..Decision::PASS
    };

    if !input.down {
        // Do not swallow the modifier release: Windows would keep the key
        // logically held, which breaks normal typing afterwards.
        return match session {
            Some(mode) if is_release_of(input.shortcut(mode).modifier, input.vk) => Decision {
                action: Action::Commit,
                ..pass
            },
            _ => pass,
        };
    }

    if session.is_some() && input.vk == VK_ESCAPE.0 as u32 {
        return Decision {
            action: Action::Cancel,
            swallow: true,
            mask: input.alt || input.win,
            ..pass
        };
    }
    let matches = |mode: SwitchMode| {
        input
            .shortcut(mode)
            .matches(input.vk, input.scan_code, input.alt, input.win)
    };
    if let Some(mode) = session {
        if matches(mode) {
            return Decision {
                action: Action::Advance {
                    reverse: input.shift,
                },
                swallow: true,
                mask: true,
                ..pass
            };
        }
        // Arrow keys move the selection, like Windows' own Alt+Tab, and never
        // reach the app in front while the switcher is open.
        if let Some(direction) = arrow_direction(input.vk) {
            return Decision {
                action: Action::Navigate(direction),
                swallow: true,
                mask: true,
                ..pass
            };
        }
    }
    // The window shortcut wins if both ever match the same key.
    let mode = if matches(SwitchMode::Window) {
        SwitchMode::Window
    } else if matches(SwitchMode::App) {
        SwitchMode::App
    } else {
        return pass;
    };
    // Only swallow the key when a switcher opens. Otherwise let Windows
    // handle it, so Win+Tab still opens Task View when there is nothing to
    // switch between.
    if !input.opens(mode) {
        return pass;
    }
    Decision {
        action: Action::Open {
            mode,
            reverse: input.shift,
        },
        swallow: true,
        mask: true,
        ..pass
    }
}

fn arrow_direction(vk: u32) -> Option<Direction> {
    [
        (VK_LEFT, Direction::Left),
        (VK_RIGHT, Direction::Right),
        (VK_UP, Direction::Up),
        (VK_DOWN, Direction::Down),
    ]
    .iter()
    .find(|(key, _)| key.0 as u32 == vk)
    .map(|&(_, direction)| direction)
}

fn is_modifier_key(vk: u32) -> bool {
    [
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
    ]
    .iter()
    .any(|modifier| modifier.0 as u32 == vk)
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_OEM_3, VK_TAB};

    use super::*;
    use crate::hotkey::{DEFAULT_APP_SHORTCUT, DEFAULT_WINDOW_SHORTCUT};

    const TAB: u32 = VK_TAB.0 as u32;
    const BACKTICK: u32 = VK_OEM_3.0 as u32;
    const ESC: u32 = VK_ESCAPE.0 as u32;
    const LWIN: u32 = VK_LWIN.0 as u32;
    const LALT: u32 = VK_LMENU.0 as u32;

    /// Win held, Tab pressed, no session, an ordinary app in front.
    fn base() -> KeyInput {
        KeyInput {
            vk: TAB,
            scan_code: 0x0F,
            down: true,
            alt: false,
            win: true,
            shift: false,
            session: None,
            recording: false,
            foreground: Foreground::Other,
            app_targets: 3,
            window_shortcut: DEFAULT_WINDOW_SHORTCUT,
            app_shortcut: DEFAULT_APP_SHORTCUT,
        }
    }

    fn open(mode: SwitchMode, reverse: bool) -> Decision {
        Decision {
            close_stale: false,
            action: Action::Open { mode, reverse },
            swallow: true,
            mask: true,
        }
    }

    #[test]
    fn win_tab_opens_the_app_switcher() {
        assert_eq!(decide_key(&base()), open(SwitchMode::App, false));
    }

    #[test]
    fn shift_opens_reversed() {
        let input = KeyInput {
            shift: true,
            ..base()
        };
        assert_eq!(decide_key(&input), open(SwitchMode::App, true));
    }

    #[test]
    fn alt_backtick_opens_the_window_switcher_even_for_one_window() {
        let input = KeyInput {
            vk: BACKTICK,
            scan_code: 0x29,
            alt: true,
            win: false,
            app_targets: 0,
            ..base()
        };
        assert_eq!(decide_key(&input), open(SwitchMode::Window, false));
        // The French "²" key: another virtual key, the same scan code.
        let french = KeyInput { vk: 0xDE, ..input };
        assert_eq!(decide_key(&french), open(SwitchMode::Window, false));
    }

    #[test]
    fn win_tab_with_one_app_falls_through_to_task_view() {
        let input = KeyInput {
            app_targets: 1,
            ..base()
        };
        assert_eq!(decide_key(&input), Decision::PASS);
    }

    #[test]
    fn from_settings_one_other_app_is_enough() {
        let input = KeyInput {
            app_targets: 1,
            foreground: Foreground::Own,
            ..base()
        };
        assert_eq!(decide_key(&input), open(SwitchMode::App, false));
    }

    #[test]
    fn nothing_opens_without_a_foreground_window() {
        let input = KeyInput {
            foreground: Foreground::None,
            ..base()
        };
        assert_eq!(decide_key(&input), Decision::PASS);
    }

    #[test]
    fn tab_and_shift_tab_advance_an_open_session() {
        let open = KeyInput {
            session: Some(SwitchMode::App),
            ..base()
        };
        let forward = decide_key(&open);
        assert_eq!(forward.action, Action::Advance { reverse: false });
        assert!(forward.swallow && forward.mask && !forward.close_stale);
        let back = decide_key(&KeyInput {
            shift: true,
            ..open
        });
        assert_eq!(back.action, Action::Advance { reverse: true });
    }

    #[test]
    fn arrows_navigate_an_open_session_and_are_swallowed() {
        let open = KeyInput {
            session: Some(SwitchMode::App),
            ..base()
        };
        for (vk, direction) in [
            (VK_LEFT, Direction::Left),
            (VK_RIGHT, Direction::Right),
            (VK_UP, Direction::Up),
            (VK_DOWN, Direction::Down),
        ] {
            let decision = decide_key(&KeyInput {
                vk: vk.0 as u32,
                scan_code: 0,
                ..open
            });
            assert_eq!(decision.action, Action::Navigate(direction));
            assert!(decision.swallow && decision.mask && !decision.close_stale);
        }
        // Without a session (or after a missed release) arrows are ordinary.
        let left = KeyInput {
            vk: VK_LEFT.0 as u32,
            scan_code: 0x4B,
            ..base()
        };
        assert_eq!(decide_key(&left), Decision::PASS);
        let stale = decide_key(&KeyInput {
            session: Some(SwitchMode::App),
            win: false,
            ..left
        });
        assert!(stale.close_stale && !stale.swallow);
        assert_eq!(stale.action, Action::PassThrough);
        // Releasing an arrow passes through and keeps the session open.
        let up = KeyInput {
            down: false,
            session: Some(SwitchMode::App),
            ..left
        };
        assert_eq!(decide_key(&up), Decision::PASS);
    }

    #[test]
    fn releasing_the_modifier_commits_without_swallowing() {
        let release = KeyInput {
            vk: LWIN,
            scan_code: 0x5B,
            down: false,
            session: Some(SwitchMode::App),
            ..base()
        };
        let decision = decide_key(&release);
        assert_eq!(decision.action, Action::Commit);
        assert!(!decision.swallow && !decision.mask);
        // Releasing Tab (or another key) keeps the session open.
        let tab_up = KeyInput { vk: TAB, ..release };
        assert_eq!(decide_key(&tab_up), Decision::PASS);
    }

    #[test]
    fn the_commit_key_follows_the_configured_modifier() {
        // Shortcuts swapped in Settings: windows on Win+Tab, apps on Alt+`.
        let swapped = KeyInput {
            window_shortcut: DEFAULT_APP_SHORTCUT,
            app_shortcut: DEFAULT_WINDOW_SHORTCUT,
            session: Some(SwitchMode::Window),
            down: false,
            ..base()
        };
        let win_up = KeyInput {
            vk: LWIN,
            ..swapped
        };
        assert_eq!(decide_key(&win_up).action, Action::Commit);
        let alt_up = KeyInput {
            vk: LALT,
            alt: true,
            ..swapped
        };
        assert_eq!(decide_key(&alt_up).action, Action::PassThrough);
    }

    #[test]
    fn escape_cancels_and_is_swallowed() {
        let input = KeyInput {
            vk: ESC,
            scan_code: 0x01,
            session: Some(SwitchMode::App),
            ..base()
        };
        let decision = decide_key(&input);
        assert_eq!(decision.action, Action::Cancel);
        assert!(decision.swallow && decision.mask);
        // Without a session, Escape is an ordinary key.
        let closed = KeyInput {
            session: None,
            ..input
        };
        assert_eq!(decide_key(&closed), Decision::PASS);
    }

    #[test]
    fn a_missed_modifier_release_closes_the_session() {
        // The session is open but Win is no longer held: the next key closes
        // it and is then handled as if nothing was open.
        let input = KeyInput {
            vk: 0x41, // A
            scan_code: 0x1E,
            win: false,
            session: Some(SwitchMode::App),
            ..base()
        };
        let decision = decide_key(&input);
        assert!(decision.close_stale);
        assert_eq!(decision.action, Action::PassThrough);
        assert!(!decision.swallow);
        // ...even when that key is Escape, which then passes through.
        let escape = decide_key(&KeyInput { vk: ESC, ..input });
        assert!(escape.close_stale && !escape.swallow);
        // A new shortcut after the missed release opens a fresh session.
        let reopen = decide_key(&KeyInput {
            vk: BACKTICK,
            scan_code: 0x29,
            alt: true,
            ..input
        });
        assert!(reopen.close_stale);
        assert_eq!(
            reopen.action,
            Action::Open {
                mode: SwitchMode::Window,
                reverse: false
            }
        );
    }

    #[test]
    fn the_other_shortcut_switches_modes_while_open() {
        let input = KeyInput {
            vk: BACKTICK,
            scan_code: 0x29,
            alt: true,
            session: Some(SwitchMode::App),
            ..base()
        };
        assert_eq!(decide_key(&input), open(SwitchMode::Window, false));
    }

    #[test]
    fn recording_swallows_combinations_and_passes_modifiers() {
        let recording = KeyInput {
            recording: true,
            ..base()
        };
        let decision = decide_key(&recording);
        assert_eq!(decision.action, Action::RecordKey);
        assert!(decision.swallow && decision.mask);
        // A plain key needs no mask key.
        let plain = decide_key(&KeyInput {
            vk: 0x51, // Q
            win: false,
            ..recording
        });
        assert_eq!(plain.action, Action::RecordKey);
        assert!(plain.swallow && !plain.mask);
        // Modifiers and key releases pass through.
        let modifier = KeyInput {
            vk: LWIN,
            ..recording
        };
        assert_eq!(decide_key(&modifier), Decision::PASS);
        let release = KeyInput {
            down: false,
            ..recording
        };
        assert_eq!(decide_key(&release), Decision::PASS);
    }

    #[test]
    fn unrelated_keys_pass_through() {
        let input = KeyInput {
            vk: 0x41,
            scan_code: 0x1E,
            ..base()
        };
        assert_eq!(decide_key(&input), Decision::PASS);
        let alt_tab = KeyInput {
            alt: true,
            win: false,
            ..base()
        };
        assert_eq!(decide_key(&alt_tab), Decision::PASS);
    }
}
