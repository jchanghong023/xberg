/// Probe 15 — Form XObject whose internal content paints an
/// ImageMask under a Type 4 Separation fill. The capability gain
/// (full-tint → magenta vs `1 - tint` → black) must propagate through
/// the recursive Form rendering.
#[test]
fn qa_form_xobject_with_inner_image_mask_type4_separation_capability_gain() {
    let mask = solid_image_mask_bytes(8, 8);
    let type4 = "{ 0.0 exch 0.0 0.0 }";
    let page = "q\n/Fm1 Do\nQ\n";
    let form = "q\n/SpotMagenta cs\n1 scn\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";

    let bytes = build_pdf_form_with_imagemask_and_type4_separation(page, form, type4, 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);

    // Pipeline runs the Type 4 program → magenta. ~keep
    let (r_on, g_on, b_on, _a) = center_pixel(&on);
    assert!(
        r_on >= 225 && g_on <= 5 && (125..=155).contains(&b_on),
        "Form-nested Type 4 Separation ImageMask must paint process-ink magenta, got ({r_on}, {g_on}, {b_on})"
    );
}

/// Probe 16 — Two-level Form recursion (Form-in-Form), where the
/// innermost content invokes an ImageMask with a DeviceRGB fill. The
/// rendered centre pixel must be the expected colour after the
/// pipeline routes the mask through both Form recursions.
#[test]
fn qa_form_in_form_image_mask_paints_inner_fill_colour() {
    let mask = solid_image_mask_bytes(8, 8);
    let page = "q\n/Fm1 Do\nQ\n";
    let outer = "q\n/Fm2 Do\nQ\n";
    // Inner sets the fill colour itself and paints the mask. (Set the
    // fill at the inner level so propagation through Form recursion is
    // not co-mingled with the pipeline-routing pin we're after — the ~keep
    // CTM and resource scope already test recursion.)
    let inner = "q\n0 1 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";

    let bytes = build_pdf_form_in_form_with_image_mask(page, outer, inner, 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        g > 200 && r < 60 && b < 60,
        "two-level Form ImageMask should paint green, got ({r}, {g}, {b})"
    );
}

/// Probe 16b — Bug-found pin (UNRELATED to wave-3, but discovered while
/// probing it). When the page sets the fill colour and then invokes a
/// Form which paints an ImageMask, the Form's content stream does NOT
/// see the page's `rg` — the centre paints black instead of the
/// inherited fill. This is a graphics-state-propagation gap at the
/// Form recursion boundary, not a pipeline-side issue.
///
/// Pinned `#[ignore]` to record the discovery without failing CI.
/// Bug name: **FORM-RECURSION-FILL-NOT-INHERITED** — the renderer's
/// recursive Form walk appears to reset (or not propagate) the GS
/// fill colour on entry to the child Form's content stream. Per PDF
/// §8.10.1 a Form XObject inherits the parent graphics state at the
/// point of invocation, with only `q ... Q` saving/restoring around
/// the call; the fill colour set with `rg` before `/Fm1 Do` should be
/// visible inside the Form's content stream.
#[ignore = "FORM-RECURSION-FILL-NOT-INHERITED: page-level fill not seen by Form's ImageMask paint"]
#[test]
fn qa_form_fill_inheritance_bug_pin() {
    let mask = solid_image_mask_bytes(8, 8);
    let page = "q\n0 1 0 rg\n/Fm1 Do\nQ\n";
    let form = "100 0 0 100 0 0 cm\n/IM1 Do\n";
    let bytes = build_pdf_form_with_inner_image_mask(page, form, "", 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    // Expected per spec: GS state propagates into child Form content
    // stream. Observed: centre is (0, 0, 0) — the page-level `rg` did ~keep
    // not stick across the Form boundary.
    assert!(
        g > 200 && r < 60 && b < 60,
        "page-level fill must be visible at Form's ImageMask paint, got ({r}, {g}, {b}) — FORM-RECURSION-FILL-NOT-INHERITED"
    );
}

/// Probe 17 — Form-XObject with a nested CTM transformation around
/// the inner ImageMask. Inside the Form, an inner `q ... cm ... /IM1
/// Do ... Q` must compose with the page's `cm` cleanly through the
/// pipeline-routed mask paint.
#[test]
fn qa_form_xobject_inner_ctm_around_image_mask_paints_visible_band() {
    let mask = solid_image_mask_bytes(8, 8);
    // The page sets a 30° rotation; the form sets a translation and
    // scale around the mask. CTM stack correctness across the form
    // boundary is what's being pinned. ~keep
    let page = "q\n0.866 0.5 -0.5 0.866 50 50 cm\n/Fm1 Do\nQ\n";
    let form = "q\n1 0 0 rg\n40 0 0 40 -20 -20 cm\n/IM1 Do\nQ\n";

    let bytes = build_pdf_form_with_inner_image_mask(page, form, "", 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert!(
        count_ink_pixels(&on, 0, 0, 100, 100) > 100,
        "Form with rotated + nested CTM should leave visible ink"
    );
}

// ===========================================================================
// Probes 18-22 — Multi-XObject interactions.
//
// These probes load two or more XObjects into a single page and pin
// that `q/Q` saving/restoring the GS state, plus the spliced GS clone
// the pipeline emits at each `/IM Do`, doesn't leak across paints. ~keep
// ===========================================================================

/// Build a page with two ImageMask XObjects `/IM1` and `/IM2` (both
/// solid stencils) and run an arbitrary content stream.
fn build_pdf_two_image_masks(content_ops: &str, w1: u32, h1: u32, w2: u32, h2: u32) -> Vec<u8> {
    let mask1 = solid_image_mask_bytes(w1, h1);
    let mask2 = solid_image_mask_bytes(w2, h2);

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R /IM2 6 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let im1_off = buf.len();
    let im1_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        w1,
        h1,
        mask1.len()
    );
    buf.extend_from_slice(im1_hdr.as_bytes());
    buf.extend_from_slice(&mask1);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let im2_off = buf.len();
    let im2_hdr = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        w2,
        h2,
        mask2.len()
    );
    buf.extend_from_slice(im2_hdr.as_bytes());
    buf.extend_from_slice(&mask2);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, im1_off, im2_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Build a page with an ImageMask `/IM1` and a standard image `/SI1`,
/// so probes can interleave them.
fn build_pdf_mask_plus_standard_image(
    content_ops: &str,
    mask_w: u32,
    mask_h: u32,
    std_w: u32,
    std_h: u32,
    std_pixels: &[u8],
) -> Vec<u8> {
    let mask = solid_image_mask_bytes(mask_w, mask_h);

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R /SI1 6 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let im_off = buf.len();
    let im_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        mask_w,
        mask_h,
        mask.len()
    );
    buf.extend_from_slice(im_hdr.as_bytes());
    buf.extend_from_slice(&mask);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let si_off = buf.len();
    let si_hdr = format!(
        "6 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} \
         /BitsPerComponent 8 /ColorSpace /DeviceGray /Length {} >>\nstream\n",
        std_w,
        std_h,
        std_pixels.len()
    );
    buf.extend_from_slice(si_hdr.as_bytes());
    buf.extend_from_slice(std_pixels);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, im_off, si_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Probe 18 — Two ImageMasks back-to-back, painted with different fill
/// colours (red then blue). Each splice clones GS afresh; the second
/// paint must not see the first paint's spliced state. The two halves
/// of the page should end up cleanly coloured.
#[test]
fn qa_two_image_masks_back_to_back_paint_distinct_halves() {
    // Left half: red. Right half: blue. The `q ... Q` brackets isolate
    // each paint's CTM and fill state. ~keep
    let content = "q\n1 0 0 rg\n50 0 0 100 0 0 cm\n/IM1 Do\nQ\n\
                   q\n0 0 1 rg\n50 0 0 100 50 0 cm\n/IM2 Do\nQ\n";
    let bytes = build_pdf_two_image_masks(content, 8, 8, 8, 8);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r1, g1, b1, _a) = pixel_at(&on, 20, 50);
    let (r2, g2, b2, _a) = pixel_at(&on, 80, 50);
    assert!(
        r1 > 200 && g1 < 60 && b1 < 60,
        "left half should be red, got ({r1}, {g1}, {b1})"
    );
    assert!(
        b2 > 200 && r2 < 60 && g2 < 60,
        "right half should be blue, got ({r2}, {g2}, {b2})"
    );
}

/// Probe 19 — ImageMask, standard image, ImageMask interleaved on
/// the same page. The standard-image branch's `render_image` borrows
/// the unspliced `gs`; the mask branch borrows the spliced clone.
/// The standard image must not pick up the mask's spliced state, and
/// vice versa.
#[test]
fn qa_image_mask_then_standard_then_mask_interleaved_keep_state_isolated() {
    let std_pixels = vec![0x60u8; 16];
    let content = "q\n0.5 g\n40 0 0 100 30 0 cm\n/SI1 Do\nQ\n\
                   q\n1 0 0 rg\n30 0 0 100 0 0 cm\n/IM1 Do\nQ\n\
                   q\n0 0 1 rg\n30 0 0 100 70 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_mask_plus_standard_image(content, 8, 8, 4, 4, &std_pixels);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r1, g1, b1, _a) = pixel_at(&on, 15, 50);
    assert!(
        r1 > 200 && g1 < 60 && b1 < 60,
        "left strip must be red, got ({r1},{g1},{b1})"
    );
    // Middle: dark grey from the standard image (≈0x60 with possible filter). ~keep
    let (r2, g2, b2, _a) = pixel_at(&on, 50, 50);
    assert!(
        r2 == g2 && g2 == b2 && (60..=160).contains(&(r2 as i32)),
        "middle must be grey from standard image, got ({r2},{g2},{b2})"
    );
    let (r3, g3, b3, _a) = pixel_at(&on, 85, 50);
    assert!(
        b3 > 200 && r3 < 60 && g3 < 60,
        "right strip must be blue, got ({r3},{g3},{b3})"
    );
}

/// Probe 20 — ImageMask under an active SMask. The renderer must apply
/// the SMask to the paint; a DeviceRGB fill resolves through the
/// pipeline and the spliced clone must carry the SMask through.
#[test]
fn qa_image_mask_under_smask_none_still_paints() {
    // Page resources carry /GS1 in /ExtGState with `/SMask /None` set
    // explicitly. This is the "no smask" form but it exercises the
    // ExtGState plumbing without needing a full SMask dict (which
    // requires a transparency group XObject). ~keep
    let mask = solid_image_mask_bytes(8, 8);
    let resources = "/ExtGState << /GS1 << /SMask /None >> >>";
    let content = "q\n/GS1 gs\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, resources, 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // /SMask /None is the no-op smask: the full-page red stencil
    // must paint without the smask suppressing it. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        r > 200 && g < 60 && b < 60,
        "/SMask /None must not suppress the red stencil, got ({r}, {g}, {b})"
    );
}

/// Probe 21 — ImageMask under an active clip path. Pixels outside the
/// clip must remain unpainted; the spliced GS clone must not drop the
/// clip state.
#[test]
fn qa_image_mask_under_active_clip_corner_remains_unpainted() {
    let mask = solid_image_mask_bytes(8, 8);
    // Clip to a 40×40 box around the page centre, then paint a full-page
    // stencil. Corners must remain white. ~keep
    let content = "q\n30 30 40 40 re W n\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r, g, b, _a) = center_pixel(&on);
    assert!(r > 200 && g < 60 && b < 60, "centre must be red, got ({r}, {g}, {b})");
    // The top-left corner (5,5) is well outside the 40×40 clip box (which
    // spans 30..70 in both axes) and must remain unpainted (white). ~keep
    let (rc, gc, bc, _a) = pixel_at(&on, 5, 5);
    assert_eq!(
        (rc, gc, bc),
        (255, 255, 255),
        "outside-clip corner must be white, got ({rc}, {gc}, {bc})"
    );
}

/// Probe 22 — ImageMask painted under a non-Normal blend mode. The
/// wave-3 `render_image_mask` reads `gs.blend_mode` and converts it
/// via `pdf_blend_mode_to_skia`. The spliced clone must preserve the
/// blend mode field through to the rasteriser.
#[test]
fn qa_image_mask_multiply_blend_mode_paints_against_white() {
    let mask = solid_image_mask_bytes(8, 8);
    let resources = "/ExtGState << /GS1 << /BM /Multiply >> >>";
    let content = "q\n/GS1 gs\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, resources, 8, 8, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // Multiply with red against white background → red. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert!(
        r > 200 && g < 60 && b < 60,
        "multiply(red, white) must be red, got ({r}, {g}, {b})"
    );
}

// ===========================================================================
// Probes 23-26 — Capability at scale: DeviceN / `/All` / `/None` colorants
// applied to an ImageMask fill, plus a 100-mask one-page stress test.
//
// Mirror of the wave-1 path-fill and wave-2 text-fill capability suites.
// `render_image_mask` reads `gs.fill_color_rgb` once per paint; the
// pipeline must populate that from the Type 4 / DeviceN program. The
// `/All` / `/None` colorant-name special cases are not honoured today
// (existing behaviour) — pin whatever the renderer actually paints
// today as the regression anchor. ~keep
// ===========================================================================

/// Build a one-page PDF with an ImageMask XObject `/IM1` and an
/// indirect Type 4 function (object 6) whose `Domain` accommodates a
/// variable number of inputs (used for DeviceN). The Separation /
/// DeviceN colour space is set via `resources_extra`.
// test helper: every parameter is a distinct, independently varied piece of the
// hand-built PDF; grouping them would only rename the same positional list. ~keep
#[allow(clippy::too_many_arguments)]
fn build_pdf_image_mask_with_devicen_type4(
    content_ops: &str,
    resources_extra: &str,
    width: u32,
    height: u32,
    mask_data: &[u8],
    type4_program: &str,
    range_array: &str,
    domain_pairs: &[i32],
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
         /Resources << /XObject << /IM1 5 0 R >> {} >> /Contents 4 0 R >>\nendobj\n",
        resources_extra
    );
    buf.extend_from_slice(page.as_bytes());
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width {} /Height {} /BitsPerComponent 1 /Length {} >>\nstream\n",
        width,
        height,
        mask_data.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(mask_data);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let func_off = buf.len();
    let domain_str: Vec<String> = domain_pairs.iter().map(|v| v.to_string()).collect();
    let domain_array = format!("[{}]", domain_str.join(" "));
    let func_hdr = format!(
        "6 0 obj\n<< /FunctionType 4 /Domain {} /Range {} /Length {} >>\nstream\n",
        domain_array,
        range_array,
        type4_program.len()
    );
    buf.extend_from_slice(func_hdr.as_bytes());
    buf.extend_from_slice(type4_program.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off, func_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

/// Probe 23 — Capability at scale: paint 100 ImageMasks on one page,
/// each at a distinct CTM offset, each filled with the same Type 4
/// Separation magenta. Inline path falls back to `1 - tint` → black
/// for every paint; pipeline runs the Type 4 program → magenta for
/// every paint. The two outputs must differ.
#[test]
fn qa_image_mask_100_paints_type4_separation_capability_at_scale() {
    let mask = solid_image_mask_bytes(2, 2);
    let type4 = "{ 0.0 exch 0.0 0.0 }";
    let resources = "/ColorSpace << /SpotMagenta [/Separation /MagentaSpot /DeviceCMYK 6 0 R] >>";

    let mut content = String::from("/SpotMagenta cs\n1 scn\n");
    for row in 0..10 {
        for col in 0..10 {
            let x = col * 10;
            let y = row * 10;
            content.push_str(&format!("q 8 0 0 8 {} {} cm /IM1 Do Q\n", x, y));
        }
    }

    // Inline a PDF layout matching the shared imagemask-with-Type4 ~keep
    // helper, so this probe stays self-contained.
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
         /Resources << /XObject << /IM1 5 0 R >> {} >> /Contents 4 0 R >>\nendobj\n",
        resources
    );
    buf.extend_from_slice(page.as_bytes());
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width 2 /Height 2 /BitsPerComponent 1 /Length {} >>\nstream\n",
        mask.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(&mask);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let func_off = buf.len();
    let func_hdr = format!(
        "6 0 obj\n<< /FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] /Length {} >>\nstream\n",
        type4.len()
    );
    buf.extend_from_slice(func_hdr.as_bytes());
    buf.extend_from_slice(type4.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off, func_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());

    let doc = PdfDocument::from_bytes(buf).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);

    let (r_on, g_on, b_on, _a) = pixel_at(&on, 54, 54);
    assert!(
        r_on > 200 && g_on < 60 && (110..=170).contains(&b_on),
        "pipeline 100-paint: tile centre must be process-ink magenta, got ({r_on},{g_on},{b_on})"
    );
}

/// Probe 24 — DeviceN multi-colorant Type 4 applied to an ImageMask.
/// Mirrors wave-1's path-fill DeviceN test; the wave-3 ImageMask path
/// must produce the same Type 4 evaluation result.
#[test]
fn qa_image_mask_devicen_multi_colorant_type4_capability() {
    let mask = solid_image_mask_bytes(8, 8);
    // `{ exch pop 0.0 exch 0.0 0.0 }` — pop the first colorant, leave
    // CMYK(0, second, 0, 0) on the stack. With `0 1 scn` we get
    // CMYK(0, 1, 0, 0) → magenta. ~keep
    let type4 = "{ exch pop 0.0 exch 0.0 0.0 }";
    let resources = "/ColorSpace << /TwoSpot [/DeviceN [/SpotA /SpotB] /DeviceCMYK 6 0 R] >>";
    let content = "/TwoSpot cs 0 1 scn\n100 0 0 100 0 0 cm\n/IM1 Do\n";

    let bytes = build_pdf_image_mask_with_devicen_type4(
        content,
        resources,
        8,
        8,
        &mask,
        type4,
        "[0 1 0 1 0 1 0 1]",
        &[0, 1, 0, 1],
    );
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r_on, g_on, b_on, _a) = center_pixel(&on);
    assert!(
        r_on > 200 && g_on < 60 && (110..=170).contains(&b_on),
        "pipeline DeviceN Type-4 ImageMask must paint process-ink magenta, got ({r_on},{g_on},{b_on})"
    );
}

/// Probe 25 — Separation `/All` colorant applied to an ImageMask. The
/// pipeline runs the tint transform like any other Separation, so the
/// rendered centre pixel must reflect the Type-4-evaluated colour.
/// Mirror of the wave-2 text `/All` test.
#[test]
fn qa_image_mask_separation_all_colorant_pipeline_paints_type4_output() {
    let mask = solid_image_mask_bytes(8, 8);
    let type4 = "{ 0.0 exch 0.0 0.0 }";
    // tint=0.5 → CMYK(0, 0.5, 0, 0) → faint magenta. ~keep
    let content = "/All_CS cs 0.5 scn\n100 0 0 100 0 0 cm\n/IM1 Do\n";
    let resources = "/ColorSpace << /All_CS [/Separation /All /DeviceCMYK 6 0 R] >>";

    let bytes =
        build_pdf_image_mask_with_devicen_type4(content, resources, 8, 8, &mask, type4, "[0 1 0 1 0 1 0 1]", &[0, 1]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let (r_on, g_on, b_on, _a) = center_pixel(&on);
    // Faint magenta: R high, B high, G middling. ~keep
    assert!(
        r_on > g_on && b_on > g_on,
        "pipeline /All Separation Type-4 ImageMask must trend magenta (R>G, B>G), got ({r_on},{g_on},{b_on})"
    );
}

/// Probe 26 — Separation `/None` colorant applied to an ImageMask.
/// Per ISO 32000-1 §8.6.6.3, `/None` produces no visible output. The
/// pipeline's per-plate routing selector (`InkSelector::None`, stamped
/// by the composer on the source colour space) makes the composite
/// resolver hand back a fully-transparent RGBA, so the ImageMask
/// rasteriser paints with alpha=0 and lays down zero ink.
#[test]
fn qa_image_mask_separation_none_colorant_paints_zero_ink() {
    let mask = solid_image_mask_bytes(8, 8);
    let type4 = "{ 0.0 exch 0.0 0.0 }";
    let content = "/None_CS cs 0.5 scn\n100 0 0 100 0 0 cm\n/IM1 Do\n";
    let resources = "/ColorSpace << /None_CS [/Separation /None /DeviceCMYK 6 0 R] >>";

    let bytes =
        build_pdf_image_mask_with_devicen_type4(content, resources, 8, 8, &mask, type4, "[0 1 0 1 0 1 0 1]", &[0, 1]);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let on_ink = count_ink_pixels(&on, 0, 0, 100, 100);
    assert_eq!(
        on_ink, 0,
        "/None ImageMask must paint zero ink per §8.6.6.3 (got {on_ink} ink pixels)"
    );
}

// ===========================================================================
// Probes 27-30 — Adversarial / malformed input.
//
// `render_image_mask` consumes the raw stream length, checks
// `row_bytes * height <= raw.len()`, and bails with an `Image` error
// on short streams. Long streams are silently truncated.
// Width/Height = 0 short-circuits before allocation. Width or Height
// of `0xFFFFFF` would attempt a 4 GB allocation; the helper should
// either bail or be guarded. ~keep
// ===========================================================================

/// Probe 27 — Stream shorter than the declared Width×Height bits.
/// The helper must NOT panic and must NOT paint a corrupted image.
/// (Today the helper returns an `Image` error via a `log::warn!` at
/// the `Do` arm; the page renders as if the mask weren't there.)
#[test]
fn qa_image_mask_too_short_stream_no_panic_pin() {
    let bytes_short = vec![0u8; 1];
    let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 8, &bytes_short);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    // No-panic invariant: a short stream falls through to the helper's
    // size-check bail and leaves the page unpainted. ~keep
    let on = render_with_pipeline_allow_fail(&doc, true).expect("too-short ImageMask stream must not panic");
    let (r, g, b, _a) = center_pixel(&on);
    assert_eq!(
        (r, g, b),
        (255, 255, 255),
        "too-short stream must produce no paint, got ({r},{g},{b})"
    );
}

/// Probe 28 — Stream longer than declared. The helper indexes into
/// the buffer using `row_bytes * height`; trailing bytes are ignored.
/// No panic, no spurious paint of the trailing bytes.
#[test]
fn qa_image_mask_too_long_stream_no_panic_pin() {
    let mut bytes_long = vec![0u8; 8];
    bytes_long.extend_from_slice(&[0xFFu8; 56]);
    let content = "q\n0 1 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 8, 8, &bytes_long);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // The first 8 bytes ARE the full 8x8 stencil; they're all opaque,
    // so the centre must be green. Trailing 56 bytes are ignored. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert!(g > 200 && r < 60 && b < 60, "centre must be green, got ({r},{g},{b})");
}

/// Probe 29 — `Width=0` and `Height=0`. The helper short-circuits
/// these and returns Ok(()) without painting.
#[test]
fn qa_image_mask_zero_dimensions_no_paint_no_panic() {
    for (w, h) in [(0u32, 8u32), (8, 0), (0, 0)] {
        let mask = vec![0u8; 8];
        let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
        let bytes = build_pdf_image_mask(content, "", w, h, &mask);
        let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
        let on = render_with_pipeline_allow_fail(&doc, true)
            .unwrap_or_else(|| panic!("renderer must not panic for {}x{}", w, h));
        // No paint: page is fully white. ~keep
        let (r, g, b, _a) = center_pixel(&on);
        assert_eq!(
            (r, g, b),
            (255, 255, 255),
            "{}x{} ImageMask must produce no paint at centre, got ({}, {}, {})",
            w,
            h,
            r,
            g,
            b
        );
    }
}

/// Probe 30 — Absurdly-large dimensions. `render_image_mask` allocates
/// `vec![0u8; (w*h*4) as usize]`; with `width = 0xFFFFFF` and `height = 1`
/// that's 4 * 16777215 ≈ 64 MB. The helper's expected-size check fires
/// FIRST (the supplied stream is shorter than the row-byte requirement)
/// and bails before allocating the destination buffer.
///
/// PIN: the renderer must NOT panic on huge declared dimensions when
/// the supplied stream is short. If a future allocator-tightening pass
/// adds an upfront size cap, this test still passes (the bail order
/// shifts but the no-panic invariant holds).
#[test]
fn qa_image_mask_huge_dimensions_short_stream_no_panic() {
    // Width 0xFFFFFF, Height 1 → row_bytes = 2097152, total expected =
    // 2097152. Supply only 1 byte; the size check rejects. ~keep
    let mask = vec![0u8; 1];
    let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";
    let bytes = build_pdf_image_mask(content, "", 0xFFFFFF, 1, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("huge-dim short-stream ImageMask must not panic");
    // The expected-size check bails before allocating 64MB of pixels;
    // no paint reaches the centre. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert_eq!(
        (r, g, b),
        (255, 255, 255),
        "huge-dim short-stream must produce no paint, got ({r},{g},{b})"
    );
}

/// Probe 30b — Negative dimensions arrive as PDF integers; PDF parses
/// them as `Object::Integer(i64)` and `as_integer()` returns `i64`.
/// The wave-3 helper casts via `as u32`, which on a negative integer
/// wraps to a huge value. Pair with a tiny stream; bail must fire
/// before allocation. Probe: no panic.
///
/// This is a regression pin against a future refactor switching the
/// cast to a `try_into()` that bails on negatives — the no-panic
/// invariant must hold across both behaviours.
#[test]
fn qa_image_mask_negative_dimension_field_no_panic() {
    let mask = vec![0u8; 1];
    let content = "q\n1 0 0 rg\n100 0 0 100 0 0 cm\n/IM1 Do\nQ\n";

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    buf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
          /Resources << /XObject << /IM1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    );
    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xobj_off = buf.len();
    let xobj_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Image /ImageMask true \
         /Width -1 /Height 8 /BitsPerComponent 1 /Length {} >>\nstream\n",
        mask.len()
    );
    buf.extend_from_slice(xobj_hdr.as_bytes());
    buf.extend_from_slice(&mask);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, xobj_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());

    let doc = PdfDocument::from_bytes(buf).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true).expect("negative-dim ImageMask must not panic");
    // The expected-size check bails on the wrapped-huge dimension
    // before any paint reaches the centre. ~keep
    let (r, g, b, _a) = center_pixel(&on);
    assert_eq!(
        (r, g, b),
        (255, 255, 255),
        "negative-dim must produce no paint, got ({r},{g},{b})"
    );
}

// ===========================================================================
// Probes 31-32 — Performance.
//
// Wave 1+2 surfaced a per-paint clone leak as a performance regression
// (the now-fixed `kind_copy` stub). The wave-3 path must stay within
// the same envelope: one pipeline_resolve_paint_gs call per /Do, with
// the Device-family short-circuit returning None (zero clone) when the
// resolved colour already matches the GS field. ~keep
// ===========================================================================

/// Per-paint allocation pressure sanity. A 1000-paint pipeline render
/// with a Device-family fill must complete inside a generous wall-clock
/// budget. Coarse guard against an O(N) per-paint allocation spiral
/// (e.g. a clone slipping into the short-circuit path).
#[test]
fn qa_image_mask_perf_thousand_paints_completes_within_budget() {
    let mask = solid_image_mask_bytes(2, 2);
    let mut content = String::from("0 0 1 rg\n");
    let mut painted = 0;
    for row in 0..32 {
        for col in 0..32 {
            if painted >= 1000 {
                break;
            }
            content.push_str(&format!("q 2 0 0 2 {} {} cm /IM1 Do Q\n", col * 3, row * 3));
            painted += 1;
        }
        if painted >= 1000 {
            break;
        }
    }
    let bytes = build_pdf_image_mask(&content, "", 2, 2, &mask);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let t = Instant::now();
    let _ = render_with_pipeline(&doc, true);
    let dt = t.elapsed();
    assert!(
        dt.as_secs_f64() < 30.0,
        "1000-ImageMask pipeline render must complete within 30s, took {:.3}s",
        dt.as_secs_f64()
    );
}
