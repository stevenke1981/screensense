//! Pixel redaction, scaling and JPEG encoding of a captured window.
//!
//! Pure: no I/O, no logging. The only inputs are the pixels and the boxes to black out.
#![allow(dead_code)] // wired up in main.rs

use anyhow::{bail, Context};
use image::{codecs::jpeg::JpegEncoder, imageops, ExtendedColorType, RgbImage};
use screensense_core::types::PxRect;
use screensense_platform::Image;

/// Black out `boxes` (clamped to the image), downscale to `max_width` if wider (boxes are applied
/// BEFORE scaling, so the black areas can only grow by the filter's blur, never disappear),
/// encode JPEG. `max_width == 0` disables scaling.
pub fn redact_and_encode(
    img: &Image,
    boxes: &[PxRect],
    max_width: u32,
    jpeg_quality: u8,
) -> anyhow::Result<Vec<u8>> {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        bail!("image has a zero dimension ({w}x{h})");
    }
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("image dimensions overflow")?;
    if img.rgba.len() != expected {
        bail!(
            "pixel buffer has {} bytes, expected {expected} for {w}x{h} RGBA",
            img.rgba.len()
        );
    }

    // RGBA -> RGB in one pass (JPEG has no alpha): saves a copy of the whole frame.
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for px in img.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    let mut canvas = RgbImage::from_raw(w, h, rgb).context("cannot build the RGB buffer")?;

    for b in boxes {
        blacken(&mut canvas, b);
    }

    let canvas = if max_width > 0 && w > max_width {
        let new_h =
            ((u64::from(h) * u64::from(max_width) + u64::from(w) / 2) / u64::from(w)).max(1) as u32;
        imageops::resize(&canvas, max_width, new_h, imageops::FilterType::Triangle)
    } else {
        canvas
    };

    let mut out = Vec::with_capacity(64 * 1024);
    let quality = jpeg_quality.clamp(1, 100);
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode(
            canvas.as_raw(),
            canvas.width(),
            canvas.height(),
            ExtendedColorType::Rgb8,
        )
        .context("JPEG encoding failed")?;
    Ok(out)
}

/// Fill the part of `r` that lies inside the image with black.
fn blacken(img: &mut RgbImage, r: &PxRect) {
    let (w, h) = (img.width(), img.height());
    let x0 = r.x.min(w);
    let y0 = r.y.min(h);
    let x1 = r.x.saturating_add(r.w).min(w);
    let y1 = r.y.saturating_add(r.h).min(h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let stride = w as usize * 3;
    let raw: &mut [u8] = img.as_mut();
    for y in y0..y1 {
        let row = y as usize * stride;
        raw[row + x0 as usize * 3..row + x1 as usize * 3].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Light gray background with a dark diagonal pattern so the frame is not blank.
    fn test_image(w: u32, h: u32) -> Image {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = if (x / 8 + y / 8) % 2 == 0 { 235 } else { 200 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        Image {
            width: w,
            height: h,
            rgba,
        }
    }

    fn decode(bytes: &[u8]) -> image::RgbImage {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
            .expect("valid jpeg")
            .to_rgb8()
    }

    fn luma(p: &image::Rgb<u8>) -> u32 {
        (u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3
    }

    #[test]
    fn output_is_a_jpeg_of_the_input_size() {
        let jpg = redact_and_encode(&test_image(320, 200), &[], 1600, 70).unwrap();
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        let d = decode(&jpg);
        assert_eq!((d.width(), d.height()), (320, 200));
    }

    #[test]
    fn boxes_are_black_and_the_rest_is_not() {
        let boxes = [PxRect {
            x: 40,
            y: 40,
            w: 64,
            h: 32,
        }];
        let d = decode(&redact_and_encode(&test_image(320, 200), &boxes, 1600, 90).unwrap());
        assert!(luma(d.get_pixel(72, 56)) < 25, "inside the box is black");
        assert!(
            luma(d.get_pixel(200, 150)) > 150,
            "outside the box is untouched"
        );
        assert!(luma(d.get_pixel(20, 20)) > 150);
    }

    #[test]
    fn boxes_outside_or_over_the_edge_are_clamped() {
        let boxes = [
            PxRect {
                x: 300,
                y: 180,
                w: 500,
                h: 500,
            }, // overlaps the bottom-right corner
            PxRect {
                x: 1000,
                y: 1000,
                w: 10,
                h: 10,
            }, // entirely outside
            PxRect {
                x: u32::MAX - 2,
                y: 5,
                w: 100,
                h: 100,
            }, // would overflow u32
            PxRect {
                x: 5,
                y: 5,
                w: 0,
                h: 10,
            }, // empty
        ];
        let d = decode(&redact_and_encode(&test_image(320, 200), &boxes, 1600, 90).unwrap());
        assert_eq!((d.width(), d.height()), (320, 200));
        assert!(
            luma(d.get_pixel(315, 195)) < 25,
            "the visible part of the corner box is black"
        );
        assert!(luma(d.get_pixel(100, 100)) > 150);
    }

    #[test]
    fn downscale_keeps_the_aspect_ratio() {
        let d = decode(&redact_and_encode(&test_image(800, 450), &[], 400, 70).unwrap());
        assert_eq!((d.width(), d.height()), (400, 225));
        let d = decode(&redact_and_encode(&test_image(1001, 333), &[], 500, 70).unwrap());
        assert_eq!(d.width(), 500);
        assert_eq!(d.height(), 166); // 333 * 500 / 1001 = 166.3
    }

    #[test]
    fn narrow_images_are_not_upscaled_and_zero_means_no_limit() {
        let d = decode(&redact_and_encode(&test_image(300, 100), &[], 400, 70).unwrap());
        assert_eq!((d.width(), d.height()), (300, 100));
        let d = decode(&redact_and_encode(&test_image(300, 100), &[], 0, 70).unwrap());
        assert_eq!((d.width(), d.height()), (300, 100));
    }

    #[test]
    fn boxes_are_applied_before_scaling() {
        // A box in full-resolution coordinates must land in the scaled image proportionally.
        let boxes = [PxRect {
            x: 400,
            y: 200,
            w: 160,
            h: 80,
        }];
        let d = decode(&redact_and_encode(&test_image(800, 450), &boxes, 400, 90).unwrap());
        assert!(luma(d.get_pixel(240, 120)) < 30, "centre of the scaled box");
        assert!(luma(d.get_pixel(50, 50)) > 150);
    }

    #[test]
    fn bad_buffers_are_errors() {
        let mut img = test_image(10, 10);
        img.rgba.pop();
        assert!(redact_and_encode(&img, &[], 100, 70).is_err());
        assert!(redact_and_encode(
            &Image {
                width: 0,
                height: 5,
                rgba: vec![]
            },
            &[],
            100,
            70
        )
        .is_err());
        assert!(redact_and_encode(
            &Image {
                width: 5,
                height: 0,
                rgba: vec![]
            },
            &[],
            100,
            70
        )
        .is_err());
        let huge = Image {
            width: u32::MAX,
            height: u32::MAX,
            rgba: vec![0; 16],
        };
        assert!(redact_and_encode(&huge, &[], 100, 70).is_err());
    }

    #[test]
    fn quality_extremes_do_not_panic() {
        assert!(redact_and_encode(&test_image(64, 64), &[], 100, 0).is_ok());
        assert!(redact_and_encode(&test_image(64, 64), &[], 100, 255).is_ok());
    }

    #[test]
    fn one_pixel_image_encodes() {
        let img = Image {
            width: 1,
            height: 1,
            rgba: vec![9, 9, 9, 255],
        };
        assert!(redact_and_encode(
            &img,
            &[PxRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1
            }],
            10,
            70
        )
        .is_ok());
    }
}
