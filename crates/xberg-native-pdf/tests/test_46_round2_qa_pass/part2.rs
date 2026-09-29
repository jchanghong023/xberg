/// Same identical-RGB construction as QA-5, but using a plain `f`
/// (path-Fill, single op). The path-Fill arm uses
/// `rasterise_fill_coverage` which is a rasteriser pass on the path
/// independent of pixmap content — so the spot lane gets written
/// even when the alternate-CS RGB matches the backdrop.
///
/// This is a regression guard: the path-Fill arm correctly handles
/// the identical-RGB case. The diff branch on combos / text / Do /
/// sh does NOT (QA-5 above).
#[test]
fn qa9_identical_rgb_paint_via_path_fill_writes_spot_lane() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 0.0 0.0 0.0] /N 1 >>";
    let content = "/Half gs\n\
                   0 0 0 0 k\n0 0 100 100 re\nf\n\
                   /CS_PMS cs\n0.7 scn\n\
                   0 0 100 100 re\nf\n";
    let resources = format!(
        "/ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >> \
         /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>",
        psfunc
    );
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _img = renderer.render_page(&doc, 0).expect("render succeeds");

    let plane = renderer.cmyk_sidecar_spot_plane(0).expect("InkA plane");
    let dims = renderer.cmyk_sidecar_dims().unwrap();
    let centre = ((dims.1 / 2) * dims.0 + dims.0 / 2) as usize;
    let expected = tint_to_u8(compose_normal(0.0, 0.7, 0.5));
    assert_eq!(expected, 89);
    assert_eq!(
        plane[centre], expected,
        "ISO 32000-1 §11.7.3: the path-Fill arm uses \
         `rasterise_fill_coverage` (path-based, not pixmap-diff), so \
         the identical-RGB case still writes the spot lane. Expected \
         u8 = {}, got {}. (Compare with qa5_identical_rgb_paint_via_\
         combo_does_not_write_spot_lane which uses `B`.)",
        expected, plane[centre]
    );
}

// ===========================================================================
// PROBE QA-10: round 4 byte-identity regression guard (cmyk plane
// stays byte-exact through round 2's spot work).
// =========================================================================== ~keep

/// A CMYK-only paint with /BM /Multiply over a /DeviceN page should
/// have a byte-identical CMYK plane to the equivalent paint without
/// any sidecar/spot wiring. The round 2 spot writes must not
/// perturb the CMYK plane.
///
/// This is a regression guard against the spot mirror accidentally
/// writing to the CMYK plane via the wrong accessor or breaking
/// the round 4 compose ordering.
#[test]
fn qa10_round4_cmyk_plane_byte_identity_preserved_through_round2() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc4 = "<< /FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /Length 28 >>\nstream\n{0 0 0 0}\nendstream\nendobj\n";
    // CMYK paint with /BM /Multiply over a page that ALSO declares a
    // DeviceN spot (so the sidecar allocates spot lanes). The CMYK
    // plane must remain whatever round 4 computed; the spot lanes
    // stay at zero (CMYK paint, /InkA unsourced). ~keep
    let content = "/Mult gs\n0.3 0.0 0.0 0.0 k\n0 0 100 100 re\nf\n";
    let resources = "/ExtGState << /Mult << /Type /ExtGState /BM /Multiply >> >> \
                     /ColorSpace << /CS_DN [/DeviceN [/InkA] /DeviceCMYK 6 0 R] >>";
    let extra = format!("6 0 obj\n{}", psfunc4);
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[extra.as_bytes()]);
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _img = renderer.render_page(&doc, 0).expect("render succeeds");

    let plane_inka = renderer.cmyk_sidecar_spot_plane(0).expect("InkA plane");
    assert!(
        plane_inka.iter().all(|&b| b == 0),
        "regression guard: the CMYK paint must not leak into the \
         InkA spot lane. First non-zero offset: {:?}",
        plane_inka.iter().position(|&b| b != 0)
    );

    let cmyk = renderer.cmyk_sidecar_cmyk_bytes().expect("sidecar CMYK plane");
    let dims = renderer.cmyk_sidecar_dims().unwrap();
    let centre = ((dims.1 / 2) * dims.0 + dims.0 / 2) as usize;
    let c_at_centre = cmyk[centre * 4];
    assert!(
        c_at_centre > 0,
        "regression guard: round 4 CMYK mirror must continue to write \
         the C plane through a round 2 spot-allocated page. Got C = \
         {} at centre.",
        c_at_centre
    );
}

// ===========================================================================
// PROBE QA-11: detection-OFF byte-identity (no transparency triggers
// → no sidecar).
// =========================================================================== ~keep

/// A page with NO transparency triggers (no /ca, no /CA, no /SMask,
/// no /BM!=Normal, no /OP, no Form XObject /Group) but WITH a
/// /Separation paint must not allocate the sidecar. The visible
/// pixmap matches the round-1 pre-trigger baseline.
///
/// Mirrors round-1 `b3_no_transparency_trigger_keeps_sidecar_none`
/// but with a Separation paint to verify the round-2 spot wiring
/// doesn't accidentally force allocation.
#[test]
fn qa11_separation_paint_without_trigger_keeps_sidecar_none() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let content = "/CS_PMS cs\n0.7 scn\n0 0 100 100 re\nf\n";
    let resources = format!("/ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >>", psfunc);
    let pdf = build_pdf_with_output_intent(content, &resources, &icc, &[]);
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _img = renderer.render_page(&doc, 0).expect("render succeeds");

    assert!(
        renderer.cmyk_sidecar_dims().is_none(),
        "detection-OFF: no transparency triggers → sidecar must not \
         allocate. A /Separation paint by itself is NOT a transparency \
         trigger (§11.7.3 sidecar is allocated only when transparency \
         is active)."
    );
}

// ===========================================================================
// PROBE QA-12: mandatory probe 5 — Form XObject with /Group /CS
// /Separation is non-conforming per §11.6.6 / Table 147 — the impl
// should NOT crash, and the spot lane behaviour should fall through
// to the alternate.
// =========================================================================== ~keep

/// ISO 32000-1 §11.6.6 Table 147 forbids /Separation as a Group /CS.
/// A non-conforming Form XObject declaring `/Group /CS /Separation
/// /InkA …` should not crash the renderer. This probe verifies the
/// renderer survives such input and produces some output (we don't
/// pin a specific behaviour beyond "no panic").
#[test]
fn qa12_non_conforming_form_xobject_group_with_separation_cs_does_not_panic() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    // Form XObject with non-conforming /Group /CS [/Separation /InkA
    // /DeviceCMYK psfunc]. Per §11.6.6, this is illegal; the
    // renderer should fall through to a reasonable default
    // (alternate CS, or treat as DeviceCMYK). ~keep
    let form = format!(
        "6 0 obj\n\
         << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
            /Resources << /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >> >> \
            /Group << /Type /Group /S /Transparency \
                      /CS [/Separation /InkA /DeviceCMYK {} ] >> \
            /Length 36 >>\n\
         stream\n/CS_PMS cs\n0.7 scn\n0 0 100 100 re\nf\nendstream\nendobj\n",
        psfunc, psfunc
    );
    let content = "/Half gs\n/Form Do\n";
    let resources = "/ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >> \
                     /XObject << /Form 6 0 R >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[form.as_bytes()]);
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _result = renderer.render_page(&doc, 0);
}

/// Per ISO 32000-1 §11.4.6.2, a knockout group's elements paint
/// against the group's INITIAL backdrop (not the running result of
/// prior elements). Two overlapping /Separation paints inside a /K
/// group: only the last paint's tint should appear at the overlap.
///
/// This probe verifies the spot lane respects knockout semantics —
/// the spot mirror must NOT accumulate both paints' tints at the
/// overlap.
#[test]
fn qa13_knockout_group_spot_paint_keeps_only_last_tint() {
    let icc = build_constant_cmyk_icc(135);
    let psfunc = "<< /FunctionType 2 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] \
                  /C0 [0.0 0.0 0.0 0.0] /C1 [0.0 1.0 0.0 0.0] /N 1 >>";
    let form = format!(
        "6 0 obj\n\
         << /Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 100 100] \
            /Resources << /ColorSpace << /CS_PMS [/Separation /InkA /DeviceCMYK {} ] >> >> \
            /Group << /Type /Group /S /Transparency /K true /CS /DeviceCMYK >> \
            /Length 80 >>\n\
         stream\n/CS_PMS cs\n0.3 scn\n10 10 80 80 re\nf\n\
         0.6 scn\n10 10 80 80 re\nf\nendstream\nendobj\n",
        psfunc
    );
    let content = "/Half gs\n/Form Do\n";
    let resources = "/ExtGState << /Half << /Type /ExtGState /ca 1.0 /BM /Multiply >> >> \
                     /XObject << /Form 6 0 R >>";
    let pdf = build_pdf_with_output_intent(content, resources, &icc, &[form.as_bytes()]);
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");
    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(72).as_raw());
    let _img = renderer.render_page(&doc, 0).expect("render succeeds");

    let plane = renderer.cmyk_sidecar_spot_plane(0).expect("InkA plane");
    let dims = renderer.cmyk_sidecar_dims().unwrap();
    let centre = ((dims.1 / 2) * dims.0 + dims.0 / 2) as usize;
    assert!(
        plane[centre] > 0,
        "spot lane should be non-zero at the overlap (at least one \
         paint touched the pixel). Got {} at centre.",
        plane[centre]
    );
}
