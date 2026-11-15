#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod timing;

use std::{
    collections::HashMap,
    env,
    ffi::c_void,
    fs,
    io::Cursor,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    window::Color,
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Position, Size, Theme, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Dwm::{
                DwmGetWindowAttribute, DwmQueryThumbnailSourceSize, DwmRegisterThumbnail,
                DwmSetWindowAttribute, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
                DWMWA_CLOAKED, DWMWA_TRANSITIONS_FORCEDISABLED, DWM_THUMBNAIL_PROPERTIES,
                DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY,
                DWM_TNP_VISIBLE,
            },
            Gdi::{
                DeleteObject, EnumDisplayMonitors, GetDIBits, GetMonitorInfoW, GetObjectW,
                MonitorFromPoint, MonitorFromWindow, ReleaseDC, BITMAP, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO,
                MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
            },
        },
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::{
            Com::StructuredStorage::PropVariantToStringAlloc,
            Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED},
            Threading::{
                AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
                QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetKeyNameTextW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD,
                KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_ESCAPE,
                VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_OEM_3, VK_RCONTROL,
                VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_TAB,
            },
            Shell::{
                IShellItem,
                PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
                SHCreateItemFromParsingName, SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON,
                SHGFI_LARGEICON, SIGDN_NORMALDISPLAY,
            },
            WindowsAndMessaging::{
                BringWindowToTop, CallNextHookEx, DispatchMessageW, EnumWindows, GetClassLongPtrW,
                GetCursorPos, GetForegroundWindow, GetIconInfo, GetMessageW, GetWindow,
                GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
                GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MessageBoxW,
                SendMessageTimeoutW, SetForegroundWindow, SetWindowsHookExW, ShowWindow,
                TranslateMessage, UnhookWindowsHookEx, GCLP_HICON, GWL_EXSTYLE, GWL_STYLE,
                GW_OWNER, HHOOK, HICON, ICONINFO, ICON_BIG, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
                MB_ICONERROR, MB_OK, MSG, SMTO_ABORTIFHUNG, SW_RESTORE, WH_KEYBOARD_LL, WM_GETICON,
                WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WS_CHILD, WS_EX_APPWINDOW,
                WS_EX_TOOLWINDOW,
            },
        },
    },
};

const MODE_WINDOW: &str = "window";
const MODE_APP: &str = "app";

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
const OVERLAY_LABEL: &str = "switcher";
const SETTINGS_LABEL: &str = "settings";

/// Where the overlay appears. Stored in `config.json` as `show_switcher_on`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorMode {
    Primary,
    Every,
    Cursor,
}

impl MonitorMode {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" => Some(Self::Primary),
            "every" | "all" => Some(Self::Every),
            "cursor" | "mouse" => Some(Self::Cursor),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Every => "every",
            Self::Cursor => "cursor",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Every,
            2 => Self::Cursor,
            _ => Self::Primary,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Primary => 0,
            Self::Every => 1,
            Self::Cursor => 2,
        }
    }
}

/// How the cards are arranged. Stored in `config.json` as `layout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Cards side by side in rows (the original layout).
    Horizontal,
    /// Cards stacked as a list, wrapping into extra columns when needed.
    Vertical,
}

impl Layout {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "horizontal" | "row" => Some(Self::Horizontal),
            "vertical" | "column" | "list" => Some(Self::Vertical),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// What a switcher card shows. Stored in `config.json` as `card_style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardStyle {
    /// The app icon (the original compact cards).
    Icon,
    /// A live DWM thumbnail of the window, like the built-in Alt+Tab.
    Preview,
}

impl CardStyle {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "icon" => Some(Self::Icon),
            "preview" | "thumbnail" => Some(Self::Preview),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Icon => "icon",
            Self::Preview => "preview",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
struct AppConfig {
    window_hotkey: String,
    app_hotkey: String,
    /// "primary", "every" or "cursor"; anything else falls back to "primary".
    show_switcher_on: String,
    /// "icon" or "preview"; anything else falls back to "icon".
    card_style: String,
    /// "horizontal" or "vertical"; anything else falls back to "horizontal".
    layout: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            // The backtick shortcut also accepts the physical French "²" key:
            // both are scancode 0x29, so the scan-code match covers each layout.
            window_hotkey: "Alt+Backtick".into(),
            app_hotkey: "Win+Tab".into(),
            show_switcher_on: MonitorMode::Primary.as_str().into(),
            card_style: CardStyle::Icon.as_str().into(),
            layout: Layout::Horizontal.as_str().into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    Alt,
    Win,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shortcut {
    modifier: Modifier,
    virtual_key: u32,
    scan_code: u32,
}

impl Shortcut {
    fn matches(&self, vk: u32, scan_code: u32, alt_down: bool, win_down: bool) -> bool {
        let modifier_down = match self.modifier {
            Modifier::Alt => alt_down,
            Modifier::Win => win_down,
        };
        // Matching the physical scan code keeps the shortcut working across
        // layouts: the US backtick and the French `²` key are both scancode 0x29.
        modifier_down
            && (self.virtual_key == vk || (self.scan_code != 0 && self.scan_code == scan_code))
    }

    /// Display label for the overlay, derived from the configured shortcut so
    /// a custom config shows the truth instead of a hardcoded hint.
    fn hint(&self) -> String {
        let modifier = match self.modifier {
            Modifier::Alt => "Alt",
            Modifier::Win => "Win",
        };
        let key = if self.virtual_key == VK_OEM_3.0 as u32 {
            "`"
        } else if self.virtual_key == VK_TAB.0 as u32 {
            "Tab"
        } else {
            "?"
        };
        format!("{modifier} + {key}")
    }
}

/// The built-in shortcuts, also used when `config.json` holds a value the
/// parser rejects. Keep in sync with `AppConfig::default`.
const DEFAULT_WINDOW_SHORTCUT: Shortcut = Shortcut {
    modifier: Modifier::Alt,
    virtual_key: VK_OEM_3.0 as u32,
    scan_code: 0x29,
};
const DEFAULT_APP_SHORTCUT: Shortcut = Shortcut {
    modifier: Modifier::Win,
    virtual_key: VK_TAB.0 as u32,
    scan_code: 0x0F,
};

impl Shortcut {
    /// Parses the `config.json` format: one modifier (`Alt` or `Win`), `+`,
    /// then `Tab` or `Backtick`. Case and spaces are ignored.
    fn parse(value: &str) -> Option<Self> {
        let normalized = value.to_ascii_lowercase().replace(' ', "");
        let (modifier, key) = normalized.split_once('+')?;
        let modifier = match modifier {
            "alt" => Modifier::Alt,
            "win" | "windows" => Modifier::Win,
            _ => return None,
        };
        let (virtual_key, scan_code) = match key {
            "tab" => (VK_TAB.0 as u32, 0x0F),
            "backtick" | "`" | "oem3" | "²" | "oem8" => (VK_OEM_3.0 as u32, 0x29),
            _ => return None,
        };
        Some(Self {
            modifier,
            virtual_key,
            scan_code,
        })
    }

    /// Canonical `config.json` spelling, e.g. "Alt+Backtick" or "Win+Tab".
    fn config_value(&self) -> String {
        let modifier = match self.modifier {
            Modifier::Alt => "Alt",
            Modifier::Win => "Win",
        };
        let key = if self.virtual_key == VK_TAB.0 as u32 {
            "Tab"
        } else {
            "Backtick"
        };
        format!("{modifier}+{key}")
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

/// The live shortcuts. Written at startup and by the Settings window, read
/// by the keyboard hook on every key press.
static WINDOW_SHORTCUT: AtomicU64 = AtomicU64::new(0);
static APP_SHORTCUT: AtomicU64 = AtomicU64::new(0);

fn window_shortcut() -> Shortcut {
    Shortcut::from_bits(WINDOW_SHORTCUT.load(Ordering::SeqCst))
}

fn app_shortcut() -> Shortcut {
    Shortcut::from_bits(APP_SHORTCUT.load(Ordering::SeqCst))
}

fn store_shortcuts(window: Shortcut, app: Shortcut) {
    WINDOW_SHORTCUT.store(window.to_bits(), Ordering::SeqCst);
    APP_SHORTCUT.store(app.to_bits(), Ordering::SeqCst);
}

#[derive(Clone, Serialize)]
struct SwitchItem {
    id: u64,
    title: String,
    subtitle: String,
    icon: Option<String>,
    accent: String,
    window_count: usize,
    /// Preview style only: the overlay shows a live thumbnail of window `id`
    /// in this card. False for minimized windows, which DWM cannot render.
    thumbnail: bool,
}

#[derive(Clone)]
struct Target {
    hwnd: HWND,
    process_path: PathBuf,
    /// Explicit AppUserModelID for packaged apps and browser PWAs, when the
    /// window exposes one. It is what the taskbar groups by.
    app_id: Option<String>,
    title: String,
    /// Number of windows behind this card (always 1 for window mode).
    window_count: usize,
}

// `HWND` is an opaque, process-wide handle value. The switcher only stores it
// and hands it back to Win32 calls, so moving it between our threads is safe.
unsafe impl Send for Target {}

#[derive(Clone, Serialize)]
struct SwitcherPayload {
    mode: String,
    selected: usize,
    items: Vec<SwitchItem>,
    /// Label for the active mode, e.g. "Alt + `" or "Win + Tab".
    hint: String,
    /// "icon" or "preview".
    card_style: String,
    /// "horizontal" or "vertical".
    layout: String,
    /// Echoed back by `update_thumbnails` so late reports from an overlay
    /// that already closed are ignored.
    generation: u64,
    /// Latency logging is on (see `timing.rs`): the overlay reports when it
    /// rendered and painted each opening.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    timing: bool,
    /// Latency logging only: Unix time in ms when the event was sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    sent_at: Option<f64>,
}

#[derive(Clone)]
struct ActiveSwitcher {
    mode: String,
    hint: String,
    /// Identifies this overlay instance; icon workers from an older generation
    /// stop instead of mutating the new state.
    generation: u64,
    /// Card style captured when the overlay opened, so a settings change
    /// never mixes layouts within one overlay.
    card_style: CardStyle,
    /// Layout captured when the overlay opened, for the same reason.
    layout: Layout,
    targets: Vec<Target>,
    items: Vec<SwitchItem>,
    selected: usize,
}

#[derive(Default)]
struct SwitcherState {
    active: Option<ActiveSwitcher>,
}

struct HookRuntime {
    app: AppHandle,
    state: Arc<Mutex<SwitcherState>>,
}

static RUNTIME: OnceLock<HookRuntime> = OnceLock::new();
static HOOK_HANDLE: AtomicUsize = AtomicUsize::new(0);
static SWITCHER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Read on the keyboard hook thread, so it is an atomic rather than a lock.
static MONITOR_MODE: AtomicU8 = AtomicU8::new(0);
/// `true` while cards show window previews instead of app icons.
static PREVIEW_CARDS: AtomicBool = AtomicBool::new(false);
/// `true` while cards are stacked vertically instead of side by side.
static VERTICAL_LAYOUT: AtomicBool = AtomicBool::new(false);
/// Set while a worker thread creates the extra overlays for other monitors.
static CREATING_OVERLAYS: AtomicBool = AtomicBool::new(false);

/// Icon data URIs and shell display names are expensive (COM, GDI, PNG
/// encoding) and identical for every window of an app, so they are cached by
/// AUMID or lowercased executable path. `None` memoises a failed lookup.
type TextCache = Mutex<HashMap<String, Option<String>>>;
static ICON_CACHE: OnceLock<TextCache> = OnceLock::new();
static NAME_CACHE: OnceLock<TextCache> = OnceLock::new();

/// Registered DWM thumbnails: overlay HWND -> (source HWND -> HTHUMBNAIL).
/// Handles are stored as integers because raw HWNDs are not `Send`.
type ThumbnailMap = Mutex<HashMap<isize, HashMap<isize, isize>>>;
static THUMBNAILS: OnceLock<ThumbnailMap> = OnceLock::new();

/// Overlay label -> HWND, recorded when each overlay is created, so the hook
/// thread can read an overlay's current rect without a round trip to the main
/// thread (Tauri's `hwnd()` getter waits for it).
static OVERLAY_HWNDS: OnceLock<Mutex<HashMap<String, isize>>> = OnceLock::new();

fn main() {
    let log_plugin = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::LogDir {
                file_name: Some("window-switcher".to_string()),
            },
        ))
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Stdout,
        ))
        .build();

    let built = tauri::Builder::default()
        // The single-instance guard must come first: a second launch has to
        // exit before setup installs a second hook and tray icon.
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {
            log::warn!("another instance asked to start; keeping the running one");
        }))
        .plugin(log_plugin)
        .invoke_handler(tauri::generate_handler![
            switcher_snapshot,
            get_settings,
            set_setting,
            reset_hotkeys,
            set_hotkey_recording,
            update_thumbnails,
            overlay_timing
        ])
        .setup(|app| setup_app(app))
        .build(tauri::generate_context!());

    let app = match built {
        Ok(app) => app,
        Err(error) => fatal(&format!("Window Switcher could not start: {error}")),
    };

    app.run(|_app, event| {
        if let tauri::RunEvent::Exit = event {
            // Leave nothing behind: remove the global hook before the process goes away.
            remove_keyboard_hook();
        }
    });
}

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Window Switcher {} starting", env!("CARGO_PKG_VERSION"));

    let config = load_config();
    let monitor_mode = MonitorMode::parse(&config.show_switcher_on).unwrap_or_else(|| {
        log::warn!(
            "unknown show_switcher_on value {:?}; using primary",
            config.show_switcher_on
        );
        MonitorMode::Primary
    });
    MONITOR_MODE.store(monitor_mode.to_u8(), Ordering::SeqCst);
    log::info!("switcher shows on: {}", monitor_mode.as_str());
    let card_style = CardStyle::parse(&config.card_style).unwrap_or_else(|| {
        log::warn!(
            "unknown card_style value {:?}; using icon",
            config.card_style
        );
        CardStyle::Icon
    });
    PREVIEW_CARDS.store(card_style == CardStyle::Preview, Ordering::SeqCst);
    log::info!("card style: {}", card_style.as_str());
    let layout = Layout::parse(&config.layout).unwrap_or_else(|| {
        log::warn!("unknown layout value {:?}; using horizontal", config.layout);
        Layout::Horizontal
    });
    VERTICAL_LAYOUT.store(layout == Layout::Vertical, Ordering::SeqCst);
    log::info!("layout: {}", layout.as_str());
    let window_shortcut = parse_shortcut(&config.window_hotkey, DEFAULT_WINDOW_SHORTCUT);
    let mut app_shortcut = parse_shortcut(&config.app_hotkey, DEFAULT_APP_SHORTCUT);
    if window_shortcut == app_shortcut {
        // Pick whichever default the window shortcut is not using.
        app_shortcut = if window_shortcut == DEFAULT_APP_SHORTCUT {
            DEFAULT_WINDOW_SHORTCUT
        } else {
            DEFAULT_APP_SHORTCUT
        };
        log::warn!(
            "window_hotkey and app_hotkey are identical ({}); using {} for apps",
            window_shortcut.hint(),
            app_shortcut.hint()
        );
    }
    log::info!(
        "window shortcut: {} | app shortcut: {}",
        window_shortcut.hint(),
        app_shortcut.hint()
    );
    store_shortcuts(window_shortcut, app_shortcut);

    let runtime = HookRuntime {
        app: app.handle().clone(),
        state: Arc::new(Mutex::new(SwitcherState::default())),
    };
    let _ = RUNTIME.set(runtime);
    install_keyboard_hook();

    if let Err(error) = install_tray(app) {
        log::error!("unable to install the tray icon: {error}");
        return Err(error.into());
    }
    log::info!("tray icon installed");

    if let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) {
        prepare_overlay_window(&overlay);
    }
    // Fill the icon and name caches now, so even the first opening draws
    // every card complete.
    warm_item_caches();
    if monitor_mode == MonitorMode::Every {
        // Create the other monitors' overlays now so the first hotkey press
        // does not wait for their webviews.
        spawn_overlay_windows(app.handle().clone(), all_monitor_areas().len());
    }
    Ok(())
}

/// Surfaces a fatal startup problem to the user, records it, and stops the
/// process. A message box is the only channel available before the webview
/// exists.
fn fatal(message: &str) -> ! {
    log::error!("{message}");
    show_error_message(message);
    std::process::exit(1);
}

fn show_error_message(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Window Switcher".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

const TRAY_ID: &str = "switcher-tray";

fn tray_tooltip() -> String {
    format!(
        "Window Switcher — {} for windows, {} for apps",
        window_shortcut().hint(),
        app_shortcut().hint()
    )
}

fn refresh_tray_tooltip(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(error) = tray.set_tooltip(Some(tray_tooltip())) {
            log::warn!("unable to update the tray tooltip: {error}");
        }
    }
}

fn install_tray(app: &tauri::App) -> tauri::Result<()> {
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Window Switcher", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&settings, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tray_tooltip())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => open_settings(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// Shows the settings window, creating it on first use. Building a webview
/// from the menu event handler (main thread) can deadlock on Windows, so a
/// new window is built from a worker thread.
fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-settings".into())
        .spawn(move || {
            let built = WebviewWindowBuilder::new(
                &app,
                SETTINGS_LABEL,
                WebviewUrl::App("settings.html".into()),
            )
            .title("Window Switcher Settings")
            .inner_size(440.0, 680.0)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .center()
            .theme(Some(Theme::Dark))
            .background_color(Color(36, 36, 36, 255))
            .focused(true)
            .build();
            match built {
                // Windows' foreground lock leaves a window created from this
                // worker thread behind the active app; raise it explicitly.
                Ok(window) => {
                    window.on_window_event(|event| {
                        if let WindowEvent::Destroyed = event {
                            // Never leave the hook capturing keys for a
                            // window that no longer exists.
                            RECORDING_HOTKEY.store(false, Ordering::SeqCst);
                            SETTINGS_HWND.store(0, Ordering::SeqCst);
                        }
                    });
                    if let Ok(hwnd) = window.hwnd() {
                        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
                        activate_window(HWND(hwnd.0));
                    }
                }
                Err(error) => log::error!("unable to open the settings window: {error}"),
            }
        });
    if let Err(error) = spawned {
        log::error!("unable to start the settings thread: {error}");
    }
}

fn app_data_path() -> PathBuf {
    let root = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join("Window Switcher")
}

fn load_config() -> AppConfig {
    let directory = app_data_path();
    let file = directory.join("config.json");

    if !file.exists() {
        let config = AppConfig::default();
        if let Err(error) = fs::create_dir_all(&directory) {
            log::warn!("unable to create {}: {error}", directory.display());
            return config;
        }
        match serde_json::to_string_pretty(&config) {
            Ok(contents) => {
                if let Err(error) = fs::write(&file, contents) {
                    log::warn!("unable to write {}: {error}", file.display());
                }
            }
            Err(error) => log::warn!("unable to serialize the default config: {error}"),
        }
        return config;
    }

    let contents = match fs::read_to_string(&file) {
        Ok(contents) => contents,
        Err(error) => {
            log::error!("unable to read {}: {error}; using defaults", file.display());
            return AppConfig::default();
        }
    };

    match serde_json::from_str(&contents) {
        Ok(config) => config,
        Err(error) => {
            // Never overwrite the file: keep the user's text for inspection and
            // start from defaults so the hotkeys still work.
            let invalid = directory.join("config.invalid.json");
            log::error!(
                "{} is not a valid config ({error}); moving it to {}",
                file.display(),
                invalid.display()
            );
            if let Err(rename_error) = fs::rename(&file, &invalid) {
                log::error!("unable to move the invalid config aside: {rename_error}");
            }
            AppConfig::default()
        }
    }
}

/// Writes one key into `config.json`, keeping the rest of the file as the user
/// left it (including keys this version does not know).
fn save_config_value(key: &str, value: &str) -> Result<(), String> {
    let directory = app_data_path();
    let file = directory.join("config.json");
    let mut root = fs::read_to_string(&file)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| {
            serde_json::to_value(AppConfig::default()).unwrap_or(serde_json::json!({}))
        });
    if let Some(object) = root.as_object_mut() {
        object.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    let contents = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("unable to create {}: {error}", directory.display()))?;
    fs::write(&file, contents)
        .map_err(|error| format!("unable to write {}: {error}", file.display()))
}

fn install_keyboard_hook() {
    let spawned = thread::Builder::new()
        .name("switcher-hook".into())
        .spawn(|| unsafe {
            // The window identity helpers below use shell COM APIs.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                Ok(hook) => hook,
                Err(error) => {
                    let message =
                        format!("Window Switcher could not install its global hotkeys.\n\n{error}");
                    log::error!("{message}");
                    show_error_message(&message);
                    std::process::exit(1);
                }
            };
            log::info!("keyboard hook installed");
            HOOK_HANDLE.store(hook.0 as usize, Ordering::SeqCst);
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let _ = UnhookWindowsHookEx(hook);
            HOOK_HANDLE.store(0, Ordering::SeqCst);
        });

    if let Err(error) = spawned {
        let message = format!("Window Switcher could not start its hotkey thread.\n\n{error}");
        log::error!("{message}");
        show_error_message(&message);
        std::process::exit(1);
    }
}

fn remove_keyboard_hook() {
    let raw = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, message, data);
    }
    let Some(runtime) = RUNTIME.get() else {
        return CallNextHookEx(None, code, message, data);
    };
    let key = *(data.0 as *const KBDLLHOOKSTRUCT);
    if key.dwExtraInfo == MASK_KEY_TAG {
        // Our own mask key (see `send_mask_key`): never treat it as a hotkey.
        return CallNextHookEx(None, code, message, data);
    }
    let vk = key.vkCode;
    let message = message.0 as u32;
    let alt_down = key.flags.0 & LLKHF_ALTDOWN.0 != 0
        || key_is_down(VK_LMENU.0 as i32)
        || key_is_down(VK_RMENU.0 as i32);
    let win_down = key_is_down(VK_LWIN.0 as i32) || key_is_down(VK_RWIN.0 as i32);
    let shift_down = key_is_down(VK_SHIFT.0 as i32)
        || key_is_down(VK_LSHIFT.0 as i32)
        || key_is_down(VK_RSHIFT.0 as i32);

    if RECORDING_HOTKEY.load(Ordering::SeqCst) && settings_is_foreground() {
        // A Settings field is recording a new shortcut: hand it the
        // combination instead of acting on it, so neither the current hotkeys
        // nor Windows' own Win+Tab / Alt+Tab fire while the user types it.
        // Modifiers pass through so the webview and the shell stay in sync.
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) && !is_modifier_key(vk) {
            if alt_down || win_down {
                send_mask_key();
            }
            let ctrl_down = key_is_down(VK_CONTROL.0 as i32);
            emit_recorded_hotkey(runtime, &key, alt_down, win_down, ctrl_down, shift_down);
            return LRESULT(1);
        }
        return CallNextHookEx(None, code, WPARAM(message as usize), data);
    }

    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if vk == VK_ESCAPE.0 as u32 && cancel_switcher(runtime) {
                if alt_down || win_down {
                    send_mask_key();
                }
                return LRESULT(1);
            }
            let already_open = runtime
                .state
                .lock()
                .ok()
                .and_then(|state| state.active.clone());
            if let Some(active) = already_open {
                let shortcut = shortcut_for(&active.mode);
                if shortcut.matches(vk, key.scanCode, alt_down, win_down) {
                    send_mask_key();
                    advance_switcher(runtime, shift_down);
                    return LRESULT(1);
                }
            }
            // Only swallow the key when the overlay actually opened. Otherwise
            // let Windows handle it, so Win+Tab still opens Task View when
            // there is nothing to switch between.
            let opened = if window_shortcut().matches(vk, key.scanCode, alt_down, win_down) {
                open_switcher(runtime, MODE_WINDOW, shift_down)
            } else if app_shortcut().matches(vk, key.scanCode, alt_down, win_down) {
                open_switcher(runtime, MODE_APP, shift_down)
            } else {
                false
            };
            if opened {
                send_mask_key();
                return LRESULT(1);
            }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            let release_alt = vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32;
            let release_win = vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32;
            let should_activate = runtime
                .state
                .lock()
                .ok()
                .and_then(|state| {
                    state.active.as_ref().map(|active| {
                        (active.mode == MODE_WINDOW && release_alt)
                            || (active.mode == MODE_APP && release_win)
                    })
                })
                .unwrap_or(false);
            if should_activate {
                commit_switcher(runtime);
                // Do not swallow the modifier release: Windows would keep the
                // key logically held, which breaks normal typing afterwards.
                // Fall through so the key-up reaches the shell.
            }
        }
        _ => {}
    }
    CallNextHookEx(None, code, WPARAM(message as usize), data)
}

/// `dwExtraInfo` stamped on the mask key so the hook can recognise it.
const MASK_KEY_TAG: usize = 0x5753_4D4B; // "WSMK"
/// Unassigned virtual key (the same one AutoHotkey recommends as its menu
/// mask key): no application or shell shortcut reacts to it.
const MASK_KEY: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

/// The hook swallows the hotkey's second key (Tab, backtick, Escape), so the
/// shell only sees the modifier go down and come back up with nothing in
/// between. A lone Win press/release opens Start, and a lone Alt
/// press/release activates the focused window's menu bar. Injecting a
/// harmless key while the modifier is still held tells Windows a combination
/// was used, so the release does nothing. Must be called while the modifier
/// is down, i.e. from the key-down that is being swallowed.
fn send_mask_key() {
    let event = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: MASK_KEY,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: MASK_KEY_TAG,
            },
        },
    };
    let inputs = [event(KEYBD_EVENT_FLAGS(0)), event(KEYEVENTF_KEYUP)];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        log::warn!(
            "unable to send the mask key ({sent} of {} events)",
            inputs.len()
        );
    }
}

unsafe fn key_is_down(vk: i32) -> bool {
    GetAsyncKeyState(vk) as u16 & 0x8000 != 0
}

/// Set while a hotkey field in the Settings window is recording.
static RECORDING_HOTKEY: AtomicBool = AtomicBool::new(false);
/// HWND of the Settings window (0 when closed). Recording only captures keys
/// while that window is in the foreground.
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

fn settings_is_foreground() -> bool {
    let settings = SETTINGS_HWND.load(Ordering::SeqCst);
    settings != 0 && unsafe { GetForegroundWindow() }.0 as isize == settings
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

/// A key combination captured for the Settings window, which validates it.
#[derive(Clone, Serialize)]
struct RecordedHotkey {
    alt: bool,
    win: bool,
    ctrl: bool,
    shift: bool,
    escape: bool,
    /// The `config.json` key name ("Tab" or "Backtick"), or `None` when the
    /// hotkey format cannot express the pressed key.
    key: Option<String>,
    /// Readable name of the pressed key for error messages, e.g. "Q".
    key_name: String,
}

fn emit_recorded_hotkey(
    runtime: &HookRuntime,
    key: &KBDLLHOOKSTRUCT,
    alt: bool,
    win: bool,
    ctrl: bool,
    shift: bool,
) {
    // Same physical-key rule as `Shortcut::matches`.
    let name = if key.vkCode == VK_TAB.0 as u32 || key.scanCode == 0x0F {
        Some("Tab")
    } else if key.vkCode == VK_OEM_3.0 as u32 || key.scanCode == 0x29 {
        Some("Backtick")
    } else {
        None
    };
    let recorded = RecordedHotkey {
        alt,
        win,
        ctrl,
        shift,
        escape: key.vkCode == VK_ESCAPE.0 as u32,
        key: name.map(str::to_string),
        key_name: key_name(key),
    };
    if let Err(error) = runtime
        .app
        .emit_to(SETTINGS_LABEL, "settings:hotkey-recorded", recorded)
    {
        log::warn!("unable to send the recorded hotkey: {error}");
    }
}

/// The key's name in the current layout ("Q", "F5", "Page Down").
fn key_name(key: &KBDLLHOOKSTRUCT) -> String {
    const LLKHF_EXTENDED: u32 = 0x01;
    let mut lparam = ((key.scanCode & 0xFF) as i32) << 16;
    if key.flags.0 & LLKHF_EXTENDED != 0 {
        lparam |= 1 << 24;
    }
    let mut buffer = [0u16; 64];
    let length = unsafe { GetKeyNameTextW(lparam, &mut buffer) };
    if length > 0 {
        String::from_utf16_lossy(&buffer[..length as usize])
    } else {
        format!("key {:#04x}", key.vkCode)
    }
}

/// Opens the overlay for `mode`. Runs on the low-level keyboard hook thread,
/// so it must stay fast: it only enumerates windows and reuses cached icons,
/// then hands the expensive icon/display-name work to a worker thread.
/// Returns whether the overlay opened, so the caller knows if the hotkey was
/// consumed.
fn open_switcher(runtime: &HookRuntime, mode: &str, reverse: bool) -> bool {
    let mut watch = timing::Stopwatch::start("open");
    let active = unsafe { GetForegroundWindow() };
    if active.0.is_null() {
        return false;
    }
    let targets = match mode {
        MODE_WINDOW => windows_for_current_app(active),
        MODE_APP => application_targets(active),
        _ => Vec::new(),
    };
    timing::mark(&mut watch, "enumerated");
    // The window switcher also opens for a single window, so the hotkey always
    // gives visible feedback; releasing Alt just re-activates that window. The
    // app switcher still needs two apps, otherwise Win+Tab falls through to
    // Task View. With no window at all (desktop, taskbar) both pass through.
    let min_targets = if mode == MODE_WINDOW { 1 } else { 2 };
    if targets.len() < min_targets {
        log::debug!("nothing to switch to for mode {mode}");
        return false;
    }

    // Bump first so any icon worker from a previous overlay drops its work.
    let generation = SWITCHER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let card_style = current_card_style();
    let items = targets
        .iter()
        .map(|target| target_to_item_cached(target, card_style))
        .collect::<Vec<_>>();
    let selected = if reverse {
        targets.len() - 1
    } else {
        1 % targets.len()
    };
    let hint = shortcut_for(mode).hint();
    let needs_work = needs_item_work(&targets);
    timing::mark(&mut watch, "items");
    if let Some(watch) = &watch {
        timing::opened(generation, watch.start_instant());
    }

    match runtime.state.lock() {
        Ok(mut state) => {
            state.active = Some(ActiveSwitcher {
                mode: mode.to_string(),
                hint,
                generation,
                card_style,
                layout: current_layout(),
                targets: targets.clone(),
                items,
                selected,
            });
        }
        Err(_) => return false,
    }

    emit_and_show(runtime, &mut watch);
    if watch.is_some() {
        let detail = format!(
            "#{generation} mode={mode} cards={} icons_cached={} worker={needs_work}",
            targets.len(),
            targets
                .iter()
                .filter(|target| cache_lookup(icon_cache(), &target_key(target)).is_some())
                .count(),
        );
        timing::finish(watch, &detail);
    }
    if needs_work {
        spawn_item_worker(runtime, generation, targets);
    }
    true
}

fn shortcut_for(mode: &str) -> Shortcut {
    if mode == MODE_WINDOW {
        window_shortcut()
    } else {
        app_shortcut()
    }
}

fn advance_switcher(runtime: &HookRuntime, reverse: bool) {
    let mut watch = timing::Stopwatch::start("advance");
    if let Ok(mut state) = runtime.state.lock() {
        if let Some(active) = &mut state.active {
            if reverse {
                active.selected =
                    (active.selected + active.targets.len() - 1) % active.targets.len();
            } else {
                active.selected = (active.selected + 1) % active.targets.len();
            }
        }
    }
    emit_selection(runtime, &mut watch);
    timing::finish(watch, "");
}

#[derive(Clone, Serialize)]
struct SelectionPayload {
    generation: u64,
    selected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent_at: Option<f64>,
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

fn cancel_switcher(runtime: &HookRuntime) -> bool {
    let was_active = runtime
        .state
        .lock()
        .ok()
        .map(|mut state| state.active.take().is_some())
        .unwrap_or(false);
    if was_active {
        let mut watch = timing::Stopwatch::start("cancel");
        hide_overlay(&runtime.app);
        timing::mark(&mut watch, "hidden");
        timing::finish(watch, "");
    }
    was_active
}

fn commit_switcher(runtime: &HookRuntime) {
    let mut watch = timing::Stopwatch::start("release");
    let selection = runtime.state.lock().ok().and_then(|mut state| {
        state
            .active
            .take()
            .map(|active| (active.targets, active.selected))
    });
    hide_overlay(&runtime.app);
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

fn activate_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            log::warn!("cannot activate a window that no longer exists");
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // The overlay never takes focus, so Windows' foreground lock rejects
        // SetForegroundWindow from this background thread. Sharing input state
        // with the current foreground thread lifts that restriction. Retry
        // once: the first call can lose a race with the shell.
        for attempt in 0..2 {
            let foreground = GetForegroundWindow();
            let foreground_thread = GetWindowThreadProcessId(foreground, None);
            let current_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, BOOL(1)).as_bool();
            let _ = BringWindowToTop(hwnd);
            let raised = SetForegroundWindow(hwnd).as_bool();
            if attached {
                let _ = AttachThreadInput(current_thread, foreground_thread, BOOL(0));
            }
            if raised {
                return true;
            }
            if attempt == 0 {
                thread::sleep(Duration::from_millis(10));
            }
        }
        log::warn!("SetForegroundWindow failed for the selected window");
        false
    }
}

fn payload_snapshot(state: &SwitcherState) -> Option<SwitcherPayload> {
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
fn emit_and_show(runtime: &HookRuntime, watch: &mut Option<timing::Stopwatch>) {
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

#[derive(Clone, Copy)]
struct WorkArea {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

/// A monitor's work area in physical pixels plus its own DPI scale, so the
/// overlay can be sized for the monitor it is about to move to rather than the
/// one it is on. Uses Win32 directly because `WebviewWindow::current_monitor()`
/// blocks on the window thread.
#[derive(Clone, Copy)]
struct MonitorArea {
    work: WorkArea,
    scale: f64,
    primary: bool,
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
fn primary_monitor_area() -> Option<MonitorArea> {
    // A null HWND intersects no monitor, so DEFAULTTOPRIMARY picks the primary one.
    monitor_area(unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) })
}

fn cursor_monitor_area() -> Option<MonitorArea> {
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point).ok()?;
        monitor_area(MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST))
    }
}

/// Every connected monitor, primary first so the overlay from the config
/// keeps the primary monitor and the extra ones cover the others.
fn all_monitor_areas() -> Vec<MonitorArea> {
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
            HDC::default(),
            None,
            Some(visit),
            LPARAM(&mut areas as *mut Vec<MonitorArea> as isize),
        );
    }
    areas.sort_by_key(|area| (!area.primary, area.work.left, area.work.top));
    areas
}

fn current_monitor_mode() -> MonitorMode {
    MonitorMode::from_u8(MONITOR_MODE.load(Ordering::SeqCst))
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
fn extra_overlay_index(label: &str) -> Option<usize> {
    label
        .strip_prefix(OVERLAY_LABEL)?
        .strip_prefix('-')?
        .parse()
        .ok()
}

fn show_overlay(app: &AppHandle, payload: &SwitcherPayload) {
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
    let max_width = (work_width - 40.0).max(460.0);
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
    let max_width = (work_width - 40.0).max(MIN_OVERLAY_WIDTH);
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

/// Creates `switcher-1` .. `switcher-{count-1}` from the overlay's config.
/// Runs on its own thread: building a webview waits on the main thread, which
/// the keyboard hook must never do.
fn spawn_overlay_windows(app: AppHandle, count: usize) {
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
                if let Some(runtime) = RUNTIME.get() {
                    emit_and_show(runtime, &mut None);
                }
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
fn prepare_overlay_window(window: &WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else { return };
    if let Ok(mut hwnds) = OVERLAY_HWNDS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        hwnds.insert(window.label().to_string(), hwnd.0 as isize);
    }
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
fn hide_overlay(app: &AppHandle) {
    clear_thumbnails();
    for (label, window) in app.webview_windows() {
        if label == OVERLAY_LABEL || extra_overlay_index(&label).is_some() {
            let _ = window.hide();
        }
    }
}

fn icon_cache() -> &'static TextCache {
    ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn name_cache() -> &'static TextCache {
    NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_lookup(cache: &TextCache, key: &str) -> Option<Option<String>> {
    cache
        .lock()
        .ok()
        .and_then(|entries| entries.get(key).cloned())
}

fn cache_store(cache: &TextCache, key: &str, value: Option<String>) {
    if let Ok(mut entries) = cache.lock() {
        entries.insert(key.to_string(), value);
    }
}

fn needs_item_work(targets: &[Target]) -> bool {
    targets.iter().any(|target| {
        let key = target_key(target);
        if cache_lookup(icon_cache(), &key).is_none() {
            return true;
        }
        target.app_id.is_some() && cache_lookup(name_cache(), &key).is_none()
    })
}

/// Fills the missing icons and display names for one overlay off the hook
/// thread, then re-emits the payload so the frontend can swap them in.
fn spawn_item_worker(runtime: &HookRuntime, generation: u64, targets: Vec<Target>) {
    let state = Arc::clone(&runtime.state);
    let app = runtime.app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-items".into())
        .spawn(move || {
            unsafe {
                // Icon and shell name lookups use GDI and shell COM.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let current = || SWITCHER_GENERATION.load(Ordering::SeqCst) == generation;
            if !fill_item_caches(&targets, current) {
                return;
            }
            if SWITCHER_GENERATION.load(Ordering::SeqCst) != generation {
                return;
            }

            let payload = {
                let Ok(mut guard) = state.lock() else { return };
                let Some(active) = &mut guard.active else {
                    return;
                };
                if active.generation != generation {
                    return;
                }
                let style = active.card_style;
                active.items = active
                    .targets
                    .iter()
                    .map(|target| target_to_item_cached(target, style))
                    .collect();
                payload_snapshot(&guard)
            };
            // Only the items changed: do not resize or re-show the overlay.
            if let Some(payload) = payload {
                let _ = app.emit("switcher:state", &payload);
                timing::since_open(generation, "icon worker emitted", "");
            }
        });
    if let Err(error) = spawned {
        log::warn!("unable to start the icon worker: {error}");
    }
}

/// Looks up the icon and display name of every target that is not cached
/// yet. Stops early, returning false, once `keep_going` says to.
fn fill_item_caches(targets: &[Target], keep_going: impl Fn() -> bool) -> bool {
    for target in targets {
        if !keep_going() {
            return false;
        }
        let key = target_key(target);
        if cache_lookup(icon_cache(), &key).is_none() {
            let icon = window_icon(target.hwnd).or_else(|| icon_for_path(&target.process_path));
            cache_store(icon_cache(), &key, icon);
        }
        if let Some(app_id) = &target.app_id {
            if cache_lookup(name_cache(), &key).is_none() {
                cache_store(name_cache(), &key, app_display_name(app_id));
            }
        }
    }
    true
}

/// Fills the caches for the windows open at startup, off the main thread.
fn warm_item_caches() {
    let spawned = thread::Builder::new()
        .name("switcher-warm".into())
        .spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let started = std::time::Instant::now();
            let targets = eligible_windows();
            fill_item_caches(&targets, || true);
            log::info!(
                "warmed icons for {} windows in {} ms",
                targets.len(),
                started.elapsed().as_millis()
            );
        });
    if let Err(error) = spawned {
        log::warn!("unable to start the cache warm-up: {error}");
    }
}

fn windows_for_current_app(active: HWND) -> Vec<Target> {
    let Some(path) = process_path_for_window(active) else {
        return Vec::new();
    };
    let key = window_key(active, &path);
    eligible_windows()
        .into_iter()
        .filter(|target| target_key(target) == key)
        .collect()
}

fn application_targets(active: HWND) -> Vec<Target> {
    let active_path = process_path_for_window(active);
    let active_key = active_path.as_ref().map(|path| window_key(active, path));
    // Preserve the EnumWindows z-order: the first window seen for an app
    // decides where that app appears, so the order is stable between invocations.
    let mut order = Vec::<String>::new();
    let mut groups = HashMap::<String, Vec<Target>>::new();
    for target in eligible_windows() {
        let key = target_key(&target);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(target);
    }
    let mut apps = order
        .into_iter()
        .filter_map(|key| {
            let mut windows = groups.remove(&key)?;
            windows.sort_by_key(|target| if target.hwnd == active { 0 } else { 1 });
            let count = windows.len();
            let mut first = windows.into_iter().next()?;
            first.window_count = count;
            Some(first)
        })
        .collect::<Vec<_>>();
    if let Some(index) = apps
        .iter()
        .position(|target| Some(target_key(target)) == active_key)
    {
        let active_app = apps.remove(index);
        apps.insert(0, active_app);
    }
    apps
}

/// Identity used to group windows into one switcher card. Packaged apps and
/// browser PWAs expose an AppUserModelID, which is what the taskbar groups by;
/// everything else groups by executable path.
fn target_key(target: &Target) -> String {
    match &target.app_id {
        Some(id) => format!("aumid:{id}"),
        None => format!("path:{}", path_key(&target.process_path)),
    }
}

fn window_key(hwnd: HWND, path: &Path) -> String {
    match app_user_model_id(hwnd) {
        Some(id) => format!("aumid:{id}"),
        None => format!("path:{}", path_key(path)),
    }
}

fn app_user_model_id(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok()?;
        let id = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const c_void));
        id.filter(|value| !value.is_empty())
    }
}

fn app_display_name(app_id: &str) -> Option<String> {
    unsafe {
        let path = format!("shell:AppsFolder\\{app_id}");
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const c_void));
        text.filter(|value| !value.is_empty())
    }
}

/// The window's own icon (PWAs and UWP apps use a different one than their
/// host process). The returned handle belongs to the window, so it is not freed.
/// Only ever called from the item worker, never from the keyboard hook.
fn window_icon(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut result = 0usize;
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            50,
            Some(&mut result),
        );
        let mut icon = result as *mut c_void;
        if icon.is_null() {
            icon = GetClassLongPtrW(hwnd, GCLP_HICON) as *mut c_void;
        }
        if icon.is_null() {
            return None;
        }
        icon_to_data_uri(HICON(icon))
    }
}

fn eligible_windows() -> Vec<Target> {
    #[derive(Default)]
    struct Context {
        windows: Vec<Target>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let context = &mut *(lparam.0 as *mut Context);
        if is_switchable(hwnd) {
            if let Some(process_path) = process_path_for_window(hwnd) {
                context.windows.push(Target {
                    hwnd,
                    process_path,
                    app_id: app_user_model_id(hwnd),
                    title: window_title(hwnd),
                    window_count: 1,
                });
            }
        }
        BOOL(1)
    }
    let mut context = Context::default();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut context as *mut Context as isize));
    }
    context.windows
}

fn is_switchable(hwnd: HWND) -> bool {
    unsafe {
        // Our own overlay must never be offered as a target.
        if !IsWindowVisible(hwnd).as_bool() || is_own_window(hwnd) {
            return false;
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_CHILD.0 != 0 || ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let has_owner = GetWindow(hwnd, GW_OWNER)
            .map(|owner| !owner.0.is_null())
            .unwrap_or(false);
        if has_owner && ex_style & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return false;
        }
        !window_title(hwnd).is_empty()
    }
}

fn is_own_window(hwnd: HWND) -> bool {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..copied as usize])
    }
}

fn process_path_for_window(hwnd: HWND) -> Option<PathBuf> {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let process: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(process);
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])))
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

fn fallback_name(target: &Target) -> String {
    target
        .process_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Application")
        .to_string()
}

/// Builds a card using only cached icons and display names. Called from the
/// keyboard hook, so it must never touch GDI, COM, or the shell.
fn target_to_item_cached(target: &Target, style: CardStyle) -> SwitchItem {
    let key = target_key(target);
    let subtitle = target
        .app_id
        .as_ref()
        .and_then(|_| cache_lookup(name_cache(), &key))
        .flatten()
        .unwrap_or_else(|| fallback_name(target));
    let title = if target.title.is_empty() {
        subtitle.clone()
    } else {
        target.title.clone()
    };
    SwitchItem {
        id: target.hwnd.0 as u64,
        title,
        subtitle: subtitle.clone(),
        icon: cache_lookup(icon_cache(), &key).flatten(),
        accent: accent_for(&subtitle),
        window_count: target.window_count,
        // IsIconic is a cheap user32 read, safe on the hook thread.
        thumbnail: style == CardStyle::Preview && !unsafe { IsIconic(target.hwnd) }.as_bool(),
    }
}

fn accent_for(value: &str) -> String {
    const PALETTE: [&str; 7] = [
        "#2678d9", "#8b54d6", "#d55d77", "#0c9b88", "#c77d19", "#4f7aa9", "#8c5a42",
    ];
    let hash = value.bytes().fold(0usize, |total, byte| {
        total.wrapping_mul(31).wrapping_add(byte as usize)
    });
    PALETTE[hash % PALETTE.len()].to_string()
}

fn icon_for_path(path: &Path) -> Option<String> {
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    unsafe {
        let mut info = SHFILEINFOW::default();
        SHGetFileInfoW(
            PCWSTR(path_wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if info.hIcon.0.is_null() {
            return None;
        }
        let encoded = icon_to_data_uri(info.hIcon);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(info.hIcon);
        encoded
    }
}

unsafe fn icon_to_data_uri(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<String> {
    let mut icon_info = ICONINFO::default();
    GetIconInfo(icon, &mut icon_info).ok()?;
    let color_bitmap = icon_info.hbmColor;
    if color_bitmap.0.is_null() {
        let _ = DeleteObject(icon_info.hbmMask);
        return None;
    }
    let mut bitmap = BITMAP::default();
    if GetObjectW(
        color_bitmap,
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bitmap as *mut _ as *mut c_void),
    ) == 0
    {
        let _ = DeleteObject(color_bitmap);
        let _ = DeleteObject(icon_info.hbmMask);
        return None;
    }
    let width = bitmap.bmWidth;
    let height = bitmap.bmHeight.unsigned_abs() as i32;
    if width <= 0 || height <= 0 {
        let _ = DeleteObject(color_bitmap);
        let _ = DeleteObject(icon_info.hbmMask);
        return None;
    }
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let dc: HDC = windows::Win32::Graphics::Gdi::GetDC(None);
    let copied = GetDIBits(
        dc,
        color_bitmap,
        0,
        height as u32,
        Some(bgra.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    let has_alpha = copied != 0 && bgra.chunks_exact(4).any(|pixel| pixel[3] != 0);

    // Icons without an alpha channel rely on their 1bpp AND mask for
    // transparency. Without it, transparent pixels render as an opaque black box.
    let mask_stride = ((width as usize + 31) / 32) * 4;
    let mut mask_bits: Option<Vec<u8>> = None;
    if copied != 0 && !has_alpha && !icon_info.hbmMask.0.is_null() {
        let mut mask = vec![0u8; mask_stride * height as usize];
        let mut mask_bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 1,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let read = GetDIBits(
            dc,
            icon_info.hbmMask,
            0,
            height as u32,
            Some(mask.as_mut_ptr() as *mut c_void),
            &mut mask_bmi,
            DIB_RGB_COLORS,
        );
        if read != 0 {
            mask_bits = Some(mask);
        }
    }

    let _ = ReleaseDC(None, dc);
    let _ = DeleteObject(color_bitmap);
    let _ = DeleteObject(icon_info.hbmMask);
    if copied == 0 {
        return None;
    }
    for (index, pixel) in bgra.chunks_exact_mut(4).enumerate() {
        pixel.swap(0, 2);
        if has_alpha {
            continue;
        }
        pixel[3] = match &mask_bits {
            Some(mask) => {
                let x = index % width as usize;
                let y = index / width as usize;
                let transparent = mask[y * mask_stride + x / 8] & (0x80 >> (x % 8)) != 0;
                if transparent {
                    0
                } else {
                    255
                }
            }
            None => 255,
        };
    }
    let image = RgbaImage::from_raw(width as u32, height as u32, bgra)?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(format!("data:image/png;base64,{}", BASE64.encode(png)))
}

/// Returns the payload currently on screen, if the overlay is open. The
/// frontend calls this right after registering its listener so a fast hotkey
/// press cannot be missed.
#[tauri::command]
fn switcher_snapshot() -> Option<SwitcherPayload> {
    RUNTIME.get().and_then(|runtime| {
        runtime
            .state
            .lock()
            .ok()
            .and_then(|state| payload_snapshot(&state))
    })
}

/// Every setting the Settings window shows, keyed like `config.json`.
#[derive(Clone, Serialize)]
struct SettingsPayload {
    show_switcher_on: String,
    card_style: String,
    layout: String,
    app_hotkey: String,
    window_hotkey: String,
}

fn current_layout() -> Layout {
    if VERTICAL_LAYOUT.load(Ordering::SeqCst) {
        Layout::Vertical
    } else {
        Layout::Horizontal
    }
}

fn current_card_style() -> CardStyle {
    if PREVIEW_CARDS.load(Ordering::SeqCst) {
        CardStyle::Preview
    } else {
        CardStyle::Icon
    }
}

#[tauri::command]
fn get_settings() -> SettingsPayload {
    SettingsPayload {
        show_switcher_on: current_monitor_mode().as_str().to_string(),
        card_style: current_card_style().as_str().to_string(),
        layout: current_layout().as_str().to_string(),
        app_hotkey: app_shortcut().config_value(),
        window_hotkey: window_shortcut().config_value(),
    }
}

/// Applies one setting immediately and saves it to `config.json`, then
/// returns every setting. Each key validates its own value; a new setting is
/// one more match arm. Async so it runs off the main thread: it may create or
/// destroy webviews.
#[tauri::command]
async fn set_setting(
    app: AppHandle,
    key: String,
    value: String,
) -> Result<SettingsPayload, String> {
    match key.as_str() {
        "show_switcher_on" => apply_monitor_mode(&app, &value)?,
        "card_style" => apply_card_style(&value)?,
        "layout" => apply_layout(&value)?,
        "app_hotkey" | "window_hotkey" => apply_hotkey(&app, &key, &value)?,
        _ => return Err(format!("unknown setting {key:?}")),
    }
    Ok(get_settings())
}

fn apply_monitor_mode(app: &AppHandle, value: &str) -> Result<(), String> {
    let mode =
        MonitorMode::parse(value).ok_or_else(|| format!("unknown monitor mode {value:?}"))?;
    MONITOR_MODE.store(mode.to_u8(), Ordering::SeqCst);
    log::info!("switcher shows on: {}", mode.as_str());

    if mode == MonitorMode::Every {
        spawn_overlay_windows(app.clone(), all_monitor_areas().len());
    } else {
        // The extra overlays are only used in every-monitor mode; free them.
        for (label, window) in app.webview_windows() {
            if extra_overlay_index(&label).is_some() {
                let _ = window.destroy();
            }
        }
    }
    save_config_value("show_switcher_on", mode.as_str())
}

/// Swaps one hotkey live: the keyboard hook reads it on the next key press,
/// and the overlay hint follows because it is derived when the overlay opens.
fn apply_hotkey(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let shortcut = Shortcut::parse(value).ok_or_else(|| {
        format!("unsupported shortcut {value:?}: use Alt or Win with Tab or Backtick")
    })?;
    let (window, apps) = if key == "window_hotkey" {
        (shortcut, app_shortcut())
    } else {
        (window_shortcut(), shortcut)
    };
    if window == apps {
        return Err(format!(
            "{} is already used by the other switcher",
            shortcut.hint()
        ));
    }
    save_config_value(key, &shortcut.config_value())?;
    store_shortcuts(window, apps);
    log::info!(
        "window shortcut: {} | app shortcut: {}",
        window.hint(),
        apps.hint()
    );
    refresh_tray_tooltip(app);
    Ok(())
}

/// Restores both hotkeys to their defaults in one step, so swapped hotkeys
/// never trip the "already used" check halfway through.
#[tauri::command]
fn reset_hotkeys(app: AppHandle) -> Result<SettingsPayload, String> {
    save_config_value("window_hotkey", &DEFAULT_WINDOW_SHORTCUT.config_value())?;
    save_config_value("app_hotkey", &DEFAULT_APP_SHORTCUT.config_value())?;
    store_shortcuts(DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT);
    log::info!("hotkeys reset to defaults");
    refresh_tray_tooltip(&app);
    Ok(get_settings())
}

/// Starts or stops capturing key combinations for the Settings window. While
/// active and the window is in the foreground, the hook swallows every
/// non-modifier key press and sends it, with the modifiers held, as a
/// `settings:hotkey-recorded` event; the hotkeys themselves do not fire.
#[tauri::command]
fn set_hotkey_recording(active: bool) {
    RECORDING_HOTKEY.store(active, Ordering::SeqCst);
}

/// Takes effect the next time the switcher opens.
fn apply_card_style(value: &str) -> Result<(), String> {
    let style = CardStyle::parse(value).ok_or_else(|| format!("unknown card style {value:?}"))?;
    PREVIEW_CARDS.store(style == CardStyle::Preview, Ordering::SeqCst);
    log::info!("card style: {}", style.as_str());
    save_config_value("card_style", style.as_str())
}

/// Takes effect the next time the switcher opens.
fn apply_layout(value: &str) -> Result<(), String> {
    let layout = Layout::parse(value).ok_or_else(|| format!("unknown layout {value:?}"))?;
    VERTICAL_LAYOUT.store(layout == Layout::Vertical, Ordering::SeqCst);
    log::info!("layout: {}", layout.as_str());
    save_config_value("layout", layout.as_str())
}

// ---------------------------------------------------------------------------
// Window previews
// ---------------------------------------------------------------------------
//
// DWM composes a live thumbnail of each source window straight onto the
// overlay window, above the webview's content. The webview lays the cards out
// and reports where each preview slot is; the selection highlight is the card
// border around the slot, so nothing HTML ever has to draw over a thumbnail.

/// A preview slot in the overlay, in CSS pixels relative to the viewport.
#[derive(Deserialize)]
struct ThumbnailSlot {
    id: u64,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn thumbnails() -> &'static ThumbnailMap {
    THUMBNAILS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unregisters every preview on every overlay.
fn clear_thumbnails() {
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

/// Called by an overlay webview after layout: places one DWM thumbnail per
/// slot on that overlay window and removes the ones it no longer shows.
#[tauri::command]
fn update_thumbnails(
    window: WebviewWindow,
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
                    .map(|item| item.id)
                    .collect::<Vec<_>>(),
            )
        });
    let Some(allowed) = allowed else {
        // A late report from an overlay that already closed or reopened.
        return Ok(());
    };

    let sources = map.entry(destination.0 as isize).or_default();
    let mut wanted = Vec::with_capacity(slots.len());
    for slot in &slots {
        // Only windows on the current cards: the webview cannot ask for a
        // preview of an arbitrary window.
        if !allowed.contains(&slot.id) || wanted.contains(&(slot.id as isize)) {
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
    timing::since_open(
        generation,
        "thumbnails placed",
        &format!("{} on {label}", wanted.len()),
    );
    Ok(())
}

/// Latency logging only: the overlay reports when it rendered or painted an
/// opening (`webview_ms` is measured from the event's arrival in the webview),
/// or, for stage "delivered", how long an event took to reach it.
#[tauri::command]
fn overlay_timing(window: WebviewWindow, generation: u64, stage: String, webview_ms: f64) {
    if stage.starts_with("delivered") && stage.len() <= 32 && timing::enabled() {
        log::info!(
            "[timing] {stage} #{generation} in {webview_ms:.2} ms to {}",
            window.label()
        );
        return;
    }
    if stage.len() <= 32 && webview_ms.is_finite() {
        timing::since_open(
            generation,
            &stage,
            &format!("({webview_ms:.2} ms after arrival, {})", window.label()),
        );
    }
}

/// The slot in physical client pixels, shrunk to the source window's aspect
/// ratio and centred, since DWM stretches a thumbnail to whatever it gets.
fn fit_thumbnail(thumbnail: isize, slot: &ThumbnailSlot, scale: f64) -> RECT {
    let width = (slot.width * scale).max(1.0);
    let height = (slot.height * scale).max(1.0);
    let (mut fit_width, mut fit_height) = (width, height);
    if let Ok(size) = unsafe { DwmQueryThumbnailSourceSize(thumbnail) } {
        if size.cx > 0 && size.cy > 0 {
            let ratio = (width / size.cx as f64).min(height / size.cy as f64);
            fit_width = size.cx as f64 * ratio;
            fit_height = size.cy as f64 * ratio;
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
