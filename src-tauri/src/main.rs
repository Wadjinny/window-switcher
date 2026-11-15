#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod timing;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    ffi::c_void,
    fs,
    io::Cursor,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        mpsc, Arc, Mutex, MutexGuard, OnceLock,
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
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
                GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, KillTimer,
                MessageBoxW, PeekMessageW, PostThreadMessageW, SendMessageTimeoutW,
                SetForegroundWindow, SetTimer, SetWindowsHookExW, ShowWindowAsync,
                TranslateMessage, UnhookWindowsHookEx, GCLP_HICON, GWL_EXSTYLE, GWL_STYLE,
                GW_OWNER, HHOOK, HICON, ICONINFO, ICON_BIG, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
                MB_ICONERROR, MB_OK, MSG, PM_NOREMOVE, SMTO_ABORTIFHUNG, SW_RESTORE, WH_KEYBOARD_LL,
                WM_APP, WM_GETICON, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
                WS_CHILD, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
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

impl Modifier {
    fn is_down(self, alt_down: bool, win_down: bool) -> bool {
        match self {
            Modifier::Alt => alt_down,
            Modifier::Win => win_down,
        }
    }
}

impl Shortcut {
    fn matches(&self, vk: u32, scan_code: u32, alt_down: bool, win_down: bool) -> bool {
        let modifier_down = self.modifier.is_down(alt_down, win_down);
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
    // `WINDOW_SWITCHER_DEBUG=1` adds debug lines (e.g. hotkey recording).
    let debug = env::var_os("WINDOW_SWITCHER_DEBUG").is_some_and(|value| value != "0");
    let log_plugin = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .level_for(
            "window_switcher",
            if debug {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
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
    // Read the timing flag now rather than on the first key press.
    timing::enabled();
    start_controller();
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

/// Set while the worker thread builds the Settings window, so a second quick
/// click on "Settings…" does not start building another one.
static SETTINGS_OPENING: AtomicBool = AtomicBool::new(false);

/// Settings window size in logical pixels. The height is capped to the work
/// area of the monitor it opens on; the content scrolls when it is shorter.
const SETTINGS_WIDTH: f64 = 460.0;
const SETTINGS_HEIGHT: f64 = 660.0;
const SETTINGS_MIN_WIDTH: f64 = 400.0;
const SETTINGS_MIN_HEIGHT: f64 = 320.0;
/// Room kept for the title bar and some breathing space above and below.
const SETTINGS_VERTICAL_MARGIN: f64 = 72.0;

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
    if SETTINGS_OPENING.swap(true, Ordering::SeqCst) {
        // Already being built by an earlier click.
        return;
    }
    let app = app.clone();
    let spawned = thread::Builder::new()
        .name("switcher-settings".into())
        .spawn(move || {
            // `center()` uses the monitor the window is created on; size it
            // for the same one so it never extends past the work area.
            let height = cursor_monitor_area()
                .or_else(primary_monitor_area)
                .map(|area| area.work.height as f64 / area.scale - SETTINGS_VERTICAL_MARGIN)
                .map_or(SETTINGS_HEIGHT, |available| {
                    SETTINGS_HEIGHT.min(available).max(SETTINGS_MIN_HEIGHT)
                });
            let built = WebviewWindowBuilder::new(
                &app,
                SETTINGS_LABEL,
                WebviewUrl::App("settings.html".into()),
            )
            .title("Window Switcher Settings")
            .inner_size(SETTINGS_WIDTH, height)
            .min_inner_size(SETTINGS_MIN_WIDTH, SETTINGS_MIN_HEIGHT)
            .resizable(true)
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
                    window.on_window_event(|event| match event {
                        // Never leave the hook capturing keys for a window
                        // that is in the background or no longer exists.
                        WindowEvent::Focused(false) => {
                            RECORDING_HOTKEY.store(false, Ordering::SeqCst);
                        }
                        WindowEvent::Destroyed => {
                            RECORDING_HOTKEY.store(false, Ordering::SeqCst);
                            SETTINGS_HWND.store(0, Ordering::SeqCst);
                        }
                        _ => {}
                    });
                    if let Ok(hwnd) = window.hwnd() {
                        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
                        activate_window(HWND(hwnd.0));
                    }
                }
                Err(error) => log::error!("unable to open the settings window: {error}"),
            }
            SETTINGS_OPENING.store(false, Ordering::SeqCst);
        });
    if let Err(error) = spawned {
        SETTINGS_OPENING.store(false, Ordering::SeqCst);
        log::error!("unable to start the settings thread: {error}");
    }
}

fn app_data_path() -> PathBuf {
    let root = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join("Window Switcher")
}

/// Serializes every read-modify-write of `config.json`. Setting commands are
/// async, so two quick clicks can run at the same time; whoever holds this
/// also updates the live setting, so the file and memory never disagree.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Proof that the caller holds `CONFIG_LOCK`.
type ConfigLock = MutexGuard<'static, ()>;

/// Locks a mutex that only orders work (it guards no data of its own), so a
/// panic while it was held leaves nothing half-updated to worry about.
fn lock_unpoisoned<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_config() -> ConfigLock {
    lock_unpoisoned(&CONFIG_LOCK)
}

/// Saves `values`, then makes them live with `apply`, under the config lock:
/// a failed save changes nothing, and two quick changes can never leave
/// memory and the file holding different values.
fn persist(values: &[(&str, String)], apply: impl FnOnce()) -> Result<(), String> {
    let lock = lock_config();
    save_config_values(&lock, values)?;
    apply();
    Ok(())
}

fn config_file() -> PathBuf {
    app_data_path().join("config.json")
}

/// Writes `config.json` through a temporary file and a rename, so a crash or
/// power loss mid-write leaves either the old or the new file, never half of one.
fn write_config_file(_lock: &ConfigLock, root: &serde_json::Value) -> Result<(), String> {
    let directory = app_data_path();
    let file = config_file();
    let temporary = directory.join("config.json.tmp");
    let contents = serde_json::to_string_pretty(root).map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("unable to create {}: {error}", directory.display()))?;
    fs::write(&temporary, contents)
        .map_err(|error| format!("unable to write {}: {error}", temporary.display()))?;
    // On Windows `rename` replaces an existing file (MOVEFILE_REPLACE_EXISTING).
    fs::rename(&temporary, &file).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("unable to replace {}: {error}", file.display())
    })
}

/// Takes each known setting from the file when it has the default's JSON
/// type; a missing or wrong-typed one keeps its default, so one bad value
/// never resets the others. Driven by `AppConfig` itself, so a new field
/// needs no extra code here.
fn config_from_object(object: &serde_json::Map<String, serde_json::Value>) -> AppConfig {
    let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(AppConfig::default())
    else {
        return AppConfig::default();
    };
    for (key, default) in merged.iter_mut() {
        match object.get(key) {
            Some(value) if std::mem::discriminant(value) == std::mem::discriminant(default) => {
                *default = value.clone();
            }
            Some(value) => {
                log::warn!("config.json: {key} has the wrong type ({value}); using {default}")
            }
            None => {}
        }
    }
    serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default()
}

fn load_config() -> AppConfig {
    let lock = lock_config();
    let directory = app_data_path();
    let file = config_file();

    let contents = match fs::read_to_string(&file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let config = AppConfig::default();
            match serde_json::to_value(&config) {
                Ok(root) => {
                    if let Err(error) = write_config_file(&lock, &root) {
                        log::warn!("{error}");
                    }
                }
                Err(error) => log::warn!("unable to serialize the default config: {error}"),
            }
            return config;
        }
        Err(error) => {
            log::error!("unable to read {}: {error}; using defaults", file.display());
            return AppConfig::default();
        }
    };

    match serde_json::from_str::<serde_json::Value>(&contents) {
        Ok(serde_json::Value::Object(object)) => config_from_object(&object),
        parsed => {
            // Never overwrite the file: keep the user's text for inspection and
            // start from defaults so the hotkeys still work. The timestamp
            // keeps an earlier backup from being replaced.
            let reason = match parsed {
                Err(error) => error.to_string(),
                Ok(_) => "the top level is not an object".to_string(),
            };
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            let invalid = directory.join(format!("config.invalid-{stamp}.json"));
            log::error!(
                "{} is not a valid config ({reason}); moving it to {}",
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

/// Writes keys into `config.json`, keeping the rest of the file as the user
/// left it (including keys this version does not know). All values land in
/// one write. A file that exists but cannot be parsed is left untouched and
/// the save fails, rather than replacing the user's text with defaults.
fn save_config_values(lock: &ConfigLock, values: &[(&str, String)]) -> Result<(), String> {
    let file = config_file();
    let mut root = match fs::read_to_string(&file) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) if value.is_object() => value,
            _ => {
                return Err(format!(
                    "{} is not valid JSON; fix or delete it, then try again",
                    file.display()
                ))
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_value(AppConfig::default()).map_err(|error| error.to_string())?
        }
        Err(error) => return Err(format!("unable to read {}: {error}", file.display())),
    };
    if let Some(object) = root.as_object_mut() {
        for (key, value) in values {
            object.insert(key.to_string(), serde_json::Value::String(value.clone()));
        }
    }
    write_config_file(lock, &root)
}

// ---------------------------------------------------------------------------
// Keyboard hook and switcher controller
// ---------------------------------------------------------------------------
//
// While a low-level keyboard hook runs, keyboard input for the whole session
// waits for it, and Windows silently removes a hook that takes longer than
// `LowLevelHooksTimeout`. So the hook (`keyboard_hook`) only makes a pure,
// in-memory decision (`decide_key`): swallow or pass the key, and which
// `Command` to post. Everything slow (enumerating windows, COM, building the
// payload, emitting events, showing and hiding overlays, activating windows)
// runs in order on the controller thread (`run_controller`).
//
// The hook owns the "is a switcher session open" bit (`HOOK_SESSION`), so it
// can decide the next key without waiting for the controller. The controller
// clears that bit (compare-and-swap on the generation) when it closes a
// session on its own: no targets, or the watchdog found the modifier up.

/// Which switcher a session belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwitchMode {
    Window,
    App,
}

impl SwitchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Window => MODE_WINDOW,
            Self::App => MODE_APP,
        }
    }

    fn bits(self) -> u64 {
        match self {
            Self::Window => 1,
            Self::App => 2,
        }
    }
}

/// The open session as the hook sees it: `(generation << 2) | mode bits`, or
/// 0 when no switcher is open. Written by the hook; the controller only ever
/// clears it, and only for the generation it is closing.
static HOOK_SESSION: AtomicU64 = AtomicU64::new(0);

fn hook_session() -> Option<(u64, SwitchMode)> {
    let packed = HOOK_SESSION.load(Ordering::SeqCst);
    let mode = match packed & 3 {
        1 => SwitchMode::Window,
        2 => SwitchMode::App,
        _ => return None,
    };
    Some((packed >> 2, mode))
}

fn set_hook_session(generation: u64, mode: SwitchMode) {
    HOOK_SESSION.store((generation << 2) | mode.bits(), Ordering::SeqCst);
}

/// Closes the hook's session if it is still `generation`. Returns false when
/// the hook already closed or replaced it (its own command is then queued).
fn close_hook_session(generation: u64) -> bool {
    let current = HOOK_SESSION.load(Ordering::SeqCst);
    current != 0
        && current >> 2 == generation
        && HOOK_SESSION
            .compare_exchange(current, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
}

/// Number of app cards Win+Tab would show, kept fresh by the controller. The
/// hook reads it to decide whether to swallow the app shortcut (with fewer
/// than two apps, Win+Tab falls through to Task View). It is a hint: when it
/// is stale the controller replays the shortcut (see `replay_shortcut`).
static APP_TARGETS_HINT: AtomicUsize = AtomicUsize::new(2);

/// What the foreground window is, for the swallow decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Foreground {
    /// No foreground window (e.g. during a desktop switch).
    None,
    /// One of ours (Settings): the switcher works relative to the last app.
    Own,
    Other,
}

/// Everything the hook decision needs, read from the key event and memory.
#[derive(Clone, Copy, Debug)]
struct KeyInput {
    vk: u32,
    scan_code: u32,
    down: bool,
    alt: bool,
    win: bool,
    shift: bool,
    /// The open session's mode, if any.
    session: Option<SwitchMode>,
    /// Settings is in front and one of its hotkey fields is recording.
    recording: bool,
    foreground: Foreground,
    app_targets: usize,
    window_shortcut: Shortcut,
    app_shortcut: Shortcut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    PassThrough,
    Open { mode: SwitchMode, reverse: bool },
    Advance { reverse: bool },
    Commit,
    Cancel,
    RecordKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decision {
    /// The open session's modifier is no longer held (its release was lost,
    /// e.g. to the secure desktop): close it before acting on this key.
    close_stale: bool,
    action: Action,
    /// Swallow the key instead of passing it on.
    swallow: bool,
    /// Inject the mask key (see `send_mask_key`) before returning.
    mask: bool,
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
fn decide_key(input: &KeyInput) -> Decision {
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

/// Work for the controller thread, posted by the hook (and a few others).
enum Command {
    Open {
        mode: SwitchMode,
        reverse: bool,
        generation: u64,
        /// The swallowed key, replayed if nothing opens.
        vk: u32,
        scan_code: u32,
        /// When the hook saw the key, for the latency log.
        at: Instant,
    },
    Advance {
        generation: u64,
        reverse: bool,
        at: Instant,
    },
    Commit {
        generation: u64,
        at: Instant,
    },
    Cancel {
        generation: u64,
        reason: &'static str,
    },
    RecordKey(RecordedKey),
    /// Show the open switcher again (new overlays were created).
    Reshow,
}

/// The raw key captured while recording; named on the controller thread.
#[derive(Clone, Copy)]
struct RecordedKey {
    vk: u32,
    scan_code: u32,
    extended: bool,
    alt: bool,
    win: bool,
    ctrl: bool,
    shift: bool,
}

/// Wakes the controller's message loop; the commands wait in the channel.
const WM_CONTROLLER_WAKE: u32 = WM_APP + 1;
/// Posted by the hook procedure to its own thread: send the mask key right
/// after the hook returns (see `queue_mask_key`).
const WM_HOOK_SEND_MASK: u32 = WM_APP + 2;
/// The hook thread's id, for `WM_HOOK_SEND_MASK`.
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

struct ControllerHandle {
    sender: Mutex<mpsc::Sender<Command>>,
    thread_id: AtomicU32,
}

static CONTROLLER: OnceLock<ControllerHandle> = OnceLock::new();

/// Queues `command` for the controller thread. Never blocks for long: the
/// sender lock is only held for the channel push.
fn post_command(command: Command) {
    let Some(controller) = CONTROLLER.get() else {
        return;
    };
    let sent = lock_unpoisoned(&controller.sender).send(command).is_ok();
    let thread_id = controller.thread_id.load(Ordering::SeqCst);
    if sent && thread_id != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_CONTROLLER_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

/// How often the hook thread considers reinstalling the hook, and how long
/// a hook lives before it is reinstalled at the next quiet moment.
const REHOOK_CHECK_MS: u32 = 5_000;
const REHOOK_AFTER: std::time::Duration = std::time::Duration::from_secs(30);
/// No key event for this long counts as a quiet moment.
const REHOOK_QUIET: std::time::Duration = std::time::Duration::from_secs(1);
/// Set on exit so the hook thread does not reinstall a removed hook.
static HOOK_EXITING: AtomicBool = AtomicBool::new(false);
/// Microseconds since `process_start()` of the last key event the hook saw.
static LAST_KEY_EVENT_US: AtomicU64 = AtomicU64::new(0);

fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn micros_since_start() -> u64 {
    process_start().elapsed().as_micros() as u64
}

fn install_keyboard_hook() {
    process_start();
    let spawned = thread::Builder::new()
        .name("switcher-hook".into())
        .spawn(|| unsafe {
            // This thread only runs the hook, so it is always free to take
            // the next key event; no COM or window work happens here.
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
            HOOK_THREAD_ID.store(GetCurrentThreadId(), Ordering::SeqCst);
            let mut installed_at = Instant::now();
            let rehook_timer = SetTimer(None, 0, REHOOK_CHECK_MS, None);
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                if message.hwnd.0.is_null() && message.message == WM_HOOK_SEND_MASK {
                    send_mask_key();
                    continue;
                }
                if message.hwnd.0.is_null()
                    && message.message == WM_TIMER
                    && message.wParam.0 == rehook_timer
                {
                    if installed_at.elapsed() >= REHOOK_AFTER && hook_is_quiet() {
                        reinstall_keyboard_hook();
                        installed_at = Instant::now();
                    }
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            remove_keyboard_hook();
        });

    if let Err(error) = spawned {
        let message = format!("Window Switcher could not start its hotkey thread.\n\n{error}");
        log::error!("{message}");
        show_error_message(&message);
        std::process::exit(1);
    }
}

/// No switcher open, nothing recording, no modifier held and no key event
/// for a moment: reinstalling the hook now cannot split a key sequence.
fn hook_is_quiet() -> bool {
    let idle_us = micros_since_start().saturating_sub(LAST_KEY_EVENT_US.load(Ordering::SeqCst));
    HOOK_SESSION.load(Ordering::SeqCst) == 0
        && !RECORDING_HOTKEY.load(Ordering::SeqCst)
        && idle_us >= REHOOK_QUIET.as_micros() as u64
        && !alt_key_down()
        && !win_key_down()
        && !unsafe { key_is_down(VK_CONTROL.0 as i32) || key_is_down(VK_SHIFT.0 as i32) }
}

/// Windows removes a low-level hook without notice when one call exceeds
/// `LowLevelHooksTimeout` (a stall elsewhere in the system can cause it, not
/// only a slow hook), and there is no API to ask whether a hook is still
/// installed. So the hook thread periodically installs a fresh hook and then
/// removes the old one. Both calls run on this thread, which also runs the
/// hook procedure, so no key event is handled between them: there is never a
/// moment without a hook, and no event is seen twice.
fn reinstall_keyboard_hook() {
    if HOOK_EXITING.load(Ordering::SeqCst) {
        return;
    }
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) } {
        Ok(hook) => {
            let old = HOOK_HANDLE.swap(hook.0 as usize, Ordering::SeqCst);
            if old != 0 {
                unsafe {
                    let _ = UnhookWindowsHookEx(HHOOK(old as *mut c_void));
                }
            }
            log::debug!("keyboard hook reinstalled");
        }
        Err(error) => log::warn!("unable to reinstall the keyboard hook: {error}"),
    }
}

/// The window the switcher works relative to: the foreground window, or,
/// when one of ours (Settings) is in front, the topmost other app's window,
/// i.e. the one the user came from (our windows are never switch targets).
/// The flag says that window is not focused, so its card is a real move.
fn switch_origin() -> Option<(HWND, bool)> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    if !is_own_window(foreground) {
        return Some((foreground, false));
    }
    eligible_windows()
        .into_iter()
        .next()
        .map(|target| (target.hwnd, true))
}

fn remove_keyboard_hook() {
    HOOK_EXITING.store(true, Ordering::SeqCst);
    let raw = HOOK_HANDLE.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

/// Hook procedure timing (only with `WINDOW_SWITCHER_TIMING=1`), logged and
/// reset by the controller after each switch.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static HOOK_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static HOOK_MAX_NS: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 {
        let started = Instant::now();
        // A panic must never unwind into Windows (that aborts the process):
        // on a panic the key simply passes through.
        let swallow = std::panic::catch_unwind(|| {
            let key = *(data.0 as *const KBDLLHOOKSTRUCT);
            handle_key_event(&key, message.0 as u32, started)
        })
        .unwrap_or_else(|_| {
            log::error!("the keyboard hook panicked; the key was passed through");
            false
        });
        if timing::enabled() {
            let elapsed = started.elapsed().as_nanos() as u64;
            HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
            HOOK_TOTAL_NS.fetch_add(elapsed, Ordering::Relaxed);
            HOOK_MAX_NS.fetch_max(elapsed, Ordering::Relaxed);
        }
        if swallow {
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, message, data)
}

/// Reads the inputs for `decide_key`, applies its decision (session bit,
/// mask key, command) and returns whether to swallow the key. Only atomics,
/// `GetAsyncKeyState`, `GetForegroundWindow` and a channel push: no locks
/// that other threads hold for long, no COM, no window messages.
fn handle_key_event(key: &KBDLLHOOKSTRUCT, message: u32, at: Instant) -> bool {
    if key.dwExtraInfo == MASK_KEY_TAG {
        // Our own mask key or replayed shortcut: never treat it as a hotkey.
        return false;
    }
    LAST_KEY_EVENT_US.store(micros_since_start(), Ordering::SeqCst);
    let down = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return false,
    };
    let alt = key.flags.0 & LLKHF_ALTDOWN.0 != 0 || alt_key_down();
    let win = win_key_down();
    let shift = unsafe {
        key_is_down(VK_SHIFT.0 as i32)
            || key_is_down(VK_LSHIFT.0 as i32)
            || key_is_down(VK_RSHIFT.0 as i32)
    };
    let session = hook_session();
    let foreground = unsafe { GetForegroundWindow() };
    let input = KeyInput {
        vk: key.vkCode,
        scan_code: key.scanCode,
        down,
        alt,
        win,
        shift,
        session: session.map(|(_, mode)| mode),
        recording: RECORDING_HOTKEY.load(Ordering::SeqCst) && settings_is_foreground(),
        foreground: if foreground.0.is_null() {
            Foreground::None
        } else if is_own_window(foreground) {
            Foreground::Own
        } else {
            Foreground::Other
        },
        app_targets: APP_TARGETS_HINT.load(Ordering::SeqCst),
        window_shortcut: window_shortcut(),
        app_shortcut: app_shortcut(),
    };
    let decision = decide_key(&input);

    if decision.close_stale {
        if let Some((generation, _)) = session {
            if close_hook_session(generation) {
                post_command(Command::Cancel {
                    generation,
                    reason: "switcher modifier is no longer held",
                });
            }
        }
    }
    match decision.action {
        Action::PassThrough => {}
        Action::Open { mode, reverse } => {
            let generation = SWITCHER_GENERATION
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            set_hook_session(generation, mode);
            post_command(Command::Open {
                mode,
                reverse,
                generation,
                vk: key.vkCode,
                scan_code: key.scanCode,
                at,
            });
        }
        Action::Advance { reverse } => {
            if let Some((generation, _)) = session {
                post_command(Command::Advance {
                    generation,
                    reverse,
                    at,
                });
            }
        }
        Action::Commit => {
            if let Some((generation, _)) = session {
                if close_hook_session(generation) {
                    post_command(Command::Commit { generation, at });
                }
            }
        }
        Action::Cancel => {
            if let Some((generation, _)) = session {
                if close_hook_session(generation) {
                    post_command(Command::Cancel {
                        generation,
                        reason: "Escape",
                    });
                }
            }
        }
        Action::RecordKey => {
            const LLKHF_EXTENDED: u32 = 0x01;
            post_command(Command::RecordKey(RecordedKey {
                vk: key.vkCode,
                scan_code: key.scanCode,
                extended: key.flags.0 & LLKHF_EXTENDED != 0,
                alt,
                win,
                ctrl: unsafe { key_is_down(VK_CONTROL.0 as i32) },
                shift,
            }));
        }
    }
    // After the command, so the controller starts on it first.
    if decision.mask {
        queue_mask_key();
    }
    decision.swallow
}

/// Sends the mask key from the hook thread as soon as the hook procedure
/// returns. `SendInput` called inside the hook procedure itself takes about
/// 2 ms (the input system is busy waiting for this very hook), which would
/// make every swallowed key that slow. The message is handled within
/// microseconds, while the modifier is still physically held, and injected
/// input is appended to the input queue either way, so the order of events
/// is the same as sending it inline.
fn queue_mask_key() {
    let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
    let posted = thread_id != 0
        && unsafe { PostThreadMessageW(thread_id, WM_HOOK_SEND_MASK, WPARAM(0), LPARAM(0)) }
            .is_ok();
    if !posted {
        send_mask_key();
    }
}

/// `dwExtraInfo` stamped on the mask key so the hook can recognise it.
const MASK_KEY_TAG: usize = 0x5753_4D4B; // "WSMK"
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
fn send_mask_key() {
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
fn replay_shortcut(modifier: Modifier, vk: u32, scan_code: u32) {
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

fn alt_key_down() -> bool {
    unsafe { key_is_down(VK_LMENU.0 as i32) || key_is_down(VK_RMENU.0 as i32) }
}

fn win_key_down() -> bool {
    unsafe { key_is_down(VK_LWIN.0 as i32) || key_is_down(VK_RWIN.0 as i32) }
}

/// Generation and mode of the open switcher, if any.
fn active_switcher(runtime: &HookRuntime) -> Option<(u64, String)> {
    let state = runtime.state.lock().ok()?;
    let active = state.active.as_ref()?;
    Some((active.generation, active.mode.clone()))
}

/// How often the watchdog checks that the switcher's modifier is still held.
const WATCHDOG_INTERVAL_MS: u32 = 200;
/// How often the controller recounts the apps for `APP_TARGETS_HINT`.
const APP_HINT_INTERVAL_MS: u32 = 2_000;

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

fn emit_recorded_hotkey(runtime: &HookRuntime, key: &RecordedKey) {
    log::debug!(
        "recording: vk={:#x} scan={:#x} alt={} win={}",
        key.vk,
        key.scan_code,
        key.alt,
        key.win
    );
    // Same physical-key rule as `Shortcut::matches`.
    let name = if key.vk == VK_TAB.0 as u32 || key.scan_code == 0x0F {
        Some("Tab")
    } else if key.vk == VK_OEM_3.0 as u32 || key.scan_code == 0x29 {
        Some("Backtick")
    } else {
        None
    };
    let recorded = RecordedHotkey {
        alt: key.alt,
        win: key.win,
        ctrl: key.ctrl,
        shift: key.shift,
        escape: key.vk == VK_ESCAPE.0 as u32,
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
fn key_name(key: &RecordedKey) -> String {
    let mut lparam = ((key.scan_code & 0xFF) as i32) << 16;
    if key.extended {
        lparam |= 1 << 24;
    }
    let mut buffer = [0u16; 64];
    let length = unsafe { GetKeyNameTextW(lparam, &mut buffer) };
    if length > 0 {
        String::from_utf16_lossy(&buffer[..length as usize])
    } else {
        format!("key {:#04x}", key.vk)
    }
}

/// Starts the controller thread. Commands posted before it runs wait in the
/// channel. Must be called before the hook is installed.
fn start_controller() {
    let (sender, receiver) = mpsc::channel();
    let _ = CONTROLLER.set(ControllerHandle {
        sender: Mutex::new(sender),
        thread_id: AtomicU32::new(0),
    });
    let spawned = thread::Builder::new()
        .name("switcher-controller".into())
        .spawn(move || run_controller(receiver));
    if let Err(error) = spawned {
        let message = format!("Window Switcher could not start its switcher thread.\n\n{error}");
        log::error!("{message}");
        show_error_message(&message);
        std::process::exit(1);
    }
}

fn run_controller(receiver: mpsc::Receiver<Command>) {
    let Some(runtime) = RUNTIME.get() else {
        log::error!("the switcher controller started without a runtime");
        return;
    };
    let Some(handle) = CONTROLLER.get() else { return };
    unsafe {
        // Window identity (AppUserModelID) and shell names use COM. This
        // thread pumps messages, as a single-threaded apartment should.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // Create the message queue before publishing the thread id, so no
        // wake-up message can be lost; then drain what was already queued.
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, WM_APP, WM_APP, PM_NOREMOVE);
        handle
            .thread_id
            .store(GetCurrentThreadId(), Ordering::SeqCst);
    }
    let mut controller = Controller {
        runtime,
        receiver,
        pending: VecDeque::new(),
        watchdog_timer: 0,
        watchdog_misses: 0,
        app_hint_timer: unsafe { SetTimer(None, 0, APP_HINT_INTERVAL_MS, None) },
        app_keys: HashMap::new(),
    };
    controller.refresh_app_hint();
    controller.drain();
    let mut message = MSG::default();
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.hwnd.0.is_null() {
                if message.message == WM_CONTROLLER_WAKE {
                    controller.drain();
                    continue;
                }
                if message.message == WM_TIMER {
                    controller.on_timer(message.wParam.0);
                    continue;
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Owns the switcher session: everything the hook asks for happens here, one
/// command at a time, in the order the keys were pressed.
struct Controller {
    runtime: &'static HookRuntime,
    receiver: mpsc::Receiver<Command>,
    /// Commands taken from the channel but not handled yet, so an opening can
    /// see that its release is already queued.
    pending: VecDeque<Command>,
    watchdog_timer: usize,
    watchdog_misses: u8,
    app_hint_timer: usize,
    /// HWND -> (process id, app key), so recounting apps only queries new
    /// windows.
    app_keys: HashMap<isize, (u32, String)>,
}

impl Controller {
    fn receive(&mut self) {
        while let Ok(command) = self.receiver.try_recv() {
            self.pending.push_back(command);
        }
    }

    fn drain(&mut self) {
        self.receive();
        while let Some(command) = self.pending.pop_front() {
            // A panic in one command must not take the switcher down with it.
            let handled =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(command)));
            if handled.is_err() {
                log::error!("the switcher controller panicked while handling a command");
            }
            self.receive();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Open {
                mode,
                reverse,
                generation,
                vk,
                scan_code,
                at,
            } => self.open(mode, reverse, generation, (vk, scan_code), at),
            Command::Advance {
                generation,
                reverse,
                at,
            } => {
                let mut watch = timing::Stopwatch::start_at("advance", at);
                timing::mark(&mut watch, "dequeued");
                advance_switcher(self.runtime, generation, reverse, &mut watch);
                timing::finish(watch, "");
            }
            Command::Commit { generation, at } => {
                self.stop_watchdog();
                commit_switcher(self.runtime, generation, at);
                log_hook_timing();
                self.refresh_app_hint();
            }
            Command::Cancel { generation, reason } => {
                self.stop_watchdog();
                if cancel_generation(self.runtime, generation) {
                    log::info!("switcher closed: {reason}");
                }
                log_hook_timing();
            }
            Command::RecordKey(key) => emit_recorded_hotkey(self.runtime, &key),
            Command::Reshow => emit_and_show(self.runtime, &mut None),
        }
    }

    /// Whether a later command already closes or replaces opening
    /// `generation` (`(closes, commits)`).
    fn queued_close(&self, generation: u64) -> (bool, bool) {
        let mut closes = false;
        let mut commits = false;
        for command in &self.pending {
            match command {
                Command::Commit { generation: g, .. } if *g == generation => {
                    closes = true;
                    commits = true;
                }
                Command::Cancel { generation: g, .. } if *g == generation => closes = true,
                Command::Open { generation: g, .. } if *g != generation => closes = true,
                _ => {}
            }
        }
        (closes, commits)
    }

    fn open(
        &mut self,
        mode: SwitchMode,
        reverse: bool,
        generation: u64,
        key: (u32, u32),
        at: Instant,
    ) {
        let mut watch = timing::Stopwatch::start_at("open", at);
        timing::mark(&mut watch, "dequeued");
        self.receive();
        let (closed, committed) = self.queued_close(generation);
        if closed && !committed {
            // Escape or another opening is already queued: nothing to show.
            return;
        }
        let origin = switch_origin();
        let targets = match (mode, origin) {
            (SwitchMode::Window, Some((active, _))) => windows_for_current_app(active),
            (SwitchMode::App, Some((active, _))) => application_targets(active),
            (_, None) => Vec::new(),
        };
        timing::mark(&mut watch, "enumerated");
        let from_own_window = origin.is_some_and(|(_, own)| own);
        if mode == SwitchMode::App {
            APP_TARGETS_HINT.store(targets.len(), Ordering::SeqCst);
        }
        let min_targets = if mode == SwitchMode::Window || from_own_window {
            1
        } else {
            2
        };
        if targets.len() < min_targets {
            log::debug!("nothing to switch to for mode {}", mode.as_str());
            // The hook swallowed the shortcut on a stale guess; hand it to
            // Windows after all (also after a quick tap whose release is
            // queued), unless Escape or another opening already ended it.
            if close_hook_session(generation) || committed {
                replay_shortcut(shortcut_for(mode.as_str()).modifier, key.0, key.1);
            }
            return;
        }

        let worker_targets = (!committed && needs_item_work(&targets)).then(|| targets.clone());
        let needs_work = worker_targets.is_some();
        let card_style = current_card_style();
        let items = targets
            .iter()
            .map(|target| target_to_item_cached(target, card_style))
            .collect::<Vec<_>>();
        let selected = if reverse {
            targets.len() - 1
        } else if from_own_window {
            0
        } else {
            1 % targets.len()
        };
        let hint = shortcut_for(mode.as_str()).hint();
        timing::mark(&mut watch, "items");
        if let Some(watch) = &watch {
            timing::opened(generation, watch.start_instant());
        }
        let count = targets.len();
        let icons_cached = if watch.is_some() {
            targets
                .iter()
                .filter(|target| cache_lookup(icon_cache(), &target_key(target)).is_some())
                .count()
        } else {
            0
        };
        if let Ok(mut state) = self.runtime.state.lock() {
            state.active = Some(ActiveSwitcher {
                mode: mode.as_str().to_string(),
                hint,
                generation,
                card_style,
                layout: current_layout(),
                targets,
                items,
                selected,
            });
        } else {
            return;
        }
        if committed {
            // A quick tap: the release is already queued, so skip drawing an
            // overlay that would be hidden again right away.
            timing::finish(watch, &format!("#{generation} quick tap, not shown"));
            return;
        }
        emit_and_show(self.runtime, &mut watch);
        timing::finish(
            watch,
            &format!(
                "#{generation} mode={} cards={count} icons_cached={icons_cached} worker={needs_work}",
                mode.as_str()
            ),
        );
        if let Some(targets) = worker_targets {
            spawn_item_worker(self.runtime, generation, targets);
        }
        self.start_watchdog();
    }

    /// Starts the watchdog that closes the overlay if its modifier is
    /// released without the hook seeing it (no key event follows a release
    /// lost to the secure desktop). The timer runs on this thread's loop.
    fn start_watchdog(&mut self) {
        self.watchdog_misses = 0;
        if self.watchdog_timer == 0 {
            self.watchdog_timer = unsafe { SetTimer(None, 0, WATCHDOG_INTERVAL_MS, None) };
        }
    }

    fn stop_watchdog(&mut self) {
        if self.watchdog_timer != 0 {
            unsafe {
                let _ = KillTimer(None, self.watchdog_timer);
            }
            self.watchdog_timer = 0;
        }
    }

    fn on_timer(&mut self, id: usize) {
        if id == self.app_hint_timer {
            self.refresh_app_hint();
        } else if id == self.watchdog_timer {
            self.check_watchdog();
        }
    }

    /// Requires two misses in a row so a single odd read never closes it.
    fn check_watchdog(&mut self) {
        let Some((generation, mode)) = active_switcher(self.runtime) else {
            self.stop_watchdog();
            return;
        };
        if shortcut_for(&mode)
            .modifier
            .is_down(alt_key_down(), win_key_down())
        {
            self.watchdog_misses = 0;
            return;
        }
        self.watchdog_misses += 1;
        // Only close a session the hook still has open; otherwise its own
        // commit or cancel is already queued.
        if self.watchdog_misses >= 2 && close_hook_session(generation) {
            log::warn!("switcher modifier was released unseen; closing the overlay");
            self.stop_watchdog();
            cancel_generation(self.runtime, generation);
        }
    }

    /// Recounts the apps Win+Tab would show, for the hook's swallow decision.
    /// Only windows not seen before are queried for their process and app id.
    fn refresh_app_hint(&mut self) {
        if HOOK_SESSION.load(Ordering::SeqCst) != 0 {
            return;
        }
        let mut seen = HashSet::new();
        let mut apps = HashSet::new();
        for hwnd in switchable_windows() {
            let raw = hwnd.0 as isize;
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let cached = self
                .app_keys
                .get(&raw)
                .filter(|(cached_pid, _)| *cached_pid == pid)
                .map(|(_, key)| key.clone());
            let key = match cached {
                Some(key) => Some(key),
                None => process_path_for_window(hwnd).map(|path| {
                    let key = window_key(hwnd, &path);
                    self.app_keys.insert(raw, (pid, key.clone()));
                    key
                }),
            };
            if let Some(key) = key {
                seen.insert(raw);
                apps.insert(key);
            }
        }
        self.app_keys.retain(|hwnd, _| seen.contains(hwnd));
        APP_TARGETS_HINT.store(apps.len(), Ordering::SeqCst);
    }
}

/// Logs how long the hook procedure took since the last switch.
fn log_hook_timing() {
    if !timing::enabled() {
        return;
    }
    let calls = HOOK_CALLS.swap(0, Ordering::Relaxed);
    let total = HOOK_TOTAL_NS.swap(0, Ordering::Relaxed);
    let max = HOOK_MAX_NS.swap(0, Ordering::Relaxed);
    if calls > 0 {
        log::info!(
            "[timing] hook: {calls} events, avg {:.1} us, max {:.1} us",
            total as f64 / calls as f64 / 1000.0,
            max as f64 / 1000.0
        );
    }
}

fn shortcut_for(mode: &str) -> Shortcut {
    if mode == MODE_WINDOW {
        window_shortcut()
    } else {
        app_shortcut()
    }
}

fn advance_switcher(
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

/// Held while the switcher state is checked and the overlays are shown or
/// hidden as a result, so a show from another thread can never land after
/// the controller closed the switcher and hid them.
static OVERLAY_VISIBILITY: Mutex<()> = Mutex::new(());

fn lock_overlay_visibility() -> MutexGuard<'static, ()> {
    lock_unpoisoned(&OVERLAY_VISIBILITY)
}

/// Closes the overlay only if it is still showing `generation`.
fn cancel_generation(runtime: &HookRuntime, generation: u64) -> bool {
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

fn commit_switcher(runtime: &HookRuntime, generation: u64, at: Instant) {
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

/// Brings `hwnd` to the front from a background thread. Never sleeps or
/// waits on the target: a hung app cannot stall the controller.
fn activate_window(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            log::warn!("cannot activate a window that no longer exists");
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            // Asynchronous: `ShowWindow` on another process's window waits
            // for that process to handle the restore.
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        if set_foreground(hwnd) {
            return true;
        }
        // The shell can win a race for the foreground right after a key
        // release. Input we inject counts as user activity for the
        // foreground lock, so send the harmless mask key and try once more.
        send_mask_key();
        if set_foreground(hwnd) {
            log::info!("activated the window on the second attempt");
            return true;
        }
        log::warn!("SetForegroundWindow failed for the selected window");
        false
    }
}

/// The overlay never takes focus, so Windows' foreground lock rejects
/// `SetForegroundWindow` from this background thread. Sharing input state
/// with the current foreground thread lifts that restriction.
unsafe fn set_foreground(hwnd: HWND) -> bool {
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
    raised
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
fn prepare_overlay_window(window: &WebviewWindow) {
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

/// Top-level windows the switcher could offer, in z-order, without the
/// per-window process and COM lookups of `eligible_windows`.
fn switchable_windows() -> Vec<HWND> {
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = &mut *(lparam.0 as *mut Vec<HWND>);
        if is_switchable(hwnd) {
            windows.push(hwnd);
        }
        BOOL(1)
    }
    let mut windows = Vec::<HWND>::new();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut windows as *mut Vec<HWND> as isize));
    }
    windows
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
///
/// Every setting is saved first and only then made live, both under the
/// config lock: a failed save changes nothing, and two quick changes can
/// never leave memory and the file holding different values.
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
    persist(&[("show_switcher_on", mode.as_str().to_string())], || {
        MONITOR_MODE.store(mode.to_u8(), Ordering::SeqCst)
    })?;
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
    Ok(())
}

/// Swaps one hotkey live: the keyboard hook reads it on the next key press,
/// and the overlay hint follows because it is derived when the overlay opens.
/// Choosing the other switcher's shortcut swaps the two, saved in one write.
fn apply_hotkey(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let shortcut = Shortcut::parse(value).ok_or_else(|| {
        format!("unsupported shortcut {value:?}: use Alt or Win with Tab or Backtick")
    })?;
    let (window, apps) = {
        let lock = lock_config();
        let (mut window, mut apps) = (window_shortcut(), app_shortcut());
        let (changed, other) = if key == "window_hotkey" {
            (&mut window, &mut apps)
        } else {
            (&mut apps, &mut window)
        };
        if *other == shortcut {
            *other = *changed;
        }
        *changed = shortcut;
        save_config_values(
            &lock,
            &[
                ("window_hotkey", window.config_value()),
                ("app_hotkey", apps.config_value()),
            ],
        )?;
        store_shortcuts(window, apps);
        (window, apps)
    };
    log::info!(
        "window shortcut: {} | app shortcut: {}",
        window.hint(),
        apps.hint()
    );
    refresh_tray_tooltip(app);
    Ok(())
}

/// Restores both hotkeys to their defaults in one write.
#[tauri::command]
fn reset_hotkeys(app: AppHandle) -> Result<SettingsPayload, String> {
    persist(
        &[
            ("window_hotkey", DEFAULT_WINDOW_SHORTCUT.config_value()),
            ("app_hotkey", DEFAULT_APP_SHORTCUT.config_value()),
        ],
        || store_shortcuts(DEFAULT_WINDOW_SHORTCUT, DEFAULT_APP_SHORTCUT),
    )?;
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
    log::debug!(
        "set_hotkey_recording({active}); settings foreground: {}",
        settings_is_foreground()
    );
    RECORDING_HOTKEY.store(active, Ordering::SeqCst);
}

/// Takes effect the next time the switcher opens.
fn apply_card_style(value: &str) -> Result<(), String> {
    let style = CardStyle::parse(value).ok_or_else(|| format!("unknown card style {value:?}"))?;
    persist(&[("card_style", style.as_str().to_string())], || {
        PREVIEW_CARDS.store(style == CardStyle::Preview, Ordering::SeqCst)
    })?;
    log::info!("card style: {}", style.as_str());
    Ok(())
}

/// Takes effect the next time the switcher opens.
fn apply_layout(value: &str) -> Result<(), String> {
    let layout = Layout::parse(value).ok_or_else(|| format!("unknown layout {value:?}"))?;
    persist(&[("layout", layout.as_str().to_string())], || {
        VERTICAL_LAYOUT.store(layout == Layout::Vertical, Ordering::SeqCst)
    })?;
    log::info!("layout: {}", layout.as_str());
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn session_bits_round_trip_and_close_only_their_generation() {
        set_hook_session(41, SwitchMode::Window);
        assert_eq!(hook_session(), Some((41, SwitchMode::Window)));
        assert!(!close_hook_session(40));
        assert_eq!(hook_session(), Some((41, SwitchMode::Window)));
        assert!(close_hook_session(41));
        assert_eq!(hook_session(), None);
        assert!(!close_hook_session(41));
    }
}
