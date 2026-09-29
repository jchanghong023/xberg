//! GH#1883: a BARE four-component JPEG 2000 codestream in a `/JPXDecode` image whose dictionary
//! omits `/ColorSpace` must still be read as four components.
//!
//! ISO 32000-1 Table 89 lets a `/JPXDecode` image omit `/ColorSpace`, so §7.4.9's authoritative
//! component count does not exist for such an image and extraction installs a `/DeviceRGB`
//! placeholder to let the rest of the pipeline run. Handing that placeholder's 3 to the decoder
//! asserted a count the document never made: hayro-jpeg2000 had already invented an alpha channel
//! for the bare codestream, the two agreed at 3, and the K plane was dropped. The count now comes
//! from the codestream's own `SIZ` header (ISO/IEC 15444-1 A.5.1) instead.
//!
//! This is the wiring, not the decoder: `decoders::jpx`'s own tests cover the `SIZ` read, and they
//! pass while this file fails if the placeholder's 3 is still passed down.

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::extractors::{ColorSpace, ImageData, PixelFormat};

/// A 16x16 lossless four-component raw J2K codestream, one channel saturated per quadrant. Shared
/// with `gh1839_jpx_cmyk_output_intent.rs` and `test_separation_image_rendering.rs`; bare, so it
/// carries no `colr` box declaring CMYK and no `cdef` box able to declare an alpha channel. ~keep
const CMYK_QUADRANTS_J2K: &[u8] = include_bytes!("fixtures/jpx/gh1855_cmyk_quadrants.j2k");

/// Build a single-page PDF painting `codestream` through a `/JPXDecode` image XObject that declares
/// **no** `/ColorSpace`.
fn build_pdf_with_jpx_image_no_colorspace(codestream: &[u8]) -> Vec<u8> {
    let content: &[u8] = b"q\n50 0 0 50 25 25 cm\n/Im1 Do\nQ\n";
    let mut buf: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.5\n");

    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    offsets.push(buf.len());
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
           /Resources << /XObject << /Im1 5 0 R >> >> >>\nendobj\n",
    );

    offsets.push(buf.len());
    buf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    buf.extend_from_slice(content);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XObject /Subtype /Image /Width 16 /Height 16 \
             /BitsPerComponent 8 /Filter /JPXDecode /Length {} >>\nstream\n",
            codestream.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(codestream);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = buf.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in &offsets {
        buf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
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

#[test]
fn a_bare_cmyk_jpx_image_with_no_colorspace_entry_keeps_its_fourth_plane() {
    let doc = PdfDocument::from_bytes(build_pdf_with_jpx_image_no_colorspace(CMYK_QUADRANTS_J2K))
        .expect("parse the bare-CMYK-JPX fixture");

    let images = doc.extract_images(0).expect("extract page 0");
    assert_eq!(
        images.len(),
        1,
        "the JPXDecode image must decode; extract_images silently drops an image whose extraction \
         errors, which would leave the assertion below unreached"
    );
    assert_eq!(
        *images[0].color_space(),
        ColorSpace::DeviceCMYK,
        "GH#1883: with no /ColorSpace to consult, the codestream's Csiz of 4 must be honoured \
         rather than the /DeviceRGB placeholder's 3"
    );

    match images[0].data() {
        ImageData::Raw { pixels, format } => {
            assert_eq!(*format, PixelFormat::CMYK, "the decoded buffer must be four-channel");
            assert_eq!(
                pixels.len(),
                16 * 16 * 4,
                "all four planes must reach the extracted image, not three"
            );
        }
        other => panic!("a JPXDecode image must decode to raw samples, got {other:?}"),
    }
}
