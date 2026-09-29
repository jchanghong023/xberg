//! A stencil image mask (`/ImageMask true`) drawn into the separation plates
//! leaves its unmarked pixels transparent: the plate under them keeps its
//! value (ISO 32000-1 §8.9.6.2).

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{SeparationPlate, render_separations};

fn sample(plate: &SeparationPlate, x: u32, y: u32) -> u8 {
    plate.data[(y * plate.width + x) as usize]
}

fn plate<'a>(plates: &'a [SeparationPlate], name: &str) -> &'a SeparationPlate {
    plates.iter().find(|p| p.ink_name == name).unwrap_or_else(|| {
        panic!(
            "missing plate {name:?}; have {:?}",
            plates.iter().map(|p| p.ink_name.as_str()).collect::<Vec<_>>()
        )
    })
}

/// One 100x100 page. The content first paints `under` (may be empty), then
/// draws an 8x8 stencil over the square 25..75 with the spot ink at tint 1.
/// Each stencil row is `0x0F`: the left four pixels are marked (sample 0),
/// the right four are transparent (sample 1).
fn build(under: &str) -> Vec<u8> {
    let stencil: Vec<u8> = vec![0x0F; 8];
    let content = format!("{under}q\n/CS1 cs\n1 scn\n50 0 0 50 25 25 cm\n/Im1 Do\nQ\n");
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
           /Contents 4 0 R \
           /Resources << /XObject << /Im1 5 0 R >> \
                        /ColorSpace << /CS1 6 0 R >> >> >>\nendobj\n",
    );
    offsets.push(buf.len());
    buf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XObject /Subtype /Image /Width 8 /Height 8 \
             /ImageMask true /BitsPerComponent 1 /Length {} >>\nstream\n",
            stencil.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&stencil);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"6 0 obj\n[/Separation /Spot-A /DeviceCMYK 7 0 R]\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        b"7 0 obj\n<< /FunctionType 2 /Domain [0 1] /N 1 \
            /C0 [0 0 0 0] /C1 [0 0.85 0.45 0] >>\nendobj\n",
    );
    let xref_offset = buf.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for off in &offsets {
        buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    buf
}

fn spot(under: &str) -> SeparationPlate {
    let doc = PdfDocument::from_bytes(build(under)).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    plate(&plates, "Spot-A").clone()
}

#[test]
fn stencil_leaves_its_unmarked_pixels_off_an_empty_plate() {
    let p = spot("");
    assert!(
        sample(&p, 30, 50) > 200,
        "marked stencil pixel paints the spot plate; got {}",
        sample(&p, 30, 50)
    );
    assert_eq!(
        sample(&p, 70, 50),
        0,
        "an unmarked stencil pixel must leave the empty plate at no ink"
    );
}

#[test]
fn stencil_leaves_the_ink_under_its_unmarked_pixels_unchanged() {
    // Paint the right half of the image area at tint 0.25 first, then draw
    // the stencil at tint 1 over it. Under the unmarked pixels the plate
    // keeps the tint 0.25 value (64), not 64 plus the stencil's colour.
    let p = spot("q\n/CS1 cs\n0.25 scn\n50 0 50 100 re\nf\nQ\n");
    let under = sample(&p, 90, 50);
    assert!(
        (60..=68).contains(&under),
        "control: the rectangle outside the image paints tint 0.25; got {under}"
    );
    assert_eq!(
        sample(&p, 70, 50),
        under,
        "an unmarked stencil pixel must keep the ink under it"
    );
}
