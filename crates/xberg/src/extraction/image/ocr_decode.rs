//! OCR-only input decoding. Export/metadata decoders keep their existing behavior.

use std::io::Cursor;

use image::{ImageDecoder, RgbImage};

use crate::error::{Result, XbergError};
use crate::extraction::image_decode::{ImageDecodeBudget, decoded_byte_count, image_dimension_error};
use crate::extractors::security::SecurityLimits;

pub(super) fn decode_ocr_input(bytes: &[u8], limits: &SecurityLimits) -> Option<Result<RgbImage>> {
    let header_size = bytes
        .get(..4)
        .map(|value| u32::from_le_bytes(value.try_into().unwrap()));
    if matches!(header_size, Some(12 | 40 | 52 | 56 | 108 | 124)) {
        return Some(decode_dib(bytes, limits));
    }

    // Only replace the full-frame RGBA -> RGB allocation for non-interlaced RGBA8 PNGs
    // that cannot fit it. Ordinary images, other bit depths and interlacing stay unchanged.
    if bytes.get(..8) != Some(b"\x89PNG\r\n\x1a\n")
        || bytes.get(8..16) != Some(b"\x00\x00\x00\x0dIHDR")
        || bytes.get(24..29) != Some(&[8, 6, 0, 0, 0])
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    let full_frame_peak = decoded_byte_count(width, height, 7).and_then(|decoded| {
        decoded
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| image_dimension_error(width, height, u64::MAX, limits.max_content_size as u64))
    });
    match full_frame_peak {
        Ok(peak) if peak <= limits.max_content_size as u64 => None,
        _ => Some(decode_rgba_png_rows(bytes, width, height, limits)),
    }
}

fn decode_dib(bytes: &[u8], limits: &SecurityLimits) -> Result<RgbImage> {
    let budget = ImageDecodeBudget::from_security_limits(limits);
    // BMP's palette/header scratch is bounded by the image decoder (256 entries).
    budget.validate(1, 1, (bytes.len() as u64).saturating_add(4096))?;
    let decoder = image::codecs::bmp::BmpDecoder::new_without_file_header(Cursor::new(bytes))
        .map_err(|error| XbergError::parsing(format!("Failed to decode DIB for OCR: {error}")))?;
    let (width, height) = decoder.dimensions();
    let conversion = if decoder.color_type() == image::ColorType::Rgb8 {
        0
    } else {
        decoded_byte_count(width, height, 3)?
    };
    let peak = decoder
        .total_bytes()
        .checked_add(conversion)
        .and_then(|value| value.checked_add(bytes.len() as u64))
        .and_then(|value| value.checked_add(u64::from(width) * 4 + 4096))
        .ok_or_else(|| image_dimension_error(width, height, u64::MAX, limits.max_content_size as u64))?;
    budget.validate(width, height, peak)?;
    image::DynamicImage::from_decoder(decoder)
        .map(image::DynamicImage::into_rgb8)
        .map_err(|error| XbergError::parsing(format!("Failed to decode DIB pixels for OCR: {error}")))
}

fn decode_rgba_png_rows(bytes: &[u8], width: u32, height: u32, limits: &SecurityLimits) -> Result<RgbImage> {
    let rgb_bytes = decoded_byte_count(width, height, 3)?;
    let row_bytes = decoded_byte_count(width, 1, 4)?;
    // png 0.18.1's unfiltering buffer shifts after max(4 rows, 128 KiB), and Vec
    // growth can double capacity. Reserve 16 raw rows + 1 MiB for that buffer,
    // previous/output rows, inflater and initial chunk storage. Metadata allocations
    // are separately limited to the remaining live-byte budget by png::Limits.
    let resident = row_bytes
        .checked_add(1)
        .and_then(|row| row.checked_mul(16))
        .and_then(|scratch| scratch.checked_add(1024 * 1024))
        .and_then(|scratch| scratch.checked_add(rgb_bytes))
        .and_then(|value| value.checked_add(bytes.len() as u64))
        .ok_or_else(|| image_dimension_error(width, height, u64::MAX, limits.max_content_size as u64))?;
    ImageDecodeBudget::from_security_limits(limits).validate(width, height, resident)?;
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: limits.max_content_size - resident as usize,
        },
    );
    // OCR consumes pixels only, not text/ICC metadata. Keep CRC/error checks enabled.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let map_error = |error: png::DecodingError| {
        if matches!(error, png::DecodingError::LimitsExceeded) {
            image_dimension_error(
                width,
                height,
                limits.max_content_size as u64 + 1,
                limits.max_content_size as u64,
            )
        } else {
            XbergError::parsing(format!("Failed to decode PNG rows for OCR: {error}"))
        }
    };
    let mut reader = decoder.read_info().map_err(map_error)?;
    if reader.info().width != width
        || reader.info().height != height
        || reader.info().interlaced
        || reader.output_color_type() != (png::ColorType::Rgba, png::BitDepth::Eight)
    {
        return Err(XbergError::parsing("Unsupported PNG row layout for OCR"));
    }
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(rgb_bytes as usize)
        .map_err(|error| XbergError::parsing(format!("Failed to reserve OCR RGB pixels: {error}")))?;
    rgb.resize(rgb_bytes as usize, 0);
    let mut row = Vec::new();
    row.try_reserve_exact(row_bytes as usize)
        .map_err(|error| XbergError::parsing(format!("Failed to reserve OCR PNG row: {error}")))?;
    row.resize(row_bytes as usize, 0);
    for target in rgb.chunks_exact_mut(width as usize * 3) {
        if reader.read_row(&mut row).map_err(map_error)?.is_none() {
            return Err(XbergError::parsing("PNG ended before all OCR rows were decoded"));
        }
        for (source, target) in row.chunks_exact(4).zip(target.chunks_exact_mut(3)) {
            // Match DynamicImage::into_rgb8: retain RGB samples, discard alpha.
            target.copy_from_slice(&source[..3]);
        }
    }
    if reader.read_row(&mut row).map_err(map_error)?.is_some() {
        return Err(XbergError::parsing("PNG has more OCR rows than its declared height"));
    }
    reader.finish().map_err(map_error)?;
    RgbImage::from_raw(width, height, rgb).ok_or_else(|| XbergError::parsing("Invalid OCR RGB buffer dimensions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extraction::image::load_image_for_ocr;

    fn limits(max_content_size: usize) -> SecurityLimits {
        SecurityLimits {
            max_content_size,
            ..Default::default()
        }
    }

    fn dib_header(width: i32, height: i32, bits: u16, header_size: u32, compression: u32) -> Vec<u8> {
        let mut bytes = vec![0; header_size as usize];
        bytes[..4].copy_from_slice(&header_size.to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..12].copy_from_slice(&height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1_u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bits.to_le_bytes());
        bytes[16..20].copy_from_slice(&compression.to_le_bytes());
        bytes
    }

    fn padded_dib(top_down: bool) -> Vec<u8> {
        let mut bytes = dib_header(3, if top_down { -2 } else { 2 }, 24, 40, 0);
        // BGR rows, with conspicuous non-pixel padding after the third pixel.
        let top = [0, 0, 255, 0, 255, 0, 255, 0, 0, 91, 92, 93];
        let bottom = [30, 20, 10, 60, 50, 40, 90, 80, 70, 94, 95, 96];
        for row in if top_down { [top, bottom] } else { [bottom, top] } {
            bytes.extend_from_slice(&row);
        }
        bytes
    }

    #[test]
    fn dib_padding_and_both_row_orientations_preserve_rgb_pixels() {
        let expected = [255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30, 40, 50, 60, 70, 80, 90];
        for top_down in [false, true] {
            let image = load_image_for_ocr(&padded_dib(top_down), &SecurityLimits::default())
                .expect("decode padded DIB")
                .into_rgb8();
            assert_eq!(image.dimensions(), (3, 2));
            assert_eq!(image.as_raw().as_slice(), expected);
        }
    }

    #[test]
    fn dib_bitfields_discard_alpha_without_compositing_rgb() {
        let mut bytes = dib_header(2, -1, 32, 56, 3);
        for (target, mask) in
            bytes[40..56]
                .chunks_exact_mut(4)
                .zip([0x00ff_0000_u32, 0x0000_ff00, 0x0000_00ff, 0xff00_0000])
        {
            target.copy_from_slice(&mask.to_le_bytes());
        }
        bytes.extend_from_slice(&[30, 20, 10, 0, 60, 50, 40, 127]);
        let image = load_image_for_ocr(&bytes, &SecurityLimits::default())
            .expect("decode alpha bitfields")
            .into_rgb8();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.as_raw(), &[10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn dib_rejects_malformed_headers_and_truncated_pixels() {
        let valid = padded_dib(false);
        let mut invalid_planes = valid.clone();
        invalid_planes[12..14].copy_from_slice(&0_u16.to_le_bytes());
        for bytes in [&valid[..20], &invalid_planes, &valid[..valid.len() - 4]] {
            let error = load_image_for_ocr(bytes, &SecurityLimits::default())
                .expect_err("malformed DIB must not produce partial pixels");
            assert!(matches!(error, XbergError::Parsing { .. }), "{error}");
        }
    }

    #[test]
    fn dib_budget_refusal_precedes_pixel_allocation() {
        let bytes = dib_header(32_000, 32_000, 24, 40, 0);
        let budget = limits(8_192);
        let error = load_image_for_ocr(&bytes, &budget).expect_err("oversized DIB must be refused");
        assert!(matches!(error, XbergError::Validation { .. }), "{error}");
        let message = error.to_string();
        assert!(message.contains("32000x32000"), "{message}");
        assert!(message.contains(&budget.max_content_size.to_string()), "{message}");
    }

    const PNG_WIDTH: u32 = 1_024;
    const PNG_HEIGHT: u32 = 512;

    fn rgba_pixels() -> image::RgbaImage {
        image::RgbaImage::from_fn(PNG_WIDTH, PNG_HEIGHT, |x, y| {
            image::Rgba([
                x as u8,
                y as u8,
                (x.wrapping_add(y)) as u8,
                [0, 127, 255][x as usize % 3],
            ])
        })
    }

    fn rgba_png(pixels: &image::RgbaImage, animated: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, pixels.width(), pixels.height());
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if animated {
                encoder.set_animated(1, 0).expect("configure APNG");
                encoder.set_sep_def_img(true).expect("separate default frame");
            }
            let mut writer = encoder.write_header().expect("write PNG header");
            writer.write_image_data(pixels.as_raw()).expect("write default pixels");
            if animated {
                let other_frame =
                    image::RgbaImage::from_pixel(pixels.width(), pixels.height(), image::Rgba([9, 8, 7, 255]));
                writer
                    .write_image_data(other_frame.as_raw())
                    .expect("write animation frame");
            }
            writer.finish().expect("finish PNG");
        }
        bytes
    }

    fn row_budget(encoded_bytes: usize) -> SecurityLimits {
        // Admit final RGB + the declared row/scratch allowance with only two raw rows
        // left over for png's own charged allocations, not a second full pixel frame.
        limits(
            PNG_WIDTH as usize * PNG_HEIGHT as usize * 3
                + (PNG_WIDTH as usize * 4 + 1) * 16
                + 1024 * 1024
                + encoded_bytes
                + PNG_WIDTH as usize * 8,
        )
    }

    #[test]
    fn rgba_png_row_path_matches_full_decode_and_discards_each_alpha_value() {
        let pixels = rgba_pixels();
        let expected = image::DynamicImage::ImageRgba8(pixels.clone()).into_rgb8();
        let bytes = rgba_png(&pixels, false);
        let budget = row_budget(bytes.len());
        let full_peak = PNG_WIDTH as usize * PNG_HEIGHT as usize * 7 + bytes.len();
        assert!(budget.max_content_size < full_peak);
        assert!(decode_ocr_input(&bytes, &SecurityLimits::default()).is_none());
        let full = load_image_for_ocr(&bytes, &SecurityLimits::default())
            .expect("full-frame control")
            .into_rgb8();
        let rows = decode_ocr_input(&bytes, &budget)
            .expect("budget must select the row path")
            .expect("RGB and bounded scratch fit");
        assert_eq!(rows, expected);
        assert_eq!(rows, full);
    }

    #[test]
    fn rgba_png_rejects_a_budget_smaller_than_final_rgb() {
        let bytes = rgba_png(&rgba_pixels(), false);
        let budget = limits(PNG_WIDTH as usize * PNG_HEIGHT as usize * 3 - 1);
        assert!(bytes.len() < budget.max_content_size);
        let error = load_image_for_ocr(&bytes, &budget).expect_err("RGB alone exceeds the budget");
        assert!(matches!(error, XbergError::Validation { .. }), "{error}");
        assert!(error.to_string().contains(&budget.max_content_size.to_string()));
    }

    #[test]
    fn rgba_png_rows_reject_corrupt_idat_and_incomplete_streams() {
        let bytes = rgba_png(&rgba_pixels(), false);
        let mut offset = 8;
        let (data_start, data_end) = loop {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            if &bytes[offset + 4..offset + 8] == b"IDAT" {
                break (offset + 8, offset + 8 + length);
            }
            offset += 12 + length;
        };
        let mut corrupt_crc = bytes.clone();
        corrupt_crc[data_end] ^= 1;
        let short_idat = bytes[..data_start + (data_end - data_start) / 2].to_vec();
        let short_iend = bytes[..bytes.len() - 1].to_vec();
        for invalid in [corrupt_crc, short_idat, short_iend] {
            let error = load_image_for_ocr(&invalid, &row_budget(invalid.len()))
                .expect_err("partial or corrupt PNG must never produce accepted OCR pixels");
            assert!(matches!(error, XbergError::Parsing { .. }), "{error}");
        }
    }

    #[test]
    fn apng_separate_default_frame_is_identical_under_high_and_low_budgets() {
        let pixels = rgba_pixels();
        let expected = image::DynamicImage::ImageRgba8(pixels.clone()).into_rgb8();
        let bytes = rgba_png(&pixels, true);
        let budget = row_budget(bytes.len());
        assert!(budget.max_content_size < PNG_WIDTH as usize * PNG_HEIGHT as usize * 7 + bytes.len());
        let full = load_image_for_ocr(&bytes, &SecurityLimits::default())
            .expect("APNG default-frame control")
            .into_rgb8();
        assert_eq!(
            full, expected,
            "the static decode must choose the separate default frame"
        );
        let rows = load_image_for_ocr(&bytes, &budget)
            .expect("a smaller admissible budget must preserve default-frame compatibility")
            .into_rgb8();
        assert_eq!(rows, full);
    }
}
