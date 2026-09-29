//! Round-3 QA pass for composite-then-decompose separation rendering.
//!
//! Adversarial probes against the round-3 commits a01a855, 69498b0,
//! 29379f8, ad38e2a. Each probe scrutinises a specific architectural
//! choice or edge case the round-3 design+impl agent self-flagged or
//! that the QA brief enumerated as a drill target. Probes either pin
//! correct byte-exact behaviour (regression guards) OR mark a
//! `QA_BUG_*` or pin a `HONEST_GAP_*` policy explicitly.
//!
//! Methodology references:
//!  - `docs/research/2026-06-06-nonsep-blends-in-devicen.md`
//!  - `tests/test_46_round3_separations.rs` — round-3 design+impl
//!    probes; this QA file augments without overlap.
//!
//! Spec citations:
//!  - ISO 32000-1 §7.3.5 Name objects (case-sensitive)
//!  - ISO 32000-1 §8.6.6.3 reserved `/All` / `/None` + "no plate"
//!  - ISO 32000-1 §8.6.6.4 `/Separation`
//!  - ISO 32000-1 §10.5 separated plate output per ink
//!  - ISO 32000-1 §11.3.3 basic compositing formula
//!  - ISO 32000-1 §11.3.5.2 separable blend modes
//!  - ISO 32000-1 §11.3.5.3 non-separable blend modes
//!  - ISO 32000-1 §11.4.6.2 knockout group composition rule
//!  - ISO 32000-1 §11.4.7 soft masks
//!  - ISO 32000-1 §11.6.6 transparency group CS exclusions
//!  - ISO 32000-1 §11.7.3 spot colours and transparency (sidecar)
//!  - ISO 32000-1 §11.7.4.2 BM split per lane class
//!  - ISO 32000-1 §11.7.4.3 PDF 1.3 overprint mode (OPM=0)
//!  - ISO 32000-1 §11.7.4.4 PDF 1.4 nonzero overprint mode (OPM=1)

#![allow(dead_code)]

use xberg_native_pdf::document::PdfDocument;
use xberg_native_pdf::rendering::{PageRenderer, RenderOptions, render_separations};

// ===========================================================================
// HONEST_GAP markers — documented spec gaps round 3 declared (or
// should have).
//
// The round-3 QA_BUG_OPM0_COMPOSITE_PATH_ADDITIVELY_MERGES_PLATES
// marker was REMOVED in round 4. The composite-path
// apply_overprint_after_paint now implements the ISO 32000-1 §11.7.4.3
// CompatibleOverprint blend function (Table 149) per-channel, replacing
// the (src + dst).min(1.0) additive-merge approximation. QA-A1/A2/A3
// below are now byte-exact references against the spec rule; see
// `tests/test_46_round4_overprint_spec.rs` for the full
// source-CS-class × OPM matrix.
// =========================================================================== ~keep

/// The /K knockout group's per-byte merge skips byte-equality with the
/// backdrop. If an element's paint produces a byte value identical to
/// the backdrop byte at a pixel (e.g. a Multiply paint over a 0
/// backdrop at low source tint that rounds to 0), the merge cannot
/// distinguish "paint wrote backdrop value" from "paint didn't write".
/// The accumulator stays at backdrop, and any subsequent element's
/// paint at that pixel is preserved.
///
/// Defensible because:
///   - For Normal-mode paints at α<1, a paint that produces exactly
///     the backdrop's byte is indistinguishable from "didn't touch".
///     The merge correctly treats both the same.
///   - For paints whose result happens to equal the backdrop byte
///     (e.g. Multiply over 0 with tint 0), the paint's net effect was
///     "leave backdrop alone" → the merge result is still backdrop,
///     which matches §11.4.6.2's "compose against initial backdrop".
///
/// The brief asked: is there a case where a paint writes a non-trivial
/// value that ROUND-TRIPS to the backdrop byte? Multiply at low tint
/// with low backdrop: backdrop 0.0, source 0.001 → blend = 0.0, after
/// composition lane = 0.0 → u8 0 = backdrop. The paint "had no
/// observable per-plate effect" on this pixel; the spec gives no
/// per-byte distinguishability. So skipping is defensible.
pub const HONEST_GAP_KNOCKOUT_MERGE_BYTE_EQUALITY_SKIP: &str = "HONEST_GAP_KNOCKOUT_MERGE_BYTE_EQUALITY_SKIP: the /K group's \
     per-byte merge treats `post[i] == backdrop[i]` as 'this element \
     did not touch the byte'. For paints whose composed lane value \
     rounds to the backdrop byte, the merge cannot distinguish 'paint \
     wrote backdrop value' from 'paint did not touch'. Spec offers no \
     per-byte distinguishability; for the spec-defined per-pixel \
     §11.4.6.2 rule the cases coincide (paint composes against \
     backdrop, lane stays at backdrop). Round 3 adopts the byte-skip \
     because (a) it is the same rule the pixmap merge uses for the \
     RGBA layer, (b) under §11.4.6.2's per-pixel composition rule, a \
     paint that produces a value byte-equal to the backdrop has no \
     observable per-plate effect at that pixel.";

fn build_pdf_with_output_intent(
    content: &str,
    resources_inner: &str,
    icc_profile: &[u8],
    extra_objs: &[&str],
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /OutputIntents [<< /Type /OutputIntent /S /GTS_PDFX /OutputCondition (Synthetic Non-Linear CMYK) /DestOutputProfile 5 0 R >>] >>\nendobj\n",
    );
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << {} >> /Contents 4 0 R >>\nendobj\n",
        resources_inner
    );
    buf.extend_from_slice(page.as_bytes());
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let icc_off = buf.len();
    let icc_hdr = format!("5 0 obj\n<< /N 4 /Length {} >>\nstream\n", icc_profile.len());
    buf.extend_from_slice(icc_hdr.as_bytes());
    buf.extend_from_slice(icc_profile);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let mut extra_offs: Vec<usize> = Vec::new();
    for obj in extra_objs {
        extra_offs.push(buf.len());
        buf.extend_from_slice(obj.as_bytes());
    }

    let xref_off = buf.len();
    let total_objs = 5 + extra_objs.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", total_objs + 1).as_bytes());
    for off in [cat_off, pages_off, page_off, stream_off, icc_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    for off in extra_offs {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            total_objs + 1,
            xref_off
        )
        .as_bytes(),
    );
    buf
}

fn build_pdf_no_output_intent(content: &str, resources_inner: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << {} >> /Contents 4 0 R >>\nendobj\n",
        resources_inner
    );
    buf.extend_from_slice(page.as_bytes());
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    let total_objs = 4;
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", total_objs).as_bytes());
    for off in [cat_off, pages_off, page_off, stream_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            total_objs, xref_off
        )
        .as_bytes(),
    );
    buf
}

fn build_constant_cmyk_icc(l_byte: u8) -> Vec<u8> {
    let in_chan: u8 = 4;
    let out_chan: u8 = 3;
    let grid: u8 = 2;
    let mut lut = Vec::with_capacity(2048);

    lut.extend_from_slice(&0x6d66_7431u32.to_be_bytes());
    lut.extend_from_slice(&0u32.to_be_bytes());
    lut.push(in_chan);
    lut.push(out_chan);
    lut.push(grid);
    lut.push(0);

    let identity: [i32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x0001_0000];
    for v in identity {
        lut.extend_from_slice(&(v as u32).to_be_bytes());
    }
    for _ in 0..in_chan {
        for i in 0..256u16 {
            lut.push(i as u8);
        }
    }
    let grid_size = (grid as usize).pow(in_chan as u32);
    for _ in 0..grid_size {
        lut.push(l_byte);
        lut.push(128);
        lut.push(128);
    }
    for _ in 0..out_chan {
        for i in 0..256u16 {
            lut.push(i as u8);
        }
    }

    let mut profile = vec![0u8; 128];
    let total_size: u32 = 128 + 4 + 12 + lut.len() as u32;
    profile[0..4].copy_from_slice(&total_size.to_be_bytes());
    profile[8..12].copy_from_slice(&0x0240_0000u32.to_be_bytes());
    profile[12..16].copy_from_slice(b"prtr");
    profile[16..20].copy_from_slice(b"CMYK");
    profile[20..24].copy_from_slice(b"Lab ");
    profile[36..40].copy_from_slice(b"acsp");
    profile[64..68].copy_from_slice(&0u32.to_be_bytes());
    profile[68..72].copy_from_slice(&0x0000_F6D6u32.to_be_bytes());
    profile[72..76].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    profile[76..80].copy_from_slice(&0x0000_D32Du32.to_be_bytes());

    profile.extend_from_slice(&1u32.to_be_bytes());
    profile.extend_from_slice(&0x4132_4230u32.to_be_bytes());
    profile.extend_from_slice(&144u32.to_be_bytes());
    profile.extend_from_slice(&(lut.len() as u32).to_be_bytes());
    profile.extend_from_slice(&lut);

    profile
}

fn plate<'a>(
    plates: &'a [xberg_native_pdf::rendering::SeparationPlate],
    name: &str,
) -> &'a xberg_native_pdf::rendering::SeparationPlate {
    plates
        .iter()
        .find(|p| p.ink_name == name)
        .unwrap_or_else(|| panic!("no plate named {}", name))
}

fn centre(plate: &xberg_native_pdf::rendering::SeparationPlate) -> u8 {
    let off = ((plate.height / 2) * plate.width + plate.width / 2) as usize;
    plate.data[off]
}

// ===========================================================================
// SCRUTINY (a) — transparency + overprint co-occurrence.
//
// The dispatch criterion `page_declares_transparency` excludes /OP, /op.
// Pages with /ca<1 + /OP route to the composite path. The composite
// path's overprint handler does additive merge per channel for OPM=0,
// which is a composite-preview approximation, not the per-plate spec
// behaviour. These probes pin observed byte-exact behaviour for the
// transparency+overprint co-occurrence so the QA report can compare
// against §11.7.4.3 / §11.7.4.4.
// =========================================================================== ~keep

/// QA-A1: transparency + OPM=0 with DeviceCMYK source, /OP true.
///
/// Backdrop = full /ca=1 DeviceCMYK paint (0.4, 0, 0, 0) = Cyan-40%.
/// Foreground = /ca=0.5 DeviceCMYK paint (0, 0.5, 0, 0) with /OP true
/// (OPM=0 default).
///
/// ISO 32000-1 §11.7.4.3 Table 149 row 1 (DeviceCMYK direct, OP=true,
/// OPM=0): `B(c_b, c_s) = c_s` for every C/M/Y/K channel. §11.3.3
/// composition: `c_r = α · c_s + (1 - α) · c_b`.
///
///   C: 0.5·0   + 0.5·0.4 = 0.20 → u8 51.
///   M: 0.5·0.5 + 0.5·0   = 0.25 → u8 64.
#[test]
fn qa_a1_transparency_opm0_devicecmyk_overprint_observed_byte_exact() {
    let icc = build_constant_cmyk_icc(135);
    let content = "0.4 0 0 0 k\n0 0 100 100 re\nf\n\
                   /Ov gs\n0 0.5 0 0 k\n0 0 100 100 re\nf\n";
    let resources = "/ExtGState << /Ov << /Type /ExtGState /OP true /ca 0.5 >> >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let c = plate(&plates, "Cyan");
    let m = plate(&plates, "Magenta");

    assert_eq!(
        centre(c),
        51,
        "ISO 32000-1 §11.7.4.3 Table 149 row 1 (DeviceCMYK direct, \
         OP=true, OPM=0): C lane c_r = 0.5·0 + 0.5·0.4 = 0.2 → u8 51. \
         Got u8 {}.",
        centre(c)
    );
    assert_eq!(
        centre(m),
        64,
        "ISO 32000-1 §11.7.4.3 Table 149 row 1 (DeviceCMYK direct, \
         OP=true, OPM=0): M lane c_r = 0.5·0.5 + 0.5·0 = 0.25 → u8 64. \
         Got u8 {}.",
        centre(m)
    );
}

/// QA-A2: transparency + OPM=1 with DeviceCMYK source, /OP true.
///
/// ISO 32000-1 §11.7.4.3 Table 149 row 1 (DeviceCMYK direct, OP=true,
/// OPM=1): `B(c_b, c_s) = c_s` if `c_s ≠ 0`, else `c_b`.
///
/// Backdrop = (0.4, 0, 0, 0), Foreground = (0, 0.5, 0, 0) at /ca = 0.5:
///   C: c_s=0   → B = c_b = 0.4. r = 0.5·0.4 + 0.5·0.4 = 0.4  → u8 102.
///   M: c_s=0.5 → B = c_s = 0.5. r = 0.5·0.5 + 0.5·0   = 0.25 → u8 64.
#[test]
fn qa_a2_transparency_opm1_devicecmyk_overprint_observed_byte_exact() {
    let icc = build_constant_cmyk_icc(135);
    let content = "0.4 0 0 0 k\n0 0 100 100 re\nf\n\
                   /Ov gs\n0 0.5 0 0 k\n0 0 100 100 re\nf\n";
    let resources = "/ExtGState << /Ov << /Type /ExtGState /OP true /OPM 1 /ca 0.5 >> >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let c = plate(&plates, "Cyan");
    let m = plate(&plates, "Magenta");

    assert_eq!(
        centre(c),
        102,
        "ISO 32000-1 §11.7.4.3 Table 149 row 1 (OPM=1): C c_s=0 → \
         B = c_b = 0.4. c_r = 0.5·0.4 + 0.5·0.4 = 0.4 → u8 102. \
         Got u8 {}.",
        centre(c)
    );
    assert_eq!(
        centre(m),
        64,
        "ISO 32000-1 §11.7.4.3 Table 149 row 1 (OPM=1): M c_s=0.5 ≠ 0 \
         → B = c_s. c_r = 0.5·0.5 + 0.5·0 = 0.25 → u8 64. Got u8 {}.",
        centre(m)
    );
}

/// QA-A3: transparency + overprint with /K knockout group.
///
/// A /K (non-isolated) group containing a single OP+OPM=1+α=0.5
/// DeviceCMYK paint over an opaque DeviceCMYK backdrop. Per §11.4.6.2
/// the knockout group's elements compose against the group's initial
/// backdrop = outer page state (since non-isolated). With a single
/// inside paint and outer-group /Normal+α=1 the final plate output is
/// identical to QA-A2's reference.
///
/// Per §11.4.6.2 + §11.7.4.3 Table 149 row 1 (OPM=1):
///   C: c_s=0  → B = c_b = 0.4. c_r = 0.5·0.4 + 0.5·0.4 = 0.4 → u8 102.
///   M: c_s=0.5 → B = c_s.       c_r = 0.5·0.5 + 0.5·0 = 0.25 → u8 64.
#[test]
fn qa_a3_transparency_overprint_inside_knockout_group_byte_exact() {
    let icc = build_constant_cmyk_icc(135);
    let form = "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ExtGState << /Ov << /Type /ExtGState /OP true /OPM 1 /ca 0.5 >> >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length 41 >>\n\
        stream\n/Ov gs\n0 0.5 0 0 k\n0 0 100 100 re\nf\nendstream\nendobj\n";
    let content = "0.4 0 0 0 k\n0 0 100 100 re\nf\n/Form Do\n";
    let resources = "/XObject << /Form 6 0 R >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let c = plate(&plates, "Cyan");
    let m = plate(&plates, "Magenta");

    assert_eq!(
        centre(c),
        102,
        "ISO 32000-1 §11.4.6.2 + §11.7.4.3 Table 149 row 1 (OPM=1): \
         /K element composes against group's initial backdrop = outer \
         state (0.4, 0, 0, 0). C: c_s=0 → B = c_b = 0.4. c_r = 0.4 → \
         u8 102. Got u8 {}.",
        centre(c)
    );
    assert_eq!(
        centre(m),
        64,
        "ISO 32000-1 §11.4.6.2 + §11.7.4.3 Table 149 row 1 (OPM=1): \
         M c_s=0.5 → B = c_s. c_r = 0.25 → u8 64. Got u8 {}.",
        centre(m)
    );
}

/// QA-B1: `process_plate` called for a non-process ink name returns
/// None. `spot_plate` called for a process ink name ("Cyan") returns
/// None. The cross-namespace mistake (looking up a process name in
/// the spot table) is a common glue-code bug; this probe pins the
/// API contract.
///
/// We exercise this indirectly through `render_separations` since
/// `process_plate` / `spot_plate` are pub(crate). A page whose plate
/// list contains a spot ink named "Cyan" would be ambiguous; the
/// composite path resolves the "Cyan" name to the process plate
/// extractor (not the spot table) because of the `matches!(ink,
/// "Cyan" | ...)` dispatch in `render_plates_via_composite`. We
/// pin that dispatch by declaring a /Separation /Cyan colorant
/// (which is a permissible ink name per §11.6.7 — author can name
/// a spot anything, including a process name) and asserting the
/// plate output for ink "Cyan" reflects the PROCESS Cyan channel,
/// not the spot lane.
///
/// Spec note: §8.6.6.5 actually says `/Cyan` inside `/DeviceN`
/// is a /Process colorant and gets filtered out of the spot set
/// (round-2 fix). But `/Separation /Cyan` is technically allowed
/// (separation can name any colorant). The discovery walker filters
/// `/Cyan` from spot set in /DeviceN context but NOT in /Separation
/// context.
///
/// Round 3 dispatches by ink-name string matching, not by sidecar
/// table lookup priority. The plate for ink "Cyan" therefore comes
/// from `process_plate("Cyan")`, even if a /Separation /Cyan declared
/// the same name.
#[test]
fn qa_b1_process_ink_name_dispatch_priority_over_spot_table() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [1.0 0.0 0.0 0.0] /N 1 >>";
    let content = "/Trig gs\n\
                   /CS_PMS cs\n0.8 scn\n0 0 100 100 re\nf\n\
                   0.2 0 0 0 k\n0 0 100 100 re\nf\n";
    let resources = format!(
        "/ExtGState << /Trig << /Type /ExtGState /ca 0.5 >> >> \
         /ColorSpace << /CS_PMS [/Separation /Cyan /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");

    let cyan_plates: Vec<_> = plates.iter().filter(|p| p.ink_name == "Cyan").collect();
    assert_eq!(
        cyan_plates.len(),
        1,
        "ISO 32000-1 §10.5: plate names must be unique per ink. Got \
         {} plates named 'Cyan' → name collision in the composite \
         dispatch. Plate list: {:?}",
        cyan_plates.len(),
        plates.iter().map(|p| p.ink_name.as_str()).collect::<Vec<_>>()
    );
    let c = cyan_plates[0];
    assert!(
        centre(c) > 0,
        "Process Cyan plate at centre = {}. Expected non-zero — the \
         DeviceCMYK paint at 0.2 with /ca 0.5 composes a non-zero \
         Cyan tint, and /Separation /Cyan at tint 0.8 with /ca 0.5 \
         also composes (process_plate dispatch resolves to the \
         CMYK channel which receives the alternate-CS contribution).",
        centre(c)
    );
}

/// QA-B2: `process_plate("cyan")` (lowercase) must NOT match
/// the process name. PDF names are case-sensitive per §7.3.5.
///
/// Probe exercises `render_separations` for a page that requests a
/// plate for the lowercase ink name. The collect_page_inks walker
/// emits Cyan|Magenta|Yellow|Black with the spec-canonical
/// capitalisation, so lowercase requests don't naturally arise. We
/// exercise it via `render_separation` (single-ink), which takes the
/// user-supplied ink name verbatim.
#[test]
fn qa_b2_process_plate_name_lookup_is_case_sensitive() {
    use xberg_native_pdf::rendering::render_separation;
    let icc = build_constant_cmyk_icc(135);
    let content = "/Trig gs\n0.4 0 0 0 k\n0 0 100 100 re\nf\n";
    let resources = "/ExtGState << /Trig << /Type /ExtGState /ca 0.5 >> >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");

    let cyan = render_separation(&doc, 0, "Cyan", 72).expect("render Cyan");
    let cyan_centre = centre(&cyan);
    assert!(
        cyan_centre > 0,
        "Cyan plate at centre = {}; expected non-zero from /ca 0.5 \
         DeviceCMYK paint",
        cyan_centre
    );

    let cyan_lc = render_separation(&doc, 0, "cyan", 72).expect("render cyan");
    let centre_lc = centre(&cyan_lc);
    assert_eq!(
        centre_lc, 0,
        "ISO 32000-1 §7.3.5: PDF names are case-sensitive. \
         render_separation(\"cyan\") must NOT match process_plate(\
         \"Cyan\") via case-insensitive lookup. centre = {} (expected \
         0 → no plate produced).",
        centre_lc
    );
}

/// QA-C1: /K group paint that produces a per-byte result equal to the
/// backdrop. The byte-skip merge cannot distinguish "paint wrote
/// backdrop value" from "didn't paint"; both produce the same result.
///
/// Setup: backdrop has /Separation /InkA at tint 0.0 (no contribution,
/// effectively 0). Inside /K, paint InkA at tint 0.0 with /ca 1.0.
/// Composed lane = 0.0 → u8 0 = backdrop u8.
///
/// The merge's "skip if post == backdrop" treats this as "didn't
/// paint"; the accumulator stays at backdrop 0. So the final lane is
/// 0 — same as if no paint had occurred. This is correct under
/// §11.4.6.2 (compose against backdrop produces backdrop) AND under
/// the round-2 HONEST_GAP_SPOT_LANE_UNSOURCED_PRESERVE_BACKDROP
/// (a zero source has no observable effect).
///
/// The probe asserts the centre value is 0 byte-exact AND pins the
/// HONEST_GAP_KNOCKOUT_MERGE_BYTE_EQUALITY_SKIP policy.
#[test]
fn qa_c1_knockout_merge_byte_equality_skip_preserves_correctness() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let form = "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
              << /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                 /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length 35 >>\n\
        stream\n/CS_PMS cs\n0.0 scn\n0 0 100 100 re\nf\nendstream\nendobj\n";
    let content = "/CS_PMS cs\n0.0 scn\n0 0 100 100 re\nf\n/Form Do\n";
    let resources = format!(
        "/XObject << /Form 6 0 R >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let inka = plate(&plates, "InkA");
    assert_eq!(
        centre(inka),
        0,
        "{} — /K group with InkA tint-0 paint over backdrop tint 0. \
         Composed lane = 0 = backdrop. Byte-equality skip in /K merge \
         treats post==backdrop as 'didn't paint'; under §11.4.6.2 the \
         paint composed against the backdrop produces the backdrop \
         value, so the skip is observationally correct. Got {}.",
        HONEST_GAP_KNOCKOUT_MERGE_BYTE_EQUALITY_SKIP,
        centre(inka)
    );
}

/// QA-C2: /K group with reversed paint order from probe 8 in the
/// design+impl file. Round-3 P8 had paint 1 = /InkA, paint 2 = /InkB.
/// Reversed: paint 1 = /InkB, paint 2 = /InkA. Asymmetry would
/// indicate the impl's policy is non-symmetric across ink swaps,
/// which would be a real bug.
///
/// Per the CompatibleOverprint policy (HONEST_GAP_KNOCKOUT_DIFFERENT_
/// INK_SPOT_INTERACTION): paint 2 (now to /InkA) leaves the InkB
/// lane alone (because paint 2 doesn't name InkB). Paint 1's InkB
/// tint 0.4 → InkB lane = 0.4 → u8 102.
/// Paint 2's InkA tint 0.6 → InkA lane = 0.6 → u8 153.
///
/// Same as P8 result modulo the ink-name swap. The probe pins
/// byte-exact under the swap; failure means the impl's /K replay is
/// order-sensitive.
#[test]
fn qa_c2_knockout_different_inks_symmetric_under_order_swap() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let form = "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ColorSpace << \
              /CS_A [/Separation /InkA /DeviceCMYK \
                 << /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                    /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] \
              /CS_B [/Separation /InkB /DeviceCMYK \
                 << /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                    /C0 [0.0 0.0 0.0 0.0] /C1 [1.0 0.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length 71 >>\n\
        stream\n/CS_B cs\n0.4 scn\n0 0 100 100 re\nf\n\
/CS_A cs\n0.6 scn\n0 0 100 100 re\nf\n\
endstream\nendobj\n";
    let content = "/Form Do\n";
    let resources = format!(
        "/XObject << /Form 6 0 R >> \
         /ColorSpace << /CS_A [/Separation /InkA /DeviceCMYK {} ] \
                        /CS_B [/Separation /InkB /DeviceCMYK {} ] >>",
        psfunc, psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let inka = plate(&plates, "InkA");
    let inkb = plate(&plates, "InkB");
    assert_eq!(
        centre(inka),
        153,
        "Reversed-order /K group: InkA last paint should compose to \
         u8 153 (tint 0.6 over backdrop 0 at α=1). Got {}.",
        centre(inka)
    );
    assert_eq!(
        centre(inkb),
        102,
        "Reversed-order /K group: InkB first paint should survive \
         paint 2 (which targets InkA only) → u8 102. Got {}. \
         Asymmetry vs round-3 P8 indicates the /K replay is order-\
         sensitive in a way the impl should not be.",
        centre(inkb)
    );
}

/// QA-5: a fresh PageRenderer used for composite preview after a
/// separation render must produce byte-identical output to a fresh
/// PageRenderer used in isolation. The `force_cmyk_sidecar` flag is
/// pub(crate) and set only inside `render_plates_via_composite`,
/// which constructs a NEW renderer; a new external renderer cannot
/// inherit the flag.
///
/// We exercise the constructor invariant: a freshly-constructed
/// PageRenderer reports `force_cmyk_sidecar = false` indirectly by
/// rendering a page whose sidecar would only be allocated under
/// force OR OutputIntent. A no-OutputIntent + transparency page
/// MUST produce sidecar=None on a fresh renderer.
#[test]
fn qa_5_fresh_page_renderer_has_no_sidecar_force_default() {
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let content = "/Trig gs\n\
                   /CS_PMS cs\n0.5 scn\n0 0 100 100 re\nf\n";
    let resources = format!(
        "/ExtGState << /Trig << /Type /ExtGState /ca 0.5 >> >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_no_output_intent(content, &resources);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _img = renderer.render_page(&doc, 0).expect("render");

    assert!(
        renderer.cmyk_sidecar_dims().is_none(),
        "Fresh PageRenderer has force_cmyk_sidecar = false by \
         default; no OutputIntent → sidecar None. dims = {:?}, \
         expected None.",
        renderer.cmyk_sidecar_dims()
    );
}

/// QA-6: /K group containing a Form XObject /K group containing
/// /Separation paints. The /K replay logic must handle nesting — the
/// outer /K's sidecar snapshot must not be clobbered by the inner /K's
/// replay state machine.
///
/// Setup:
///   Outer /K Form:
///     paint InkA at tint 0.6 (α=1)
///     /Inner Do  (which is itself /K)
///   Inner /K Form:
///     paint InkA at tint 0.3 (α=0.5)
///
/// Per §11.4.6.2: each group's constituent objects compose against
/// THAT group's initial backdrop. Applied to nested /K groups:
///
///   - The OUTER /K's two elements (paint 1 and /Inner Do) each
///     compose against the OUTER /K's initial backdrop.
///   - The INNER /K's single element composes against the INNER /K's
///     initial backdrop. The inner /K's initial backdrop is whatever
///     state the sidecar holds at inner-/K entry.
///
/// In outer /K iteration 2 (which is /Inner Do):
///   - The outer /K resets the sidecar to its own initial backdrop
///     (= 0; no page-level paint) before replaying iteration 2.
///   - The replay enters /Inner Do, which triggers inner /K with
///     sidecar = 0.
///   - Inner /K composes: (1-0.5)·0 + 0.5·0.3 = 0.15 → u8 38.
///   - Inner /K installs its accum (38) into the sidecar at exit.
///   - Outer /K iteration 2 merge: post-sidecar=38, backdrop=0 →
///     outer accum picks 38, overwriting iteration 1's 153.
///   - Outer /K exit: install outer accum (38) into sidecar.
///
/// Spec-correct: lane = 38.
///
/// Failure modes:
///  - 153: iteration 2's merge didn't overwrite iteration 1 — either
///    inner /K didn't fire, OR inner /K's install-on-exit didn't
///    survive Form Do return, OR outer /K's iteration 2 merge missed
///    the change.
///  - 115: inner /K saw paint 1's contribution as its backdrop
///    (outer /K's reset between iterations didn't reach the inner
///    snapshot path).
///  - 0: paint contributions lost entirely.
#[test]
fn qa_6_nested_knockout_groups_compose_against_each_levels_initial_backdrop() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let inner_stream = "/Half gs\n/CS_PMS cs\n0.3 scn\n0 0 100 100 re\nf\n";
    let inner_form = format!(
        "7 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >> \
                         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
                           << /FunctionType 2 /Domain [0 1] \
                              /Range [0 1 0 1 0 1 0 1] /C0 [0.0 0.0 0.0 0.0] \
                              /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length {} >>\n\
        stream\n{}endstream\nendobj\n",
        inner_stream.len(),
        inner_stream
    );
    let outer_stream = "/CS_PMS cs\n0.6 scn\n0 0 100 100 re\nf\n/Inner Do\n";
    let outer_form = format!(
        "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /XObject << /Inner 7 0 R >> \
                         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
                           << /FunctionType 2 /Domain [0 1] \
                              /Range [0 1 0 1 0 1 0 1] /C0 [0.0 0.0 0.0 0.0] \
                              /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length {} >>\n\
        stream\n{}endstream\nendobj\n",
        outer_stream.len(),
        outer_stream
    );
    let content = "/Outer Do\n";
    let resources = format!(
        "/XObject << /Outer 6 0 R >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[&outer_form, &inner_form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let inka = plate(&plates, "InkA");

    // §11.4.6.2 per-group: each group's elements compose against
    // that group's INITIAL backdrop.
    //
    // Outer /K iteration 2's element = `/Inner Do`. Outer /K resets
    // sidecar to outer backdrop (= 0) before this iteration's replay.
    // Inner /K then snapshots sidecar = 0, replays its single paint
    // at tint 0.3 α=0.5 over backdrop 0 → lane = 0.15 → u8 38.
    // Inner /K installs 38 into the sidecar at exit. Outer /K
    // iteration 2 merge: post=38, backdrop=0, 38 ≠ 0 → outer accum
    // picks 38. Outer /K iteration 1 had set the accum to 153
    // (paint 1's contribution), but iteration 2 overwrites at every
    // painted pixel because last-write wins on per-byte collision.
    //
    // Final InkA centre = 38.
    //
    // Failure modes that pin a real bug:
    //  - 153: iteration 2 didn't update the accum (inner /K's paint
    //    didn't fire OR inner /K's install-on-exit doesn't reach the
    //    outer /K's view of the sidecar).
    //  - 115: inner /K snapshot captured paint 1's contribution
    //    (outer /K's reset between iterations didn't fire); inner
    //    paint composed against 0.6 → 0.45 → u8 115.
    //  - 0: complete loss of paint contribution.
    //  - 38: spec-correct. ~keep
    let observed = centre(inka);
    assert_eq!(
        observed, 38,
        "ISO 32000-1 §11.4.6.2 per-group: outer /K iteration 2's \
         element (/Inner Do) sees the outer /K's INITIAL backdrop \
         (= 0 here, no page-level paint). Inner /K composes its \
         paint against 0 → lane 0.15 → u8 38. Got {}. \
         If 153: outer /K iteration 2 didn't override iteration 1 \
         (state machine lost inner /K's contribution). \
         If 115: inner /K saw paint 1 as backdrop (outer /K's reset \
         didn't fire). Either way is a spec-violating bug.",
        observed
    );
}

/// QA-6-DIAG-1: render the inner /K Form ALONE (page calls /Inner Do
/// directly, no outer /K wrapper). Confirms the inner /K's paint
/// produces u8 38 when not nested. Used to isolate the QA-6 nesting
/// regression — if this passes with 38 but QA-6 fails with 153, the
/// bug is specifically in the outer /K's iteration 2 handling of
/// /Inner Do as a sub-paint that produces a nested-/K contribution.
#[test]
fn qa_6_diag_single_knockout_form_alone_produces_38() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let inner_stream = "/Half gs\n/CS_PMS cs\n0.3 scn\n0 0 100 100 re\nf\n";
    let inner_form = format!(
        "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >> \
                         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
                           << /FunctionType 2 /Domain [0 1] \
                              /Range [0 1 0 1 0 1 0 1] /C0 [0.0 0.0 0.0 0.0] \
                              /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length {} >>\n\
        stream\n{}endstream\nendobj\n",
        inner_stream.len(),
        inner_stream
    );
    let content = "/Inner Do\n";
    let resources = format!(
        "/XObject << /Inner 6 0 R >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[&inner_form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let inka = plate(&plates, "InkA");
    assert_eq!(
        centre(inka),
        38,
        "Inner /K Form alone (no outer /K wrapper) at tint 0.3 α=0.5: \
         lane = 0.15 → u8 38. Got {}. This is the floor signal for \
         the nested-/K probe — if this passes with 38 but QA-6 fails, \
         the bug is in outer /K's iteration handling of Do as a \
         nested-/K element.",
        centre(inka)
    );
}

/// QA-6-DIAG-2: outer /K containing paint 1 (Fill InkA at 0.6) and
/// paint 2 = /Inner Do where Inner is a plain Form (NOT /K) with a
/// /Separation /InkA fill at tint 0.3 α=0.5.
///
/// Per §11.4.6.2: outer /K's iteration 2 element (= Inner Do) composes
/// against the OUTER /K's initial backdrop (= 0). Inner Form is plain
/// (no /Group, no /K), so it just renders its content into the
/// scratch pixmap as if it were inline. Inner paint at tint 0.3 α=0.5
/// over backdrop 0 → lane = 0.15 → u8 38.
///
/// Outer /K iteration 2 merge: sidecar (= 38) vs outer backdrop (=0)
/// → outer accum picks 38, overrides iteration 1's 153.
///
/// Final lane = 38.
///
/// If this passes with 38 but QA-6 (nested /K Form) fails with 153,
/// the bug is SPECIFIC to nested /K interactions (the inner /K's
/// install-on-exit either doesn't fire or doesn't reach the outer /K's
/// view of the sidecar).
///
/// If this ALSO fails with 153, the bug is broader — any nested Form
/// inside an outer /K iteration 2 loses the inner Form's sidecar
/// contribution.
#[test]
fn qa_6_diag2_outer_k_with_plain_inner_form_propagates_inner_sidecar_write() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let inner_stream = "/Half gs\n/CS_PMS cs\n0.3 scn\n0 0 100 100 re\nf\n";
    let inner_form = format!(
        "7 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >> \
                         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
                           << /FunctionType 2 /Domain [0 1] \
                              /Range [0 1 0 1 0 1 0 1] /C0 [0.0 0.0 0.0 0.0] \
                              /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Length {} >>\n\
        stream\n{}endstream\nendobj\n",
        inner_stream.len(),
        inner_stream
    );
    let outer_stream = "/CS_PMS cs\n0.6 scn\n0 0 100 100 re\nf\n/Inner Do\n";
    let outer_form = format!(
        "6 0 obj\n\
        << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
           /Resources << /XObject << /Inner 7 0 R >> \
                         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK \
                           << /FunctionType 2 /Domain [0 1] \
                              /Range [0 1 0 1 0 1 0 1] /C0 [0.0 0.0 0.0 0.0] \
                              /C1 [0.0 1.0 0.0 0.0] /N 1 >> ] >> >> \
           /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
           /Length {} >>\n\
        stream\n{}endstream\nendobj\n",
        outer_stream.len(),
        outer_stream
    );
    let content = "/Outer Do\n";
    let resources = format!(
        "/XObject << /Outer 6 0 R >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[&outer_form, &inner_form]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");
    let inka = plate(&plates, "InkA");

    let observed = centre(inka);
    assert_eq!(
        observed, 38,
        "Outer /K with plain (non-/K) inner Form: iteration 2's \
         Inner Do should render the inner fill at tint 0.3 α=0.5 over \
         backdrop 0 → 0.15 → u8 38. Outer /K iteration 2 merge picks \
         38 over iteration 1's 153. Got {}. \
         If 153: outer /K iteration 2's Inner Do didn't update the \
         sidecar — the bug is in how outer /K handles ANY nested \
         Form as a paint element, not nested-/K specifically.",
        observed
    );
}

// Split for file-too-long (#1567): the module continues below via `include!`,
// not a sibling tests/*_part2.rs file -- Cargo auto-discovers every *.rs file
// directly under tests/ as its own test binary, so a sibling would fail to
// compile on its own and, if it somehow did, run every #[test] in it twice.
// A subdirectory is not auto-discovered. ~keep
include!("test_46_round3_qa_pass/part2.rs");
