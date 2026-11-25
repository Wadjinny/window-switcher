//! The switcher session: its state, the controller thread that owns it, and
//! the cards it shows.

pub(crate) mod controller;
pub(crate) mod items;
pub(crate) mod session;

use std::sync::{atomic::AtomicU64, Arc, Mutex, OnceLock};

use tauri::AppHandle;

use crate::{
    ipc::SwitchItem,
    settings_model::{CardStyle, Layout},
    targets::Target,
};

pub(crate) const MODE_WINDOW: &str = "window";
const MODE_APP: &str = "app";

#[derive(Clone)]
pub(crate) struct ActiveSwitcher {
    pub(crate) mode: String,
    pub(crate) hint: String,
    /// Identifies this overlay instance; icon workers from an older generation
    /// stop instead of mutating the new state.
    pub(crate) generation: u64,
    /// Card style captured when the overlay opened, so a settings change
    /// never mixes layouts within one overlay.
    pub(crate) card_style: CardStyle,
    /// Layout captured when the overlay opened, for the same reason.
    pub(crate) layout: Layout,
    pub(crate) targets: Vec<Target>,
    pub(crate) items: Vec<SwitchItem>,
    pub(crate) selected: usize,
    /// Where the overlay laid out each card (reported by the frontend, see
    /// `switcher_grid`), for the arrows that move across lines. Empty until
    /// the first report: the cards are then treated as one line.
    pub(crate) grid: Vec<GridCell>,
}

/// One card's place in the overlay's grid: the line it is on (row in the
/// horizontal layout, column in the vertical one, counted from 0) and the
/// center of the card along that line, in CSS pixels. Rows can be centered,
/// so a card's index alone does not say which card is under it.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
pub(crate) struct GridCell {
    pub(crate) line: u32,
    pub(crate) center: f64,
}

#[derive(Default)]
pub(crate) struct SwitcherState {
    pub(crate) active: Option<ActiveSwitcher>,
}

pub(crate) struct HookRuntime {
    pub(crate) app: AppHandle,
    pub(crate) state: Arc<Mutex<SwitcherState>>,
}

pub(crate) static RUNTIME: OnceLock<HookRuntime> = OnceLock::new();

pub(crate) static SWITCHER_GENERATION: AtomicU64 = AtomicU64::new(0);

/// An arrow key pressed while a switcher is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Which switcher a session belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchMode {
    Window,
    App,
}

impl SwitchMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Window => MODE_WINDOW,
            Self::App => MODE_APP,
        }
    }

    pub(crate) fn bits(self) -> u64 {
        match self {
            Self::Window => 1,
            Self::App => 2,
        }
    }
}
