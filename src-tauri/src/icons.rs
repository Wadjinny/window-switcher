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
use image::{imageops, ImageFormat, RgbaImage};
use tauri::Emitter;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, SIZE, WPARAM},
        Graphics::Gdi::{
            DeleteObject, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
            BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
        },
        System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
        UI::{
            HiDpi::GetDpiForSystem,
            Shell::{
                IShellItemImageFactory, SHCreateItemFromParsingName, SHGetFileInfoW, SHFILEINFOW,
                SHGFI_ICON, SHGFI_LARGEICON, SIIGBF_ICONONLY,
            },
            WindowsAndMessaging::{
                GetClassLongPtrW, GetIconInfo, SendMessageTimeoutW, GCLP_HICON, HICON, ICONINFO,
                ICON_BIG, SMTO_ABORTIFHUNG, WM_GETICON,
            },
        },
    },
};

use crate::{
    colors::{dominant_colors, AppColors},
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
/// Theme colors taken from each icon, filled together with `ICON_CACHE`.
type ColorCache = Mutex<HashMap<String, Option<AppColors>>>;
static COLOR_CACHE: OnceLock<ColorCache> = OnceLock::new();

pub(crate) fn app_colors(key: &str) -> Option<AppColors> {
    COLOR_CACHE
        .get()
        .and_then(|cache| cache.lock().ok())
        .and_then(|entries| entries.get(key).cloned())
        .flatten()
}

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
            let image = item_icon(target);
            let colors = image.as_ref().and_then(dominant_colors);
            if let Ok(mut entries) = COLOR_CACHE
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
            {
                entries.insert(key.clone(), colors);
            }
            cache_store(icon_cache(), &key, image.as_ref().and_then(png_data_uri));
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

/// The card icon of `target`, rendered by the shell at
/// `icon_size()` pixels where possible so it stays sharp when drawn large.
/// Only ever called from the item workers, never from the keyboard hook.
fn item_icon(target: &Target) -> Option<RgbaImage> {
    let size = icon_size();
    // Packaged apps and PWAs: the Start menu entry has the app's real icon,
    // which their windows (often hosted by another process) may not.
    if let Some(app_id) = &target.app_id {
        if let Some(image) = shell_image(&format!("shell:AppsFolder\\{app_id}"), size) {
            return Some(image);
        }
    }
    let window = window_icon(target.hwnd);
    let program = icon_for_path(&target.process_path);
    // A window may show another icon than its program (script hosts such as
    // python.exe or mmc.exe): only then is the window's own small icon kept.
    let shows_program_icon = match (&window, &program) {
        (None, _) => true,
        (Some(window), Some(program)) => looks_alike(window, program),
        (Some(_), None) => false,
    };
    if shows_program_icon {
        let path = target.process_path.to_string_lossy();
        if let Some(image) = shell_image(&path, size) {
            return Some(image);
        }
    }
    window.or(program)
}

/// Icon size to render: 48 logical pixels at the system DPI, enough for the
/// largest place an icon is drawn (the preview cards' corner icon).
fn icon_size() -> u32 {
    let dpi = unsafe { GetDpiForSystem() }.max(96);
    (48 * dpi / 96).clamp(48, 128)
}

/// True when two icons show the same picture, allowing for a different size.
fn looks_alike(a: &RgbaImage, b: &RgbaImage) -> bool {
    const SIDE: u32 = 16;
    let a = imageops::resize(a, SIDE, SIDE, imageops::FilterType::Triangle);
    let b = imageops::resize(b, SIDE, SIDE, imageops::FilterType::Triangle);
    let total: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(x, y)| x.abs_diff(*y) as u64)
        .sum();
    total / (SIDE * SIDE * 4) as u64 <= 24
}

/// The icon the shell shows for `parsing_name` (a path, or a
/// `shell:AppsFolder\<AUMID>` entry), rendered at up to `size` pixels.
fn shell_image(parsing_name: &str, size: u32) -> Option<RgbaImage> {
    let wide: Vec<u16> = parsing_name.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let requested = SIZE {
            cx: size as i32,
            cy: size as i32,
        };
        let bitmap = factory.GetImage(requested, SIIGBF_ICONONLY).ok()?;
        let image = bitmap_to_rgba(bitmap);
        let _ = DeleteObject(bitmap.into());
        let image = image?;
        if image.width() > size || image.height() > size {
            return Some(imageops::resize(
                &image,
                size,
                size,
                imageops::FilterType::Lanczos3,
            ));
        }
        Some(image)
    }
}

/// A 32bpp shell bitmap as straight-alpha RGBA.
unsafe fn bitmap_to_rgba(bitmap: HBITMAP) -> Option<RgbaImage> {
    let mut info = BITMAP::default();
    if GetObjectW(
        bitmap.into(),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut info as *mut _ as *mut c_void),
    ) == 0
    {
        return None;
    }
    let width = info.bmWidth;
    let height = info.bmHeight.unsigned_abs() as i32;
    if width <= 0 || height <= 0 || width > 1024 || height > 1024 {
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
    let dc = windows::Win32::Graphics::Gdi::GetDC(None);
    let copied = GetDIBits(
        dc,
        bitmap,
        0,
        height as u32,
        Some(bgra.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    let _ = ReleaseDC(None, dc);
    if copied == 0 {
        return None;
    }
    unpremultiply_bgra(&mut bgra);
    RgbaImage::from_raw(width as u32, height as u32, bgra)
}

/// Turns the shell's BGRA pixels into straight-alpha RGBA. The shell hands
/// out premultiplied pixels (no channel above its alpha); a bitmap that
/// breaks that rule is taken as straight alpha, and one with no alpha at all
/// as opaque.
fn unpremultiply_bgra(bgra: &mut [u8]) {
    let pixels = bgra.as_chunks_mut::<4>().0;
    let has_alpha = pixels.iter().any(|pixel| pixel[3] != 0);
    let premultiplied = has_alpha
        && pixels
            .iter()
            .all(|pixel| pixel[0].max(pixel[1]).max(pixel[2]) <= pixel[3]);
    for pixel in pixels {
        pixel.swap(0, 2);
        if !has_alpha {
            pixel[3] = 255;
        } else if premultiplied && pixel[3] != 0 && pixel[3] != 255 {
            let alpha = pixel[3] as u32;
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

/// The window's own icon (PWAs and UWP apps use a different one than their
/// host process). The returned handle belongs to the window, so it is not freed.
/// Only ever called from the item worker, never from the keyboard hook.
fn window_icon(hwnd: HWND) -> Option<RgbaImage> {
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
        icon_to_rgba(HICON(icon))
    }
}

fn icon_for_path(path: &Path) -> Option<RgbaImage> {
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
        let encoded = icon_to_rgba(info.hIcon);
        let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(info.hIcon);
        encoded
    }
}

unsafe fn icon_to_rgba(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<RgbaImage> {
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
    RgbaImage::from_raw(width as u32, height as u32, bgra)
}

fn png_data_uri(image: &RgbaImage) -> Option<String> {
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(format!("data:image/png;base64,{}", BASE64.encode(png)))
}

#[cfg(test)]
mod shell_pixel_tests {
    use super::*;

    #[test]
    fn premultiplied_pixels_are_unpremultiplied_and_swapped() {
        // BGRA: blue 0, green 0, red 64 at alpha 128 -> RGBA red 128.
        let mut pixels = vec![0, 0, 64, 128, 10, 20, 30, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![128, 0, 0, 128, 30, 20, 10, 255]);
    }

    #[test]
    fn straight_alpha_pixels_are_kept() {
        let mut pixels = vec![0, 0, 200, 100];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![200, 0, 0, 100]);
    }

    #[test]
    fn a_bitmap_without_alpha_is_opaque() {
        let mut pixels = vec![1, 2, 3, 0];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, vec![3, 2, 1, 255]);
    }

    #[test]
    fn icons_of_different_sizes_can_look_alike() {
        let small = RgbaImage::from_pixel(32, 32, image::Rgba([200, 30, 30, 255]));
        let large = RgbaImage::from_pixel(48, 48, image::Rgba([200, 30, 30, 255]));
        let other = RgbaImage::from_pixel(32, 32, image::Rgba([30, 30, 200, 255]));
        assert!(looks_alike(&small, &large));
        assert!(!looks_alike(&small, &other));
    }
}
