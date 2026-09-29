// ===========================================================================
// Compose-first bounded loss when backdrop went through the ICC
// ===========================================================================
//
// Round-3 fix: apply_cmyk_compose_after_paint snapshots the post-paint
// RGB before the transparent paint, inverts via §10.3.5 additive clamp
// to recover CMYK, then composites + re-runs the ICC. When the backdrop
// pixel was produced by an *opaque* prior CMYK paint that ALSO went
// through the non-linear ICC, the inversion is lossy — the round-3
// agent's own commit message admits this. The probe quantifies the
// byte-delta vs the press-accurate compose-first reference (a
// single-paint render of the composed CMYK at full opacity, which is
// what a separation-backend route would produce).
//
// Fixture A: opaque backdrop CMYK(0.5, 0, 0, 0) (cyan 50%), then
// transparent CMYK(0, 0, 0.5, 0) (yellow 50%) at /ca 0.5, both under
// the non-linear ICC. The compose-first impl inverts the cyan-ICC RGB
// back through additive-clamp, composites with the yellow CMYK at
// α=0.5, then re-converts. The composed CMYK should be
// CMYK(0.25, 0, 0.25, 0).
//
// Reference: a single-paint render of CMYK(0.25, 0, 0.25, 0) at full
// opacity through the same ICC. That's the value a press-accurate
// backend (which keeps CMYK plates resident) would land on. ~keep

fn fixture_compose_first_with_icc_backdrop() -> Vec<u8> {
    let content = "0.5 0 0 0 k\n10 10 80 80 re\nf\n\
                   /Half gs\n\
                   0 0 0.5 0 k\n\
                   10 10 80 80 re\nf\n";
    let resources = "/ExtGState << /Half << /Type /ExtGState /ca 0.5 >> >>";
    let profile = build_nonlinear_cmyk_to_lab_lut8_profile();
    build_pdf_with_optional_output_intent(content, resources, &[], Some(&profile))
}

/// Compose-first under an ICC-derived backdrop: the round-3
/// apply_cmyk_compose_after_paint inverted the post-ICC backdrop RGB
/// via §10.3.5 additive-clamp, which loses colorimetric information
/// when the backdrop went through a non-linear ICC. The Priority-4
/// CMYK-plate-retention fix keeps the backdrop CMYK quadruple resident
/// so the compose-first path reads CMYK directly instead of inverting
/// RGB.
///
/// Reference: single-paint render of the composed CMYK quadruple
/// (0.25, 0, 0.25, 0) at full opacity through the same ICC. Under the
/// fix, the two-paint render's overlap region matches byte-exact.
#[test]
fn qa_round3_compose_first_under_icc_backdrop_press_accurate() {
    let rgba_two = render_rgba(fixture_compose_first_with_icc_backdrop());
    let rgba_ref = render_rgba(fixture_nonlinear_icc_single_cmyk(0.25, 0.0, 0.25, 0.0));

    let (r_actual, g_actual, b_actual) = mean_rgb(&rgba_two, 35, 65, 35, 65);
    let (r_ref, g_ref, b_ref) = mean_rgb(&rgba_ref, 35, 65, 35, 65);

    let actual = (
        r_actual.round() as i32,
        g_actual.round() as i32,
        b_actual.round() as i32,
    );
    let press = (r_ref.round() as i32, g_ref.round() as i32, b_ref.round() as i32);

    assert_eq!(
        actual, press,
        "ISO 32000-1 §11.4 compose-first under ICC backdrop must hit the \
         press-accurate single-paint reference; got overlap={actual:?} vs \
         reference={press:?}"
    );
}
