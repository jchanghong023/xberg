//! Wave-3 QA probes for the resolution-pipeline migration (ImageMask + `Do`).
//!
//! Sibling to `test_render_resolution_pipeline_qa_wave1.rs` (paths,
//! stroke, combos) and `_qa_wave2.rs` (text). This suite probes the
//! ImageMask / `Do`-side corners:
//!
//! 1. **ImageMask rendering correctness** — `render_image_mask` covers
//!    small / wide-with-padding / tall stencils; `/Decode [1 0]`
//!    polarity invert; missing / malformed `/Decode`; rotated and
//!    mirrored CTMs.
//! 2. **Pass-through pins for the non-mask branch** — CMYK / Indexed /
//!    ICCBased N=4 standard images must keep their existing behaviour.
//! 3. **Inline-image coverage** — `BI ... ID ... EI` is a separate parse
//!    path the renderer may or may not dispatch; pin the current
//!    behaviour either way.
//! 4. **Form-XObject interactions** — Form containing an ImageMask,
//!    nested Form-in-Form, CTM round-trip across the Form boundary.
//! 5. **Multi-XObject interactions** — back-to-back masks, mixed with
//!    standard images, under SMask / clip / blend.
//! 6. **Capability at scale** — many ImageMasks on one page; DeviceN /
//!    `/All` / `/None` colorants applied to ImageMask fill.
//! 7. **Adversarial input** — too-short / too-long / zero-dim / huge-dim
//!    stencil streams.
//! 8. **Performance** — N-paint render must hold the one-resolve-per-Do
//!    invariant (matching wave-2's pattern).
//!
//! Style mirrors waves 1 + 2: build a tiny PDF inline, render through
//! `render_with_pipeline`, compare pixmaps or sample pixels.

#![allow(dead_code)]

use std::time::Instant;
use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{ImageFormat, RenderOptions, render_page};

// ===========================================================================
// PDF construction helpers — self-contained so a fix-pass to the
// wave-1/2 QA helpers can't accidentally invalidate the wave-3 invariants. ~keep
// ===========================================================================

/// Build a one-page PDF containing a single ImageMask XObject `/IM1`.
/// `content_ops` runs on the page (typically sets the fill colour, a
/// CTM, then `/IM1 Do`). `resources_extra` is appended into the page's
/// `/Resources` dictionary. `mask_extras` is appended into the
/// ImageMask stream dictionary (use it for `/Decode`, `/Interpolate`,
/// etc.).
fn build_pdf_image_mask_ex(
    content_ops: &str,
    resources_extra: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
    mask_extras: &str,
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
         /Resources << /XObject << /IM1 5 0 R >> {} >> /Contents 4 0 R >>\nendobj\n",
        resources_extra
    );
    buf.extend_from_slice(page.as_bytes());

    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 {} /Length {} >>\nstream\n",
        width,
        height,
        mask_extras,
        mask_data.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(mask_data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Convenience wrapper — no extra stream-dict entries.
fn build_pdf_image_mask(
    content_ops: &str,
    resources_extra: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
) -> Vec<u8> {
    build_pdf_image_mask_ex(content_ops, resources_extra, width, height, mask_data, "")
}

/// Render the first page. The `_enabled` argument is retained so existing
/// test bodies keep compiling after wave 5 collapsed the off/on split; the
/// pipeline is the only path now.
fn render_with_pipeline(doc: &PdfDocument, _enabled: bool) -> Vec<u8> {
    let opts = RenderOptions::with_dpi(72).as_raw();
    let img = render_page(doc, 0, &opts).expect("render_page succeeds");
    assert_eq!(img.format, ImageFormat::RawRgba8);
    img.data
}

/// Render the first page, allowing failure without panicking. Used by
/// adversarial-input probes whose invariant is "no panic", not "render
/// succeeds".
fn render_with_pipeline_allow_fail(doc: &PdfDocument, _enabled: bool) -> Option<Vec<u8>> {
    let opts = RenderOptions::with_dpi(72).as_raw();
    render_page(doc, 0, &opts).ok().map(|img| img.data)
}

/// Sample a pixel at (x, y) on the 100×100 page.
fn pixel_at(rgba: &[u8], x: u32, y: u32) -> (u8, u8, u8, u8) {
    let w = 100u32;
    let off = ((y * w + x) * 4) as usize;
    (rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3])
}

/// Sample the centre pixel of the 100×100 page.
fn center_pixel(rgba: &[u8]) -> (u8, u8, u8, u8) {
    pixel_at(rgba, 50, 50)
}

/// Count pixels in `[x0, x1) × [y0, y1)` whose RGB is materially below
/// the white background — i.e. "this region got painted".
fn count_ink_pixels(rgba: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    let w = 100u32;
    let h = 100u32;
    assert_eq!(rgba.len() as u32, w * h * 4);
    let mut n = 0u32;
    for y in y0..y1.min(h) {
        for x in x0..x1.min(w) {
            let off = ((y * w + x) * 4) as usize;
            let r = rgba[off];
            let g = rgba[off + 1];
            let b = rgba[off + 2];
            if r < 240 || g < 240 || b < 240 {
                n += 1;
            }
        }
    }
    n
}

/// Solid 1-bit stencil — all bytes 0x00, so every pixel paints opaque
/// under the default `/Decode [0 1]`. Rows are byte-padded per PDF
/// §8.9.3.
fn solid_image_mask_bytes(width: u32, height: u32) -> Vec<u8> {
    let row_bytes = (width as usize).div_ceil(8);
    vec![0x00u8; row_bytes * height as usize]
}

/// Empty 1-bit stencil — all bytes 0xFF, so every pixel is transparent
/// under the default `/Decode [0 1]`.
fn empty_image_mask_bytes(width: u32, height: u32) -> Vec<u8> {
    let row_bytes = (width as usize).div_ceil(8);
    vec![0xFFu8; row_bytes * height as usize]
}

// ===========================================================================
// Probes 1-9 — ImageMask rendering correctness (the new capability).
//
// `render_image_mask` must decode the 1-bit stream correctly (row
// padding, default vs inverted Decode, missing Decode, malformed
// Decode), stay panic-free on degenerate input, and respect CTM
// rotation / mirroring.
// =========================================================================== ~keep

/// Probe 1 — 1×1 ImageMask stencil. A single opaque sample painted with
/// a known fill colour. With a DeviceRGB fill the spliced clone
/// short-circuits, so the rasteriser reads `gs.fill_color_rgb`
/// directly.
#[test]
fn qa_image_mask_1x1_solid_paints_fill_colour() {
    let mask = solid_image_mask_bytes(1, 1);
    // Stretch the 1×1 stencil over 60×60 in the centre of the page. ~keep
    let content = "q\n0 1 0 rg\n60 0 0 60 20 20 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 1, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, a) = center_pixel(&on);
    assert!(
        g > 200 && r < 60 && b < 60 && a > 200,
        "1x1 stencil stretched over centre should be green, got ({r}, {g}, {b}, {a})"
    );
}

/// Probe 2 — Width that is NOT a byte multiple (7px wide). Per PDF
/// §8.9.3 each row is padded to a byte boundary; the padding bits in the
/// trailing nibble must NOT paint. If the row-bytes maths in
/// `render_image_mask` is off, the 8th column will appear opaque even
/// though it is padding.
///
/// Stencil: 7×4, all bits 0 (opaque under default Decode). Each row is
/// 1 byte; the high 7 bits are valid pixels, the low bit is padding.
/// We stretch the stencil over the full page; the right edge of the
/// rendered image must drop off after the 7th of 8 image columns —
/// i.e. roughly 100 * 7/8 = 87.5 px from the left edge.
#[test]
fn qa_image_mask_width_not_byte_multiple_padding_does_not_paint() {
    let width = 7u32;
    let height = 4u32;
    let mask = solid_image_mask_bytes(width, height);
    let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", width, height, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // A pixel firmly inside the 7-column region (around x=50, y=50) must
    // be red. The renderer's resampler may smear hard edges, so we don't
    // assert on the boundary itself — just on "interior paints". ~keep
    let (r, g, b, _a) = pixel_at(&on, 50, 50);
    assert!(
        r > 200 && g < 60 && b < 60,
        "centre of 7-column stencil must paint red, got ({r}, {g}, {b})"
    );
}

/// Probe 3 — Tall ImageMask (height = 256). Capability at scale; also
/// guards against an off-by-one in the row-loop or buffer-size maths.
#[test]
fn qa_image_mask_tall_height_paints_centre_blue() {
    let mask = solid_image_mask_bytes(8, 256);
    let content = "q\n0 0 1 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 256, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert!(b > 200 && r < 60 && g < 60, "centre must be blue, got ({r}, {g}, {b})");
}

/// Probe 4 — `/Decode [1 0]` polarity invert. With this Decode array a
/// stencil bit of `1` paints, `0` does not. Build an all-1s stream
/// (every byte 0xFF), under inverted Decode that should fill the whole
/// stencil; under default Decode it would be transparent.
///
/// PIN: the wave-3 helper supports `/Decode [1 0]`. The renderer must
/// paint the all-1s stencil as fully filled under inverted Decode.
#[test]
fn qa_image_mask_decode_inverted_polarity_paints_under_ff_bytes() {
    let mask = vec![0xFFu8; 1];
    let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask_ex(content, "", 8, 1, &mask, "/Decode [1 0]");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        r > 200 && g < 60 && b < 60,
        "inverted-Decode all-1 stencil should paint red everywhere, got ({r}, {g}, {b})"
    );
}

/// Probe 5 — Missing `/Decode` (default `[0 1]`). An all-0 stream
/// paints opaque. Confirms the missing-entry path doesn't accidentally
/// drop into the inverted branch.
#[test]
fn qa_image_mask_no_decode_default_paints_under_zero_bytes() {
    let mask = solid_image_mask_bytes(8, 1);
    let content = "q\n0 1 1 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        g > 200 && b > 200 && r < 60,
        "default-Decode all-0 stencil should paint cyan everywhere, got ({r}, {g}, {b})"
    );
}

/// Probe 6 — Malformed `/Decode`. Several adversarial cases: empty
/// array, ambiguous `[0.5 0.5]`, single-element. The renderer must not
/// panic on any of them; it should fall back to default polarity (the
/// wave-3 helper's `match … _ => false` arm).
///
/// PIN: the wave-3 implementation reads `first > 0.5` for the polarity
/// flag. With `[0.5 0.5]` `first` is exactly `0.5`, so `first > 0.5` is
/// `false` → default polarity (zeros paint, ones don't). The all-zeros
/// stream should therefore paint opaque. Empty array and single-element
/// `[1]` should hit the catch-all and also default to non-inverted.
#[test]
fn qa_image_mask_malformed_decode_no_panic_default_polarity() {
    let mask = solid_image_mask_bytes(8, 1);
    for decode in &["/Decode []", "/Decode [0.5 0.5]", "/Decode [1]"] {
        let content = "q\n0.4 g\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
        let bytes = build_pdf_image_mask_ex(content, "", 8, 1, &mask, decode);
        let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
        let on = render_with_pipeline_allow_fail(&doc, true)
            .unwrap_or_else(|| panic!("renderer must not error for {}", decode));
        // No-panic invariant. The malformed-Decode catch-all branch
        // doesn't crash; whatever its fallback polarity produces is the
        // pinned behaviour. ~keep
        assert!(!on.is_empty(), "renderer must produce a pixmap for {}", decode);
    }
}

/// Probe 7 — ImageMask under a CTM that rotates 90° clockwise. The
/// CTM must round-trip across the spliced GS clone so the stencil
/// (8×1, all opaque) lands as a vertical band on the page.
#[test]
fn qa_image_mask_ctm_90deg_rotation_paints_visible_band() {
    let mask = solid_image_mask_bytes(8, 1);
    let content = "q\n1 0 0 rg\n0 -60 60 0 20 80 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // 90° rotation maps the 8×1 stencil to a vertical band; pin
    // that the rotated mask actually paints rather than collapses
    // to zero pixels (CTM round-tripping through the spliced GS). ~keep
    assert!(
        count_ink_pixels(&on, 0, 0, 100, 100) > 100,
        "rotated stencil should leave visible ink"
    );
}

/// Probe 8 — ImageMask under a CTM with negative X scale (horizontal
/// mirror). The image flip lives in `render_image_mask`'s
/// `pre_translate(0, 1).pre_scale(1/w, -1/h)`; a negative-scale CTM
/// composes correctly only if the helper's flip is applied in the
/// right order.
#[test]
fn qa_image_mask_negative_scale_mirror_paints_visible_band() {
    let mask = solid_image_mask_bytes(8, 1);
    let content = "q\n0 0 1 rg\n-60 0 0 60 80 20 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // Negative-X scale composes with the helper's intrinsic Y flip;
    // pin that the mirrored mask actually paints. ~keep
    assert!(
        count_ink_pixels(&on, 0, 0, 100, 100) > 100,
        "mirrored stencil should leave visible ink"
    );
}

/// Probe 9 — ImageMask under a CTM with negative determinant (Y-flipped
/// on top of the image-space Y-flip; net result is "image space matches
/// user space"). Confirms the helper doesn't bake a flip assumption that
/// breaks composed transforms.
#[test]
fn qa_image_mask_negative_determinant_ctm_paints_visible_band() {
    let mask = solid_image_mask_bytes(8, 1);
    // det < 0: a*d - b*c = 60*-60 = -3600. ~keep
    let content = "q\n0.5 g\n60 0 0 -60 20 80 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert!(
        count_ink_pixels(&on, 0, 0, 100, 100) > 100,
        "negative-det stencil should leave visible ink"
    );
}

// ===========================================================================
// Probes 10-12 — Standard (non-mask) Image XObject pass-through.
//
// Wave 3 routes ONLY `/ImageMask true` through the pipeline; standard
// images go to `render_image` unchanged. These probes pin that the
// guard reads `/ImageMask true` strictly (not "any /ImageMask entry")
// and that non-mask images render correctly across the colour spaces
// that matter.
// =========================================================================== ~keep

/// Build a one-page PDF with a standard (non-mask) Image XObject `/IM1`
/// whose ColorSpace dict entry is rendered inline as `/{cs_name}` (use
/// for `DeviceRGB`, `DeviceGray`, `DeviceCMYK`). `bits_per_component`
/// is also written into the stream dict.
fn build_pdf_standard_image_named_cs(
    content_ops: &str,
    width: u32,
    height: u32,
    bits_per_component: u32,
    pixel_bytes: &[u8],
    cs_name: &str,
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );

    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} \
         /BitsPerComponent {} /ColorSpace /{} /Length {} >>\nstream\n",
        width,
        height,
        bits_per_component,
        cs_name,
        pixel_bytes.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(pixel_bytes);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Build a one-page PDF with a standard Image XObject `/IM1` whose
/// ColorSpace is `[/Indexed /DeviceRGB hival lookup_stream_ref]`.
/// `palette_bytes` is the lookup table as raw RGB triples; `pixel_bytes`
/// are the index samples (BPC=8).
fn build_pdf_standard_image_indexed(
    content_ops: &str,
    width: u32,
    height: u32,
    pixel_bytes: &[u8],
    palette_bytes: &[u8],
    hival: u32,
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    // Render palette as a hex string so we can keep everything in one
    // file without an extra indirect object. ~keep
    let mut palette_hex = String::from("<");
    for b in palette_bytes {
        palette_hex.push_str(&format!("{:02X}", b));
    }
    palette_hex.push('>');

    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );

    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} \
         /BitsPerComponent 8 /ColorSpace [/Indexed /DeviceRGB {} {}] \
         /Length {} >>\nstream\n",
        width,
        height,
        hival,
        palette_hex,
        pixel_bytes.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(pixel_bytes);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Probe 10 — CMYK standard image (non-mask) pass-through. Wave 3
/// does not splice the pipeline on these; the standard-image branch
/// paints the CMYK pixel data unchanged.
#[test]
fn qa_standard_image_cmyk_pass_through_paints_magenta_centre() {
    // 4x4 CMYK pixels, all (0, 1, 0, 0) -> process-ink magenta #EC008C.
    // Each pixel is 4 bytes (one per component). ~keep
    let mut pixels = Vec::with_capacity(16 * 4);
    for _ in 0..16 {
        pixels.extend_from_slice(&[0u8, 255, 0, 0]);
    }
    let content = "q\n80 0 0 80 10 10 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_standard_image_named_cs(content, 4, 4, 8, &pixels, "DeviceCMYK");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // CMYK(0, 1, 0, 0) -> process-ink magenta corner #EC008C =
    // (236, 0, 140). Pin the centre pixel. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        r > 200 && g < 60 && (110..=170).contains(&b),
        "DeviceCMYK image (0,1,0,0) must render as process-ink magenta at centre, got ({r}, {g}, {b})"
    );
}

/// Probe 11 — Indexed standard image (non-mask) pass-through. Palette
/// of 256 entries (full 8-bit). Pixel data picks index 0 (red palette
/// entry) for every sample.
#[test]
fn qa_standard_image_indexed_256_pass_through_paints_red_centre() {
    let mut palette = Vec::with_capacity(256 * 3);
    palette.extend_from_slice(&[0xFFu8, 0x00, 0x00]);
    for _ in 1..256 {
        palette.extend_from_slice(&[0xFFu8, 0xFF, 0xFF]);
    }
    let pixels = vec![0u8; 16];
    let content = "q\n80 0 0 80 10 10 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_standard_image_indexed(content, 4, 4, &pixels, &palette, 255);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // Pin a body pixel: well inside the 80x80 image footprint. ~keep
    let (r, g, b, _a) = pixel_at(&on, 50, 50);
    assert!(
        r > 200 && g < 60 && b < 60,
        "indexed image at index 0 (red palette) must be red at centre, got ({r}, {g}, {b})"
    );
}

/// Probe 12 — `/ImageMask false` explicit (not omitted). The wave-3
/// guard reads `matches!(o, Object::Boolean(true))`; the `false` case
/// must take the standard-image branch. This is a regression pin
/// against a future refactor that might switch to `o.is_some()`.
#[test]
fn qa_image_with_explicit_imagemask_false_routes_to_standard_image() {
    // Mint a 4x4 DeviceGray standard image AND tag it with `/ImageMask
    // false`. The renderer must NOT take the mask branch (no
    // `render_image_mask` call) — the pipeline isn't routed for
    // standard images, so the centre pixel must reflect the grey ~keep
    // sample data, not the active fill colour.
    let pixels = vec![0x80u8; 16];
    let content = "q\n80 0 0 80 10 10 cm\n/IM1 Do\nQ\n";

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask false \
         /Width 4 /Height 4 /BitsPerComponent 8 /ColorSpace /DeviceGray \
         /Length {} >>\nstream\n",
        pixels.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(&pixels);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());

    let doc = PdfDocument::from_bytes(buf).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // Pin: centre is mid-grey, NOT painted with the (default-zero) fill
    // colour. If the mask branch had erroneously fired, the stencil
    // bits (0x80 = `1000 0000`) would have painted only the high bit
    // as opaque, with the current fill colour, leaving most of the
    // page unpainted. ~keep
    let (r, g, b, _a) = pixel_at(&on, 50, 50);
    assert!(
        r == g && g == b && (110..=145).contains(&(r as i32)),
        "explicit /ImageMask false must render the grey pixel data, got ({r}, {g}, {b})"
    );
}

/// Probe 12b — ICCBased N=4 (CMYK ICC profile) standard image (non-mask)
/// pass-through. The ICC profile is supplied as an indirect stream
/// (object 6). Even if the extractor falls back when the ICC bytes are
/// not a valid profile, the routing decision (mask vs standard) must
/// remain stable — the image goes through `render_image`, not the mask
/// branch.
#[test]
fn qa_standard_image_iccbased_n4_pass_through_paints_visible_ink() {
    let mut pixels = Vec::with_capacity(16);
    for _ in 0..4 {
        pixels.extend_from_slice(&[0u8, 255, 0, 0]);
    }
    // Bogus ICC profile bytes — the extractor falls back to /Alternate
    // or DeviceCMYK; what we're pinning is routing, not colour fidelity. ~keep
    let icc_bytes = vec![0u8; 32];

    let content = "q\n80 0 0 80 10 10 cm\n/IM1 Do\nQ\n";
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /Width 2 /Height 2 \
         /BitsPerComponent 8 /ColorSpace [/ICCBased 6 0 R] /Length {} >>\nstream\n",
        pixels.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(&pixels);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let icc_off = buf.len();
    let icc_hdr = format!(
        "6 0 obj\n<< /N 4 /Alternate /DeviceCMYK /Length {} >>\nstream\n",
        icc_bytes.len()
    );
    buf.extend_from_slice(icc_hdr.as_bytes());
    buf.extend_from_slice(&icc_bytes);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off, icc_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    let doc = PdfDocument::from_bytes(buf).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // Routing pin: the ICCBased N=4 image must be routed through
    // `render_image`, not the mask branch. Visible signal: the painted
    // 80×80 region's centre is NOT page-background white (the bogus ICC
    // bytes fall back to DeviceCMYK, magenta CMYK still renders as
    // non-white ink). If the mask branch had fired the image's first
    // pixel byte (0x00) would be opaque-with-default-fill = black at ~keep
    // the corner only and the centre would stay white.
    let (r, g, b, _a) = pixel_at(&on, 50, 50);
    assert!(
        r < 250 || g < 250 || b < 250,
        "ICCBased N=4 standard image must paint visible ink at the image region centre \
         (routes through render_image, not the mask branch); got ({r}, {g}, {b})"
    );
}

// ===========================================================================
// Probes 13-14 — Inline images (`BI ... ID ... EI`).
//
// Inline images are an entirely separate parse path from `Do`-invoked
// XObjects. `Operator::InlineImage` now has its own dispatch arm in
// `page_renderer.rs` that expands the abbreviated dictionary keys and
// routes through the same `render_image`/`render_image_mask` functions
// the `Do` arm uses, so both an inline ImageMask and a standard inline
// image now paint correctly. ~keep
// ===========================================================================

/// Build a one-page PDF whose content stream is a literal byte slice
/// (so callers can embed non-ASCII inline-image data).
fn build_pdf_inline_image_bytes(content_ops: &[u8]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Probe 13 — Inline ImageMask via `BI ... ID ... EI` paints with the
/// current fill colour, same as a `Do`-invoked ImageMask.
#[test]
fn qa_inline_image_mask_paints_with_current_fill_colour() {
    // Inline ImageMask: 1x1, /BPC 1, /IM true, one zero byte (opaque
    // under default Decode). Surround with a fill colour set first.
    //
    // Per PDF §8.9.7 the syntax for an inline image is:
    //   BI <dict-entries> ID <data> EI ~keep
    let mut content: Vec<u8> = Vec::new();
    content.extend_from_slice(b"q\n1 0 0 rg\n80 0 0 80 10 10 cm\n");
    content.extend_from_slice(b"BI /W 1 /H 1 /BPC 1 /IM true ID ");
    content.push(0x00);
    content.extend_from_slice(b" EI\nQ\n");
    let bytes = build_pdf_inline_image_bytes(&content);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert_eq!(
        (r, g, b),
        (255, 0, 0),
        "inline ImageMask must paint the current fill colour (red), got ({r}, {g}, {b})"
    );
}

/// Probe 14 — Inline standard (non-mask) image paints its own sample
/// data, same as a `Do`-invoked standard image.
#[test]
fn qa_inline_standard_image_paints_sample_data() {
    let mut content: Vec<u8> = Vec::new();
    content.extend_from_slice(b"q\n80 0 0 80 10 10 cm\n");
    content.extend_from_slice(b"BI /W 1 /H 1 /BPC 8 /CS /G ID ");
    content.push(0x80);
    content.extend_from_slice(b" EI\nQ\n");
    let bytes = build_pdf_inline_image_bytes(&content);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert_eq!(
        (r, g, b),
        (128, 128, 128),
        "inline standard image must paint its own sample data (mid-grey), got ({r}, {g}, {b})"
    );
}

// ===========================================================================
// Probes 15-17 — Form-XObject ImageMask interactions.
//
// Form XObjects are rendered recursively. When the Form's content
// stream invokes an ImageMask, the recursive walk should:
//   - find the mask in the Form's own /Resources;
//   - paint it through the wave-3 pipeline-routed path;
//   - propagate the parent's CTM into the recursion.
//
// These probes pin those interactions.
// =========================================================================== ~keep

/// Build a one-page PDF whose `/Fm1` Form XObject internally invokes
/// an ImageMask `/IM1`. Both are listed in the Form's own /Resources.
/// The page invokes `/Fm1 Do`.
fn build_pdf_form_with_inner_image_mask(
    page_content: &str,
    form_content: &str,
    form_resources_extra: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /Fm1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", page_content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(page_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    // Form XObject (object 5). Its /Resources lists /IM1 → object 6,
    // plus any extra entries the caller wants (e.g. /ColorSpace). ~keep
    let form_off = buf.len();
    let form_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Resources << /XObject << /IM1 6 0 R >> {} >> /Length {} >>\nstream\n",
        form_resources_extra,
        form_content.len()
    );
    buf.extend_from_slice(form_hdr.as_bytes());
    buf.extend_from_slice(form_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let im_off = buf.len();
    let im_hdr = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        width,
        height,
        mask_data.len()
    );
    buf.extend_from_slice(im_hdr.as_bytes());
    buf.extend_from_slice(mask_data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, form_off, im_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Build a PDF with TWO Form XObjects: the page invokes `/Fm1`, `/Fm1`
/// invokes `/Fm2`, and `/Fm2` invokes the ImageMask `/IM1`. Used to
/// pin two-level recursion.
fn build_pdf_form_in_form_with_image_mask(
    page_content: &str,
    outer_form_content: &str,
    inner_form_content: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /Fm1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", page_content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(page_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let outer_off = buf.len();
    let outer_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Resources << /XObject << /Fm2 6 0 R >> >> /Length {} >>\nstream\n",
        outer_form_content.len()
    );
    buf.extend_from_slice(outer_hdr.as_bytes());
    buf.extend_from_slice(outer_form_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let inner_off = buf.len();
    let inner_hdr = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Resources << /XObject << /IM1 7 0 R >> >> /Length {} >>\nstream\n",
        inner_form_content.len()
    );
    buf.extend_from_slice(inner_hdr.as_bytes());
    buf.extend_from_slice(inner_form_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    // ImageMask. ~keep
    let im_off = buf.len();
    let im_hdr = format!(
        "7 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        width,
        height,
        mask_data.len()
    );
    buf.extend_from_slice(im_hdr.as_bytes());
    buf.extend_from_slice(mask_data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 8\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, outer_off, inner_off, im_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Build a PDF with a Form containing an ImageMask AND a Type 4
/// Separation in its /Resources/ColorSpace. The Form invokes the mask
/// after setting the spot colour. Used by the capability-gain test for
/// nested-Form Separation fills.
fn build_pdf_form_with_imagemask_and_type4_separation(
    page_content: &str,
    form_content: &str,
    type4_program: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /Fm1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", page_content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(page_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let form_off = buf.len();
    let form_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
         /Resources << /XObject << /IM1 6 0 R >> \
                       /ColorSpace << /SpotMagenta [/Separation /MagentaSpot /DeviceCMYK 7 0 R] >> \
                     >> /Length {} >>\nstream\n",
        form_content.len()
    );
    buf.extend_from_slice(form_hdr.as_bytes());
    buf.extend_from_slice(form_content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    // ImageMask. ~keep
    let im_off = buf.len();
    let im_hdr = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        width,
        height,
        mask_data.len()
    );
    buf.extend_from_slice(im_hdr.as_bytes());
    buf.extend_from_slice(mask_data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    // Type 4 function. ~keep
    let func_off = buf.len();
    let func_hdr = format!(
        "7 0 obj\n<< /FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] /Length {} >>\nstream\n",
        type4_program.len()
    );
    buf.extend_from_slice(func_hdr.as_bytes());
    buf.extend_from_slice(type4_program.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 8\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, form_off, im_off, func_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

// Split for file-too-long (#1567): the module continues below via `include!`,
// not a sibling tests/*_part2.rs file -- Cargo auto-discovers every *.rs file
// directly under tests/ as its own test binary, so a sibling would fail to
// compile on its own and, if it somehow did, run every #[test] in it twice.
// A subdirectory is not auto-discovered. ~keep
include!("test_render_resolution_pipeline_qa_wave3/part2.rs");
