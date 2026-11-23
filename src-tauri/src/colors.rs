//! An app's theme colors, taken from its icon: the dominant hue fills the
//! card and a second distinct hue, when the icon has one, draws its border.
//! Pure, so it is unit tested.

use image::RgbaImage;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AppColors {
    /// `#rrggbb`: the icon's dominant color, for the card background.
    pub(crate) fill: String,
    /// `#rrggbb`: a second distinct color of the icon, or `fill` again.
    pub(crate) border: String,
}

/// Hue buckets of 15 degrees.
const BINS: usize = 24;
/// Pixels below this saturation or value count as neutral (white, gray,
/// black outlines) and never pick a theme.
const MIN_SATURATION: f32 = 0.22;
const MIN_VALUE: f32 = 0.2;
/// A mostly neutral icon (less colored weight than this share of its opaque
/// pixels) gets no theme, instead of one picked from a few stray pixels.
const MIN_COLORED_SHARE: f32 = 0.08;
/// The second color must be at least this far around the hue circle from
/// the first (in bins, 3 = 45 degrees) and this strong relative to it.
const MIN_SECOND_DISTANCE: usize = 3;
const MIN_SECOND_SHARE: f32 = 0.25;

#[derive(Clone, Copy, Default)]
struct Bin {
    weight: f32,
    red: f32,
    green: f32,
    blue: f32,
}

/// The icon's theme colors, or `None` for a mostly neutral icon.
pub(crate) fn dominant_colors(image: &RgbaImage) -> Option<AppColors> {
    let mut bins = [Bin::default(); BINS];
    let mut opaque = 0f32;
    for pixel in image.pixels() {
        let [red, green, blue, alpha] = pixel.0;
        if alpha < 128 {
            continue;
        }
        opaque += 1.0;
        let (hue, saturation, value) = hsv(red, green, blue);
        if saturation < MIN_SATURATION || value < MIN_VALUE {
            continue;
        }
        // Vivid pixels count more than washed-out ones.
        let weight = saturation * value;
        let bin = &mut bins[((hue / 360.0 * BINS as f32) as usize).min(BINS - 1)];
        bin.weight += weight;
        bin.red += red as f32 * weight;
        bin.green += green as f32 * weight;
        bin.blue += blue as f32 * weight;
    }
    let colored: f32 = bins.iter().map(|bin| bin.weight).sum();
    if opaque == 0.0 || colored < opaque * MIN_COLORED_SHARE {
        return None;
    }

    // A hue that straddles two buckets is still one color: bins are
    // compared with their neighbours added.
    let around = |index: usize| -> Bin {
        let mut sum = Bin::default();
        for offset in [BINS - 1, 0, 1] {
            let bin = bins[(index + offset) % BINS];
            sum.weight += bin.weight;
            sum.red += bin.red;
            sum.green += bin.green;
            sum.blue += bin.blue;
        }
        sum
    };
    let strongest = |candidates: &mut dyn Iterator<Item = usize>| {
        candidates
            .map(|index| (index, around(index)))
            .max_by(|a, b| a.1.weight.total_cmp(&b.1.weight))
    };

    let (first_index, first) = strongest(&mut (0..BINS))?;
    let distance = |index: usize| {
        let gap = index.abs_diff(first_index);
        gap.min(BINS - gap)
    };
    let second = strongest(&mut (0..BINS).filter(|&index| distance(index) >= MIN_SECOND_DISTANCE))
        .map(|(_, bin)| bin)
        .filter(|bin| bin.weight >= first.weight * MIN_SECOND_SHARE);

    let fill = hex(&first);
    let border = second.as_ref().map_or_else(|| fill.clone(), hex);
    Some(AppColors { fill, border })
}

fn hex(bin: &Bin) -> String {
    let channel = |sum: f32| (sum / bin.weight).round().clamp(0.0, 255.0) as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(bin.red),
        channel(bin.green),
        channel(bin.blue)
    )
}

/// Hue in degrees [0, 360), saturation and value in [0, 1].
fn hsv(red: u8, green: u8, blue: u8) -> (f32, f32, f32) {
    let (r, g, b) = (
        red as f32 / 255.0,
        green as f32 / 255.0,
        blue as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let saturation = if max > 0.0 { delta / max } else { 0.0 };
    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue.rem_euclid(360.0), saturation, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A 10x10 icon: `counts` pixels of each color, the rest transparent.
    fn icon(parts: &[([u8; 3], u32)]) -> RgbaImage {
        let mut image = RgbaImage::new(10, 10);
        let mut pixels = image.pixels_mut();
        for &(color, count) in parts {
            for _ in 0..count {
                let [r, g, b] = color;
                *pixels.next().unwrap() = Rgba([r, g, b, 255]);
            }
        }
        image
    }

    #[test]
    fn a_single_color_icon_uses_it_for_fill_and_border() {
        let colors = dominant_colors(&icon(&[([240, 190, 40], 60)])).unwrap();
        assert_eq!(colors.fill, "#f0be28");
        assert_eq!(colors.border, "#f0be28");
    }

    #[test]
    fn two_distinct_colors_give_a_fill_and_a_border() {
        let colors = dominant_colors(&icon(&[([30, 120, 220], 50), ([230, 60, 50], 25)])).unwrap();
        assert_eq!(colors.fill, "#1e78dc");
        assert_eq!(colors.border, "#e63c32");
    }

    #[test]
    fn a_minor_second_color_is_ignored() {
        let colors = dominant_colors(&icon(&[([30, 120, 220], 80), ([230, 60, 50], 5)])).unwrap();
        assert_eq!(colors.border, colors.fill);
    }

    #[test]
    fn neighbouring_shades_count_as_one_color() {
        // Two blues a bucket apart, then a weaker red: red is the border.
        let colors = dominant_colors(&icon(&[
            ([30, 120, 220], 30),
            ([40, 90, 220], 30),
            ([230, 60, 50], 25),
        ]))
        .unwrap();
        assert_eq!(colors.border, "#e63c32");
    }

    #[test]
    fn neutral_icons_get_no_theme() {
        assert_eq!(
            dominant_colors(&icon(&[([128, 128, 128], 90), ([20, 20, 20], 10)])),
            None
        );
        assert_eq!(
            dominant_colors(&icon(&[([250, 250, 250], 95), ([200, 40, 40], 2)])),
            None
        );
        assert_eq!(dominant_colors(&RgbaImage::new(4, 4)), None);
    }

    #[test]
    fn hsv_matches_known_colors() {
        assert_eq!(hsv(255, 0, 0), (0.0, 1.0, 1.0));
        assert_eq!(hsv(0, 255, 0), (120.0, 1.0, 1.0));
        assert_eq!(hsv(0, 0, 255), (240.0, 1.0, 1.0));
        assert_eq!(hsv(255, 255, 255).1, 0.0);
    }
}
