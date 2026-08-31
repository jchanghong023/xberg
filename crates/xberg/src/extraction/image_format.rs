//! Shared image format detection for Office document extractors.

use std::borrow::Cow;

const PLACEABLE_WMF_KEY: [u8; 4] = [0xD7, 0xCD, 0xC6, 0x9A];
const WMF_HEADER_BYTES: usize = 18;
const EMF_HEADER_MIN_BYTES: usize = 88;

fn read_u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    Some(u16::from_le_bytes(data.get(offset..end)?.try_into().ok()?))
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(data.get(offset..end)?.try_into().ok()?))
}

fn is_standard_wmf(data: &[u8]) -> bool {
    if data.len() < WMF_HEADER_BYTES {
        return false;
    }

    let Some(mt_type) = read_u16_le(data, 0) else {
        return false;
    };
    let Some(mt_header_size) = read_u16_le(data, 2) else {
        return false;
    };
    let Some(mt_version) = read_u16_le(data, 4) else {
        return false;
    };
    let Some(mt_size_words) = read_u32_le(data, 6) else {
        return false;
    };
    let Some(mt_max_record_words) = read_u32_le(data, 12) else {
        return false;
    };
    let Some(mt_size_bytes) = usize::try_from(mt_size_words)
        .ok()
        .and_then(|words| words.checked_mul(2))
    else {
        return false;
    };
    let max_record_fits = usize::try_from(mt_max_record_words)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .zip(mt_size_bytes.checked_sub(WMF_HEADER_BYTES))
        .is_some_and(|(record_bytes, records_bytes)| record_bytes <= records_bytes);

    matches!(mt_type, 1 | 2)
        && mt_header_size == 9
        && matches!(mt_version, 0x0100 | 0x0300)
        && mt_size_bytes >= WMF_HEADER_BYTES
        && mt_size_bytes <= data.len()
        && mt_max_record_words >= 3
        && max_record_fits
}

fn is_emf(data: &[u8]) -> bool {
    if data.len() < EMF_HEADER_MIN_BYTES {
        return false;
    }

    let Some(record_type) = read_u32_le(data, 0) else {
        return false;
    };
    let Some(header_size) = read_u32_le(data, 4).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };
    let Some(total_bytes) = read_u32_le(data, 48).and_then(|value| usize::try_from(value).ok()) else {
        return false;
    };

    record_type == 1
        && header_size >= EMF_HEADER_MIN_BYTES
        && header_size <= data.len()
        && data.get(40..44) == Some(b" EMF")
        && total_bytes >= header_size
        && total_bytes <= data.len()
        && data.get(8..24).is_some()
        && data.get(24..40).is_some()
}

/// Detect image format from raw bytes using magic byte signatures.
///
/// Returns a format string like "jpeg", "png", etc. Used by both DOCX and PPTX extractors.
pub(crate) fn detect_image_format(data: &[u8]) -> Cow<'static, str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Cow::Borrowed("jpeg")
    } else if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Cow::Borrowed("png")
    } else if data.starts_with(b"GIF") {
        Cow::Borrowed("gif")
    } else if data.starts_with(b"BM") {
        Cow::Borrowed("bmp")
    } else if data.starts_with(b"<svg") || data.starts_with(b"<?xml") {
        Cow::Borrowed("svg")
    } else if data.starts_with(b"II\x2A\x00") || data.starts_with(b"MM\x00\x2A") {
        Cow::Borrowed("tiff")
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Cow::Borrowed("webp")
    } else if data.starts_with(&PLACEABLE_WMF_KEY) || is_standard_wmf(data) {
        Cow::Borrowed("wmf")
    } else if is_emf(data) {
        Cow::Borrowed("emf")
    } else {
        Cow::Borrowed("unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard_wmf(mt_type: u16) -> Vec<u8> {
        let mut data = vec![0_u8; 24];
        data[0..2].copy_from_slice(&mt_type.to_le_bytes());
        data[2..4].copy_from_slice(&9_u16.to_le_bytes());
        data[4..6].copy_from_slice(&0x0300_u16.to_le_bytes());
        data[6..10].copy_from_slice(&12_u32.to_le_bytes());
        data[12..16].copy_from_slice(&3_u32.to_le_bytes());
        data
    }

    fn emf() -> Vec<u8> {
        let mut data = vec![0_u8; EMF_HEADER_MIN_BYTES];
        data[0..4].copy_from_slice(&1_u32.to_le_bytes());
        data[4..8].copy_from_slice(&(EMF_HEADER_MIN_BYTES as u32).to_le_bytes());
        data[40..44].copy_from_slice(b" EMF");
        data[48..52].copy_from_slice(&(EMF_HEADER_MIN_BYTES as u32).to_le_bytes());
        data
    }

    #[test]
    fn detects_raster_formats() {
        assert_eq!(detect_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpeg");
        assert_eq!(detect_image_format(&[0x89, 0x50, 0x4E, 0x47]), "png");
        assert_eq!(detect_image_format(b"GIF89a"), "gif");
        assert_eq!(detect_image_format(b"BM\0\0"), "bmp");
        assert_eq!(detect_image_format(b"II\x2A\x00"), "tiff");
        assert_eq!(detect_image_format(b"MM\x00\x2A"), "tiff");
        assert_eq!(detect_image_format(b"RIFF\0\0\0\0WEBP"), "webp");
    }

    #[test]
    fn detects_placeable_and_standard_wmf() {
        assert_eq!(detect_image_format(&PLACEABLE_WMF_KEY), "wmf");
        assert_eq!(detect_image_format(&standard_wmf(1)), "wmf");
        assert_eq!(detect_image_format(&standard_wmf(2)), "wmf");
    }

    #[test]
    fn rejects_invalid_standard_wmf() {
        let valid = standard_wmf(1);
        assert_eq!(detect_image_format(&valid[..17]), "unknown");

        let mut bad_header = valid.clone();
        bad_header[2..4].copy_from_slice(&8_u16.to_le_bytes());
        assert_eq!(detect_image_format(&bad_header), "unknown");

        let mut bad_version = valid.clone();
        bad_version[4..6].copy_from_slice(&0x0200_u16.to_le_bytes());
        assert_eq!(detect_image_format(&bad_version), "unknown");

        let mut oversized = valid;
        oversized[6..10].copy_from_slice(&13_u32.to_le_bytes());
        assert_eq!(detect_image_format(&oversized), "unknown");
    }

    #[test]
    fn validates_emf_header() {
        assert_eq!(detect_image_format(&emf()), "emf");
        let valid = emf();
        assert_eq!(detect_image_format(&valid[..44]), "unknown");

        let mut oversized = valid;
        oversized[48..52].copy_from_slice(&89_u32.to_le_bytes());
        assert_eq!(detect_image_format(&oversized), "unknown");
    }

    #[test]
    fn rejects_random_and_empty_data() {
        assert_eq!(detect_image_format(b"random data"), "unknown");
        assert_eq!(detect_image_format(b""), "unknown");
    }
}
