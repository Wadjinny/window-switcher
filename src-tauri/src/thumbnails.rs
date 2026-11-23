//! Live window previews (DWM thumbnails) on the overlay.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{Mutex, OnceLock},
};

use tauri::WebviewWindow;
use windows::core::BOOL;
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::{
        DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
        DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
        DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
    },
};

use crate::{
    icon_layer::{self, CornerIcon},
    ipc::ThumbnailSlot,
    overlay::{extra_overlay_index, OVERLAY_LABEL},
    settings_model::CardStyle,
    switcher::RUNTIME,
    timing,
};

// ---------------------------------------------------------------------------
// Window previews
// ---------------------------------------------------------------------------
//
// DWM composes a live thumbnail of each source window straight onto the
// overlay window, above the webview's content. The webview lays the cards out
// and reports where each preview slot is; the selection highlight is the card
// border around the slot, so nothing HTML ever has to draw over a thumbnail.

/// Registered DWM thumbnails: overlay HWND -> (source HWND -> HTHUMBNAIL).
/// Handles are stored as integers because raw HWNDs are not `Send`.
type ThumbnailMap = Mutex<HashMap<isize, HashMap<isize, isize>>>;
static THUMBNAILS: OnceLock<ThumbnailMap> = OnceLock::new();

fn thumbnails() -> &'static ThumbnailMap {
    THUMBNAILS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unregisters every preview on every overlay.
pub(crate) fn clear_thumbnails() {
    let Ok(mut map) = thumbnails().lock() else {
        return;
    };
    for (_, sources) in map.drain() {
        for (_, thumbnail) in sources {
            unsafe {
                let _ = DwmUnregisterThumbnail(thumbnail);
            }
        }
    }
}

/// Called by an overlay webview after layout (the `update_thumbnails`
/// command): places one DWM thumbnail per slot on that overlay window and
/// removes the ones it no longer shows.
pub(crate) fn place_thumbnails(
    window: &WebviewWindow,
    generation: u64,
    scale: f64,
    slots: Vec<ThumbnailSlot>,
) -> Result<(), String> {
    let label = window.label();
    if label != OVERLAY_LABEL && extra_overlay_index(label).is_none() {
        return Err("only the switcher overlay shows previews".into());
    }
    if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
        return Err(format!("invalid scale {scale}"));
    }
    let destination = HWND(window.hwnd().map_err(|error| error.to_string())?.0);

    // Hold the registry lock while checking the switcher state: hiding the
    // overlay clears the state first and this registry second, so a report
    // that loses that race finds no active switcher and registers nothing.
    let mut map = thumbnails()
        .lock()
        .map_err(|_| "thumbnail registry poisoned".to_string())?;
    let allowed = RUNTIME
        .get()
        .and_then(|runtime| runtime.state.lock().ok())
        .and_then(|state| {
            let active = state.active.as_ref()?;
            if active.generation != generation || active.card_style != CardStyle::Preview {
                return None;
            }
            Some(
                active
                    .items
                    .iter()
                    .filter(|item| item.thumbnail)
                    .map(|item| (item.id, item.icon.clone()))
                    .collect::<Vec<_>>(),
            )
        });
    let Some(allowed) = allowed else {
        // A late report from an overlay that already closed or reopened.
        return Ok(());
    };

    let sources = map.entry(destination.0 as isize).or_default();
    let mut wanted = Vec::with_capacity(slots.len());
    let mut icons = Vec::new();
    for slot in &slots {
        // Only windows on the current cards: the webview cannot ask for a
        // preview of an arbitrary window.
        let Some((_, icon)) = allowed.iter().find(|(id, _)| *id == slot.id) else {
            continue;
        };
        if wanted.contains(&(slot.id as isize)) {
            continue;
        }
        let source = slot.id as isize;
        let thumbnail = match sources.get(&source) {
            Some(&thumbnail) => thumbnail,
            None => {
                let source_hwnd = HWND(source as *mut c_void);
                match unsafe { DwmRegisterThumbnail(destination, source_hwnd) } {
                    Ok(thumbnail) => {
                        sources.insert(source, thumbnail);
                        thumbnail
                    }
                    Err(error) => {
                        log::debug!("unable to register a window preview: {error}");
                        continue;
                    }
                }
            }
        };
        wanted.push(source);
        let properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION
                | DWM_TNP_VISIBLE
                | DWM_TNP_OPACITY
                | DWM_TNP_SOURCECLIENTAREAONLY,
            rcDestination: fit_thumbnail(thumbnail, slot, scale),
            opacity: 255,
            fVisible: BOOL(1),
            fSourceClientAreaOnly: BOOL(0),
            ..Default::default()
        };
        if let Err(error) = unsafe { DwmUpdateThumbnailProperties(thumbnail, &properties) } {
            log::debug!("unable to place a window preview: {error}");
        }
        if let (Some(rect), Some(icon)) = (slot.icon, icon) {
            icons.push(CornerIcon {
                rect,
                icon: icon.clone(),
            });
        }
    }
    sources.retain(|source, thumbnail| {
        let keep = wanted.contains(source);
        if !keep {
            unsafe {
                let _ = DwmUnregisterThumbnail(*thumbnail);
            }
        }
        keep
    });
    drop(map);
    show_corner_icons(window, generation, destination, scale, icons);
    timing::since_open(
        generation,
        "thumbnails placed",
        &format!("{} on {label}", wanted.len()),
    );
    Ok(())
}

/// Draws the app icons over the previews' corners on the main thread, which
/// owns the icon layer windows, unless the switcher closed or reopened in the
/// meantime (hiding queues `icon_layer::hide_all` on the same thread).
fn show_corner_icons(
    window: &WebviewWindow,
    generation: u64,
    destination: HWND,
    scale: f64,
    icons: Vec<CornerIcon>,
) {
    let overlay = destination.0 as isize;
    let queued = window.run_on_main_thread(move || {
        let current = RUNTIME
            .get()
            .and_then(|runtime| runtime.state.lock().ok())
            .is_some_and(|state| {
                state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.generation == generation)
            });
        if current {
            icon_layer::show_icons(HWND(overlay as *mut c_void), scale, &icons);
        }
    });
    if let Err(error) = queued {
        log::debug!("unable to queue the preview icons: {error}");
    }
}

/// The slot in physical client pixels, shrunk to the source window's aspect
/// ratio and centred, since DWM stretches a thumbnail to whatever it gets.
fn fit_thumbnail(thumbnail: isize, slot: &ThumbnailSlot, scale: f64) -> RECT {
    let source = unsafe { DwmQueryThumbnailSourceSize(thumbnail) }
        .ok()
        .map(|size| (size.cx, size.cy));
    fit_rect(slot, scale, source)
}

/// `fit_thumbnail` for a source of `source` pixels (`None` when unknown, in
/// which case the slot is filled).
fn fit_rect(slot: &ThumbnailSlot, scale: f64, source: Option<(i32, i32)>) -> RECT {
    let width = (slot.width * scale).max(1.0);
    let height = (slot.height * scale).max(1.0);
    let (mut fit_width, mut fit_height) = (width, height);
    if let Some((cx, cy)) = source {
        if cx > 0 && cy > 0 {
            let ratio = (width / cx as f64).min(height / cy as f64);
            fit_width = cx as f64 * ratio;
            fit_height = cy as f64 * ratio;
        }
    }
    let x = slot.x * scale + (width - fit_width) / 2.0;
    let y = slot.y * scale + (height - fit_height) / 2.0;
    RECT {
        left: x.round() as i32,
        top: y.round() as i32,
        right: (x + fit_width).round() as i32,
        bottom: (y + fit_height).round() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(x: f64, y: f64, width: f64, height: f64) -> ThumbnailSlot {
        ThumbnailSlot {
            id: 1,
            x,
            y,
            width,
            height,
            icon: None,
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_wide_source_is_letterboxed_and_centred() {
        // 16:9 into a 200x200 slot: full width, 112.5 tall, centred.
        let fitted = fit_rect(&slot(10.0, 20.0, 200.0, 200.0), 1.0, Some((1920, 1080)));
        assert_eq!(fitted, rect(10, 64, 210, 176));
    }

    #[test]
    fn a_tall_source_is_pillarboxed_and_centred() {
        let fitted = fit_rect(&slot(0.0, 0.0, 240.0, 132.0), 1.0, Some((500, 1000)));
        assert_eq!(fitted, rect(87, 0, 153, 132));
    }

    #[test]
    fn the_slot_is_scaled_to_physical_pixels() {
        let fitted = fit_rect(&slot(10.0, 10.0, 160.0, 90.0), 1.5, Some((1600, 900)));
        assert_eq!(fitted, rect(15, 15, 255, 150));
    }

    #[test]
    fn an_unknown_or_empty_source_fills_the_slot() {
        let expected = rect(4, 6, 104, 56);
        let slot = slot(2.0, 3.0, 50.0, 25.0);
        assert_eq!(fit_rect(&slot, 2.0, None), expected);
        assert_eq!(fit_rect(&slot, 2.0, Some((0, 900))), expected);
        assert_eq!(fit_rect(&slot, 2.0, Some((1600, -1))), expected);
    }

    #[test]
    fn a_degenerate_slot_is_at_least_one_pixel() {
        let fitted = fit_rect(&slot(5.0, 5.0, 0.0, 0.0), 1.0, Some((100, 100)));
        assert_eq!(fitted, rect(5, 5, 6, 6));
    }
}
