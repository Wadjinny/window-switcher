//! App icons drawn over the corner of each live preview.
//!
//! DWM composes window previews above the webview's content (see
//! `thumbnails.rs`), so an HTML icon cannot overlap a preview. Each overlay
//! therefore gets a small owned popup above it: layered, click-through and
//! never activated, it only holds the icons, drawn with `UpdateLayeredWindow`.
//! The webview still draws the same icon underneath, so a card looks right
//! before this layer catches up and on cards without a preview.
//!
//! Every function here runs on the main thread, which owns these windows.

use std::{
    cell::RefCell,
    collections::{hash_map::DefaultHasher, HashMap},
    ffi::c_void,
    hash::{Hash, Hasher},
    rc::Rc,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{imageops, RgbaImage};
use windows::{
    core::{w, BOOL},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::{
            Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED},
            Gdi::{
                ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
                GetDC, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
                BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
            },
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, IsWindow, RegisterClassW, ShowWindow,
            UpdateLayeredWindow, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
};

use crate::ipc::SlotRect;

/// One icon to draw: where (CSS pixels in the overlay's client area) and
/// which image (the card's PNG data URI).
pub(crate) struct CornerIcon {
    pub(crate) rect: SlotRect,
    pub(crate) icon: String,
}

// Matches `.corner-icon img` in style.css: drop-shadow(0 2px 3px rgba(0,0,0,.4)).
const SHADOW_OFFSET: f64 = 2.0;
const SHADOW_BLUR: f64 = 1.5;
const SHADOW_ALPHA: f32 = 0.4;
const MAX_DECODED: usize = 64;

thread_local! {
    /// Overlay HWND -> its icon layer HWND.
    static LAYERS: RefCell<HashMap<isize, isize>> = RefCell::new(HashMap::new());
    /// Decoded icons by a hash of their data URI.
    static DECODED: RefCell<HashMap<u64, Option<Rc<RgbaImage>>>> = RefCell::new(HashMap::new());
    static CLASS_REGISTERED: RefCell<bool> = const { RefCell::new(false) };
}

/// Draws `icons` above `overlay`, replacing what its layer showed before.
/// `scale` is the overlay's device pixel ratio.
pub(crate) fn show_icons(overlay: HWND, scale: f64, icons: &[CornerIcon]) {
    let drawn = draw(overlay, scale, icons);
    if drawn.is_none() {
        hide_layer(overlay);
    }
}

/// Hides the icons on every overlay.
pub(crate) fn hide_all() {
    LAYERS.with(|layers| {
        layers.borrow_mut().retain(|_, layer| {
            let hwnd = HWND(*layer as *mut c_void);
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    return false;
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            true
        });
    });
}

fn hide_layer(overlay: HWND) {
    let layer = LAYERS.with(|layers| layers.borrow().get(&(overlay.0 as isize)).copied());
    if let Some(layer) = layer {
        unsafe {
            let _ = ShowWindow(HWND(layer as *mut c_void), SW_HIDE);
        }
    }
}

/// A canvas in physical screen pixels with premultiplied BGRA rows.
struct Canvas {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

/// Physical screen rectangle (left, top, width, height) of a CSS rect.
fn physical(origin: POINT, scale: f64, rect: &SlotRect) -> (i32, i32, i32, i32) {
    let left = origin.x + (rect.x * scale).round() as i32;
    let top = origin.y + (rect.y * scale).round() as i32;
    let right = origin.x + ((rect.x + rect.width) * scale).round() as i32;
    let bottom = origin.y + ((rect.y + rect.height) * scale).round() as i32;
    (left, top, (right - left).max(0), (bottom - top).max(0))
}

fn draw(overlay: HWND, scale: f64, icons: &[CornerIcon]) -> Option<()> {
    let mut origin = POINT::default();
    if !unsafe { ClientToScreen(overlay, &mut origin) }.as_bool() {
        return None;
    }

    // Each icon, fitted (object-fit: contain) and placed in screen pixels.
    let mut placed = Vec::new();
    for icon in icons {
        let (left, top, width, height) = physical(origin, scale, &icon.rect);
        if width <= 0 || height <= 0 || width > 1024 || height > 1024 {
            continue;
        }
        let Some(image) = decoded(&icon.icon) else {
            continue;
        };
        let ratio =
            (width as f64 / image.width() as f64).min(height as f64 / image.height() as f64);
        let fit_w = ((image.width() as f64 * ratio).round() as u32).max(1);
        let fit_h = ((image.height() as f64 * ratio).round() as u32).max(1);
        let resized = if (fit_w, fit_h) == image.dimensions() {
            (*image).clone()
        } else {
            imageops::resize(&*image, fit_w, fit_h, imageops::FilterType::CatmullRom)
        };
        let x = left + (width - fit_w as i32) / 2;
        let y = top + (height - fit_h as i32) / 2;
        placed.push((x, y, resized));
    }
    if placed.is_empty() {
        return None;
    }

    // Room for the shadow around every icon.
    let blur = (SHADOW_BLUR * scale) as f32;
    let offset = (SHADOW_OFFSET * scale).round() as i32;
    let margin = (blur * 3.0).ceil() as i32 + offset;
    let left = placed.iter().map(|(x, _, _)| *x).min()? - margin;
    let top = placed.iter().map(|(_, y, _)| *y).min()? - margin;
    let right = placed
        .iter()
        .map(|(x, _, image)| x + image.width() as i32)
        .max()?
        + margin;
    let bottom = placed
        .iter()
        .map(|(_, y, image)| y + image.height() as i32)
        .max()?
        + margin;
    let mut canvas = Canvas {
        left,
        top,
        width: right - left,
        height: bottom - top,
        pixels: vec![0; ((right - left) * (bottom - top) * 4) as usize],
    };

    for (x, y, image) in &placed {
        let shadow = shadow_of(image, margin as u32, blur);
        composite(&mut canvas, &shadow, x - margin, y - margin + offset);
        composite(&mut canvas, image, *x, *y);
    }

    let layer = layer_for(overlay)?;
    present(layer, &canvas)
}

/// A soft black copy of `image`'s silhouette, padded by `margin` on each side.
fn shadow_of(image: &RgbaImage, margin: u32, blur: f32) -> RgbaImage {
    let mut shadow = RgbaImage::new(image.width() + margin * 2, image.height() + margin * 2);
    for (x, y, pixel) in image.enumerate_pixels() {
        let alpha = (pixel[3] as f32 * SHADOW_ALPHA).round() as u8;
        shadow.put_pixel(x + margin, y + margin, image::Rgba([0, 0, 0, alpha]));
    }
    if blur > 0.1 {
        imageops::blur(&shadow, blur)
    } else {
        shadow
    }
}

/// Source-over of a straight-alpha RGBA image onto the premultiplied canvas.
fn composite(canvas: &mut Canvas, image: &RgbaImage, screen_x: i32, screen_y: i32) {
    let origin_x = screen_x - canvas.left;
    let origin_y = screen_y - canvas.top;
    for (x, y, pixel) in image.enumerate_pixels() {
        let cx = origin_x + x as i32;
        let cy = origin_y + y as i32;
        if cx < 0 || cy < 0 || cx >= canvas.width || cy >= canvas.height {
            continue;
        }
        let alpha = pixel[3] as u32;
        if alpha == 0 {
            continue;
        }
        let index = ((cy * canvas.width + cx) * 4) as usize;
        let dst = &mut canvas.pixels[index..index + 4];
        let inverse = 255 - alpha;
        // Canvas is BGRA; the image is RGBA.
        for (channel, source) in [(0usize, pixel[2]), (1, pixel[1]), (2, pixel[0])] {
            let premultiplied = source as u32 * alpha / 255;
            dst[channel] = (premultiplied + dst[channel] as u32 * inverse / 255).min(255) as u8;
        }
        dst[3] = (alpha + dst[3] as u32 * inverse / 255).min(255) as u8;
    }
}

fn decoded(uri: &str) -> Option<Rc<RgbaImage>> {
    let mut hasher = DefaultHasher::new();
    uri.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(known) = DECODED.with(|cache| cache.borrow().get(&key).cloned()) {
        return known;
    }
    let image = uri
        .split_once(',')
        .filter(|(header, _)| header.ends_with(";base64"))
        .and_then(|(_, data)| BASE64.decode(data.trim()).ok())
        .and_then(|bytes| image::load_from_memory(&bytes).ok())
        .map(|image| Rc::new(image.to_rgba8()));
    DECODED.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MAX_DECODED {
            cache.clear();
        }
        cache.insert(key, image.clone());
    });
    image
}

unsafe extern "system" fn layer_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The layer window of `overlay`, created on first use.
fn layer_for(overlay: HWND) -> Option<HWND> {
    let existing = LAYERS.with(|layers| layers.borrow().get(&(overlay.0 as isize)).copied());
    if let Some(layer) = existing {
        let hwnd = HWND(layer as *mut c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
    }

    let instance = unsafe { GetModuleHandleW(None) }.ok()?;
    let class = w!("WindowSwitcherIconLayer");
    let registered = CLASS_REGISTERED.with(|flag| *flag.borrow());
    if !registered {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(layer_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&wc) } == 0 {
            log::warn!("unable to register the icon layer window class");
            return None;
        }
        CLASS_REGISTERED.with(|flag| *flag.borrow_mut() = true);
    }

    // Owned by the overlay, so it always stays just above it; transparent to
    // the mouse, so clicks reach the cards underneath.
    let layer = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            Some(overlay),
            None,
            Some(instance.into()),
            None,
        )
    };
    let layer = match layer {
        Ok(layer) => layer,
        Err(error) => {
            log::warn!("unable to create the icon layer: {error}");
            return None;
        }
    };
    let disabled = BOOL::from(true);
    unsafe {
        let _ = DwmSetWindowAttribute(
            layer,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const BOOL as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
    LAYERS.with(|layers| {
        layers
            .borrow_mut()
            .insert(overlay.0 as isize, layer.0 as isize)
    });
    Some(layer)
}

fn present(layer: HWND, canvas: &Canvas) -> Option<()> {
    unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: canvas.width,
                biHeight: -canvas.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let result = match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) if !bits.is_null() => {
                std::ptr::copy_nonoverlapping(
                    canvas.pixels.as_ptr(),
                    bits as *mut u8,
                    canvas.pixels.len(),
                );
                let previous = SelectObject(memory, bitmap.into());
                let position = POINT {
                    x: canvas.left,
                    y: canvas.top,
                };
                let size = SIZE {
                    cx: canvas.width,
                    cy: canvas.height,
                };
                let source = POINT::default();
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let updated = UpdateLayeredWindow(
                    layer,
                    Some(screen),
                    Some(&position),
                    Some(&size),
                    Some(memory),
                    Some(&source),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                );
                SelectObject(memory, previous);
                let _ = DeleteObject(bitmap.into());
                match updated {
                    Ok(()) => {
                        let _ = ShowWindow(layer, SW_SHOWNOACTIVATE);
                        Some(())
                    }
                    Err(error) => {
                        log::debug!("unable to draw the icon layer: {error}");
                        None
                    }
                }
            }
            Ok(bitmap) => {
                let _ = DeleteObject(bitmap.into());
                None
            }
            Err(error) => {
                log::debug!("unable to allocate the icon layer: {error}");
                None
            }
        };
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_premultiplies_onto_an_empty_canvas() {
        let mut canvas = Canvas {
            left: 10,
            top: 10,
            width: 2,
            height: 1,
            pixels: vec![0; 8],
        };
        let mut image = RgbaImage::new(1, 1);
        image.put_pixel(0, 0, image::Rgba([255, 0, 0, 128]));
        composite(&mut canvas, &image, 11, 10);
        assert_eq!(&canvas.pixels[..4], &[0, 0, 0, 0]);
        // BGRA, premultiplied: red 255 at alpha 128 becomes 128.
        assert_eq!(&canvas.pixels[4..], &[0, 0, 128, 128]);
    }

    #[test]
    fn composite_ignores_pixels_outside_the_canvas() {
        let mut canvas = Canvas {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        };
        let mut image = RgbaImage::new(2, 2);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba([1, 2, 3, 255]);
        }
        composite(&mut canvas, &image, -1, -1);
        assert_eq!(canvas.pixels, vec![3, 2, 1, 255]);
    }

    #[test]
    fn physical_rects_are_offset_and_scaled() {
        let rect = SlotRect {
            x: 10.0,
            y: 4.0,
            width: 44.0,
            height: 44.0,
        };
        assert_eq!(
            physical(POINT { x: 100, y: 200 }, 1.5, &rect),
            (115, 206, 66, 66)
        );
    }
}
