//! Hotkeys: the `config.json` shortcut format, matching key events against
//! it, display labels, and the live shortcuts the keyboard hook reads.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::UI::Input::KeyboardAndMouse::{VK_OEM_3, VK_TAB};

use crate::switcher::MODE_WINDOW;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Modifier {
    Alt,
    Win,
}

impl Modifier {
    pub(crate) fn is_down(self, alt_down: bool, win_down: bool) -> bool {
        match self {
            Modifier::Alt => alt_down,
            Modifier::Win => win_down,
        }
    }

    /// Spelling in `config.json` and in display labels.
    fn name(self) -> &'static str {
        match self {
            Modifier::Alt => "Alt",
            Modifier::Win => "Win",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "alt" => Some(Modifier::Alt),
            "win" | "windows" => Some(Modifier::Win),
            _ => None,
        }
    }
}

/// A key the hotkey format can express. The one table behind parsing,
/// `config.json` spelling, display labels and naming a recorded key.
struct HotkeyKey {
    virtual_key: u32,
    /// Physical key, so the shortcut follows the key across layouts: the US
    /// backtick and the French `²` key are both scancode 0x29.
    scan_code: u32,
    /// `config.json` spelling.
    config: &'static str,
    /// Overlay and tray label.
    hint: &'static str,
    /// Accepted (lowercase, space-free) spellings when parsing.
    aliases: &'static [&'static str],
}

const KEYS: [HotkeyKey; 2] = [
    HotkeyKey {
        virtual_key: VK_TAB.0 as u32,
        scan_code: 0x0F,
        config: "Tab",
        hint: "Tab",
        aliases: &["tab"],
    },
    HotkeyKey {
        virtual_key: VK_OEM_3.0 as u32,
        scan_code: 0x29,
        config: "Backtick",
        hint: "`",
        aliases: &["backtick", "`", "oem3", "²", "oem8"],
    },
];

fn key_by_virtual_key(virtual_key: u32) -> Option<&'static HotkeyKey> {
    KEYS.iter().find(|key| key.virtual_key == virtual_key)
}

/// The `config.json` name ("Tab" or "Backtick") of a pressed key, matched
/// by virtual key or physical scan code like `Shortcut::matches`; `None`
/// when the hotkey format cannot express it.
pub(crate) fn config_key_name(virtual_key: u32, scan_code: u32) -> Option<&'static str> {
    KEYS.iter()
        .find(|key| key.virtual_key == virtual_key || key.scan_code == scan_code)
        .map(|key| key.config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shortcut {
    pub(crate) modifier: Modifier,
    pub(crate) virtual_key: u32,
    pub(crate) scan_code: u32,
}

/// The built-in shortcuts, also used when `config.json` holds a value the
/// parser rejects. Keep in sync with `AppConfig::default`.
pub(crate) const DEFAULT_WINDOW_SHORTCUT: Shortcut = Shortcut {
    modifier: Modifier::Alt,
    virtual_key: VK_OEM_3.0 as u32,
    scan_code: 0x29,
};
pub(crate) const DEFAULT_APP_SHORTCUT: Shortcut = Shortcut {
    modifier: Modifier::Win,
    virtual_key: VK_TAB.0 as u32,
    scan_code: 0x0F,
};

impl Shortcut {
    pub(crate) fn matches(&self, vk: u32, scan_code: u32, alt_down: bool, win_down: bool) -> bool {
        let modifier_down = self.modifier.is_down(alt_down, win_down);
        // Matching the physical scan code keeps the shortcut working across
        // layouts: the US backtick and the French `²` key are both scancode 0x29.
        modifier_down
            && (self.virtual_key == vk || (self.scan_code != 0 && self.scan_code == scan_code))
    }

    /// Display label for the overlay, derived from the configured shortcut so
    /// a custom config shows the truth instead of a hardcoded hint.
    pub(crate) fn hint(&self) -> String {
        let key = key_by_virtual_key(self.virtual_key).map_or("?", |key| key.hint);
        format!("{} + {key}", self.modifier.name())
    }

    /// Parses the `config.json` format: one modifier (`Alt` or `Win`), `+`,
    /// then `Tab` or `Backtick`. Case and spaces are ignored.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let normalized = value.to_ascii_lowercase().replace(' ', "");
        let (modifier, key) = normalized.split_once('+')?;
        let modifier = Modifier::parse(modifier)?;
        let key = KEYS
            .iter()
            .find(|candidate| candidate.aliases.contains(&key))?;
        Some(Self {
            modifier,
            virtual_key: key.virtual_key,
            scan_code: key.scan_code,
        })
    }

    /// Canonical `config.json` spelling, e.g. "Alt+Backtick" or "Win+Tab".
    pub(crate) fn config_value(&self) -> String {
        // Only Tab and Backtick are ever stored; anything else reads as Backtick.
        let key = key_by_virtual_key(self.virtual_key).map_or("Backtick", |key| key.config);
        format!("{}+{key}", self.modifier.name())
    }

    /// Packs the shortcut into one integer so the keyboard hook reads the
    /// live value from an atomic instead of taking a lock on every key press.
    fn to_bits(self) -> u64 {
        let modifier: u64 = match self.modifier {
            Modifier::Alt => 0,
            Modifier::Win => 1,
        };
        (modifier << 32)
            | ((self.virtual_key as u64 & 0xFFFF) << 16)
            | (self.scan_code as u64 & 0xFFFF)
    }

    fn from_bits(bits: u64) -> Self {
        Self {
            modifier: if bits >> 32 == 1 {
                Modifier::Win
            } else {
                Modifier::Alt
            },
            virtual_key: ((bits >> 16) & 0xFFFF) as u32,
            scan_code: (bits & 0xFFFF) as u32,
        }
    }
}

fn parse_shortcut(value: &str, fallback: Shortcut) -> Shortcut {
    Shortcut::parse(value).unwrap_or_else(|| {
        log::warn!(
            "unsupported hotkey {value:?}; using {}",
            fallback.config_value()
        );
        fallback
    })
}

/// The startup shortcuts from the two `config.json` values. Identical
/// shortcuts would make one switcher unreachable, so the app shortcut then
/// takes whichever default the window shortcut is not using.
pub(crate) fn startup_shortcuts(window_value: &str, app_value: &str) -> (Shortcut, Shortcut) {
    let window = parse_shortcut(window_value, DEFAULT_WINDOW_SHORTCUT);
    let mut app = parse_shortcut(app_value, DEFAULT_APP_SHORTCUT);
    if window == app {
        app = if window == DEFAULT_APP_SHORTCUT {
            DEFAULT_WINDOW_SHORTCUT
        } else {
            DEFAULT_APP_SHORTCUT
        };
        log::warn!(
            "window_hotkey and app_hotkey are identical ({}); using {} for apps",
            window.hint(),
            app.hint()
        );
    }
    (window, app)
}

/// The pair after choosing `shortcut` for one switcher (`window` when
/// `for_window`): choosing the other switcher's shortcut swaps the two.
pub(crate) fn assign_shortcut(
    (mut window, mut apps): (Shortcut, Shortcut),
    for_window: bool,
    shortcut: Shortcut,
) -> (Shortcut, Shortcut) {
    let (changed, other) = if for_window {
        (&mut window, &mut apps)
    } else {
        (&mut apps, &mut window)
    };
    if *other == shortcut {
        *other = *changed;
    }
    *changed = shortcut;
    (window, apps)
}

/// The live shortcuts. Written at startup and by the Settings window, read
/// by the keyboard hook on every key press.
static WINDOW_SHORTCUT: AtomicU64 = AtomicU64::new(0);
static APP_SHORTCUT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn window_shortcut() -> Shortcut {
    Shortcut::from_bits(WINDOW_SHORTCUT.load(Ordering::SeqCst))
}

pub(crate) fn app_shortcut() -> Shortcut {
    Shortcut::from_bits(APP_SHORTCUT.load(Ordering::SeqCst))
}

pub(crate) fn store_shortcuts(window: Shortcut, app: Shortcut) {
    WINDOW_SHORTCUT.store(window.to_bits(), Ordering::SeqCst);
    APP_SHORTCUT.store(app.to_bits(), Ordering::SeqCst);
}

pub(crate) fn shortcut_for(mode: &str) -> Shortcut {
    if mode == MODE_WINDOW {
        window_shortcut()
    } else {
        app_shortcut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: u32 = VK_TAB.0 as u32;
    const BACKTICK: u32 = VK_OEM_3.0 as u32;

    #[test]
    fn parses_the_config_format_ignoring_case_and_spaces() {
        assert_eq!(
            Shortcut::parse("Alt+Backtick"),
            Some(DEFAULT_WINDOW_SHORTCUT)
        );
        assert_eq!(Shortcut::parse("Win+Tab"), Some(DEFAULT_APP_SHORTCUT));
        assert_eq!(Shortcut::parse(" win + TAB "), Some(DEFAULT_APP_SHORTCUT));
        assert_eq!(Shortcut::parse("Windows+tab"), Some(DEFAULT_APP_SHORTCUT));
        for alias in ["`", "oem3", "²", "OEM8", "BackTick"] {
            assert_eq!(
                Shortcut::parse(&format!("alt+{alias}")),
                Some(DEFAULT_WINDOW_SHORTCUT),
                "{alias}"
            );
        }
        assert_eq!(
            Shortcut::parse("Win+`"),
            Some(Shortcut {
                modifier: Modifier::Win,
                ..DEFAULT_WINDOW_SHORTCUT
            })
        );
    }

    #[test]
    fn rejects_what_the_format_cannot_express() {
        for value in [
            "",
            "Tab",
            "Alt",
            "Alt+",
            "+Tab",
            "Ctrl+Tab",
            "Shift+Tab",
            "Alt+Q",
            "Alt+Tab+X",
            "Alt-Tab",
            "Alt+F4",
        ] {
            assert_eq!(Shortcut::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn config_value_round_trips_every_combination() {
        for modifier in ["Alt", "Win"] {
            for key in ["Tab", "Backtick"] {
                let value = format!("{modifier}+{key}");
                let shortcut = Shortcut::parse(&value).unwrap();
                assert_eq!(shortcut.config_value(), value);
                assert_eq!(Shortcut::parse(&shortcut.config_value()), Some(shortcut));
                assert_eq!(Shortcut::from_bits(shortcut.to_bits()), shortcut);
            }
        }
    }

    #[test]
    fn display_labels() {
        assert_eq!(DEFAULT_WINDOW_SHORTCUT.hint(), "Alt + `");
        assert_eq!(DEFAULT_APP_SHORTCUT.hint(), "Win + Tab");
        let unknown = Shortcut {
            virtual_key: 0x51,
            ..DEFAULT_APP_SHORTCUT
        };
        assert_eq!(unknown.hint(), "Win + ?");
        assert_eq!(unknown.config_value(), "Win+Backtick");
    }

    #[test]
    fn matches_by_virtual_key_or_scan_code_with_the_modifier_held() {
        let alt_backtick = DEFAULT_WINDOW_SHORTCUT;
        assert!(alt_backtick.matches(BACKTICK, 0x29, true, false));
        // French layout: "²" has another virtual key but the same scan code.
        assert!(alt_backtick.matches(0xDE, 0x29, true, false));
        assert!(alt_backtick.matches(BACKTICK, 0, true, false));
        assert!(!alt_backtick.matches(BACKTICK, 0x29, false, true));
        assert!(!alt_backtick.matches(TAB, 0x0F, true, false));
        // A zero scan code never matches by scan code.
        let no_scan = Shortcut {
            scan_code: 0,
            ..alt_backtick
        };
        assert!(!no_scan.matches(0xDE, 0, true, false));
        assert!(DEFAULT_APP_SHORTCUT.matches(TAB, 0x0F, false, true));
        assert!(!DEFAULT_APP_SHORTCUT.matches(TAB, 0x0F, true, false));
    }

    #[test]
    fn names_recorded_keys_by_virtual_key_or_scan_code() {
        assert_eq!(config_key_name(TAB, 0), Some("Tab"));
        assert_eq!(config_key_name(0, 0x0F), Some("Tab"));
        assert_eq!(config_key_name(BACKTICK, 0x29), Some("Backtick"));
        assert_eq!(config_key_name(0xDE, 0x29), Some("Backtick"));
        assert_eq!(config_key_name(0x51, 0x10), None);
    }

    #[test]
    fn invalid_values_fall_back_and_duplicates_are_separated() {
        assert_eq!(
            startup_shortcuts("Alt+Backtick", "Win+Tab"),
            (DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT)
        );
        assert_eq!(
            startup_shortcuts("nonsense", "Ctrl+Q"),
            (DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT)
        );
        // Both on Win+Tab: apps move to the free default.
        assert_eq!(
            startup_shortcuts("Win+Tab", "Win+Tab"),
            (DEFAULT_APP_SHORTCUT, DEFAULT_WINDOW_SHORTCUT)
        );
        let win_backtick = Shortcut::parse("Win+Backtick").unwrap();
        assert_eq!(
            startup_shortcuts("Win+Backtick", "Win+`"),
            (win_backtick, DEFAULT_APP_SHORTCUT)
        );
    }

    #[test]
    fn choosing_the_other_shortcut_swaps_the_two() {
        let defaults = (DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT);
        assert_eq!(
            assign_shortcut(defaults, true, DEFAULT_APP_SHORTCUT),
            (DEFAULT_APP_SHORTCUT, DEFAULT_WINDOW_SHORTCUT)
        );
        assert_eq!(
            assign_shortcut(defaults, false, DEFAULT_WINDOW_SHORTCUT),
            (DEFAULT_APP_SHORTCUT, DEFAULT_WINDOW_SHORTCUT)
        );
        let win_backtick = Shortcut::parse("Win+Backtick").unwrap();
        assert_eq!(
            assign_shortcut(defaults, true, win_backtick),
            (win_backtick, DEFAULT_APP_SHORTCUT)
        );
        assert_eq!(
            assign_shortcut(defaults, false, DEFAULT_APP_SHORTCUT),
            defaults
        );
    }
}
