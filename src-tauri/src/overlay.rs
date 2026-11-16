//! The overlay windows: which monitors they cover, their size and
//! placement, creating the extra ones, and showing or hiding them.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    thread,
};

use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, Size, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use windows::core::BOOL;
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT},
    Graphics::{
        Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED},
        Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC,
            HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
        },
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::{GetCursorPos, GetWindowRect},
    },
};

use crate::{
    ipc::SwitcherPayload,
    settings_model::{current_monitor_mode, CardStyle, Layout, MonitorMode},
    switcher::controller::{post_command, Command},
    thumbnails::clear_thumbnails,
};

/// The overlay lays cards out on a fixed horizontal pitch and row height. The
/// frontend mirrors these numbers, so they must stay in sync with `src/`.
const CARD_PITCH: f64 = 133.0;
const CARD_ROW_HEIGHT: f64 = 145.0;
const OVERLAY_CHROME: f64 = 68.0;
const OVERLAY_HEADER: f64 = 60.0;
const MIN_OVERLAY_HEIGHT: f64 = 205.0;
/// Keeps the caption ("Windows of the current app" + the hotkey hint) readable
/// when the overlay holds a single card.
const MIN_OVERLAY_WIDTH: f64 = 300.0;
/// Preview cards (`.switcher.preview .item` in `src/style.css`): 240x178 cards
/// with a 13px gap. The frontend shrinks them when the monitor is too small.
const PREVIEW_CARD_PITCH: f64 = 253.0;
const PREVIEW_ROW_HEIGHT: f64 = 191.0;
const MIN_PREVIEW_OVERLAY_HEIGHT: f64 = 251.0;
/// Vertical layout (`.switcher.vertical` in `src/style.css`): cards are rows
/// of a list that fills one column top to bottom, then wraps into the next.
/// Height above the first row: 15px top + 14px bottom padding, 2px border,
/// 19px caption, 9px gap, 5px list padding, plus 2px slack.
const VERTICAL_HEADER: f64 = 66.0;
/// Width around the columns: 2 x 18px padding, 2px border, 2 x 2px list
/// padding, plus slack; each column also adds its 13px column gap.
const VERTICAL_CHROME: f64 = 44.0;
/// Space kept free above and below the overlay on the monitor.
const VERTICAL_MARGIN: f64 = 40.0;
const VERTICAL_COLUMN_GAP: f64 = 13.0;
/// Icon rows: 340x46 with a 4px gap.
const VERTICAL_ROW: f64 = 46.0;
const VERTICAL_ROW_GAP: f64 = 4.0;
const VERTICAL_COLUMN_WIDTH: f64 = 340.0;
/// Preview rows: a 176x99 thumbnail beside the label, 404x117 with an 8px gap.
const VERTICAL_PREVIEW_ROW: f64 = 117.0;
const VERTICAL_PREVIEW_ROW_GAP: f64 = 8.0;
const VERTICAL_PREVIEW_COLUMN_WIDTH: f64 = 404.0;

/// Label of the overlay declared in `tauri.conf.json`. In "every monitor" mode
/// it shows on the primary monitor and `switcher-1`, `switcher-2`, ... are
/// created from the same config for the other monitors.
pub(crate) const OVERLAY_LABEL: &str = "switcher";

/// Set while a worker thread creates the extra overlays for other monitors.
static CREATING_OVERLAYS: AtomicBool = AtomicBool::new(false);

/// Overlay label -> HWND, recorded when each overlay is created, so the hook
/// thread can read an overlay's current rect without a round trip to the main
/// thread (Tauri's `hwnd()` getter waits for it).
static OVERLAY_HWNDS: OnceLock<Mutex<HashMap<String, isize>>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) struct WorkArea {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// A monitor's work area in physical pixels plus its own DPI scale, so the
/// overlay can be sized for the monitor it is about to move to rather than the
/// one it is on. Uses Win32 directly because `WebviewWindow::current_monitor()`
/// blocks on the window thread.
#[derive(Clone, Copy)]
pub(crate) struct MonitorArea {
    pub(crate) work: WorkArea,
    pub(crate) scale: f64,
    pub(crate) primary: bool,
}

const MONITORINFOF_PRIMARY: u32 = 1;

fn monitor_area(monitor: HMONITOR) -> Option<MonitorArea> {
    if monitor.0.is_null() {
        return None;
    }
    unsafe {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        let scale = match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
            Ok(()) if dpi_x > 0 => dpi_x as f64 / 96.0,
            _ => 1.0,
        };
        let rect = info.rcWork;
        Some(MonitorArea {
            work: WorkArea {
                left: rect.left,
                top: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            },
            scale,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        })
    }
}

/// The primary monitor, matching where the built-in Alt+Tab switcher appears.
pub(crate) fn primary_monitor_area() -> Option<MonitorArea> {
    // A null HWND intersects no monitor, so DEFAULTTOPRIMARY picks the primary one.
    monitor_area(unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) })
}

pub(crate) fn cursor_monitor_area() -> Option<MonitorArea> {
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point).ok()?;
        monitor_area(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST))
    }
}

/// Every connected monitor, primary first so the overlay from the config
/// keeps the primary monitor and the extra ones cover the others.
pub(crate) fn all_monitor_areas() -> Vec<MonitorArea> {
    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let areas = &mut *(lparam.0 as *mut Vec<MonitorArea>);
        if let Some(area) = monitor_area(monitor) {
            areas.push(area);
        }
        BOOL(1)
    }
    let mut areas = Vec::<MonitorArea>::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(&mut areas as *mut Vec<MonitorArea> as isize),
        );
    }
    areas.sort_by_key(|area| (!area.primary, area.work.left, area.work.top));
    areas
}

/// Monitors the overlay should appear on for the current setting.
fn overlay_monitors() -> Vec<MonitorArea> {
    match current_monitor_mode() {
        MonitorMode::Primary => primary_monitor_area().into_iter().collect(),
        MonitorMode::Cursor => cursor_monitor_area()
            .or_else(primary_monitor_area)
            .into_iter()
            .collect(),
        MonitorMode::Every => {
            let areas = all_monitor_areas();
            if areas.is_empty() {
                primary_monitor_area().into_iter().collect()
            } else {
                areas
            }
        }
    }
}

fn overlay_label(index: usize) -> String {
    if index == 0 {
        OVERLAY_LABEL.to_string()
    } else {
        format!("{OVERLAY_LABEL}-{index}")
    }
}

/// Index of an extra overlay (`switcher-N`), or `None` for any other window.
pub(crate) fn extra_overlay_index(label: &str) -> Option<usize> {
    label
        .strip_prefix(OVERLAY_LABEL)?
        .strip_prefix('-')?
        .parse()
        .ok()
}

pub(crate) fn show_overlay(app: &AppHandle, payload: &SwitcherPayload) {
    let count = payload.items.len();
    let style = CardStyle::parse(&payload.card_style).unwrap_or(CardStyle::Icon);
    let layout = Layout::parse(&payload.layout).unwrap_or(Layout::Horizontal);
    let monitors = overlay_monitors();
    if monitors.is_empty() {
        if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
            let fallback = WorkArea {
                left: 0,
                top: 0,
                width: 1500,
                height: 900,
            };
            let (width, height) = overlay_size(&fallback, 1.0, count, style, layout);
            let _ = window.set_size(Size::Physical(PhysicalSize::new(
                width as u32,
                height as u32,
            )));
            let _ = window.center();
            let _ = window.show();
        }
        return;
    }

    let mut missing = false;
    for (index, area) in monitors.iter().enumerate() {
        match app.get_webview_window(&overlay_label(index)) {
            Some(window) => place_overlay(&window, area, count, style, layout),
            None => missing = true,
        }
    }
    // A monitor may have been unplugged since the extra overlays were made.
    for (label, window) in app.webview_windows() {
        if extra_overlay_index(&label).is_some_and(|index| index >= monitors.len()) {
            let _ = window.hide();
        }
    }
    if missing {
        // A monitor was added (or the setting just changed): create the
        // missing overlays off this thread; they appear once they exist.
        spawn_overlay_windows(app.clone(), monitors.len());
    }
}

/// Physical overlay size for a work area at `scale`. Computing it with the
/// target monitor's scale keeps it right even while the window still sits on
/// a monitor with a different DPI.
fn overlay_size(
    work: &WorkArea,
    scale: f64,
    count: usize,
    style: CardStyle,
    layout: Layout,
) -> (i32, i32) {
    if layout == Layout::Vertical {
        return vertical_overlay_size(work, scale, count, style);
    }
    let work_width = work.width as f64 / scale;
    let work_height = work.height as f64 / scale;
    let (pitch, row_height, min_height) = match style {
        CardStyle::Icon => (CARD_PITCH, CARD_ROW_HEIGHT, MIN_OVERLAY_HEIGHT),
        CardStyle::Preview => (
            PREVIEW_CARD_PITCH,
            PREVIEW_ROW_HEIGHT,
            MIN_PREVIEW_OVERLAY_HEIGHT,
        ),
    };

    // Leave room for the overlay's own margins, then fit as many cards per
    // row as the monitor allows so no card is ever clipped. If the rows are
    // taller than the monitor, the height is capped: icon cards scroll, and
    // preview cards shrink in the frontend until they fit.
    // Never wider than the monitor, even on one narrower than 460 px.
    let max_width = (work_width - 40.0).max(460.0).min(work_width);
    let columns = (((max_width - OVERLAY_CHROME) / pitch).floor() as usize).max(1);
    let rows = count.div_ceil(columns);
    let height = (OVERLAY_HEADER + rows as f64 * row_height)
        .min(work_height)
        .max(min_height);
    let width = (count as f64 * pitch + OVERLAY_CHROME)
        .max(MIN_OVERLAY_WIDTH)
        .min(max_width);
    (
        (width * scale).round() as i32,
        (height * scale).round() as i32,
    )
}

/// Vertical layout: one column as tall as the items need, up to the work
/// area; beyond that the list wraps into balanced extra columns. If even the
/// columns are wider than the monitor, the width is capped: icon rows scroll
/// sideways, and preview rows shrink in the frontend until they fit.
fn vertical_overlay_size(
    work: &WorkArea,
    scale: f64,
    count: usize,
    style: CardStyle,
) -> (i32, i32) {
    let work_width = work.width as f64 / scale;
    let work_height = work.height as f64 / scale;
    let (row, gap, column_width) = match style {
        CardStyle::Icon => (VERTICAL_ROW, VERTICAL_ROW_GAP, VERTICAL_COLUMN_WIDTH),
        CardStyle::Preview => (
            VERTICAL_PREVIEW_ROW,
            VERTICAL_PREVIEW_ROW_GAP,
            VERTICAL_PREVIEW_COLUMN_WIDTH,
        ),
    };
    let count = count.max(1);
    let max_height = (work_height - VERTICAL_MARGIN).max(VERTICAL_HEADER + row);
    let max_width = (work_width - 40.0).max(MIN_OVERLAY_WIDTH).min(work_width);
    let rows_fit = (((max_height - VERTICAL_HEADER + gap) / (row + gap)).floor() as usize).max(1);
    let columns = count.div_ceil(rows_fit);
    let rows = count.div_ceil(columns);
    let height = (VERTICAL_HEADER + rows as f64 * (row + gap) - gap).min(max_height);
    let width = (columns as f64 * (column_width + VERTICAL_COLUMN_GAP) + VERTICAL_CHROME)
        .max(MIN_OVERLAY_WIDTH)
        .min(max_width);
    (
        (width * scale).round() as i32,
        (height * scale).round() as i32,
    )
}

fn place_overlay(
    window: &WebviewWindow,
    area: &MonitorArea,
    count: usize,
    style: CardStyle,
    layout: Layout,
) {
    let (width, height) = overlay_size(&area.work, area.scale, count, style, layout);
    let x = area.work.left + (area.work.width - width) / 2;
    let y = area.work.top + (area.work.height - height) / 2;
    // Each move or resize is a main-thread round trip that also resizes the
    // webview; reopening with the same number of cards needs neither.
    let target = RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    };
    if overlay_rect(window.label()) != Some(target) {
        let position = Position::Physical(PhysicalPosition::new(x, y));
        // Move first so a DPI change on arrival is handled before the final
        // size is applied, then re-pin the position that DPI change may have
        // shifted.
        let _ = window.set_position(position);
        let _ = window.set_size(Size::Physical(PhysicalSize::new(
            width as u32,
            height as u32,
        )));
        let _ = window.set_position(position);
    }
    let _ = window.show();
}

/// The overlay's current window rect, read straight from Win32.
fn overlay_rect(label: &str) -> Option<RECT> {
    let hwnd = *OVERLAY_HWNDS.get()?.lock().ok()?.get(label)?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(HWND(hwnd as *mut c_void), &mut rect) }.ok()?;
    Some(rect)
}

/// Whether `hwnd` is one of the overlays, for the mouse hook. Never waits:
/// `None` when the registry is busy right now (an overlay is being created
/// or destroyed), and the caller then plays it safe.
pub(crate) fn is_overlay_window(hwnd: HWND) -> Option<bool> {
    let Some(hwnds) = OVERLAY_HWNDS.get() else {
        return Some(false);
    };
    let hwnds = hwnds.try_lock().ok()?;
    Some(hwnds.values().any(|&raw| raw == hwnd.0 as isize))
}

/// Creates `switcher-1` .. `switcher-{count-1}` from the overlay's config.
/// Runs on its own thread: building a webview waits on the main thread, which
/// the keyboard hook must never do.
pub(crate) fn spawn_overlay_windows(app: AppHandle, count: usize) {
    if count < 2 || CREATING_OVERLAYS.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = thread::Builder::new()
        .name("switcher-overlays".into())
        .spawn(move || {
            let template = app
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == OVERLAY_LABEL)
                .cloned();
            let mut created = false;
            if let Some(template) = template {
                for index in 1..count {
                    let label = overlay_label(index);
                    if app.get_webview_window(&label).is_some() {
                        continue;
                    }
                    let mut config = template.clone();
                    config.label = label.clone();
                    match WebviewWindowBuilder::from_config(&app, &config)
                        .and_then(|builder| builder.build())
                    {
                        Ok(window) => {
                            prepare_overlay_window(&window);
                            created = true;
                        }
                        Err(error) => log::warn!("unable to create overlay {label}: {error}"),
                    }
                }
            }
            CREATING_OVERLAYS.store(false, Ordering::SeqCst);
            // If the switcher is open right now, put the new overlays on screen.
            if created && current_monitor_mode() == MonitorMode::Every {
                post_command(Command::Reshow);
            }
        });
    if let Err(error) = spawned {
        CREATING_OVERLAYS.store(false, Ordering::SeqCst);
        log::warn!("unable to start the overlay thread: {error}");
    }
}

/// Records a new overlay's HWND and turns off Windows' own open/close
/// animation (a ~200 ms fade and zoom) for it, so it appears and disappears
/// in a single frame. The live previews are DWM thumbnails drawn into this
/// window; without the transition they never sit mid-zoom over the wrong spot.
pub(crate) fn prepare_overlay_window(window: &WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    if let Ok(mut hwnds) = OVERLAY_HWNDS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        hwnds.insert(window.label().to_string(), hwnd.0 as isize);
    }
    let label = window.label().to_string();
    window.on_window_event(move |event| {
        if let WindowEvent::Destroyed = event {
            if let Some(Ok(mut hwnds)) = OVERLAY_HWNDS.get().map(Mutex::lock) {
                hwnds.remove(&label);
            }
        }
    });
    let disabled = BOOL::from(true);
    let result = unsafe {
        DwmSetWindowAttribute(
            HWND(hwnd.0),
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const BOOL as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
    };
    if let Err(error) = result {
        log::warn!("unable to disable the overlay transition: {error}");
    }
}

/// Hides the overlay on every monitor and drops its window previews.
pub(crate) fn hide_overlay(app: &AppHandle) {
    clear_thumbnails();
    for (label, window) in app.webview_windows() {
        if label == OVERLAY_LABEL || extra_overlay_index(&label).is_some() {
            let _ = window.hide();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(width: i32, height: i32) -> WorkArea {
        WorkArea {
            left: 0,
            top: 0,
            width,
            height,
        }
    }

    fn size(width: i32, height: i32, count: usize, style: CardStyle, layout: Layout) -> (i32, i32) {
        overlay_size(&work(width, height), 1.0, count, style, layout)
    }

    use CardStyle::{Icon, Preview};
    use Layout::{Horizontal, Vertical};

    #[test]
    fn horizontal_icon_cards_fit_one_row_with_a_minimum_size() {
        assert_eq!(size(1920, 1080, 5, Icon, Horizontal), (733, 205));
        // One card keeps the caption readable.
        assert_eq!(size(1920, 1080, 1, Icon, Horizontal), (300, 205));
        assert_eq!(size(1920, 1080, 0, Icon, Horizontal), (300, 205));
    }

    #[test]
    fn horizontal_cards_wrap_into_rows_capped_to_the_monitor() {
        // 13 cards per row on 1920 px: 30 cards are 3 rows, full width.
        assert_eq!(size(1920, 1080, 30, Icon, Horizontal), (1880, 495));
        // A short monitor caps the height (the cards scroll).
        assert_eq!(size(1920, 300, 30, Icon, Horizontal), (1880, 300));
    }

    #[test]
    fn horizontal_size_never_exceeds_a_narrow_monitor() {
        // Narrower than the 460 px floor: the monitor width wins.
        assert_eq!(size(400, 600, 5, Icon, Horizontal), (400, 495));
        assert_eq!(size(250, 600, 1, Icon, Horizontal), (250, 205));
    }

    #[test]
    fn horizontal_preview_cards() {
        assert_eq!(size(1920, 1080, 4, Preview, Horizontal), (1080, 251));
        assert_eq!(size(1920, 1080, 20, Preview, Horizontal), (1880, 633));
    }

    #[test]
    fn the_size_is_computed_in_logical_pixels_and_scaled() {
        // 2880x1620 at 150% is 1920x1080 logical; 733x205 rounds up.
        let scaled = overlay_size(&work(2880, 1620), 1.5, 5, Icon, Horizontal);
        assert_eq!(scaled, (1100, 308));
        let vertical = overlay_size(&work(3840, 2160), 2.0, 5, Icon, Vertical);
        assert_eq!(vertical, (794, 624));
    }

    #[test]
    fn vertical_rows_fill_one_column_then_wrap() {
        assert_eq!(size(1920, 1080, 5, Icon, Vertical), (397, 312));
        assert_eq!(size(1920, 1080, 0, Icon, Vertical), (397, 112));
        // 19 rows fit; 40 rows wrap into 3 balanced columns of 14.
        assert_eq!(size(1920, 1080, 40, Icon, Vertical), (1103, 762));
    }

    #[test]
    fn vertical_preview_rows() {
        assert_eq!(size(1920, 1080, 5, Preview, Vertical), (461, 683));
        // 7 rows fit; 10 rows make 2 columns of 5.
        assert_eq!(size(1920, 1080, 10, Preview, Vertical), (878, 683));
    }

    #[test]
    fn vertical_size_is_clamped_to_a_small_monitor() {
        // 40 rows on 400x600: 5 columns would be 1809 px, capped to 360.
        assert_eq!(size(400, 600, 40, Icon, Vertical), (360, 462));
        // Too short for two rows: one row per column, at least one row tall.
        assert_eq!(size(1920, 100, 3, Icon, Vertical), (1103, 112));
    }

    #[test]
    fn extra_overlays_are_numbered_after_the_main_one() {
        assert_eq!(overlay_label(0), "switcher");
        assert_eq!(overlay_label(2), "switcher-2");
        assert_eq!(extra_overlay_index("switcher-2"), Some(2));
        assert_eq!(extra_overlay_index("switcher"), None);
        assert_eq!(extra_overlay_index("settings"), None);
        assert_eq!(extra_overlay_index("switcher-x"), None);
    }
}
