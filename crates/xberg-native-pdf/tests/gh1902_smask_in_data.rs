//! With `/SMaskInData` 1 or 2 and no `/SMask`, a JPEG 2000 image's opacity channel is its soft
//! mask. (GH#1902)

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{RenderOptions, render_page};

/// 16x16, lossless. The left half is opaque `(200, 100, 50)` and the right half is
/// `(10, 220, 90)` at opacity 128.
const RGBA_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1850_rgba.jp2");
/// The right half stores `(5, 110, 45, 128)`, the premultiplied form of `(10, 220, 90, 128)`.
const PREMULTIPLIED_RGBA_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1902_rgba_premultiplied.jp2");
const SIZE: u32 = 16;

/// A one-page PDF painted red, with the JPEG 2000 image drawn over all of it. `smask` adds a fully
/// opaque 8-bit `/SMask` image.
fn pdf_with_codestream(codestream: &[u8], image_extra: &str, smask: bool) -> Vec<u8> {
    let content = format!("1 0 0 rg\n0 0 {SIZE} {SIZE} re f\nq\n{SIZE} 0 0 {SIZE} 0 0 cm\n/Im1 Do\nQ\n");
    let smask_entry = if smask { " /SMask 6 0 R" } else { "" };
    let mut buf: Vec<u8> = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {SIZE} {SIZE}] /Contents 4 0 R \
             /Resources << /XObject << /Im1 5 0 R >> >> >>\nendobj\n"
        )
        .as_bytes(),
    );
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "4 0 obj\n<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
            content.len()
        )
        .as_bytes(),
    );
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XObject /Subtype /Image /Width {SIZE} /Height {SIZE} \
             /BitsPerComponent 8 /ColorSpace /DeviceRGB /Filter /JPXDecode{smask_entry} {image_extra} \
             /Length {} >>\nstream\n",
            codestream.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(codestream);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let opaque = vec![255u8; (SIZE * SIZE) as usize];
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XObject /Subtype /Image /Width {SIZE} /Height {SIZE} \
             /BitsPerComponent 8 /ColorSpace /DeviceGray /Length {} >>\nstream\n",
            opaque.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&opaque);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref = buf.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in &offsets {
        buf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    buf
}

fn pdf(image_extra: &str, smask: bool) -> Vec<u8> {
    pdf_with_codestream(RGBA_JP2, image_extra, smask)
}

/// The rendered RGB of the pixel at the centre of the left half and of the right half.
fn left_and_right(pdf: Vec<u8>) -> ([u8; 3], [u8; 3]) {
    let doc = PdfDocument::from_bytes(pdf).expect("fixture parses");
    let page = render_page(&doc, 0, &RenderOptions::with_dpi(72).as_raw()).expect("render page 0");
    let at = |x: u32| {
        let i = ((SIZE / 2 * page.width + x) * 4) as usize;
        [page.data[i], page.data[i + 1], page.data[i + 2]]
    };
    (at(SIZE / 4), at(SIZE * 3 / 4))
}

/// `(10, 220, 90)` at opacity 128 over red.
fn is_half_blended_over_red(rgb: [u8; 3]) -> bool {
    let expected = [132i32, 110, 45];
    rgb.iter()
        .zip(expected)
        .all(|(&got, want)| (i32::from(got) - want).abs() <= 3)
}

#[test]
fn smask_in_data_makes_the_opacity_channel_the_soft_mask() {
    let (left, right) = left_and_right(pdf("/SMaskInData 1", false));
    assert_eq!(left, [200, 100, 50], "the opaque half paints its colour");
    assert!(
        is_half_blended_over_red(right),
        "the half at opacity 128 blends with the red page, got {right:?}"
    );
}

#[test]
fn smask_in_data_two_does_not_premultiply_already_premultiplied_colour_twice() {
    let (_, right) = left_and_right(pdf_with_codestream(PREMULTIPLIED_RGBA_JP2, "/SMaskInData 2", false));
    let expected = [132i32, 110, 45];
    assert!(
        right
            .iter()
            .zip(expected)
            .all(|(&got, want)| (i32::from(got) - want).abs() <= 3),
        "the stored premultiplied colour must be composited once, got {right:?}"
    );

    let (_, straight_right) = left_and_right(pdf_with_codestream(PREMULTIPLIED_RGBA_JP2, "/SMaskInData 1", false));
    assert!(
        straight_right[1] < right[1] - 40,
        "control failed: value 1 must treat the same stored colour as straight alpha; got {straight_right:?}"
    );
}

#[test]
fn without_smask_in_data_the_opacity_channel_is_ignored() {
    for extra in ["", "/SMaskInData 0"] {
        let (left, right) = left_and_right(pdf(extra, false));
        assert_eq!(left, [200, 100, 50], "{extra:?}: the left half paints its colour");
        assert_eq!(right, [10, 220, 90], "{extra:?}: the right half paints opaque");
    }
}

#[test]
fn an_smask_entry_wins_over_smask_in_data() {
    let (_, right) = left_and_right(pdf("/SMaskInData 1", true));
    assert_eq!(
        right,
        [10, 220, 90],
        "the opaque /SMask is the soft mask, not the opacity channel"
    );
}
