//! GH#1839: a `/JPXDecode` image that names no `/ColorSpace` and decodes to four components
//! must pick up the document's `/OutputIntents` CMYK profile, the same as a `/DeviceCMYK`
//! image that declares one.
//!
//! ISO 32000-1 Table 89 lets such an image omit `/ColorSpace`, so extraction installs a
//! placeholder `/DeviceRGB` and makes the ICC decision from it, before the codestream is
//! decoded. The colour space is corrected once the component count is known; the profile was
//! not, so the image reached its consumers with none and its colour went through the §10.3.5
//! additive clamp instead of the document's press characterisation.

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::extractors::ColorSpace;

/// A 16x16 lossless four-component raw J2K codestream, one channel saturated per quadrant.
/// Shared with `test_separation_image_rendering.rs`; see the comment on `CMYK_QUADRANTS_J2K`
/// there for how it was built.
///
/// `hayro-jpeg2000` 0.4's *bare-codestream* path (`j2c::parse`) never reads the true component
/// count off the codestream to pick a colour space: for `component_infos.len() >= 3` it always
/// assumes `Enumerated::Srgb`, then reconciles the mismatch (4 actual vs. 3 assumed) by treating
/// the fourth channel as alpha rather than as the fourth process colour -- so this raw
/// codestream decodes as 3-component RGB, never CMYK, on hayro's own reading of it.
/// `wrap_as_cmyk_jp2` below adds the minimal JP2 box structure carrying an explicit `/colr` box
/// (`EnumCS = 12`, ISO 15444-1 Annex I.5.3.3), which routes through `jp2::parse` instead and is
/// read as declared. GH#1883 has since made the bare form reach four components here too, by
/// reading `Csiz` off the codestream when the dictionary declares nothing -- so the wrapper is no
/// longer the *only* route to the CMYK arm, but it is the one that exercises `colr`, and keeping it
/// means this test's subject stays the OutputIntent lookup rather than the `Csiz` fallback. ~keep
const CMYK_QUADRANTS_J2K: &[u8] = include_bytes!("fixtures/jpx/gh1855_cmyk_quadrants.j2k");

/// Wrap a raw J2K codestream in the minimal JP2 box structure `hayro-jpeg2000` requires to
/// read an explicit colour space: a signature box, an `ftyp` box, a `jp2h` box containing a
/// `colr` box declaring `enum_cs`, and a `jp2c` box carrying the codestream unchanged. The
/// `ihdr` box is normally mandatory in JP2 (ISO 15444-1 I.5.3.1) but `hayro-jpeg2000`'s parser
/// does not read it (it only inspects `colr`/`cdef`/`pclr`/`cmap` inside `jp2h`), so omitting
/// it here keeps the wrapper minimal without weakening what this test exercises.
fn wrap_as_cmyk_jp2(codestream: &[u8]) -> Vec<u8> {
    fn jp2_box(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + data.len());
        out.extend_from_slice(&((8 + data.len()) as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        out
    }

    const ENUM_CS_CMYK: u32 = 12;
    let signature_box = jp2_box(b"jP  ", &[0x0D, 0x0A, 0x87, 0x0A]);
    let mut ftyp_data = b"jp2 ".to_vec();
    ftyp_data.extend_from_slice(&0u32.to_be_bytes());
    ftyp_data.extend_from_slice(b"jp2 ");
    let ftyp_box = jp2_box(b"ftyp", &ftyp_data);
    let mut colr_data = vec![1u8, 0, 0]; // METH=1 (enumerated), PREC=0, APPROX=0
    colr_data.extend_from_slice(&ENUM_CS_CMYK.to_be_bytes());
    let colr_box = jp2_box(b"colr", &colr_data);
    let jp2h_box = jp2_box(b"jp2h", &colr_box);
    let jp2c_box = jp2_box(b"jp2c", codestream);

    [signature_box, ftyp_box, jp2h_box, jp2c_box].concat()
}

/// The smallest byte sequence `IccProfile::parse` accepts as a CMYK profile: a 128-byte
/// header carrying `acsp` at 36..40 and the `CMYK` input colour-space signature at 16..20,
/// plus a zero tag count. Those two fields are all `parse` reads. No colour transform is
/// exercised here -- the assertion is *which* profile is attached, not what it computes. ~keep
fn minimal_cmyk_icc_profile() -> Vec<u8> {
    let mut profile = vec![0u8; 132];
    profile[0..4].copy_from_slice(&132u32.to_be_bytes());
    profile[8..12].copy_from_slice(&0x0240_0000u32.to_be_bytes());
    profile[12..16].copy_from_slice(b"prtr");
    profile[16..20].copy_from_slice(b"CMYK");
    profile[20..24].copy_from_slice(b"Lab ");
    profile[36..40].copy_from_slice(b"acsp");
    profile
}

/// Build a single-page PDF that paints `codestream` through a `/JPXDecode` image XObject
/// declaring **no** `/ColorSpace`, optionally declaring an `/OutputIntents` array whose
/// `/DestOutputProfile` is `icc_profile`.
fn build_pdf_with_jpx_image_no_colorspace(codestream: &[u8], icc_profile: Option<&[u8]>) -> Vec<u8> {
    let content: &[u8] = b"q\n50 0 0 50 25 25 cm\n/Im1 Do\nQ\n";
    let mut buf: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.5\n");

    offsets.push(buf.len());
    let catalog: &str = if icc_profile.is_some() {
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R /OutputIntents [<< /Type /OutputIntent \
         /S /GTS_PDFX /OutputCondition (Synthetic CMYK) /DestOutputProfile 6 0 R >>] >>\nendobj\n"
    } else {
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n"
    };
    buf.extend_from_slice(catalog.as_bytes());

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

    if let Some(icc) = icc_profile {
        offsets.push(buf.len());
        buf.extend_from_slice(format!("6 0 obj\n<< /N 4 /Length {} >>\nstream\n", icc.len()).as_bytes());
        buf.extend_from_slice(icc);
        buf.extend_from_slice(b"\nendstream\nendobj\n");
    }

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
fn jpx_cmyk_image_without_colorspace_receives_the_output_intent_profile() {
    let icc = minimal_cmyk_icc_profile();
    let codestream = wrap_as_cmyk_jp2(CMYK_QUADRANTS_J2K);
    let doc = PdfDocument::from_bytes(build_pdf_with_jpx_image_no_colorspace(&codestream, Some(&icc)))
        .expect("parse the JPX + /OutputIntents fixture");

    // Resolved before the extraction: an unparseable fixture profile would otherwise make the
    // real assertion below unreachable and the test green for the wrong reason. ~keep
    let intent = doc
        .output_intent_cmyk_profile()
        .expect("the fixture's /OutputIntents CMYK profile must parse, or this test proves nothing");

    let images = doc.extract_images(0).expect("extract page 0");
    assert_eq!(
        images.len(),
        1,
        "the JPXDecode image must decode; extract_images silently drops an image whose \
         extraction errors, which would leave the profile assertion below unreached"
    );
    assert_eq!(
        *images[0].color_space(),
        ColorSpace::DeviceCMYK,
        "the four-component codestream must correct the placeholder /DeviceRGB, or this test \
         is not exercising the CMYK arm at all"
    );

    let attached = images[0]
        .icc_profile()
        .expect("GH#1839: the decoded image must carry the document's OutputIntent CMYK profile");
    assert_eq!(
        attached.bytes(),
        intent.bytes(),
        "the profile attached to the decoded image must be the document's OutputIntent CMYK profile"
    );
    assert_eq!(attached.n_components(), 4, "the attached profile must be the CMYK one");
}

/// Control: with no `/OutputIntents` in the catalog there is nothing to inherit, so the same
/// image must carry no profile. Pins that the fix reads the document rather than fabricating a
/// profile for every four-component JPX image.
#[test]
fn jpx_cmyk_image_without_output_intent_carries_no_profile() {
    let codestream = wrap_as_cmyk_jp2(CMYK_QUADRANTS_J2K);
    let doc = PdfDocument::from_bytes(build_pdf_with_jpx_image_no_colorspace(&codestream, None))
        .expect("parse the no-/OutputIntents control");
    assert!(
        doc.output_intent_cmyk_profile().is_none(),
        "the control fixture must declare no OutputIntent"
    );

    let images = doc.extract_images(0).expect("extract page 0");
    assert_eq!(images.len(), 1, "the JPXDecode image must decode");
    assert_eq!(*images[0].color_space(), ColorSpace::DeviceCMYK);
    assert!(
        images[0].icc_profile().is_none(),
        "with no /OutputIntents the image must carry no ICC profile"
    );
}
