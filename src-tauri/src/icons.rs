//! App icons and display names: extracting them (GDI, shell COM, PNG), the
//! caches that keep them, and the worker threads that fill those caches.

use std::{
    collections::HashMap,
    ffi::c_void,
    io::Cursor,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex, OnceLock},
    thread,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{ImageFormat, RgbaImage};
use tauri::Emitter;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        Graphics::Gdi::{
            DeleteObject, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
            BI_RGB, DIB_RGB_COLORS, HDC,
        },
        System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
        UI::{
            Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON},
            WindowsAndMessaging::{
                GetClassLongPtrW, GetIconInfo, SendMessageTimeoutW, GCLP_HICON, HICON, ICONINFO,
                ICON_BIG, SMTO_ABORTIFHUNG, WM_GETICON,
            },
        },
    },
};

use crate::{
    switcher::{
        items::target_to_item_cached, session::payload_snapshot, HookRuntime, SWITCHER_GENERATION,
    },
    targets::{
        enumerate::eligible_windows,
        identity::{app_display_name, target_key},
        Target,
    },
    timing,
};

/// Icon data URIs and shell display names are expensive (COM, GDI, PNG
/// encoding) and identical for every window of an app, so they are cached by
/// AUMID or lowercased executable path. `None` memoises a failed lookup.
type TextCache = Mutex<HashMap<String, Option<String>>>;
static ICON_CACHE: OnceLock<TextCache> = OnceLock::new();
static NAME_CACHE: OnceLock<TextCache> = OnceLock::new();

pub(crate) fn icon_cache() -> &'static TextCache {
    ICON_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn name_cache() -> &'static TextCache {
    NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cache_lookup(cache: &TextCache, key: &str) -> Option<Option<String>> {
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

pub(crate) fn needs_item_work(targets: &[Target]) -> bool {
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
pub(crate) fn spawn_item_worker(runtime: &HookRuntime, generation: u64, targets: Vec<Target>) {
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
pub(crate) fn warm_item_caches() {
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
        let _ = DeleteObject(icon_info.hbmMask.into());
        return None;
    }
    let mut bitmap = BITMAP::default();
    if GetObjectW(
        color_bitmap.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bitmap as *mut _ as *mut c_void),
    ) == 0
    {
        let _ = DeleteObject(color_bitmap.into());
        let _ = DeleteObject(icon_info.hbmMask.into());
        return None;
    }
    let width = bitmap.bmWidth;
    let height = bitmap.bmHeight.unsigned_abs() as i32;
    if width <= 0 || height <= 0 {
        let _ = DeleteObject(color_bitmap.into());
        let _ = DeleteObject(icon_info.hbmMask.into());
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
    let has_alpha = copied != 0 && bgra.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0);

    // Icons without an alpha channel rely on their 1bpp AND mask for
    // transparency. Without it, transparent pixels render as an opaque black box.
    let mask_stride = (width as usize).div_ceil(32) * 4;
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
    let _ = DeleteObject(color_bitmap.into());
    let _ = DeleteObject(icon_info.hbmMask.into());
    if copied == 0 {
        return None;
    }
    for (index, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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
