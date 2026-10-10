//! OCR through `Windows.Media.Ocr` (built into Windows 10+, language packs
//! are the ones the user has installed).

use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine as WinEngine;
use windows::Storage::Streams::DataWriter;

use crate::{Image, OcrEngine, OcrLine, OcrWord, PlatformError, Result};

const NO_LANGUAGE: &str = "no Windows OCR language is installed \
(Settings → Time & language → Language → add a language with OCR)";

/// `WinEngine` (a WinRT `OcrEngine`) is agile and the windows crate already
/// marks it `Send + Sync`, so this wrapper is `Send` without any `unsafe`.
/// The daemon uses an engine from one thread at a time.
struct WindowsOcr {
    engine: WinEngine,
    tag: String,
    /// `OcrEngine.MaxImageDimension` (width and height limit), cached.
    max_dimension: u32,
}

pub fn create(language: &str) -> std::result::Result<Box<dyn OcrEngine>, String> {
    crate::win_misc::ensure_com();
    let engine = pick_engine(language.trim())?;
    let tag = engine
        .RecognizerLanguage()
        .and_then(|l| l.LanguageTag())
        .map(|t| t.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    // A failed query is treated as "no limit known": the default of the
    // Windows OCR engine is 10 240, use that rather than failing.
    let max_dimension = WinEngine::MaxImageDimension().unwrap_or(10_240).max(1);
    Ok(Box::new(WindowsOcr {
        engine,
        tag,
        max_dimension,
    }))
}

fn pick_engine(language: &str) -> std::result::Result<WinEngine, String> {
    if !language.is_empty() {
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(language)) {
            if WinEngine::IsLanguageSupported(&lang).unwrap_or(false) {
                if let Ok(engine) = WinEngine::TryCreateFromLanguage(&lang) {
                    return Ok(engine);
                }
            }
        }
    }
    // Empty or unsupported: the user's profile languages. A null result (no
    // profile language has an OCR pack) surfaces as an `Err`.
    if let Ok(engine) = WinEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }
    // Last resort: any installed recognizer.
    if let Ok(languages) = WinEngine::AvailableRecognizerLanguages() {
        if languages.Size().unwrap_or(0) > 0 {
            if let Ok(first) = languages.GetAt(0) {
                if let Ok(engine) = WinEngine::TryCreateFromLanguage(&first) {
                    return Ok(engine);
                }
            }
        }
    }
    Err(NO_LANGUAGE.to_string())
}

fn os_err(context: &'static str) -> impl Fn(windows::core::Error) -> PlatformError {
    move |e| PlatformError::Os(format!("windows ocr: {context}: {e}"))
}

impl OcrEngine for WindowsOcr {
    fn name(&self) -> String {
        format!("windows-ocr({})", self.tag)
    }

    fn recognize(&mut self, image: &Image) -> Result<Vec<OcrLine>> {
        crate::win_misc::ensure_com();
        let (w, h) = (image.width as usize, image.height as usize);
        let expected = w.checked_mul(h).and_then(|p| p.checked_mul(4));
        if w == 0 || h == 0 || expected != Some(image.rgba.len()) {
            return Err(PlatformError::Os(format!(
                "windows ocr: bad image ({}x{}, {} bytes)",
                image.width,
                image.height,
                image.rgba.len()
            )));
        }

        let factor = downscale_factor(image.width, image.height, self.max_dimension);
        let (bgra, sw, sh) = to_bgra_scaled(&image.rgba, w, h, factor as usize)
            .ok_or_else(|| PlatformError::Os("windows ocr: image conversion failed".into()))?;
        let (sw_i, sh_i) = match (i32::try_from(sw), i32::try_from(sh)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return Err(PlatformError::Os("windows ocr: image too large".into())),
        };

        let writer = DataWriter::new().map_err(os_err("DataWriter"))?;
        writer.WriteBytes(&bgra).map_err(os_err("WriteBytes"))?;
        let buffer = writer.DetachBuffer().map_err(os_err("DetachBuffer"))?;
        drop(bgra);
        let bitmap =
            SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, sw_i, sh_i)
                .map_err(os_err("SoftwareBitmap"))?;

        let result = self
            .engine
            .RecognizeAsync(&bitmap)
            .map_err(os_err("RecognizeAsync"))?
            .get()
            .map_err(os_err("recognize"))?;

        let mut out = Vec::new();
        let lines = result.Lines().map_err(os_err("Lines"))?;
        let line_count = lines.Size().map_err(os_err("Lines.Size"))?;
        for li in 0..line_count {
            let line = lines.GetAt(li).map_err(os_err("Lines.GetAt"))?;
            let words = line.Words().map_err(os_err("Words"))?;
            let word_count = words.Size().map_err(os_err("Words.Size"))?;
            let mut line_words = Vec::new();
            for wi in 0..word_count {
                let word = words.GetAt(wi).map_err(os_err("Words.GetAt"))?;
                let text = word.Text().map_err(os_err("Word.Text"))?.to_string();
                if text.is_empty() {
                    continue;
                }
                let rect = word.BoundingRect().map_err(os_err("BoundingRect"))?;
                let (x, y, bw, bh) = map_rect(
                    (rect.X, rect.Y, rect.Width, rect.Height),
                    factor,
                    image.width,
                    image.height,
                );
                line_words.push(OcrWord {
                    text,
                    x,
                    y,
                    w: bw,
                    h: bh,
                });
            }
            if !line_words.is_empty() {
                out.push(OcrLine { words: line_words });
            }
        }
        Ok(out)
    }
}

/// Smallest integer `f >= 1` such that both dimensions divided by `f`
/// (rounded up) fit in `max_dimension`.
fn downscale_factor(width: u32, height: u32, max_dimension: u32) -> u32 {
    let longest = width.max(height);
    if max_dimension == 0 || longest <= max_dimension {
        1
    } else {
        longest.div_ceil(max_dimension)
    }
}

/// RGBA → BGRA with alpha forced to 255 (screen content is opaque), box-filtered
/// down by the integer `factor` (1 = no scaling). Returns the pixels and the
/// new size, or `None` if `rgba` is shorter than `w * h * 4`.
fn to_bgra_scaled(
    rgba: &[u8],
    w: usize,
    h: usize,
    factor: usize,
) -> Option<(Vec<u8>, usize, usize)> {
    let factor = factor.max(1);
    let nw = w.div_ceil(factor);
    let nh = h.div_ceil(factor);
    let mut out = Vec::with_capacity(nw.checked_mul(nh)?.checked_mul(4)?);
    if factor == 1 {
        let all = rgba.get(..w.checked_mul(h)?.checked_mul(4)?)?;
        for px in all.chunks_exact(4) {
            if let [r, g, b, _] = *px {
                out.extend_from_slice(&[b, g, r, 255]);
            }
        }
        return Some((out, nw, nh));
    }
    for oy in 0..nh {
        let y0 = oy * factor;
        let y1 = (y0 + factor).min(h);
        for ox in 0..nw {
            let x0 = ox * factor;
            let x1 = (x0 + factor).min(w);
            let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);
            for y in y0..y1 {
                let start = y.checked_mul(w)?.checked_add(x0)?.checked_mul(4)?;
                let end = y.checked_mul(w)?.checked_add(x1)?.checked_mul(4)?;
                for px in rgba.get(start..end)?.chunks_exact(4) {
                    if let [r, g, b, _] = *px {
                        sr += u32::from(r);
                        sg += u32::from(g);
                        sb += u32::from(b);
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            // n >= 1 because x0 < w and y0 < h; max(1) keeps the division total anyway.
            let n = n.max(1);
            out.extend_from_slice(&[(sb / n) as u8, (sg / n) as u8, (sr / n) as u8, 255]);
        }
    }
    Some((out, nw, nh))
}

/// Map a bounding box from the (possibly downscaled) bitmap back to pixels of
/// the original image: scale by `factor`, round outwards, clamp to the image.
fn map_rect(
    rect: (f32, f32, f32, f32),
    factor: u32,
    img_w: u32,
    img_h: u32,
) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = rect;
    let f = factor.max(1) as f32;
    // `f32::max` ignores NaN, so NaN becomes 0; `as u32` saturates.
    let left = ((x * f).floor().max(0.0) as u32).min(img_w);
    let top = ((y * f).floor().max(0.0) as u32).min(img_h);
    let right = (((x + w) * f).ceil().max(0.0) as u32).min(img_w).max(left);
    let bottom = (((y + h) * f).ceil().max(0.0) as u32).min(img_h).max(top);
    (left, top, right - left, bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_is_one_when_the_image_fits() {
        assert_eq!(downscale_factor(1920, 1080, 10_240), 1);
        assert_eq!(downscale_factor(10_240, 10, 10_240), 1);
    }

    #[test]
    fn factor_shrinks_the_longest_side_under_the_limit() {
        assert_eq!(downscale_factor(10_241, 100, 10_240), 2);
        assert_eq!(downscale_factor(100, 25_000, 10_240), 3);
        for (w, h) in [(10_241u32, 5u32), (20_480, 20_480), (30_000, 1)] {
            let f = downscale_factor(w, h, 10_240);
            assert!(w.div_ceil(f) <= 10_240 && h.div_ceil(f) <= 10_240);
        }
    }

    #[test]
    fn rgba_becomes_bgra_with_opaque_alpha() {
        let rgba = [1u8, 2, 3, 0, 10, 20, 30, 7];
        let (bgra, w, h) = to_bgra_scaled(&rgba, 2, 1, 1).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(bgra, vec![3, 2, 1, 255, 30, 20, 10, 255]);
    }

    #[test]
    fn box_filter_averages_blocks_and_keeps_partial_edges() {
        // 3x1 image, factor 2: [avg(px0,px1)], [px2 alone].
        let rgba = [0u8, 0, 0, 255, 100, 100, 100, 255, 50, 60, 70, 255];
        let (bgra, w, h) = to_bgra_scaled(&rgba, 3, 1, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(bgra, vec![50, 50, 50, 255, 70, 60, 50, 255]);
    }

    #[test]
    fn short_buffers_are_rejected_not_indexed() {
        assert!(to_bgra_scaled(&[0u8; 7], 2, 1, 1).is_none());
        assert!(to_bgra_scaled(&[0u8; 7], 2, 1, 2).is_none());
    }

    #[test]
    fn boxes_scale_back_and_clamp() {
        assert_eq!(
            map_rect((10.0, 20.0, 30.0, 5.0), 1, 100, 100),
            (10, 20, 30, 5)
        );
        assert_eq!(
            map_rect((10.0, 20.0, 30.0, 5.0), 2, 100, 100),
            (20, 40, 60, 10)
        );
        // Sticks out past the right/bottom edge and starts left of the origin.
        assert_eq!(
            map_rect((90.0, 95.0, 30.0, 30.0), 1, 100, 100),
            (90, 95, 10, 5)
        );
        assert_eq!(
            map_rect((-5.0, -5.0, 10.0, 10.0), 1, 100, 100),
            (0, 0, 5, 5)
        );
        // NaN does not panic and yields an empty box.
        assert_eq!(
            map_rect((f32::NAN, f32::NAN, f32::NAN, f32::NAN), 1, 100, 100),
            (0, 0, 0, 0)
        );
    }
}
