//! pi's image pipeline: what an image becomes before it enters the
//! conversation. Formats models do not accept become PNG, and with
//! `images.autoResize` images are fitted to the model's limits.

use std::borrow::Cow;
use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageOutputFormat};
use yapi_types::message::{ContentBlock, ImageContent};
use yapi_types::models::ImageResize;

/// pi's note for an image that cannot be decoded for conversion.
pub const CONVERT_FAILED: &str =
    "[Image omitted: could not be converted to a supported inline image format.]";
/// pi's note for an image no encoding fits within the size limit.
pub const RESIZE_FAILED: &str =
    "[Image omitted: could not be resized below the inline image size limit.]";

/// pi's default limit: 4.5 MB of base64, below Anthropic's 5 MB.
const DEFAULT_MAX_BYTES: u64 = 4_718_592;

/// An image ready for the model, with pi's notes on how it changed.
#[derive(Debug, PartialEq)]
pub struct Processed {
    /// The image to send.
    pub image: ImageContent,
    /// pi's conversion and dimension notes, in order.
    pub hints: Vec<String>,
}

/// pi's `processImage`: an image that is not PNG, JPEG, GIF or WebP becomes
/// PNG, and with `auto_resize` the image is fitted to `limits`, pi's defaults
/// where unset: 2000×2000 and 4.5 MB of base64. The error is pi's omission
/// note.
pub fn process(
    bytes: &[u8],
    mime_type: &str,
    auto_resize: bool,
    limits: Option<&ImageResize>,
) -> Result<Processed, &'static str> {
    let base = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let (bytes, mime_type, converted_from) = match base.as_str() {
        "image/png" => (Cow::Borrowed(bytes), "image/png", None),
        "image/jpeg" | "image/jpg" => (Cow::Borrowed(bytes), "image/jpeg", None),
        "image/gif" => (Cow::Borrowed(bytes), "image/gif", None),
        "image/webp" => (Cow::Borrowed(bytes), "image/webp", None),
        _ => {
            let png = decode(bytes)
                .and_then(|image| encode(&image, ImageOutputFormat::Png))
                .ok_or(CONVERT_FAILED)?;
            (Cow::Owned(png), "image/png", Some(base))
        }
    };
    let (image, note) = if auto_resize {
        resize(&bytes, mime_type, limits).ok_or(RESIZE_FAILED)?
    } else {
        let image = ImageContent {
            data: STANDARD.encode(&bytes),
            mime_type: mime_type.to_owned(),
        };
        (image, None)
    };
    let mut hints = Vec::new();
    if let Some(from) = converted_from.filter(|from| !from.is_empty() && *from != image.mime_type) {
        hints.push(format!(
            "[Image converted from {from} to {}.]",
            image.mime_type
        ));
    }
    hints.extend(note);
    Ok(Processed { image, hints })
}

/// pi's `_normalizePromptImages`: a prompt's images processed for the
/// model, with pi's notes, which include the omission note of each image
/// that fails.
pub fn normalize_prompt(
    images: Vec<ImageContent>,
    auto_resize: bool,
    limits: Option<&ImageResize>,
) -> (Vec<ImageContent>, Vec<String>) {
    let mut normalized = Vec::with_capacity(images.len());
    let mut hints = Vec::new();
    for image in images {
        // Undecodable base64 fails as the empty image it decodes to in pi.
        let bytes = STANDARD.decode(&image.data).unwrap_or_default();
        match process(&bytes, &image.mime_type, auto_resize, limits) {
            Ok(processed) => {
                normalized.push(processed.image);
                hints.extend(processed.hints);
            }
            Err(message) => hints.push(message.to_owned()),
        }
    }
    (normalized, hints)
}

/// pi's `normalizeToolResultImages`: each image of a tool result processed
/// as for the model, followed by a text block of pi's notes when there are
/// any. An image that fails keeps its block. `None` when nothing changed.
pub fn normalize_tool_result(
    content: &[ContentBlock],
    auto_resize: bool,
    limits: Option<&ImageResize>,
) -> Option<Vec<ContentBlock>> {
    let mut changed = false;
    let mut normalized = Vec::with_capacity(content.len());
    for block in content {
        let ContentBlock::Image(image) = block else {
            normalized.push(block.clone());
            continue;
        };
        let processed = STANDARD
            .decode(&image.data)
            .ok()
            .and_then(|bytes| process(&bytes, &image.mime_type, auto_resize, limits).ok());
        match processed {
            Some(processed) if processed.image != *image || !processed.hints.is_empty() => {
                normalized.push(ContentBlock::Image(processed.image));
                if !processed.hints.is_empty() {
                    normalized.push(ContentBlock::text(processed.hints.join("\n")));
                }
                changed = true;
            }
            _ => normalized.push(block.clone()),
        }
    }
    changed.then_some(normalized)
}

/// pi's `resizeImage` with its `formatDimensionNote`: the image unchanged
/// when it is within `limits`; otherwise scaled to fit, as the first of PNG
/// and JPEG at falling qualities that fits the byte limit, shrinking by a
/// quarter until one does.
fn resize(
    bytes: &[u8],
    mime_type: &str,
    limits: Option<&ImageResize>,
) -> Option<(ImageContent, Option<String>)> {
    let limits = limits.cloned().unwrap_or_default();
    let max_width = limits.max_width.unwrap_or(2000);
    let max_height = limits.max_height.unwrap_or(2000);
    let max_bytes = limits.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let image = decode(bytes)?;
    let (original_width, original_height) = (image.width(), image.height());
    if original_width <= max_width
        && original_height <= max_height
        && base64_len(bytes.len()) < max_bytes
    {
        let image = ImageContent {
            data: STANDARD.encode(bytes),
            mime_type: mime_type.to_owned(),
        };
        return Some((image, None));
    }
    let scale = |value: u32, max: u32, other: u32| {
        yapi_types::js::round(f64::from(value) * f64::from(max) / f64::from(other)) as u32
    };
    let (mut width, mut height) = (original_width, original_height);
    if width > max_width {
        height = scale(height, max_width, width);
        width = max_width;
    }
    if height > max_height {
        width = scale(width, max_height, height);
        height = max_height;
    }
    let mut qualities = vec![limits.jpeg_quality.unwrap_or(80)];
    for quality in [85, 70, 55, 40] {
        if !qualities.contains(&quality) {
            qualities.push(quality);
        }
    }
    let formats: Vec<_> = std::iter::once(ImageOutputFormat::Png)
        .chain(qualities.into_iter().map(ImageOutputFormat::Jpeg))
        .collect();
    loop {
        // pi's Photon fails to encode an empty image.
        if width == 0 || height == 0 {
            return None;
        }
        let resized = DynamicImage::ImageRgba8(imageops::resize(
            &image,
            width,
            height,
            FilterType::Lanczos3,
        ));
        for format in &formats {
            let encoded = encode(&resized, format.clone())?;
            if base64_len(encoded.len()) < max_bytes {
                let mime_type = match format {
                    ImageOutputFormat::Png => "image/png",
                    _ => "image/jpeg",
                };
                let note = format!(
                    "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {} to map to original image.]",
                    yapi_types::js::to_fixed(f64::from(original_width) / f64::from(width), 2)
                );
                let image = ImageContent {
                    data: STANDARD.encode(encoded),
                    mime_type: mime_type.to_owned(),
                };
                return Some((image, Some(note)));
            }
        }
        let shrink = |value: u32| {
            if value == 1 {
                1
            } else {
                ((f64::from(value) * 0.75).floor() as u32).max(1)
            }
        };
        let next = (shrink(width), shrink(height));
        if next == (width, height) {
            return None;
        }
        (width, height) = next;
    }
}

/// The length of `len` bytes in base64.
fn base64_len(len: usize) -> u64 {
    len.div_ceil(3) as u64 * 4
}

/// The image as RGBA, as Photon holds it, turned upright by its EXIF
/// orientation as pi does.
fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(DynamicImage::ImageRgba8(match exif_orientation(bytes) {
        2 => imageops::flip_horizontal(&image),
        3 => imageops::rotate180(&image),
        4 => imageops::flip_vertical(&image),
        5 => imageops::flip_horizontal(&imageops::rotate90(&image)),
        6 => imageops::rotate90(&image),
        7 => imageops::flip_horizontal(&imageops::rotate270(&image)),
        8 => imageops::rotate270(&image),
        _ => image,
    }))
}

/// `image` in `format`. JPEG drops the alpha channel, as with Photon.
fn encode(image: &DynamicImage, format: ImageOutputFormat) -> Option<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).ok()?;
    Some(bytes.into_inner())
}

/// pi's `getExifOrientation`: the EXIF orientation of a JPEG or WebP image,
/// 1 when it has none.
fn exif_orientation(bytes: &[u8]) -> u16 {
    let tiff = if bytes.starts_with(&[0xff, 0xd8]) {
        jpeg_tiff_offset(bytes)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        webp_tiff_offset(bytes)
    } else {
        None
    };
    tiff.map_or(1, |start| tiff_orientation(bytes, start))
}

fn has_exif_header(bytes: &[u8], offset: usize) -> bool {
    bytes.get(offset..offset + 6) == Some(b"Exif\0\0")
}

fn jpeg_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 2;
    while offset + 1 < bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        if marker == 0xe1 {
            if offset + 4 >= bytes.len() || offset + 10 > bytes.len() {
                return None;
            }
            if has_exif_header(bytes, offset + 4) {
                return Some(offset + 10);
            }
        }
        if offset + 4 > bytes.len() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]));
        offset += 2 + length;
    }
    None
}

fn webp_tiff_offset(bytes: &[u8]) -> Option<usize> {
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let data = offset + 8;
        if &bytes[offset..offset + 4] == b"EXIF" {
            if data.saturating_add(size) > bytes.len() {
                return None;
            }
            // Some files put "Exif\0\0" before the TIFF header.
            return Some(if size >= 6 && has_exif_header(bytes, data) {
                data + 6
            } else {
                data
            });
        }
        // Chunks are padded to an even size.
        offset = data.checked_add(size)?.checked_add(size % 2)?;
    }
    None
}

fn tiff_orientation(bytes: &[u8], start: usize) -> u16 {
    if start + 8 > bytes.len() {
        return 1;
    }
    let little_endian = bytes[start..start + 2] == [0x49, 0x49];
    let read16 = |at: usize| {
        let pair = [bytes[at], bytes[at + 1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let quad = [
        bytes[start + 4],
        bytes[start + 5],
        bytes[start + 6],
        bytes[start + 7],
    ];
    let ifd = if little_endian {
        u32::from_le_bytes(quad)
    } else {
        u32::from_be_bytes(quad)
    };
    let ifd = start.saturating_add(ifd as usize);
    if ifd.saturating_add(2) > bytes.len() {
        return 1;
    }
    for entry in 0..usize::from(read16(ifd)) {
        let at = ifd + 2 + entry * 12;
        if at + 12 > bytes.len() {
            return 1;
        }
        if read16(at) == 0x0112 {
            let value = read16(at + 8);
            return if (1..=8).contains(&value) { value } else { 1 };
        }
    }
    1
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test helpers; a panic is a test failure"
    )]

    use super::*;
    use image::{Rgba, RgbaImage};

    /// A `width`×`height` image: a gradient, or noise that compresses badly.
    fn pixels(width: u32, height: u32, noisy: bool) -> DynamicImage {
        let mut seed = 7u32;
        DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let noise = if noisy { (seed >> 16) as u8 } else { 0 };
            Rgba([(x % 256) as u8 ^ noise, (y % 256) as u8, noise, 255])
        }))
    }

    fn file(image: &DynamicImage, format: ImageOutputFormat) -> Vec<u8> {
        encode(image, format).unwrap()
    }

    fn limits(width: u32, height: u32, bytes: Option<u64>) -> ImageResize {
        ImageResize {
            max_width: Some(width),
            max_height: Some(height),
            max_bytes: bytes,
            jpeg_quality: None,
        }
    }

    fn dimensions(image: &ImageContent) -> (u32, u32) {
        let decoded = image::load_from_memory(&STANDARD.decode(&image.data).unwrap()).unwrap();
        (decoded.width(), decoded.height())
    }

    #[test]
    fn images_within_the_limits_pass_unchanged() {
        let png = file(&pixels(30, 20, false), ImageOutputFormat::Png);
        let processed = process(&png, "image/png", true, None).unwrap();
        assert_eq!(processed.image.data, STANDARD.encode(&png));
        assert_eq!(processed.image.mime_type, "image/png");
        assert!(processed.hints.is_empty());
    }

    #[test]
    fn large_images_are_scaled_to_fit_with_pi_s_note() {
        let png = file(&pixels(2100, 10, false), ImageOutputFormat::Png);
        let processed = process(&png, "image/png", true, None).unwrap();
        assert_eq!(processed.image.mime_type, "image/png");
        assert_eq!(dimensions(&processed.image), (2000, 10));
        assert_eq!(
            processed.hints,
            [
                "[Image: original 2100x10, displayed at 2000x10. Multiply coordinates by 1.05 to map to original image.]"
            ]
        );

        let tall = file(&pixels(30, 90, false), ImageOutputFormat::Png);
        let limits = limits(40, 40, None);
        let processed = process(&tall, "image/png", true, Some(&limits)).unwrap();
        assert_eq!(dimensions(&processed.image), (13, 40));
        assert_eq!(
            processed.hints,
            [
                "[Image: original 30x90, displayed at 13x40. Multiply coordinates by 2.31 to map to original image.]"
            ]
        );
        // Without auto-resize the image passes as it is.
        let processed = process(&tall, "image/png", false, Some(&limits)).unwrap();
        assert_eq!(processed.image.data, STANDARD.encode(&tall));
        assert!(processed.hints.is_empty());
    }

    #[test]
    fn jpeg_replaces_png_that_exceeds_the_byte_limit() {
        let image = pixels(64, 64, true);
        let png = file(&image, ImageOutputFormat::Png);
        let limits = limits(2000, 2000, Some(base64_len(png.len())));
        let processed = process(&png, "image/png", true, Some(&limits)).unwrap();
        assert_eq!(processed.image.mime_type, "image/jpeg");
        assert_eq!(
            STANDARD.decode(&processed.image.data).unwrap(),
            file(&image, ImageOutputFormat::Jpeg(80))
        );
        assert_eq!(
            processed.hints,
            [
                "[Image: original 64x64, displayed at 64x64. Multiply coordinates by 1.00 to map to original image.]"
            ]
        );
    }

    #[test]
    fn images_shrink_until_they_fit_or_are_omitted() {
        let png = file(&pixels(40, 40, true), ImageOutputFormat::Png);
        let limits = limits(2000, 2000, Some(1200));
        let processed = process(&png, "image/png", true, Some(&limits)).unwrap();
        assert!(processed.image.data.len() < 1200);
        let (width, height) = dimensions(&processed.image);
        assert!(width < 40 && width == height);
        let none_fit = ImageResize {
            max_bytes: Some(1),
            ..ImageResize::default()
        };
        assert_eq!(
            process(&png, "image/png", true, Some(&none_fit)),
            Err(RESIZE_FAILED)
        );
    }

    #[test]
    fn exif_orientation_turns_images_upright_before_resizing() {
        let jpeg = file(&pixels(4, 2, false), ImageOutputFormat::Jpeg(90));
        // An APP1 segment with one IFD entry: orientation 6, rotate clockwise.
        let mut exif = b"\xff\xe1\x00\x22Exif\0\0MM\0\x2a\0\0\0\x08\0\x01".to_vec();
        exif.extend_from_slice(b"\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0");
        let rotated = [&jpeg[..2], &exif, &jpeg[2..]].concat();
        assert_eq!(exif_orientation(&rotated), 6);
        let limits = limits(3, 3, None);
        let processed = process(&rotated, "image/jpeg", true, Some(&limits)).unwrap();
        assert_eq!(dimensions(&processed.image), (2, 3));
        assert_eq!(
            processed.hints,
            [
                "[Image: original 2x4, displayed at 2x3. Multiply coordinates by 1.00 to map to original image.]"
            ]
        );
    }

    #[test]
    fn other_formats_become_png() {
        let image = pixels(3, 2, false);
        let bmp = file(&image, ImageOutputFormat::Bmp);
        for auto_resize in [true, false] {
            let processed = process(&bmp, "image/bmp", auto_resize, None).unwrap();
            assert_eq!(processed.image.mime_type, "image/png");
            assert_eq!(
                STANDARD.decode(&processed.image.data).unwrap(),
                file(&image, ImageOutputFormat::Png)
            );
            assert_eq!(
                processed.hints,
                ["[Image converted from image/bmp to image/png.]"]
            );
        }
        assert_eq!(
            process(b"not an image", "image/tiff", true, None),
            Err(CONVERT_FAILED)
        );
        assert_eq!(
            process(b"not an image", "image/png", true, None),
            Err(RESIZE_FAILED)
        );
    }

    #[test]
    fn prompts_and_tool_results_are_normalized() {
        let png = ImageContent {
            data: STANDARD.encode(file(&pixels(3, 2, false), ImageOutputFormat::Png)),
            mime_type: "image/png".into(),
        };
        let bmp = ImageContent {
            data: STANDARD.encode(file(&pixels(3, 2, false), ImageOutputFormat::Bmp)),
            mime_type: "image/bmp".into(),
        };
        let broken = ImageContent {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        };

        let (images, hints) =
            normalize_prompt(vec![png.clone(), bmp.clone(), broken.clone()], true, None);
        assert_eq!(images[0], png);
        assert_eq!(images[1].mime_type, "image/png");
        assert_eq!(images.len(), 2);
        assert_eq!(
            hints,
            [
                "[Image converted from image/bmp to image/png.]",
                RESIZE_FAILED
            ]
        );

        let unchanged = [ContentBlock::text("a"), ContentBlock::Image(png.clone())];
        assert_eq!(normalize_tool_result(&unchanged, true, None), None);
        let content = [
            ContentBlock::Image(bmp),
            ContentBlock::Image(broken.clone()),
            ContentBlock::text("a"),
        ];
        let normalized = normalize_tool_result(&content, true, None).unwrap();
        assert_eq!(
            normalized,
            [
                ContentBlock::Image(images[1].clone()),
                ContentBlock::text("[Image converted from image/bmp to image/png.]"),
                ContentBlock::Image(broken),
                ContentBlock::text("a"),
            ]
        );
    }
}
