//! A transparent image pixel shows the page under it. The page pixmap holds premultiplied colour,
//! so an image with straight alpha must be converted before it is drawn, or a transparent pixel
//! adds its own colour to the page.

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{RenderOptions, render_page};

const WIDTH: u32 = 40;
const HEIGHT: u32 = 20;

/// A one-page PDF painted red, with a white 8-bit RGB image over all of it whose `/SMask` is
/// `left_opacity` on the left half and fully opaque on the right half.
fn pdf(left_opacity: u8) -> Vec<u8> {
    pdf_with_image(WIDTH, HEIGHT, WIDTH / 2, left_opacity)
}

/// A one-page PDF painted red, with a white `image_width` x `image_height` image drawn over all of
/// it. Its `/SMask` is `left_opacity` left of pixel column `edge` and fully opaque from it on.
fn pdf_with_image(image_width: u32, image_height: u32, edge: u32, left_opacity: u8) -> Vec<u8> {
    let image = vec![255u8; (image_width * image_height * 3) as usize];
    let row: Vec<u8> = (0..image_width)
        .map(|x| if x < edge { left_opacity } else { 255 })
        .collect();
    let mask = row.repeat(image_height as usize);
    let content = format!("1 0 0 rg\n0 0 {WIDTH} {HEIGHT} re f\nq\n{WIDTH} 0 0 {HEIGHT} 0 0 cm\n/Im1 Do\nQ\n");
    let mut buf: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {WIDTH} {HEIGHT}] /Contents 4 0 R \
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
    for (number, color_space, samples, extra) in [
        (5, "/DeviceRGB", &image, " /SMask 6 0 R"),
        (6, "/DeviceGray", &mask, ""),
    ] {
        offsets.push(buf.len());
        buf.extend_from_slice(
            format!(
                "{number} 0 obj\n<< /Type /XObject /Subtype /Image /Width {image_width} /Height {image_height} \
                 /BitsPerComponent 8 /ColorSpace {color_space}{extra} /Length {} >>\nstream\n",
                samples.len()
            )
            .as_bytes(),
        );
        buf.extend_from_slice(samples);
        buf.extend_from_slice(b"\nendstream\nendobj\n");
    }
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

/// The rendered RGB of the middle row of the page at `dpi`, one entry per pixel column.
fn middle_row(pdf: &[u8], dpi: u32) -> Vec<[u8; 3]> {
    let doc = PdfDocument::from_bytes(pdf.to_vec()).expect("fixture parses");
    let page = render_page(&doc, 0, &RenderOptions::with_dpi(dpi).as_raw()).expect("render page 0");
    let row = page.height / 2 * page.width;
    (0..page.width)
        .map(|x| {
            let i = ((row + x) * 4) as usize;
            [page.data[i], page.data[i + 1], page.data[i + 2]]
        })
        .collect()
}

/// The rendered RGB of the pixel at the centre of the left half and of the right half.
fn left_and_right(pdf: Vec<u8>) -> ([u8; 3], [u8; 3]) {
    let row = middle_row(&pdf, 72);
    (row[(WIDTH / 4) as usize], row[(WIDTH * 3 / 4) as usize])
}

#[test]
fn a_fully_transparent_image_pixel_shows_the_page_colour() {
    let (left, right) = left_and_right(pdf(0));
    assert_eq!(
        left,
        [255, 0, 0],
        "the soft mask hides the white image, so the red page shows"
    );
    assert_eq!(right, [255, 255, 255], "control: the opaque half paints white");
}

#[test]
fn a_half_transparent_image_pixel_blends_with_the_page_colour() {
    let (left, _) = left_and_right(pdf(128));
    assert_eq!(left[0], 255, "white over red keeps full red");
    assert!(
        (126..=130).contains(&left[1]) && left[1] == left[2],
        "half white over red is pink, not white: got {left:?}"
    );
}

/// An image drawn smaller than its pixel size is resized before it is drawn. The resized edge
/// between the hidden and the opaque half must blend with the page the same way as when the image
/// is drawn at its own size, not paint its full colour over the page. (GH#1905)
#[test]
fn a_downscaled_soft_mask_edge_blends_like_a_full_size_draw() {
    // Twice the page's size in pixels, so 72 dpi draws it at half size and 144 dpi at its own.
    // The odd edge column puts the edge inside one 72 dpi pixel. ~keep
    let pdf = pdf_with_image(WIDTH * 2, HEIGHT * 2, WIDTH + 1, 0);
    let small = middle_row(&pdf, 72);
    let large = middle_row(&pdf, 144);
    let edge = (WIDTH / 2) as usize;
    let expected = (u32::from(large[2 * edge][1]) + u32::from(large[2 * edge + 1][1])) / 2;
    let got = small[edge];
    assert_eq!(got[0], 255, "white over red keeps full red, got {got:?}");
    assert!(
        u32::from(got[1]).abs_diff(expected) <= 16 && got[1] == got[2],
        "the half-covered edge pixel is pink like the full-size draw (green near {expected}), got {got:?}"
    );
    assert_eq!(
        small[edge / 2],
        [255, 0, 0],
        "control: the hidden half shows the red page"
    );
    assert_eq!(
        small[edge * 3 / 2],
        [255, 255, 255],
        "control: the opaque half paints white"
    );
}
