//! Operations on the open switcher: moving the selection, showing, hiding,
//! and committing it.

use std::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

use tauri::Emitter;

use super::{ActiveSwitcher, HookRuntime, SwitcherState};
use crate::{
    input::hook::close_hook_session,
    ipc::{SelectionPayload, SwitcherPayload},
    lock_unpoisoned,
    overlay::{hide_overlay, show_overlay},
    targets::activate::activate_window,
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

/// Whether hovering card `index` may change session `generation`'s
/// selection: the session must still be the one on screen, the index must
/// fall inside its targets, and the card must not already be selected. Pure,
/// so it is unit tested.
pub(crate) fn apply_mouse_selection(
    active: &mut ActiveSwitcher,
    generation: u64,
    index: usize,
) -> bool {
    if active.generation != generation || index >= active.targets.len() || active.selected == index
    {
        return false;
    }
    active.selected = index;
    true
}

/// The mouse moved over a card: select it and send the new selection to the
/// overlays, without re-showing or resizing them. Returns whether anything
/// changed.
pub(crate) fn select_switcher(runtime: &HookRuntime, generation: u64, index: usize) -> bool {
    let changed = runtime
        .state
        .lock()
        .ok()
        .and_then(|mut state| {
            state
                .active
                .as_mut()
                .map(|active| apply_mouse_selection(active, generation, index))
        })
        .unwrap_or(false);
    if changed {
        emit_selection(runtime, &mut None);
        log::debug!("mouse hover selected card {index} of #{generation}");
    }
    changed
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
        let selection = runtime.state.lock().ok().and_then(|mut state| {
            let selected = state
                .active
                .as_ref()
                .and_then(|active| commit_index(active, generation, clicked))?;
            state
                .active
                .take_if(|active| active.generation == generation)
                .map(|active| (active.targets, selected))
        });
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
    if clicked.is_some() {
        log::info!("switcher committed by mouse click");
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
    use crate::{
        ipc::SwitchItem,
        settings_model::{CardStyle, Layout},
        targets::Target,
    };
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
        }
    }

    #[test]
    fn hovering_a_card_selects_it() {
        let mut active = active(7, 3, 0);
        assert!(apply_mouse_selection(&mut active, 7, 2));
        assert_eq!(active.selected, 2);
        // Selecting the card the mouse already sits on changes nothing, so
        // no selection event is emitted for it.
        assert!(!apply_mouse_selection(&mut active, 7, 2));
        assert_eq!(active.selected, 2);
    }

    #[test]
    fn hovering_ignores_a_stale_generation() {
        let mut active = active(7, 3, 0);
        assert!(!apply_mouse_selection(&mut active, 6, 1));
        assert_eq!(active.selected, 0);
    }

    #[test]
    fn hovering_ignores_an_out_of_range_index() {
        let mut active = active(7, 3, 0);
        assert!(!apply_mouse_selection(&mut active, 7, 3));
        assert_eq!(active.selected, 0);
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
}
