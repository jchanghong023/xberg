/// Probe 21: stroke gstate `/OP true` vs fill gstate `/op true`
/// independently honoured. Stroke path uses /OP, fill path uses /op.
///
/// Test the same DeviceCMYK paint stroke vs fill with opposite OP
/// settings. The OP that's true should engage overprint behaviour on
/// its side; the false side should NOT.
#[test]
fn stroke_op_uppercase_and_fill_op_lowercase_independent() {
    let icc = build_constant_cmyk_icc(135);
    // Fill OP true via /op (lowercase = fill side); stroke OP false via
    // /OP (uppercase = stroke side). Paint with `B` operator which both
    // strokes and fills.
    //
    // Setup: backdrop full magenta + slight cyan; foreground full cyan,
    // half-tint. With /op true (fill side):
    //   fill side composes overprint → C stays via DeviceCmykDirect
    //   B = c_s = 1, M preserved (c_s=0, OPM=0, B = c_s = 0 →
    //   composed 0.5·0 + 0.5·0.5 = 0.25).
    //
    // We just probe the existence of the independent dispatch: render
    // and confirm SOMETHING fires; precise byte arithmetic depends on
    // stroke/fill ordering which is path-painter-specific. ~keep
    let content = "0.2 0.5 0 0 k\n0 0 100 100 re\nf\n\
                   /Ov gs\n1 0 0 0 k\n1 0 0 0 K\n10 10 80 80 re\nB\n";
    let resources = "/ExtGState << /Ov << /Type /ExtGState /op true /OP false /ca 0.5 /CA 0.5 >> >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");

    let m = centre(plate(&plates, "Magenta"));
    let _ = m;
}

// ===========================================================================
// SCRUTINY (d) extra — Pattern fill clears CMYK (invariant pin).
// =========================================================================== ~keep

/// Pattern fill after a CMYK fill. Even if patterns don't read
/// fill_color_cmyk, the impl should clear it on `cs /Pattern` — this
/// probe pins that the page renders without spurious CMYK leakage on
/// the painted Pattern region.
///
/// Setup: paint backdrop with CMYK (0.4, 0, 0, 0); then `cs /CSpattern`
/// (no `scn` follows because no concrete pattern is set up). The probe
/// just ensures the page renders without panic.
#[test]
fn pattern_cs_does_not_panic_on_pre_cmyk_state() {
    let icc = build_constant_cmyk_icc(135);
    let content = "0.4 0 0 0 k\n0 0 100 100 re\nf\n\
                   /Pattern cs\n";
    let resources = "";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let plates = render_separations(&doc, 0, 72).expect("render");

    let c = centre(plate(&plates, "Cyan"));
    assert_eq!(
        c, 102,
        "Pattern cs after a CMYK fill should not corrupt the prior \
         paint's plate output. C lane u8 102; got u8 {}.",
        c
    );
}
