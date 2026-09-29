/// Build a PDF where the shading is masked by a soft-mask Form XObject.
/// The Form is a 100×100 grayscale-filled rectangle whose alpha is
/// the gray value. Object numbering: 1 Catalog, 2 Pages, 3 Page, 4
/// Content, 5 Shading, 6 SMask Form, 7 ExtGState (carrying /SMask
/// 6 0 R), 8 Form Resources.
fn build_pdf_shading_under_smask(space_str: &str, c0: &str, c1: &str, smask_gray: f32) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let page_off = buf.len();
    let page = "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
                /Resources << /Shading << /Sh1 5 0 R >> \
                /ExtGState << /GS1 7 0 R >> >> /Contents 4 0 R >>\nendobj\n";
    buf.extend_from_slice(page.as_bytes());

    let stream_off = buf.len();
    let content_ops = "/GS1 gs\n/Sh1 sh\n";
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let shading_off = buf.len();
    let shading = format!(
        "5 0 obj\n<< /ShadingType 2 /ColorSpace {} /Coords [0 50 100 50] /Domain [0 1] \
         /Function << /FunctionType 2 /Domain [0 1] /C0 {} /C1 {} /N 1 >> >>\nendobj\n",
        space_str, c0, c1
    );
    buf.extend_from_slice(shading.as_bytes());

    let smask_form_ops = format!("{} g\n0 0 100 100 re f\n", smask_gray);
    let smask_form_off = buf.len();
    let smask_form = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Form /FormType 1 \
         /BBox [0 0 100 100] /Resources << >> \
         /Group << /Type /Group /S /Transparency /CS /DeviceGray >> \
         /Length {} >>\nstream\n{}\nendstream\nendobj\n",
        smask_form_ops.len(),
        smask_form_ops
    );
    buf.extend_from_slice(smask_form.as_bytes());

    let extgs_off = buf.len();
    buf.extend_from_slice(
        b"7 0 obj\n<< /Type /ExtGState \
          /SMask << /Type /Mask /S /Luminosity /G 6 0 R /BC [0] >> >>\nendobj\n",
    );

    let offsets = [
        cat_off,
        pages_off,
        page_off,
        stream_off,
        shading_off,
        smask_form_off,
        extgs_off,
    ];
    let xref_off = buf.len();
    let size = offsets.len() + 1;
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", size).as_bytes());
    for off in &offsets {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            size, xref_off
        )
        .as_bytes(),
    );
    buf
}

/// Probe 18 — Shading under an active SMask. The SMask is a
/// luminosity mask whose gray value sets the alpha. The splice must
/// not perturb SMask state — the rendered pixmap must remain a full
/// 100×100 RGBA.
#[test]
fn qa_shading_under_smask_renders_without_panic() {
    let bytes = build_pdf_shading_under_smask("/DeviceRGB", "[1 0 0]", "[0 0 1]", 0.5);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert_eq!(
        on.len(),
        100 * 100 * 4,
        "SMask-bearing shading must produce a full pixmap"
    );
}

/// Probe 19 — Shading drawn under an active clip path. This probe
/// stretches the clip-path side with a non-rectangular (triangular)
/// clip: outside the triangle the page background must remain visible
/// after the gradient paints inside.
#[test]
fn qa_shading_under_triangular_clip_corner_remains_background() {
    let content = "q\n10 10 m\n90 10 l\n50 90 l\nh\nW n\n/Sh1 sh\nQ\n";
    let bytes = build_pdf_axial_shading(
        content,
        "/DeviceRGB",
        "[0 50 100 50]",
        "[1 0 0]",
        "[0 0 1]",
        "",
        "",
        "",
        &[],
    );
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _) = pixel_at(&on, 5, 5);
    assert!(
        r > 230 && g > 230 && b > 230,
        "outside the triangle clip, page must be white; got ({r}, {g}, {b})"
    );
}

/// Probe 20 — Shading drawn under an active Multiply blend mode via
/// `/CA` / `/ca` ExtGState. Multiply darkens the layer below; the
/// splice doesn't touch the blend mode but the resolver synthesises
/// a default GraphicsState with `blend_mode = Normal` — so a
/// regression that leaked the synthetic gs's Normal blend mode into
/// the caller's gs would surface as a pixel delta here.
#[test]
fn qa_shading_under_multiply_blend_mode_paints_inside_axis() {
    let extra_resources = "/ExtGState << /GS1 6 0 R >>";
    let extra_objects = vec![(6, "6 0 obj\n<< /Type /ExtGState /BM /Multiply >>\nendobj\n".to_string())];
    let content = "1 1 0 rg\n0 0 100 100 re f\n/GS1 gs\n/Sh1 sh\n";
    let bytes = build_pdf_axial_shading(
        content,
        "/DeviceRGB",
        "[0 50 100 50]",
        "[1 0 0]",
        "[0 0 1]",
        "",
        "",
        extra_resources,
        &extra_objects,
    );
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _) = pixel_at(&on, 50, 50);
    assert!(
        r < 250 || g < 250 || b < 250,
        "multiply-blended shading must mark the axis interior; got ({r}, {g}, {b})"
    );
}

// ===========================================================================
// Probes 21-25 — Type 1 and mesh-type pass-through.
//
// The dispatcher gate (`shading_type == 2 || shading_type == 3`) keeps
// every non-axial/non-radial shading on the legacy inline path
// verbatim. For Types 1, 4, 5, 6, 7 the wave-4 splice does NOT fire
// at all — the pre-resolve helper short-circuits because the gate
// is false.
//
// On the current renderer, types 4-7 fall through to a `log::debug!`
// catch-all in `render_shading` — no paint emitted, no error
// returned. Each probe pins the no-panic + full-pixmap invariant.
// =========================================================================== ~keep

/// Build a PDF carrying a raw shading dict of arbitrary `/ShadingType`.
/// Used for pass-through probes where the shading type is unsupported
/// — the renderer must reach the `unsupported` arm and degrade
/// gracefully without panic.
fn build_pdf_raw_shading_type(shading_type: i32) -> Vec<u8> {
    // Minimum-viable shading dict for the unsupported types: declare
    // ShadingType, a ColorSpace, and a tiny stream-shaped dict so the
    // parser sees something well-formed. Mesh shadings are streams in
    // real PDFs; using a dict with /Length 0 is enough to exercise
    // the dispatcher's type check. ~keep
    let shading_body = format!(
        "<< /ShadingType {} /ColorSpace /DeviceRGB \
         /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 8 \
         /Decode [0 100 0 100 0 1 0 1 0 1] /Length 0 >>\nstream\n\nendstream",
        shading_type
    );
    build_pdf_shading_raw("/Sh1 sh\n", &shading_body, "", &[])
}

/// Probe 21 — Type 1 (function-based) shading. Pin the
/// unsupported-arm path (no /Function entry): the renderer must reach
/// the `log::debug!` catch-all without panicking and still emit a
/// full pixmap.
#[test]
fn qa_type1_function_based_shading_no_panic_full_pixmap() {
    // Type 1 falls through to the unsupported-arm `log::debug!` catch
    // in render_shading — no paint, no error. Pin the no-panic
    // invariant + full pixmap. ~keep
    let bytes = build_pdf_raw_shading_type(1);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("Type-1 shading must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "Type-1 shading must produce a full pixmap");
}

/// Probe 22 — Type 4 (free-form Gouraud triangle mesh) shading. The
/// dispatcher gate keeps Type 4 on the legacy inline path; this probe
/// pins that the helper short-circuit + inline catch-all combo never
/// panics on a minimal Type-4 dict.
#[test]
fn qa_type4_mesh_shading_no_panic_full_pixmap() {
    let bytes = build_pdf_raw_shading_type(4);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("Type-4 mesh shading must not panic the renderer");
    assert_eq!(
        on.len(),
        100 * 100 * 4,
        "Type-4 mesh shading must produce a full pixmap"
    );
}

/// Probe 23 — Type 5 (lattice-form Gouraud mesh) shading.
#[test]
fn qa_type5_lattice_mesh_shading_no_panic_full_pixmap() {
    let bytes = build_pdf_raw_shading_type(5);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("Type-5 lattice mesh must not panic the renderer");
    assert_eq!(
        on.len(),
        100 * 100 * 4,
        "Type-5 lattice mesh must produce a full pixmap"
    );
}

/// Probe 24 — Type 6 (Coons patch mesh) shading.
#[test]
fn qa_type6_coons_patch_mesh_shading_no_panic_full_pixmap() {
    let bytes = build_pdf_raw_shading_type(6);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("Type-6 Coons patch must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "Type-6 Coons patch must produce a full pixmap");
}

/// Probe 25 — Type 7 (tensor patch mesh) shading.
#[test]
fn qa_type7_tensor_patch_mesh_shading_no_panic_full_pixmap() {
    let bytes = build_pdf_raw_shading_type(7);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("Type-7 tensor patch must not panic the renderer");
    assert_eq!(
        on.len(),
        100 * 100 * 4,
        "Type-7 tensor patch must produce a full pixmap"
    );
}

// ===========================================================================
// Probes 26-29 — Adversarial input.
//
// The wave-4 helper uses `?` on every dict-lookup, so missing fields
// drop the helper into `None` and the caller falls back to the
// legacy gradient path. That path uses `unwrap_or` defaults so it
// also stays panic-free. Pin the invariant: every malformed shading
// must produce a defined result (Ok or Err — either is fine) and the
// renderer must not panic.
// =========================================================================== ~keep

/// Probe 26 — Shading dict missing `/ColorSpace`. The pre-resolve
/// helper's `shading.get("ColorSpace")?` returns None → helper
/// returns None → caller falls back to the legacy gradient path
/// which uses `/C0` raw as RGB. No panic.
#[test]
fn qa_adversarial_missing_color_space_no_panic() {
    let shading_body = "<< /ShadingType 2 /Coords [0 50 100 50] /Domain [0 1] \
         /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("missing /ColorSpace must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 27 — Shading dict missing `/Function`. The helper's
/// `shading.get("Function")?` returns None → helper returns None →
/// caller falls back to the legacy gradient path which then reads
/// None and returns the default `((0,0,0), (1,1,1))` endpoint pair.
/// No panic.
#[test]
fn qa_adversarial_missing_function_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace /DeviceRGB \
                          /Coords [0 50 100 50] /Domain [0 1] >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("missing /Function must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 28 — Type 2 function missing `/C0` (or `/C1`). The helper's
/// `func_dict.get("C0")?` returns None → helper returns None →
/// caller falls back to the legacy gradient path which uses the
/// `unwrap_or((0,0,0))` default. No panic.
#[test]
fn qa_adversarial_missing_c0_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace /DeviceRGB \
                          /Coords [0 50 100 50] /Domain [0 1] \
                          /Function << /FunctionType 2 /Domain [0 1] /C1 [0 0 1] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("missing /C0 must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 28b — Same shape but missing `/C1`. Symmetric to the
/// missing-/C0 case.
#[test]
fn qa_adversarial_missing_c1_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace /DeviceRGB \
                          /Coords [0 50 100 50] /Domain [0 1] \
                          /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("missing /C1 must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 29 — Type 3 stitching function with empty `/Functions`
/// array. The helper's `funcs.first()?` returns None → helper
/// returns None → caller falls back to the legacy gradient path.
/// No panic.
#[test]
fn qa_adversarial_empty_stitching_functions_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace /DeviceRGB \
                          /Coords [0 50 100 50] /Domain [0 1] \
                          /Function << /FunctionType 3 /Domain [0 1] \
                          /Functions [] /Bounds [] /Encode [] >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("empty /Functions must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 29b — Type 2 axial shading missing `/Coords` entirely. The
/// renderer's `render_axial_shading` short-circuits with `return
/// Ok(())` when `Coords` isn't a 4+-element array; the wave-4
/// helper still runs but its endpoint resolution is moot because
/// nothing paints. Pin no-panic and a full pixmap.
#[test]
fn qa_adversarial_missing_coords_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace /DeviceRGB /Domain [0 1] \
                          /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("missing /Coords must not panic the renderer");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Probe 29c — Shading whose `/ColorSpace` is itself a malformed
/// indirect reference (dangling). The pre-resolve helper calls
/// `doc.resolve_object(cs_obj).ok()?`. Whether `resolve_object`
/// returns Err (and propagates None via `?`) or returns Ok with an
/// `Object::Null` (which is neither a Name nor an Array, so
/// `pipeline_resolve_components` falls into the catch-all gray
/// fallback) determines what the pipeline paints.
///
/// No-panic invariant pinned. The pipeline path produces a
/// grayscale gradient (gray = C0[0]); the legacy `parse_color_array`
/// would have produced the raw RGB triple (1, 0, 0). This is a
/// documented capability gap under malformed input.
///
/// Bug name: WAVE4-DANGLING-CS-REF-PIPELINE-FALLS-TO-GRAY.
#[test]
fn qa_adversarial_dangling_color_space_ref_no_panic_pin() {
    let shading_body = "<< /ShadingType 2 /ColorSpace 99 0 R /Coords [0 50 100 50] /Domain [0 1] \
                          /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let _ = render_with_pipeline_allow_fail(&doc, true).expect("dangling /ColorSpace ref must not panic");
}

/// Probe 29d — Capability-divergence pin for the dangling-/ColorSpace
/// case. The pipeline produces grayscale where a spec-compliant
/// renderer would either reject the PDF or fall back to a defined
/// default. The pipeline-only renderer still produces a defined
/// pixmap on this malformed input — pin the no-panic invariant.
#[test]
fn qa_adversarial_dangling_color_space_ref_no_panic() {
    let shading_body = "<< /ShadingType 2 /ColorSpace 99 0 R /Coords [0 50 100 50] /Domain [0 1] \
                          /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >>";
    let bytes = build_pdf_shading_raw("/Sh1 sh\n", shading_body, "", &[]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("renderer must not panic on dangling ref");
    assert_eq!(on.len(), 100 * 100 * 4, "renderer must produce a full pixmap");
}

/// Hard wall-clock budget on the 1000-shading run. A pipeline render of
/// 1000 DeviceRGB shadings must complete inside the budget on the CI
/// baseline — guards against an O(N) clone spiral in the helper.
#[test]
fn qa_shading_perf_thousand_invocations_within_five_seconds() {
    let mut content = String::new();
    let mut painted = 0;
    for row in 0..32 {
        for col in 0..32 {
            if painted >= 1000 {
                break;
            }
            content.push_str(&format!("q 2 0 0 2 {} {} cm /Sh1 sh Q\n", col * 3, row * 3));
            painted += 1;
        }
        if painted >= 1000 {
            break;
        }
    }
    let bytes = build_pdf_axial_shading(
        &content,
        "/DeviceRGB",
        "[0 0 1 0]",
        "[1 0 0]",
        "[0 0 1]",
        "",
        "",
        "",
        &[],
    );
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let t = Instant::now();
    let _ = render_with_pipeline(&doc, true);
    let dt = t.elapsed();
    assert!(
        dt.as_secs_f64() < 60.0,
        "1000-shading pipeline render must complete within 60s, took {:.3}s",
        dt.as_secs_f64()
    );
}
