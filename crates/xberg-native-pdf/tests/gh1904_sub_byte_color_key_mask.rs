//! A colour-key `/Mask` is tested against the samples in the image's declared bit depth, also when
//! the extractor unpacked them to 8 bits or folded a `/Decode` into them. (GH#1904)

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{RenderOptions, render_page};

const WIDTH: u32 = 40;
const HEIGHT: u32 = 20;
const RED: [u8; 3] = [255, 0, 0];

/// An image whose left half has the samples `left` and whose right half has `right`, one value
/// per component, packed at `bpc` bits.
struct Image<'a> {
    bpc: u8,
    color_space: &'a str,
    left: &'a [u8],
    right: &'a [u8],
}

impl Image<'_> {
    fn stream(&self) -> Vec<u8> {
        let bpc = usize::from(self.bpc);
        let samples: Vec<u8> = (0..WIDTH as usize)
            .flat_map(|x| if x < WIDTH as usize / 2 { self.left } else { self.right })
            .copied()
            .collect();
        let mut row = vec![0u8; (samples.len() * bpc).div_ceil(8)];
        for (i, &sample) in samples.iter().enumerate() {
            let bit = i * bpc;
            row[bit / 8] |= sample << (8 - bpc - bit % 8);
        }
        row.repeat(HEIGHT as usize)
    }
}

/// A one-page PDF whose page is painted red, with the image drawn over all of it at one PDF unit
/// per pixel.
fn pdf(image: &Image, extra: &str) -> Vec<u8> {
    let samples = image.stream();
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
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XObject /Subtype /Image /Width {WIDTH} /Height {HEIGHT} \
             /BitsPerComponent {} /ColorSpace {} {extra} /Length {} >>\nstream\n",
            image.bpc,
            image.color_space,
            samples.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(&samples);
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

/// The rendered RGB of the pixel at the centre of the left half and of the right half.
fn left_and_right(pdf: Vec<u8>) -> ([u8; 3], [u8; 3]) {
    let doc = PdfDocument::from_bytes(pdf).expect("fixture parses");
    let page = render_page(&doc, 0, &RenderOptions::with_dpi(72).as_raw()).expect("render page 0");
    let at = |x: u32| {
        let i = ((HEIGHT / 2 * page.width + x) * 4) as usize;
        [page.data[i], page.data[i + 1], page.data[i + 2]]
    };
    (at(WIDTH / 4), at(WIDTH * 3 / 4))
}

/// Without the mask the right half paints `right_colour`; with it the red page shows there, while
/// the left half, outside the range, paints `left_colour` both times.
fn assert_right_half_masked(image: &Image, extra: &str, mask: &str, left_colour: [u8; 3], right_colour: [u8; 3]) {
    let label = format!("{}-bit {} {extra}", image.bpc, image.color_space);
    let (left, right) = left_and_right(pdf(image, extra));
    assert_eq!(left, left_colour, "control, {label}: the left half paints");
    assert_eq!(
        right, right_colour,
        "control, {label}: without a /Mask the right half paints"
    );

    let (left, right) = left_and_right(pdf(image, &format!("{extra} /Mask {mask}")));
    assert_eq!(
        left, left_colour,
        "{label}: the left half is outside the mask and still paints"
    );
    assert_eq!(right, RED, "{label}: the right half is masked, so the red page shows");
}

#[test]
fn a_colour_key_mask_on_a_1_bit_gray_image_hides_the_masked_value() {
    let image = Image {
        bpc: 1,
        color_space: "/DeviceGray",
        left: &[0],
        right: &[1],
    };
    assert_right_half_masked(&image, "", "[1 1]", [0, 0, 0], [255, 255, 255]);
}

#[test]
fn a_colour_key_mask_on_a_4_bit_gray_image_is_read_in_4_bit_values() {
    // 15 is white at 4 bits. A mask tested against the 8-bit unpacked sample (255) would miss it.
    let image = Image {
        bpc: 4,
        color_space: "/DeviceGray",
        left: &[0],
        right: &[15],
    };
    assert_right_half_masked(&image, "", "[15 15]", [0, 0, 0], [255, 255, 255]);
}

#[test]
fn a_colour_key_mask_on_a_2_bit_rgb_image_tests_every_component() {
    let image = Image {
        bpc: 2,
        color_space: "/DeviceRGB",
        left: &[0, 0, 3],
        right: &[0, 3, 0],
    };
    assert_right_half_masked(&image, "", "[0 0 3 3 0 0]", [0, 0, 255], [0, 255, 0]);
}

#[test]
fn a_colour_key_mask_on_an_8_bit_image_with_decode_tests_the_raw_samples() {
    // /Decode [1 0] turns raw 0 white and raw 255 black; the mask names the raw value 255.
    let image = Image {
        bpc: 8,
        color_space: "/DeviceGray",
        left: &[0],
        right: &[255],
    };
    assert_right_half_masked(&image, "/Decode [1 0]", "[255 255]", [255, 255, 255], [0, 0, 0]);
}
