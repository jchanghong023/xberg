//! A colour-key `/Mask` on an `/Indexed` image was never applied.
//!
//! ISO 32000-1 §8.9.6.4 states a colour-key mask's ranges in the image's raw
//! pre-Decode component space. For an `/Indexed` colour space that space has
//! exactly one component: the raw palette index (§7.4.5). Extraction expands
//! those indices to RGB for on-screen display and, in doing so, cleared the
//! `samples_are_raw` flag the renderer's colour-key path gates on -- so the
//! mask was always skipped for an Indexed image, and pixels the file marked
//! transparent were painted with their palette colour instead.
//!
//! Reproducer (GH#1899): a 2x2 `/Indexed /DeviceRGB` image, every pixel at
//! palette index 0 (pure red), with `/Mask [0 0]` marking index 0 as the
//! colour key. Every pixel must be masked fully transparent, leaving the
//! white page background visible underneath.

use xberg_native_pdf::PdfDocument;
use xberg_native_pdf::rendering::{ImageFormat, RenderOptions, render_page};

/// Build a 100x100pt one-page PDF painting a 2x2 `/Indexed /DeviceRGB` image
/// (palette index 0 = pure red) at the unit square via `50 0 0 50 25 25 cm`,
/// with a colour-key `/Mask [0 0]` keying out index 0.
fn build_pdf_with_indexed_color_key_mask() -> Vec<u8> {
    let content = b"q\n50 0 0 50 25 25 cm\n/Im1 Do\nQ\n";
    let indices: [u8; 4] = [0, 0, 0, 0];
    let palette: [u8; 3] = [255, 0, 0];

    let mut buf = Vec::new();
    let mut offsets = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
           /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\nendobj\n",
    );
    offsets.push(buf.len());
    let hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(hdr.as_bytes());
    buf.extend_from_slice(content);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    offsets.push(buf.len());
    let img_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /Width 2 /Height 2 \
         /ColorSpace 6 0 R /BitsPerComponent 8 /Mask [0 0] /Length {} >>\nstream\n",
        indices.len()
    );
    buf.extend_from_slice(img_hdr.as_bytes());
    buf.extend_from_slice(&indices);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"6 0 obj\n[/Indexed /DeviceRGB 0 7 0 R]\nendobj\n");
    offsets.push(buf.len());
    let pal_hdr = format!("7 0 obj\n<< /Length {} >>\nstream\n", palette.len());
    buf.extend_from_slice(pal_hdr.as_bytes());
    buf.extend_from_slice(&palette);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = buf.len();
    buf.extend_from_slice(b"xref\n");
    buf.extend_from_slice(format!("0 {}\n", offsets.len() + 1).as_bytes());
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            offsets.len() + 1,
            xref_offset
        )
        .as_bytes(),
    );
    buf
}

fn render_rgba_100(doc: &PdfDocument) -> Vec<u8> {
    let opts = RenderOptions::with_dpi(72).as_raw();
    let img = render_page(doc, 0, &opts).expect("render_page");
    assert_eq!(img.format, ImageFormat::RawRgba8);
    assert_eq!(img.data.len(), 100 * 100 * 4, "expected a 100x100 RGBA raster");
    img.data
}

fn pixel_at(rgba: &[u8], x: u32, y: u32) -> (u8, u8, u8, u8) {
    let off = ((y * 100 + x) * 4) as usize;
    (rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3])
}

#[test]
fn indexed_image_color_key_mask_keys_out_index_zero() {
    let doc = PdfDocument::from_bytes(build_pdf_with_indexed_color_key_mask()).expect("parse PDF");
    let rgba = render_rgba_100(&doc);

    // Centre of the painted 25..75 square: every source pixel is palette
    // index 0, keyed transparent by /Mask [0 0]. A working mask leaves the
    // white page background visible; the pre-fix behaviour paints it red. ~keep
    let (r, g, b, _a) = pixel_at(&rgba, 50, 50);
    assert!(
        r > 240 && g > 240 && b > 240,
        "index-0 pixels must stay white under /Mask [0 0]; got rgb({r},{g},{b}) \
         (red indicates the colour-key mask was skipped)"
    );
}
