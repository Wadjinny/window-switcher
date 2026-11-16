//! Operations on the open switcher: moving the selection, showing, hiding,
//! and committing it.

use std::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

use tauri::Emitter;

use super::{ActiveSwitcher, Direction, HookRuntime, SwitcherState};
use crate::{
    input::hook::close_hook_session,
    ipc::{SelectionPayload, SwitcherPayload},
    lock_unpoisoned,
    overlay::{hide_overlay, show_overlay},
    settings_model::Layout,
    targets::{activate::activate_window, Target},
    timing,
};

/// Generation and mode of the open switcher, if any.
pub(crate) fn active_switcher(runtime: &HookRuntime) -> Option<(u64, String)> {
    let state = runtime.state.lock().ok()?;
    let active = state.active.as_ref()?;
    Some((active.generation, active.mode.clone()))
}

pub(crate) fn advance_switcher(
    runtime: &HookRuntime,
    generation: u64,
    reverse: bool,
    watch: &mut Option<timing::Stopwatch>,
) {
    if let Ok(mut state) = runtime.state.lock() {
        match &mut state.active {
            Some(active) if active.generation == generation && !active.targets.is_empty() => {
                let count = active.targets.len();
                active.selected = if reverse {
                    (active.selected + count - 1) % count
                } else {
                    (active.selected + 1) % count
                };
            }
            _ => return,
        }
    }
    emit_selection(runtime, watch);
}

/// The card an arrow key moves the selection to, like Windows' own Alt+Tab.
/// The cards are laid out in lines of `per_line` cards: rows in the
/// horizontal layout, columns in the vertical one (which fills a column top
/// to bottom, then wraps into the next). The arrows along a line (Left/Right
/// horizontally, Up/Down vertically) step to the previous/next card and wrap
/// around. The arrows across lines (Up/Down horizontally, Left/Right
/// vertically) move one line, keeping the position in it, and stop at the
/// first and last line; into a shorter last line they land on its last card,
/// and with a single line (or `per_line` not reported yet) they do nothing.
/// Pure, so it is unit tested.
pub(crate) fn navigate_index(
    selected: usize,
    count: usize,
    per_line: usize,
    layout: Layout,
    direction: Direction,
) -> usize {
    if count == 0 {
        return selected;
    }
    let per_line = if per_line == 0 {
        count
    } else {
        per_line.min(count)
    };
    let (back, forward) = match layout {
        Layout::Horizontal => (Direction::Left, Direction::Right),
        Layout::Vertical => (Direction::Up, Direction::Down),
    };
    let previous_line = match (layout, direction) {
        (Layout::Horizontal, Direction::Up) | (Layout::Vertical, Direction::Left) => true,
        (Layout::Horizontal, Direction::Down) | (Layout::Vertical, Direction::Right) => false,
        _ if direction == back => return (selected + count - 1) % count,
        _ if direction == forward => return (selected + 1) % count,
        _ => return selected,
    };
    if previous_line {
        selected.checked_sub(per_line).unwrap_or(selected)
    } else if selected / per_line < (count - 1) / per_line {
        (selected + per_line).min(count - 1)
    } else {
        selected
    }
}

/// An arrow key: move the selection of session `generation` (see
/// `navigate_index`) and send it to the overlays when it changed.
pub(crate) fn navigate_switcher(
    runtime: &HookRuntime,
    generation: u64,
    direction: Direction,
    watch: &mut Option<timing::Stopwatch>,
) {
    if let Ok(mut state) = runtime.state.lock() {
        match &mut state.active {
            Some(active) if active.generation == generation && !active.targets.is_empty() => {
                let selected = navigate_index(
                    active.selected,
                    active.targets.len(),
                    active.per_line,
                    active.layout,
                    direction,
                );
                if selected == active.selected {
                    return;
                }
                active.selected = selected;
            }
            _ => return,
        }
    }
    emit_selection(runtime, watch);
}

/// The overlay reported how many cards fit in one line (row, or column in
/// the vertical layout) of session `generation`.
pub(crate) fn set_switcher_per_line(runtime: &HookRuntime, generation: u64, per_line: usize) {
    if let Ok(mut state) = runtime.state.lock() {
        if let Some(active) = state.active.as_mut() {
            if active.generation == generation {
                active.per_line = per_line;
            }
        }
    }
}

/// Only the selection moved: send just the new index. The overlay is already
/// placed and shown, and the cards (with their icon data) are already drawn.
fn emit_selection(runtime: &HookRuntime, watch: &mut Option<timing::Stopwatch>) {
    let payload = runtime.state.lock().ok().and_then(|state| {
        state.active.as_ref().map(|active| SelectionPayload {
            generation: active.generation,
            selected: active.selected,
            sent_at: timing::unix_ms(),
        })
    });
    let Some(payload) = payload else { return };
    let _ = runtime.app.emit("switcher:select", &payload);
    timing::mark(watch, "emitted");
}

/// Held while the switcher state is checked and the overlays are shown or
/// hidden as a result, so a show from another thread can never land after
/// the controller closed the switcher and hid them.
static OVERLAY_VISIBILITY: Mutex<()> = Mutex::new(());

fn lock_overlay_visibility() -> MutexGuard<'static, ()> {
    lock_unpoisoned(&OVERLAY_VISIBILITY)
}

/// Closes the overlay only if it is still showing `generation`.
pub(crate) fn cancel_generation(runtime: &HookRuntime, generation: u64) -> bool {
    let _visibility = lock_overlay_visibility();
    let was_active = runtime
        .state
        .lock()
        .ok()
        .map(|mut state| {
            state
                .active
                .take_if(|active| active.generation == generation)
                .is_some()
        })
        .unwrap_or(false);
    if was_active {
        let mut watch = timing::Stopwatch::start("cancel");
        hide_overlay(&runtime.app);
        timing::mark(&mut watch, "hidden");
        timing::finish(watch, "");
    }
    was_active
}

/// The target a commit activates: the card the mouse clicked when it falls
/// inside the session's targets, otherwise (no click — a modifier release)
/// the session's own selection. `None` leaves the session open: the
/// generation is stale or the click index is out of range. Pure, so it is
/// unit tested.
pub(crate) fn commit_index(
    active: &ActiveSwitcher,
    generation: u64,
    clicked: Option<usize>,
) -> Option<usize> {
    if active.generation != generation {
        return None;
    }
    match clicked {
        Some(index) => (index < active.targets.len()).then_some(index),
        None => Some(active.selected),
    }
}

/// Ends session `generation` for a commit and returns its targets and the
/// index to activate, or `None` (leaving the state alone) when the commit
/// does not apply — see `commit_index`. A session can only be taken once, so
/// a click and the modifier release that follows never both commit. Pure, so
/// it is unit tested.
pub(crate) fn take_commit(
    state: &mut SwitcherState,
    generation: u64,
    clicked: Option<usize>,
) -> Option<(Vec<Target>, usize)> {
    let selected = state
        .active
        .as_ref()
        .and_then(|active| commit_index(active, generation, clicked))?;
    state
        .active
        .take_if(|active| active.generation == generation)
        .map(|active| (active.targets, selected))
}

/// Commits the switcher: activates the selected window and hides the
/// overlay. `clicked` is the card the mouse clicked, overriding the current
/// selection (an out-of-range click leaves the session alone). Returns
/// whether a window was activated.
///
/// A click ends the session itself, so the hook's record of it is dropped
/// here and the modifier release that follows finds nothing to commit.
pub(crate) fn commit_switcher(
    runtime: &HookRuntime,
    generation: u64,
    clicked: Option<usize>,
    at: Instant,
) -> bool {
    let mut watch = timing::Stopwatch::start_at(
        if clicked.is_some() {
            "click"
        } else {
            "release"
        },
        at,
    );
    timing::mark(&mut watch, "dequeued");
    let selection = {
        let _visibility = lock_overlay_visibility();
        let selection = runtime
            .state
            .lock()
            .ok()
            .and_then(|mut state| take_commit(&mut state, generation, clicked));
        if selection.is_some() {
            // The click, not a modifier release, is ending this session: the
            // release that follows must not try to commit it a second time.
            if clicked.is_some() {
                close_hook_session(generation);
            }
            hide_overlay(&runtime.app);
        }
        selection
    };
    timing::mark(&mut watch, "hidden");

    let Some((targets, selected)) = selection else {
        return false;
    };
    if targets.is_empty() {
        return false;
    }
    // A window can disappear between enumeration and activation; walk forward
    // to the next card instead of silently doing nothing.
    for offset in 0..targets.len() {
        let target = &targets[(selected + offset) % targets.len()];
        if activate_window(target.hwnd) {
            timing::mark(&mut watch, "activated");
            timing::finish(watch, "");
            if offset > 0 {
                log::warn!("selected window was gone; activated the next target instead");
            }
            return true;
        }
    }
    log::warn!("unable to activate any window for the current selection");
    false
}

pub(crate) fn payload_snapshot(state: &SwitcherState) -> Option<SwitcherPayload> {
    state.active.as_ref().map(|active| SwitcherPayload {
        mode: active.mode.clone(),
        selected: active.selected,
        items: active.items.clone(),
        hint: active.hint.clone(),
        card_style: active.card_style.as_str().to_string(),
        layout: active.layout.as_str().to_string(),
        generation: active.generation,
        timing: timing::enabled(),
        sent_at: timing::unix_ms(),
    })
}

/// Emits the current payload and sizes/shows the overlay. Never holds the
/// state lock while talking to the windowing system.
pub(crate) fn emit_and_show(runtime: &HookRuntime, watch: &mut Option<timing::Stopwatch>) {
    // The visibility lock (not the state lock) spans the check and the show:
    // closing takes it before taking the state and hiding, so a show from the
    // overlay-creation thread can never land after the overlays were hidden.
    let _visibility = lock_overlay_visibility();
    let payload = runtime
        .state
        .lock()
        .ok()
        .and_then(|state| payload_snapshot(&state));
    let Some(payload) = payload else { return };
    timing::mark(watch, "payload");
    let _ = runtime.app.emit("switcher:state", &payload);
    timing::mark(watch, "emitted");
    show_overlay(&runtime.app, &payload);
    timing::mark(watch, "shown");
    if let Some(watch) = watch {
        // Window calls are queued to the main thread; this runs once it has
        // worked through them, i.e. when the overlay is really on screen.
        let start = watch.start_instant();
        let generation = payload.generation;
        let _ = runtime.app.run_on_main_thread(move || {
            log::info!(
                "[timing] open #{generation} main thread caught up at {:.2} ms",
                start.elapsed().as_secs_f64() * 1000.0
            );
        });
    }
    if watch.is_some() {
        let bytes = serde_json::to_vec(&payload)
            .map(|json| json.len())
            .unwrap_or(0);
        log::info!("[timing] payload #{} is {bytes} bytes", payload.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ipc::SwitchItem, settings_model::CardStyle};
    use windows::Win32::Foundation::HWND;

    fn target() -> Target {
        Target {
            hwnd: HWND::default(),
            process_path: std::path::PathBuf::from("C:\\apps\\one.exe"),
            app_id: None,
            title: "One".into(),
            window_count: 1,
        }
    }

    /// An open app switcher with `count` cards, `selected` highlighted.
    fn active(generation: u64, count: usize, selected: usize) -> ActiveSwitcher {
        ActiveSwitcher {
            mode: "app".into(),
            hint: "Win + Tab".into(),
            generation,
            card_style: CardStyle::Icon,
            layout: Layout::Horizontal,
            targets: (0..count).map(|_| target()).collect(),
            items: Vec::<SwitchItem>::new(),
            selected,
            per_line: 0,
        }
    }

    #[test]
    fn left_and_right_step_through_the_cards_and_wrap() {
        let layout = Layout::Horizontal;
        assert_eq!(navigate_index(0, 5, 3, layout, Direction::Right), 1);
        assert_eq!(navigate_index(4, 5, 3, layout, Direction::Right), 0);
        assert_eq!(navigate_index(0, 5, 3, layout, Direction::Left), 4);
        assert_eq!(navigate_index(3, 5, 3, layout, Direction::Left), 2);
        // One card: nothing to move to, and no card list at all is harmless.
        assert_eq!(
            navigate_index(0, 1, 0, Layout::Horizontal, Direction::Right),
            0
        );
        assert_eq!(
            navigate_index(0, 0, 0, Layout::Horizontal, Direction::Left),
            0
        );
    }

    #[test]
    fn up_and_down_move_between_rows_of_the_grid() {
        let nav =
            |selected, direction| navigate_index(selected, 8, 3, Layout::Horizontal, direction);
        // Rows: [0 1 2] [3 4 5] [6 7].
        assert_eq!(nav(1, Direction::Down), 4);
        assert_eq!(nav(4, Direction::Up), 1);
        assert_eq!(nav(4, Direction::Down), 7);
        // Into the shorter last row: its last card.
        assert_eq!(nav(5, Direction::Down), 7);
        // No wrap at the first and last rows.
        assert_eq!(nav(1, Direction::Up), 1);
        assert_eq!(nav(6, Direction::Down), 6);
        assert_eq!(nav(7, Direction::Down), 7);
    }

    #[test]
    fn up_and_down_do_nothing_in_a_single_row() {
        for per_line in [0, 4, 9] {
            for direction in [Direction::Up, Direction::Down] {
                assert_eq!(
                    navigate_index(2, 4, per_line, Layout::Horizontal, direction),
                    2
                );
            }
        }
    }

    #[test]
    fn up_and_down_step_through_a_vertical_list_and_wrap() {
        let nav = |selected, direction| navigate_index(selected, 8, 3, Layout::Vertical, direction);
        // Down goes on into the next column, and wraps like Left/Right do
        // horizontally.
        assert_eq!(nav(1, Direction::Down), 2);
        assert_eq!(nav(2, Direction::Down), 3);
        assert_eq!(nav(7, Direction::Down), 0);
        assert_eq!(nav(3, Direction::Up), 2);
        assert_eq!(nav(0, Direction::Up), 7);
    }

    #[test]
    fn left_and_right_move_between_columns_of_a_vertical_list() {
        let nav = |selected, direction| navigate_index(selected, 8, 3, Layout::Vertical, direction);
        // Columns: [0 1 2] [3 4 5] [6 7].
        assert_eq!(nav(1, Direction::Right), 4);
        assert_eq!(nav(4, Direction::Left), 1);
        assert_eq!(nav(4, Direction::Right), 7);
        // Into the shorter last column: its last card.
        assert_eq!(nav(5, Direction::Right), 7);
        // No wrap at the first and last columns.
        assert_eq!(nav(1, Direction::Left), 1);
        assert_eq!(nav(6, Direction::Right), 6);
        // A single column (or no report yet): Left/Right do nothing.
        for per_line in [0, 4] {
            for direction in [Direction::Left, Direction::Right] {
                assert_eq!(
                    navigate_index(2, 4, per_line, Layout::Vertical, direction),
                    2
                );
            }
        }
    }

    #[test]
    fn a_release_commit_activates_the_current_selection() {
        let active = active(7, 3, 1);
        assert_eq!(commit_index(&active, 7, None), Some(1));
        // A stale generation leaves the session open.
        assert_eq!(commit_index(&active, 6, None), None);
    }

    #[test]
    fn a_click_commits_the_clicked_card() {
        let active = active(7, 3, 0);
        assert_eq!(commit_index(&active, 7, Some(2)), Some(2));
        // An out-of-range click leaves the session open.
        assert_eq!(commit_index(&active, 7, Some(3)), None);
        // So does a click on a session that is no longer on screen.
        assert_eq!(commit_index(&active, 6, Some(2)), None);
    }

    #[test]
    fn a_click_then_the_release_commits_only_once() {
        let mut state = SwitcherState {
            active: Some(active(7, 3, 0)),
        };
        // The click commits the clicked card and ends the session...
        let (targets, index) = take_commit(&mut state, 7, Some(2)).expect("click commits");
        assert_eq!((targets.len(), index), (3, 2));
        assert!(state.active.is_none());
        // ...so the modifier release that follows finds nothing to commit.
        assert!(take_commit(&mut state, 7, None).is_none());
    }

    #[test]
    fn a_release_then_a_late_click_commits_only_once() {
        let mut state = SwitcherState {
            active: Some(active(7, 3, 1)),
        };
        assert_eq!(take_commit(&mut state, 7, None).map(|(_, i)| i), Some(1));
        assert!(take_commit(&mut state, 7, Some(2)).is_none());
    }

    #[test]
    fn a_rejected_click_leaves_the_session_open() {
        let mut state = SwitcherState {
            active: Some(active(7, 3, 1)),
        };
        assert!(take_commit(&mut state, 7, Some(5)).is_none());
        assert!(take_commit(&mut state, 6, Some(0)).is_none());
        assert!(state.active.is_some());
        // The release still commits the keyboard selection.
        assert_eq!(take_commit(&mut state, 7, None).map(|(_, i)| i), Some(1));
    }
}
