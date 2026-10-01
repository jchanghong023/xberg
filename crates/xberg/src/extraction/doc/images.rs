//! Picture extraction for the legacy Word 97-2003 binary format.
//!
//! Word stores a document's pictures as self-contained blobs (PNG, JPEG) in the
//! `Data` stream, and an embedded OLE object's visual representation as an
//! `EPRINT` (EMF preview) stream inside its `ObjectPool` storage. Neither is
//! reachable from the piece table alone, which is why the text-only path of
//! this extractor historically surfaced no images at all.
//!
//! The `Data` stream has no directory of blobs: they sit back to back with
//! Word-internal bookkeeping between them, so the scan locates each blob by its
//! signature and walks its self-delimiting structure (PNG chunk list, JPEG
//! marker stream) to find its end. A blob whose structure does not close within
//! the stream is dropped rather than guessed at.

use crate::error::Result;
use crate::types::extraction::ExtractedImage;
use bytes::Bytes;

/// PNG signature.
const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Upper bound on one scanned blob: matches the OOXML-side image size cap the
/// rest of the pipeline already enforces per image.
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

/// Scan the `Data` stream and every `ObjectPool` preview for picture blobs.
///
/// Returns the images in stream order (`Data` first, then each object storage's
/// `EPRINT` preview in storage order) with `page_number` left unset — the
/// legacy format carries no trustworthy page attribution here.
pub(super) fn extract_doc_images(comp: &mut cfb::CompoundFile<std::io::Cursor<&[u8]>>) -> Result<Vec<ExtractedImage>> {
    let mut images = Vec::new();

    let data = super::read_stream(comp, "/Data").unwrap_or_default();
    for (format, blob) in scan_raster_blobs(&data) {
        images.push(build_image(blob, format));
    }

    for path in object_pool_preview_paths(comp) {
        if let Ok(preview) = super::read_stream(comp, &path)
            && preview.len() >= 8
            && preview[0..4] == [0x01, 0x00, 0x00, 0x00]
        {
            // EMF record header: type 1 (EMR_HEADER), then the record's own
            // declared size. A preview that fails this shape is skipped, not
            // guessed at -- a wrong `emf` label would surface as a dead ref.
            let declared = u32::from_le_bytes([preview[4], preview[5], preview[6], preview[7]]) as usize;
            if declared >= 8 && declared <= preview.len().min(MAX_BLOB_BYTES) {
                images.push(build_image(preview, "emf"));
            }
        }
    }

    Ok(images)
}

/// Build one [`ExtractedImage`] with its sequential index fixed up by the
/// caller's push order.
fn build_image(data: Vec<u8>, format: &'static str) -> ExtractedImage {
    let (width, height) = image_dimensions(&data, format);
    ExtractedImage {
        format: std::borrow::Cow::Borrowed(format),
        data: Bytes::from(data),
        image_index: u32::MAX,
        page_number: Some(1),
        width,
        height,
        colorspace: None,
        bits_per_component: None,
        is_mask: false,
        description: None,
        ocr_result: None,
        bounding_box: None,
        source_path: None,
        image_kind: None,
        kind_confidence: None,
        cluster_id: None,
        caption: None,
        qr_codes: None,
        data_base64: None,
    }
}

/// Pixel dimensions when cheaply readable (PNG IHDR, JPEG SOF), else `None`.
fn image_dimensions(data: &[u8], format: &str) -> (Option<u32>, Option<u32>) {
    if format == "png" && data.len() >= 24 && &data[12..16] == b"IHDR" {
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return (Some(w), Some(h));
    }
    if format == "jpeg" {
        // Walk the marker stream to the first SOFn frame.
        let mut i = 2usize;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                break;
            }
            let marker = data[i + 1];
            if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
                return (Some(u32::from(w)), Some(u32::from(h)));
            }
            i += 2 + len;
        }
    }
    (None, None)
}

/// Every `ObjectPool/*/\x03EPRINT` stream path, in storage order.
fn object_pool_preview_paths(comp: &cfb::CompoundFile<std::io::Cursor<&[u8]>>) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in comp.walk() {
        // cfb joins entry paths with the platform separator (`\` on Windows);
        // normalize to `/` for the component walk below.
        let path = entry.path().to_string_lossy().replace('\\', "/");
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        if let (Some(pool), Some(_storage), Some(stream)) = (parts.next(), parts.next(), parts.next())
            && pool == "ObjectPool"
            && stream.ends_with("EPRINT")
        {
            paths.push(path);
        }
    }
    paths
}

/// Locate complete PNG and JPEG blobs in the `Data` stream, in offset order.
fn scan_raster_blobs(data: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let mut blobs = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        match data[i..].iter().position(|&b| b == 0x89 || b == 0xFF) {
            Some(rel) => i += rel,
            None => break,
        }
        if data[i..].len() >= PNG_SIG.len()
            && &data[i..i + PNG_SIG.len()] == PNG_SIG
            && let Some(end) = png_end(data, i)
        {
            blobs.push(("png", data[i..end].to_vec()));
            i = end;
            continue;
        }
        if data[i..].len() >= 3
            && data[i] == 0xFF
            && data[i + 1] == 0xD8
            && data[i + 2] == 0xFF
            && let Some(end) = jpeg_end(data, i)
        {
            blobs.push(("jpeg", data[i..end].to_vec()));
            i = end;
            continue;
        }
        i += 1;
    }
    blobs
}

/// Offset one past the IEND chunk of the PNG starting at `start`, when the
/// chunk walk closes inside the buffer.
fn png_end(data: &[u8], start: usize) -> Option<usize> {
    let mut offset = start + PNG_SIG.len();
    loop {
        let header = data.get(offset..offset + 8)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let chunk_type = &header[4..8];
        let body_end = offset.checked_add(8)?.checked_add(length)?;
        if body_end.saturating_sub(start) > MAX_BLOB_BYTES {
            return None;
        }
        data.get(body_end..body_end + 4)?; // CRC must fit too
        offset = body_end + 4;
        if chunk_type == b"IEND" {
            return Some(offset);
        }
    }
}

/// Offset one past the EOI marker of the JPEG starting at `start`, walking the
/// marker stream the way a decoder does.
fn jpeg_end(data: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 2;
    let mut in_scan = false;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            if in_scan {
                i += 1;
                continue;
            }
            return None; // marker stream is malformed before any image data
        }
        let marker = data[i + 1];
        match marker {
            0xFF => {
                i += 1;
                continue;
            }
            // Byte stuffing inside an entropy scan: FF 00 is literal data.
            0x00 if in_scan => {
                i += 2;
            }
            0xD9 => return Some(i + 2), // EOI
            0xD8 | 0x01 | 0xD0..=0xD7 => {
                i += 2;
            }
            _ => {
                if i + 3 >= data.len() {
                    return None;
                }
                let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
                if len < 2 {
                    return None;
                }
                if in_scan && !matches!(marker, 0xC0..=0xCF) {
                    // A structured marker ends the current entropy scan.
                    in_scan = false;
                }
                if marker == 0xDA {
                    in_scan = true;
                }
                i += 2 + len;
                // Inside a scan the entropy bytes follow directly; resume the
                // FF scan from there.
                if in_scan {
                    // skip one possible stuffing-free run quickly
                    continue;
                }
            }
        }
        if i.saturating_sub(start) > MAX_BLOB_BYTES {
            return None;
        }
    }
    None
}
