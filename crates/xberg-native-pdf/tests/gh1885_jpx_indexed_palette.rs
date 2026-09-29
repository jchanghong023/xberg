//! GH#1885: a `/JPXDecode` image whose dictionary names an `/Indexed` colour space must decode
//! through the dictionary's palette, on both the render path and the image-extraction path.
//!
//! The failing shape is a lossily coded index plane inside a JP2 container that also carries a
//! `pclr` palette box. The decoder resolved that box and failed the whole image on the index
//! samples the lossy coding pushed outside the palette, so the page rendered blank and
//! extraction listed nothing.

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::extractors::{ImageData, PixelFormat};
use xberg_native_pdf::rendering::{RenderOptions, render_page};

/// 120x40, dark text (index 0) on paper (index 255), one lossily coded index component in a JP2
/// container with a CMYK `pclr` palette box. See `decoders/jpx.rs` for how it was built.
const PALETTE_CMYK_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_palette_cmyk.jp2");
/// The same picture coded losslessly as a plain greyscale JP2 with no palette box.
const INDICES_GREY_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_indices_grey.jp2");
/// The same picture as an index component plus an opaque alpha component, with the CMYK `pclr`
/// box routing the index through the palette and a `cdef` box marking the second as opacity.
const PALETTE_ALPHA_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_palette_alpha.jp2");
/// The same palette image with opacity 255 on the left half and 64 on the right half.
const PALETTE_PARTIAL_ALPHA_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_palette_partial_alpha.jp2");
/// The same picture coded lossily for a 16-entry palette, ink at index 0 and paper at 15. The lossy
/// coding rings some samples to 16, one past the last entry.
const HIVAL15_LOSSY_JP2: &[u8] = include_bytes!("fixtures/jpx/gh1885_hival15_lossy.jp2");

const WIDTH: u32 = 120;
const HEIGHT: u32 = 40;

/// `[/Indexed /DeviceCMYK 255 <lookup>]` where entry `i` is `(0, 0, 0, 255 - i)`: index 0 is
/// black ink and index 255 is white paper, matching the codestream's own palette.
fn cmyk_grey_palette() -> String {
    let lookup: String = (0..=255u32).map(|i| format!("000000{:02x}", 255 - i)).collect();
    format!("[/Indexed /DeviceCMYK 255 <{lookup}>]")
}

/// `[/Indexed /DeviceRGB 255 <lookup>]` where entry `i` is `(255, i, i)`: index 0 is pure red
/// and index 255 is white. No codestream palette produces red, so a red pixel proves the
/// dictionary's palette was the one applied.
fn rgb_red_palette() -> String {
    let lookup: String = (0..=255u32).map(|i| format!("ff{i:02x}{i:02x}")).collect();
    format!("[/Indexed /DeviceRGB 255 <{lookup}>]")
}

/// `[/Indexed /DeviceRGB 15 <lookup>]` where entry `i` is `(255, 17i, 17i)`: index 0 is pure red
/// and index 15 is white. No entry is black, so a black pixel is an index past the palette.
fn rgb_red_palette_of_16() -> String {
    let lookup: String = (0..16u32).map(|i| format!("ff{:02x}{:02x}", 17 * i, 17 * i)).collect();
    format!("[/Indexed /DeviceRGB 15 <{lookup}>]")
}

/// A one-page PDF whose page is exactly the image, one PDF unit per image pixel.
fn pdf_with_jpx_image(codestream: &[u8], color_space: &str) -> Vec<u8> {
    pdf_with_jpx_image_at_depth(codestream, color_space, 8)
}

/// [`pdf_with_jpx_image`] with the dictionary's `/BitsPerComponent` set to `bpc`.
fn pdf_with_jpx_image_at_depth(codestream: &[u8], color_space: &str, bpc: u8) -> Vec<u8> {
    pdf_with_jpx_image_and_profile(codestream, color_space, bpc, None, "")
}

/// [`pdf_with_jpx_image_at_depth`] plus, when `icc_profile` is given, a four-component ICC profile
/// stream as object `6 0 R` for `color_space` to name, and `extra` appended to the image dictionary.
fn pdf_with_jpx_image_and_profile(
    codestream: &[u8],
    color_space: &str,
    bpc: u8,
    icc_profile: Option<&[u8]>,
    extra: &str,
) -> Vec<u8> {
    let content = format!("q\n{WIDTH} 0 0 {HEIGHT} 0 0 cm\n/Im1 Do\nQ\n");
    let mut buf: Vec<u8> = b"%PDF-1.5\n".to_vec();
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
             /BitsPerComponent {bpc} /ColorSpace {color_space} /Filter /JPXDecode {extra} /Length {} >>\nstream\n",
            codestream.len()
        )
        .as_bytes(),
    );
    buf.extend_from_slice(codestream);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    if let Some(icc) = icc_profile {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("6 0 obj\n<< /N 4 /Length {} >>\nstream\n", icc.len()).as_bytes());
        buf.extend_from_slice(icc);
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

/// The rendered page's RGBA pixels.
fn rendered_rgba(pdf: Vec<u8>) -> Vec<u8> {
    let doc = PdfDocument::from_bytes(pdf).expect("fixture parses");
    render_page(&doc, 0, &RenderOptions::with_dpi(72).as_raw())
        .expect("render page 0")
        .data
}

/// Number of dark pixels on the rendered page.
fn rendered_ink(pdf: Vec<u8>) -> usize {
    rendered_rgba(pdf)
        .chunks(4)
        .filter(|px| px[0] < 128 && px[1] < 128 && px[2] < 128)
        .count()
}

/// The single extracted image's RGB pixels.
fn extracted_rgb(pdf: Vec<u8>) -> Vec<u8> {
    let doc = PdfDocument::from_bytes(pdf).expect("fixture parses");
    let images = doc.extract_images(0).expect("extract page 0");
    assert_eq!(
        images.len(),
        1,
        "the /Indexed JPXDecode image must be extracted; extraction drops an image that fails to decode"
    );
    assert!(
        !images[0].samples_are_raw(),
        "palette lookup replaces the index samples, so a colour-key /Mask must not range-test them"
    );
    match images[0].data() {
        ImageData::Raw {
            pixels,
            format: PixelFormat::RGB,
        } => {
            assert_eq!(pixels.len(), (WIDTH * HEIGHT * 3) as usize, "one RGB triple per pixel");
            pixels.clone()
        }
        other => panic!("an /Indexed image must expand to raw RGB, got {other:?}"),
    }
}

#[test]
fn a_palette_jpeg2000_page_renders_its_ink() {
    let control = rendered_ink(pdf_with_jpx_image(INDICES_GREY_JP2, "/DeviceGray"));
    assert!(
        control > 100,
        "control failed: the same picture as plain greyscale painted only {control} dark pixels"
    );

    let ink = rendered_ink(pdf_with_jpx_image(PALETTE_CMYK_JP2, &cmyk_grey_palette()));
    assert!(
        ink.abs_diff(control) * 10 <= control,
        "the palette image painted {ink} dark pixels against {control} for the greyscale control; \
         the lossy picture must stay within a tenth of the lossless one"
    );
}

#[test]
fn a_palette_jpeg2000_image_is_extracted_as_ink_on_paper() {
    let rgb = extracted_rgb(pdf_with_jpx_image(PALETTE_CMYK_JP2, &cmyk_grey_palette()));
    assert_eq!(&rgb[..3], &[255, 255, 255], "the top-left pixel is paper");
    assert!(
        rgb.chunks(3).any(|px| px.iter().all(|&c| c < 64)),
        "the text must survive as dark pixels"
    );
}

/// ISO 32000-1 §7.4.9: the dictionary's colour space overrides the codestream's, so the
/// `/Indexed` palette applies and the codestream's grey `pclr` palette does not.
#[test]
fn the_dictionary_palette_wins_over_the_codestream_palette() {
    let rgb = extracted_rgb(pdf_with_jpx_image(PALETTE_CMYK_JP2, &rgb_red_palette()));
    assert!(
        rgb.chunks(3).any(|px| px == [255, 0, 0]),
        "ink must take the dictionary's red entry"
    );
    assert!(
        rgb.chunks(3).all(|px| px[0] == 255),
        "no pixel may come from the codestream's grey palette"
    );
}

/// An index plane with no codestream palette was painted as greyscale, never looked up.
#[test]
fn an_indexed_jpeg2000_without_a_codestream_palette_is_looked_up() {
    let rgb = extracted_rgb(pdf_with_jpx_image(INDICES_GREY_JP2, &rgb_red_palette()));
    assert_eq!(&rgb[..3], &[255, 255, 255], "the top-left pixel is paper");
    assert!(
        rgb.chunks(3).any(|px| px == [255, 0, 0]),
        "ink must take the dictionary's red entry"
    );
}

/// A palette box plus an opacity channel. The opacity component is not a second index component,
/// so the image must decode, as it did before the dictionary's palette took over.
#[test]
fn a_palette_jpeg2000_image_with_an_opacity_channel_renders_and_extracts() {
    let control = extracted_rgb(pdf_with_jpx_image(INDICES_GREY_JP2, &cmyk_grey_palette()));
    let rgb = extracted_rgb(pdf_with_jpx_image(PALETTE_ALPHA_JP2, &cmyk_grey_palette()));
    assert_eq!(rgb, control, "the index plane must be looked up, not the opacity plane");

    let ink = rendered_ink(pdf_with_jpx_image(PALETTE_ALPHA_JP2, &cmyk_grey_palette()));
    let control_ink = rendered_ink(pdf_with_jpx_image(INDICES_GREY_JP2, &cmyk_grey_palette()));
    assert!(control_ink > 100, "control failed: only {control_ink} dark pixels");
    assert_eq!(
        ink, control_ink,
        "the page must paint the same ink as the image without alpha"
    );
}

#[test]
fn a_palette_jpeg2000_image_keeps_its_nonopaque_channel_for_smask_in_data() {
    let opaque = rendered_ink(pdf_with_jpx_image(PALETTE_ALPHA_JP2, &cmyk_grey_palette()));
    let translucent = rendered_ink(pdf_with_jpx_image_and_profile(
        PALETTE_PARTIAL_ALPHA_JP2,
        &cmyk_grey_palette(),
        8,
        None,
        "/SMaskInData 1",
    ));
    assert!(opaque > 100, "control failed: only {opaque} opaque dark pixels");
    assert!(
        translucent < opaque * 3 / 4 && translucent > opaque / 3,
        "the translucent half must no longer count as dark: opaque={opaque}, translucent={translucent}"
    );
}

/// Lossy ringing past a short palette's last entry takes that entry. The expander paints an index
/// past the palette black, and this palette has no black entry.
#[test]
fn lossy_ringing_past_a_short_palette_paints_no_black() {
    let pdf = || pdf_with_jpx_image(HIVAL15_LOSSY_JP2, &rgb_red_palette_of_16());
    let rgb = extracted_rgb(pdf());
    assert!(
        rgb.chunks(3).any(|px| px == [255, 0, 0]),
        "control failed: ink must take the dictionary's red entry"
    );
    let black = rgb.chunks(3).filter(|px| *px == [0, 0, 0]).count();
    assert_eq!(black, 0, "{black} extracted pixels took an index past the palette");

    let painted_black = rendered_rgba(pdf()).chunks(4).filter(|px| px[..3] == [0, 0, 0]).count();
    assert_eq!(
        painted_black, 0,
        "{painted_black} rendered pixels took an index past the palette"
    );
}

/// ISO 32000-1 §7.4.9: a JPXDecode image ignores `/BitsPerComponent`, so the decoded 8-bit index
/// plane must be looked up at 8 bits whatever the dictionary says.
#[test]
fn the_dictionary_bit_depth_does_not_change_the_lookup() {
    let at_8 = extracted_rgb(pdf_with_jpx_image_at_depth(INDICES_GREY_JP2, &rgb_red_palette(), 8));
    let at_4 = extracted_rgb(pdf_with_jpx_image_at_depth(INDICES_GREY_JP2, &rgb_red_palette(), 4));
    assert!(
        at_8.chunks(3).any(|px| px == [255, 0, 0]),
        "control failed: ink must take the dictionary's red entry"
    );
    assert_eq!(
        at_4, at_8,
        "/BitsPerComponent 4 must not change how the indices are read"
    );

    let page_8 = rendered_rgba(pdf_with_jpx_image_at_depth(INDICES_GREY_JP2, &rgb_red_palette(), 8));
    let page_4 = rendered_rgba(pdf_with_jpx_image_at_depth(INDICES_GREY_JP2, &rgb_red_palette(), 4));
    assert_eq!(page_4, page_8, "/BitsPerComponent 4 must not change the rendered page");
}

/// A CMYK ICC profile whose `A2B0` table (an 8-bit `mft1` lookup) maps every CMYK value to one Lab
/// colour, lightness `l_byte`/255 with a and b neutral. A CMM that compiles it turns every palette
/// entry into the same mid grey; the profile-free CMYK formula paints black ink dark.
fn constant_grey_cmyk_profile(l_byte: u8) -> Vec<u8> {
    let (in_chan, out_chan, grid) = (4u8, 3u8, 2u8);
    let mut lut = Vec::new();
    lut.extend_from_slice(b"mft1");
    lut.extend_from_slice(&[0; 4]);
    lut.extend_from_slice(&[in_chan, out_chan, grid, 0]);
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000] {
        lut.extend_from_slice(&v.to_be_bytes());
    }
    lut.extend((0..in_chan).flat_map(|_| 0..=255u8));
    for _ in 0..(grid as usize).pow(u32::from(in_chan)) {
        lut.extend_from_slice(&[l_byte, 128, 128]);
    }
    lut.extend((0..out_chan).flat_map(|_| 0..=255u8));

    let mut profile = vec![0u8; 128];
    let total_size = u32::try_from(128 + 4 + 12 + lut.len()).expect("profile fits in u32");
    profile[0..4].copy_from_slice(&total_size.to_be_bytes());
    profile[8..12].copy_from_slice(&0x0240_0000u32.to_be_bytes());
    profile[12..16].copy_from_slice(b"prtr");
    profile[16..20].copy_from_slice(b"CMYK");
    profile[20..24].copy_from_slice(b"Lab ");
    profile[36..40].copy_from_slice(b"acsp");
    // D50 illuminant in s15Fixed16.
    profile[68..72].copy_from_slice(&0x0000_F6D6u32.to_be_bytes());
    profile[72..76].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    profile[76..80].copy_from_slice(&0x0000_D32Du32.to_be_bytes());
    // One tag: A2B0 at offset 144.
    profile.extend_from_slice(&1u32.to_be_bytes());
    profile.extend_from_slice(b"A2B0");
    profile.extend_from_slice(&144u32.to_be_bytes());
    profile.extend_from_slice(&u32::try_from(lut.len()).expect("lut fits in u32").to_be_bytes());
    profile.extend_from_slice(&lut);
    profile
}

/// An `/ICCBased` CMYK base routes each palette entry through its profile, as it does for an
/// uncompressed index stream.
#[test]
fn an_iccbased_cmyk_base_looks_the_palette_up_through_its_profile() {
    let icc = constant_grey_cmyk_profile(135);
    let device = extracted_rgb(pdf_with_jpx_image(INDICES_GREY_JP2, &cmyk_grey_palette()));
    assert!(
        device.chunks(3).any(|px| px.iter().all(|&c| c < 64)),
        "control failed: without a profile the palette's black entry must paint dark"
    );

    let lookup = cmyk_grey_palette().replace("/DeviceCMYK", "[/ICCBased 6 0 R]");
    let rgb = extracted_rgb(pdf_with_jpx_image_and_profile(
        INDICES_GREY_JP2,
        &lookup,
        8,
        Some(&icc),
        "",
    ));
    let first = [rgb[0], rgb[1], rgb[2]];
    assert!(
        first.iter().all(|&c| (64..=192).contains(&c)),
        "the profile maps every entry to mid grey, got {first:?}"
    );
    assert!(
        rgb.chunks(3).all(|px| px == first),
        "every palette entry must go through the profile, so every pixel is the same grey"
    );
}

/// A colour-key `/Mask` on an `/Indexed` JPEG 2000 image names index ranges, so the ink at index
/// 0 is masked out and the page shows paper where the text was.
#[test]
fn a_colour_key_mask_on_an_indexed_jpeg2000_image_hides_its_masked_indices() {
    let control = rendered_ink(pdf_with_jpx_image(INDICES_GREY_JP2, &cmyk_grey_palette()));
    assert!(
        control > 100,
        "control failed: the unmasked image painted only {control} dark pixels"
    );

    let masked = rendered_ink(pdf_with_jpx_image_and_profile(
        INDICES_GREY_JP2,
        &cmyk_grey_palette(),
        8,
        None,
        "/Mask [0 127]",
    ));
    assert_eq!(
        masked, 0,
        "every dark index is inside the mask range, so no ink may be painted"
    );
}
