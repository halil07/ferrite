#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::png_codec::{RgbaImage, VisualError};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaskRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl MaskRect {
    pub fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.x
            && px < self.x.saturating_add(self.width)
            && py >= self.y
            && py < self.y.saturating_add(self.height)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffOptions {
    /// Per-pixel perceptual YIQ sensitivity in `0.0..=1.0` (smaller = stricter). Default: `0.1`.
    pub threshold: f64,
    /// When `false`, 1px anti-aliased edge pixels are excluded from `diff_pixels` and marked yellow.
    pub include_aa: bool,
    /// Opacity factor for faded grayscale background of unchanged pixels. Default: `0.1`.
    pub alpha_fade: f64,
    /// Rectangular areas (e.g. dynamic timestamps, ads) ignored during comparison.
    pub mask_regions: Vec<MaskRect>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            threshold: 0.1,
            include_aa: false,
            alpha_fade: 0.1,
            mask_regions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffReport {
    pub width: u32,
    pub height: u32,
    pub total_pixels: u64,
    pub diff_pixels: u64,
    pub aa_pixels: u64,
    pub masked_pixels: u64,
    pub diff_ratio: f64,
    pub diff_image: RgbaImage,
}

fn blend(color: f64, alpha: f64) -> f64 {
    255.0 + (color - 255.0) * alpha
}

fn rgb2y(r: f64, g: f64, b: f64) -> f64 {
    r * 0.298_895_31 + g * 0.586_622_47 + b * 0.114_482_23
}

fn rgb2i(r: f64, g: f64, b: f64) -> f64 {
    r * 0.595_977_99 - g * 0.274_176_10 - b * 0.321_801_89
}

fn rgb2q(r: f64, g: f64, b: f64) -> f64 {
    r * 0.211_470_17 - g * 0.522_617_11 + b * 0.311_146_94
}

fn sample_blended(data: &[u8], pos: usize) -> (f64, f64, f64) {
    let r = data[pos] as f64;
    let g = data[pos + 1] as f64;
    let b = data[pos + 2] as f64;
    let a = data[pos + 3] as f64;
    if a < 255.0 {
        let alpha = a / 255.0;
        (blend(r, alpha), blend(g, alpha), blend(b, alpha))
    } else {
        (r, g, b)
    }
}

/// Computes perceptual YIQ distance (Kotsarenko & Ramos 2010), in `0.0..=35215.0`.
/// When `y_only` is true, returns signed luminance difference for anti-aliasing detection.
fn color_delta(img1: &[u8], img2: &[u8], k: usize, m: usize, y_only: bool) -> f64 {
    let (mut r1, mut g1, mut b1) = (img1[k] as f64, img1[k + 1] as f64, img1[k + 2] as f64);
    let (mut r2, mut g2, mut b2) = (img2[m] as f64, img2[m + 1] as f64, img2[m + 2] as f64);
    let a1 = img1[k + 3] as f64;
    let a2 = img2[m + 3] as f64;

    if a1 == a2 && r1 == r2 && g1 == g2 && b1 == b2 {
        return 0.0;
    }

    if a1 < 255.0 {
        let alpha = a1 / 255.0;
        r1 = blend(r1, alpha);
        g1 = blend(g1, alpha);
        b1 = blend(b1, alpha);
    }
    if a2 < 255.0 {
        let alpha = a2 / 255.0;
        r2 = blend(r2, alpha);
        g2 = blend(g2, alpha);
        b2 = blend(b2, alpha);
    }

    let y = rgb2y(r1, g1, b1) - rgb2y(r2, g2, b2);
    if y_only {
        return y;
    }
    let i = rgb2i(r1, g1, b1) - rgb2i(r2, g2, b2);
    let q = rgb2q(r1, g1, b1) - rgb2q(r2, g2, b2);
    let delta = 0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q;
    if y > 0.0 { -delta } else { delta }
}

fn has_many_siblings(img: &[u8], x1: u32, y1: u32, width: u32, height: u32) -> bool {
    let x0 = x1.saturating_sub(1);
    let y0 = y1.saturating_sub(1);
    let x2 = (x1 + 1).min(width - 1);
    let y2 = (y1 + 1).min(height - 1);
    let pos = ((y1 * width + x1) * 4) as usize;
    let mut zeroes = usize::from(x1 == x0 || x1 == x2 || y1 == y0 || y1 == y2);

    for x in x0..=x2 {
        for y in y0..=y2 {
            if x == x1 && y == y1 {
                continue;
            }
            let pos2 = ((y * width + x) * 4) as usize;
            if img[pos..pos + 4] == img[pos2..pos2 + 4] {
                zeroes += 1;
                if zeroes > 2 {
                    return true;
                }
            }
        }
    }
    false
}

/// Detects whether pixel `(x1, y1)` in `img` is part of a 1-pixel anti-aliased edge
/// (Vysniauskas 2009 / Mapbox pixelmatch algorithm).
fn is_antialiased(img: &[u8], x1: u32, y1: u32, width: u32, height: u32, img2: &[u8]) -> bool {
    let x0 = x1.saturating_sub(1);
    let y0 = y1.saturating_sub(1);
    let x2 = (x1 + 1).min(width - 1);
    let y2 = (y1 + 1).min(height - 1);
    let pos = ((y1 * width + x1) * 4) as usize;

    let mut zeroes = usize::from(x1 == x0 || x1 == x2 || y1 == y0 || y1 == y2);
    let mut min = 0.0f64;
    let mut max = 0.0f64;
    let mut min_x = 0u32;
    let mut min_y = 0u32;
    let mut max_x = 0u32;
    let mut max_y = 0u32;

    for x in x0..=x2 {
        for y in y0..=y2 {
            if x == x1 && y == y1 {
                continue;
            }
            let delta = color_delta(img, img, pos, ((y * width + x) * 4) as usize, true);
            if delta == 0.0 {
                zeroes += 1;
                if zeroes > 2 {
                    return false;
                }
            } else if delta < min {
                min = delta;
                min_x = x;
                min_y = y;
            } else if delta > max {
                max = delta;
                max_x = x;
                max_y = y;
            }
        }
    }

    if min == 0.0 || max == 0.0 {
        return false;
    }

    (has_many_siblings(img, min_x, min_y, width, height)
        && has_many_siblings(img2, min_x, min_y, width, height))
        || (has_many_siblings(img, max_x, max_y, width, height)
            && has_many_siblings(img2, max_x, max_y, width, height))
}

/// Compares `baseline` and `actual` RGBA images using YIQ perceptual distance, optional AA
/// filtering, and masked regions.
pub fn diff(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    options: &DiffOptions,
) -> Result<DiffReport, VisualError> {
    if baseline.width != actual.width || baseline.height != actual.height {
        return Err(VisualError::DimensionMismatch {
            bw: baseline.width,
            bh: baseline.height,
            aw: actual.width,
            ah: actual.height,
        });
    }
    if !(0.0..=1.0).contains(&options.threshold) || options.threshold.is_nan() {
        return Err(VisualError::InvalidThreshold(options.threshold));
    }

    let width = baseline.width;
    let height = baseline.height;
    let max_delta = 35_215.0 * options.threshold * options.threshold;
    let mut out = vec![0u8; baseline.data.len()];
    let mut diff_pixels = 0u64;
    let mut aa_pixels = 0u64;
    let mut masked_pixels = 0u64;

    for y in 0..height {
        for x in 0..width {
            let pos = ((y * width + x) * 4) as usize;
            if options.mask_regions.iter().any(|r| r.contains(x, y)) {
                masked_pixels += 1;
                out[pos..pos + 4].copy_from_slice(&[140, 120, 170, 255]);
                continue;
            }

            let delta = color_delta(&baseline.data, &actual.data, pos, pos, false);
            if delta.abs() > max_delta {
                let is_aa = !options.include_aa
                    && (is_antialiased(&baseline.data, x, y, width, height, &actual.data)
                        || is_antialiased(&actual.data, x, y, width, height, &baseline.data));
                if is_aa {
                    aa_pixels += 1;
                    out[pos..pos + 4].copy_from_slice(&[255, 255, 0, 255]);
                } else {
                    diff_pixels += 1;
                    out[pos..pos + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            } else {
                let (r, g, b) = sample_blended(&baseline.data, pos);
                let alpha = options.alpha_fade.clamp(0.0, 1.0) * (baseline.data[pos + 3] as f64)
                    / 255.0;
                let gray = blend(rgb2y(r, g, b), alpha).round().clamp(0.0, 255.0) as u8;
                out[pos..pos + 4].copy_from_slice(&[gray, gray, gray, 255]);
            }
        }
    }

    let total_pixels = (width as u64) * (height as u64);
    let compared = total_pixels.saturating_sub(masked_pixels).max(1);
    let diff_ratio = (diff_pixels as f64) / (compared as f64);

    Ok(DiffReport {
        width,
        height,
        total_pixels,
        diff_pixels,
        aa_pixels,
        masked_pixels,
        diff_ratio,
        diff_image: RgbaImage {
            width,
            height,
            data: out,
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::png_codec::{decode_png, encode_png};

    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage {
            width,
            height,
            data: color
                .iter()
                .copied()
                .cycle()
                .take((width * height * 4) as usize)
                .collect(),
        }
    }

    fn set_px(img: &mut RgbaImage, x: u32, y: u32, rgba: [u8; 4]) {
        let pos = ((y * img.width + x) * 4) as usize;
        img.data[pos..pos + 4].copy_from_slice(&rgba);
    }

    #[test]
    fn identical_images_and_masked_regions_produce_zero_diff() {
        let base = solid(10, 10, [255, 255, 255, 255]);
        let mut actual = solid(10, 10, [255, 255, 255, 255]);
        // Mutate pixels inside masked rectangle (2,2)-(5,5)
        for y in 2..5 {
            for x in 2..5 {
                set_px(&mut actual, x, y, [0, 0, 0, 255]);
            }
        }

        let opts = DiffOptions {
            mask_regions: vec![MaskRect {
                x: 2,
                y: 2,
                width: 3,
                height: 3,
            }],
            ..Default::default()
        };
        let report = diff(&base, &actual, &opts).unwrap();
        assert_eq!(report.diff_pixels, 0);
        assert_eq!(report.masked_pixels, 9);
    }

    #[test]
    fn excludes_antialiased_edge_pixel_when_include_aa_is_false() {
        // Construct a clean 7x7 step edge: left half white, right half black, with 1 AA gray pixel at (3,3)
        // in `actual` (brighter than white-vs-black mid-gray, sitting between white left and black right).
        let mut base = solid(7, 7, [255, 255, 255, 255]);
        for y in 0..7 {
            for x in 4..7 {
                set_px(&mut base, x, y, [0, 0, 0, 255]);
            }
        }
        let mut actual = base.clone();
        // Anti-aliased pixel at (3,3) transitions smoothly between left white (255) and right black (0).
        set_px(&mut actual, 3, 3, [128, 128, 128, 255]);

        let aa_excluded = diff(
            &base,
            &actual,
            &DiffOptions {
                include_aa: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(aa_excluded.diff_pixels, 0);
        assert_eq!(aa_excluded.aa_pixels, 1);
        let pos = ((3 * 7 + 3) * 4) as usize;
        assert_eq!(&aa_excluded.diff_image.data[pos..pos + 4], &[255, 255, 0, 255]);

        let aa_included = diff(
            &base,
            &actual,
            &DiffOptions {
                include_aa: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(aa_included.diff_pixels, 1);
        assert_eq!(&aa_included.diff_image.data[pos..pos + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn dimension_mismatch_and_png_roundtrip() {
        let a = solid(4, 4, [12, 34, 56, 255]);
        let b = solid(5, 4, [12, 34, 56, 255]);
        assert!(matches!(
            diff(&a, &b, &DiffOptions::default()),
            Err(VisualError::DimensionMismatch { .. })
        ));

        let encoded = encode_png(&a).unwrap();
        let decoded = decode_png(&encoded).unwrap();
        assert_eq!(decoded, a);
    }
}
