//! Operations on the open switcher: moving the selection, showing, hiding,
//! and committing it.

use std::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

use tauri::Emitter;

use super::{HookRuntime, SwitcherState};
use crate::{
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

pub(crate) fn commit_switcher(runtime: &HookRuntime, generation: u64, at: Instant) {
    let mut watch = timing::Stopwatch::start_at("release", at);
    timing::mark(&mut watch, "dequeued");
    let selection = {
        let _visibility = lock_overlay_visibility();
        let selection = runtime.state.lock().ok().and_then(|mut state| {
            state
                .active
                .take_if(|active| active.generation == generation)
                .map(|active| (active.targets, active.selected))
        });
        if selection.is_some() {
            hide_overlay(&runtime.app);
        }
        selection
    };
    timing::mark(&mut watch, "hidden");

    let Some((targets, selected)) = selection else {
        return;
    };
    if targets.is_empty() {
        return;
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
            return;
        }
    }
    log::warn!("unable to activate any window for the current selection");
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
