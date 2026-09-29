//! An `/Indexed` image paints the separation plates of its palette's base colour space, and a spot
//! ink named only by that base still gets its plate. (GH#1915)

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{SeparationPlate, render_separations};

/// 120x40, dark text (index 0) on paper (index 255), a losslessly coded index plane.
const INDICES_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_indices_grey.jp2");

/// The same 120x40 picture coded lossily as one index component, looked up in the JP2's own CMYK
/// `pclr` palette: entry `i` is `(0, 0, 0, 255 - i)`, so the text is black ink.
const PALETTE_CMYK_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_palette_cmyk.jp2");

/// Pure cyan then pure magenta, as `/DeviceCMYK` entries.
const CYAN_MAGENTA: &str = "<FF000000 00FF0000>";

struct Image<'a> {
    /// The image's `/ColorSpace` value.
    color_space: Option<&'a str>,
    /// Extra entries for the image dictionary.
    extra: &'a str,
    width: u32,
    height: u32,
    bpc: u8,
    data: &'a [u8],
}

/// Four pixels in a row, 8-bit indices 0 0 1 1: index 0 on the left half, 1 on the right.
fn two_indices(color_space: &str) -> Image<'_> {
    Image {
        color_space: Some(color_space),
        extra: "",
        width: 4,
        height: 1,
        bpc: 8,
        data: &[0, 0, 1, 1],
    }
}

/// One 100x100 page with `image` drawn over all of it. `resources` adds entries to the page's
/// `/ColorSpace` resources, and `objects` appends indirect objects from number 6.
fn build(image: &Image<'_>, resources: &str, objects: &[&str]) -> Vec<u8> {
    let content = "q\n100 0 0 100 0 0 cm\n/Im1 Do\nQ\n";
    let mut buf = b"%PDF-1.5\n".to_vec();
    let mut offsets = Vec::new();
    offsets.push(buf.len());
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    offsets.push(buf.len());
    buf.extend_from_slice(
        format!(
            "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
             /Resources << /XObject << /Im1 5 0 R >> /ColorSpace << {resources} >> >> >>\nendobj\n"
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
    let color_space = image
        .color_space
        .map(|value| format!("/ColorSpace {value} "))
        .unwrap_or_default();
    buf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} /BitsPerComponent {} \
             {}{} /Length {} >>\nstream\n",
            image.width,
            image.height,
            image.bpc,
            color_space,
            image.extra,
            image.data.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(image.data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    for (i, object) in objects.iter().enumerate() {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 6).as_bytes());
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

fn plates(pdf: Vec<u8>) -> Vec<SeparationPlate> {
    let doc = PdfDocument::from_bytes(pdf).expect("fixture parses");
    render_separations(&doc, 0, 72).expect("render separations")
}

fn plate<'a>(plates: &'a [SeparationPlate], name: &str) -> &'a SeparationPlate {
    plates.iter().find(|p| p.ink_name == name).unwrap_or_else(|| {
        panic!(
            "missing plate {name:?}; have {:?}",
            plates.iter().map(|p| p.ink_name.as_str()).collect::<Vec<_>>()
        )
    })
}

fn at(plate: &SeparationPlate, x: u32, y: u32) -> u8 {
    plate.data[(y * plate.width + x) as usize]
}

/// The ink on `name` at the centre of the image's first pixel and of its last, away from the
/// filtered edge between the halves.
fn left_and_right(plates: &[SeparationPlate], name: &str) -> (u8, u8) {
    let p = plate(plates, name);
    (at(p, 12, 50), at(p, 88, 50))
}

#[test]
fn an_indexed_separation_image_paints_its_spot_plate() {
    // The space is named in the page resources, where the plate list is read from.
    let resources = "/CS0 [/Indexed [/Separation /Spot-A /DeviceCMYK 6 0 R] 1 <00FF>]";
    let tint = "<< /FunctionType 2 /Domain [0 1] /N 1 /C0 [0 0 0 0] /C1 [0 0.85 0.45 0] >>";
    let (left, right) = left_and_right(&plates(build(&two_indices("/CS0"), resources, &[tint])), "Spot-A");
    assert_eq!(left, 0, "entry 0 is tint 0");
    assert!(right > 200, "entry 1 is tint 1, got {right}");
}

#[test]
fn an_indexed_devicen_image_paints_its_colorant_plates() {
    let resources =
        format!("/CS0 [/Indexed [/DeviceN [/Spot-A /Spot-B /Spot-C /Spot-D] /DeviceCMYK 6 0 R] 1 {CYAN_MAGENTA}]");
    let tint =
        "<< /FunctionType 4 /Domain [0 1 0 1 0 1 0 1] /Range [0 1 0 1 0 1 0 1] /Length 2 >>\nstream\n{}\nendstream";
    let plates = plates(build(&two_indices("/CS0"), &resources, &[tint]));
    let (a_left, a_right) = left_and_right(&plates, "Spot-A");
    let (b_left, b_right) = left_and_right(&plates, "Spot-B");
    assert!(a_left > 200, "entry 0 paints the first colorant, got {a_left}");
    assert_eq!(a_right, 0, "entry 1 leaves the first colorant empty");
    assert_eq!(b_left, 0, "entry 0 leaves the second colorant empty");
    assert!(b_right > 200, "entry 1 paints the second colorant, got {b_right}");
}

#[test]
fn a_decode_array_maps_the_indices_not_the_palette_entries() {
    // `[0 255]` is the identity for an 8-bit index (ISO 32000-1 Table 90). Applied to the
    // one-component palette entries instead, it would push the half tint of entry 0 to full ink.
    let resources = "/CS0 [/Indexed [/Separation /Spot-A /DeviceCMYK 6 0 R] 1 <80FF>]";
    let tint = "<< /FunctionType 2 /Domain [0 1] /N 1 /C0 [0 0 0 0] /C1 [0 0.85 0.45 0] >>";
    let image = Image {
        extra: "/Decode [0 255]",
        ..two_indices("/CS0")
    };
    let (left, right) = left_and_right(&plates(build(&image, resources, &[tint])), "Spot-A");
    assert!(
        (124..=132).contains(&left),
        "the half tint entry paints half ink, got {left}"
    );
    assert!(right > 200, "the full tint entry paints full ink, got {right}");
}

/// A JPEG 2000 index plane is looked up in the palette like any other index stream. (GH#1916)
#[test]
fn an_indexed_cmyk_jpeg2000_image_paints_the_process_plates() {
    // Entry 0 (the text) is pure cyan; every other entry is pure magenta (the paper).
    let mut palette = String::from("<FF000000");
    for _ in 1..256 {
        palette.push_str(" 00FF0000");
    }
    palette.push('>');
    let cs = format!("[/Indexed /DeviceCMYK 255 {palette}]");
    let image = Image {
        color_space: Some(&cs),
        extra: "/Filter /JPXDecode",
        width: 120,
        height: 40,
        bpc: 8,
        data: INDICES_JP2,
    };
    let plates = plates(build(&image, "", &[]));
    let cyan = plate(&plates, "Cyan");
    let magenta = plate(&plates, "Magenta");
    let text = cyan.data.iter().filter(|&&v| v > 200).count();
    let paper = magenta.data.iter().filter(|&&v| v > 200).count();
    assert!(text > 0, "the text pixels paint the cyan plate");
    assert!(
        paper > text,
        "the paper paints the magenta plate, got {paper} against {text}"
    );
}

/// A JPEG 2000 image that declares `/DeviceCMYK` and carries its own CMYK palette keeps its CMYK
/// samples, so its ink reaches the process plates. (GH#1903)
#[test]
fn a_codestream_palette_cmyk_jpeg2000_image_paints_the_black_plate() {
    let image = Image {
        color_space: Some("/DeviceCMYK"),
        extra: "/Filter /JPXDecode",
        width: 120,
        height: 40,
        bpc: 8,
        data: PALETTE_CMYK_JP2,
    };
    let plates = plates(build(&image, "", &[]));
    let text = plate(&plates, "Black").data.iter().filter(|&&v| v > 200).count();
    let cyan = plate(&plates, "Cyan").data.iter().filter(|&&v| v > 0).count();
    assert!(text > 0, "the text pixels paint the black plate");
    assert_eq!(cyan, 0, "the palette holds no cyan ink");
}

/// ISO 32000-1 Table 89 permits a JPEG 2000 image to omit `/ColorSpace`; in that case its
/// decoded colour space supplies the process-ink intent. (GH#1922)
#[test]
fn a_codestream_palette_cmyk_jpeg2000_image_without_color_space_paints_the_black_plate() {
    let image = Image {
        color_space: None,
        extra: "/Filter /JPXDecode",
        width: 120,
        height: 40,
        bpc: 8,
        data: PALETTE_CMYK_JP2,
    };
    let plates = plates(build(&image, "", &[]));
    let text = plate(&plates, "Black")
        .data
        .iter()
        .filter(|&&value| value > 200)
        .count();
    let cyan = plate(&plates, "Cyan").data.iter().filter(|&&value| value > 0).count();
    assert!(text > 0, "the codestream's text pixels paint the black plate");
    assert_eq!(cyan, 0, "the codestream palette holds no cyan ink");
}

#[test]
fn a_non_jpeg2000_image_without_color_space_still_paints_no_separation_plate() {
    let image = Image {
        color_space: None,
        extra: "",
        width: 1,
        height: 1,
        bpc: 8,
        data: &[0],
    };
    let plates = plates(build(&image, "", &[]));
    assert!(
        plates.iter().all(|plate| plate.data.iter().all(|&value| value == 0)),
        "the decoded fallback is specific to JPEG 2000 images"
    );
}

#[test]
fn an_indexed_two_colorant_devicen_image_paints_its_colorant_plates() {
    // Each palette entry holds one byte per colorant, two here, not the four of a CMYK entry. (GH#1913)
    let resources = "/CS0 [/Indexed [/DeviceN [/Spot-A /Spot-B] /DeviceCMYK 6 0 R] 1 <FF00 00FF>]";
    let program = "{pop pop 0 0 0 0}";
    let tint = format!(
        "<< /FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1 0 1] /Length {} >>\nstream\n{program}\nendstream",
        program.len()
    );
    let plates = plates(build(&two_indices("/CS0"), resources, &[&tint]));
    let (a_left, a_right) = left_and_right(&plates, "Spot-A");
    let (b_left, b_right) = left_and_right(&plates, "Spot-B");
    assert!(a_left > 200, "entry 0 paints the first colorant, got {a_left}");
    assert_eq!(a_right, 0, "entry 1 leaves the first colorant empty");
    assert_eq!(b_left, 0, "entry 0 leaves the second colorant empty");
    assert!(b_right > 200, "entry 1 paints the second colorant, got {b_right}");
}
