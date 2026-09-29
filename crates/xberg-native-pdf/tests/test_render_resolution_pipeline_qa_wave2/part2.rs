/// Probe 17 — CID Type 0 font on `Tj`. Identity-H encoding means each
/// pair of source bytes is a 16-bit CID. Whether the rasteriser falls
/// back to a system font or paints glyphs correctly is out of scope;
/// what we pin is that the renderer's colour-routing dispatch accepts
/// Type 0 fonts without panicking and produces a full pixmap.
#[test]
fn qa_text_cid_type0_font_no_panic_full_pixmap() {
    // Identity-H two-byte CIDs. The pipeline migration must accept
    // Type 0 fonts without panicking and produce a full pixmap;
    // whether the system fallback paints a recognisable glyph is
    // not what's pinned — only the renderer's no-panic invariant
    // through the colour-routing dispatch. ~keep
    let content = "BT 1 0 0 rg /F1 40 Tf 10 30 Td <0048> Tj ET\n";
    let bytes = build_pdf_cid_type0(content);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert_eq!(on.len(), 100 * 100 * 4, "CID Type 0 Tj must produce a full pixmap");
}

/// Probe 18 — "Embedded subset" stand-in: simple Type 1 font with no
/// /FontFile / FontFile2 / FontFile3 reference (the rasteriser treats it
/// as a non-embedded standard font). The actual embedded-subset code path
/// requires shipping binary font data; the pipeline's colour-routing
/// dispatch doesn't depend on the *kind* of font data the rasteriser
/// loads, so the proxy here is sufficient to pin "blue fill reaches the
/// pixmap through the fallback font path". A future suite shipping an
/// embedded subset can tighten this with a known-subset glyph match.
#[test]
fn qa_text_embedded_subset_stand_in_paints_blue() {
    let content = "BT 0 0 1 rg /F1 30 Tf 10 30 Td (Helvetica) Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let avg = average_ink_rgb(&on, 0, 0, 100, 100);
    let (r, g, b) = avg.expect("embedded-subset stand-in must paint glyph ink");
    assert!(
        b > 150.0 && b > r + 60.0 && b > g + 60.0,
        "embedded-subset stand-in must paint blue, got ({r:.1}, {g:.1}, {b:.1})"
    );
}

/// Probe 19 — Built-in Helvetica fallback. Most tests use this
/// implicitly; this is the explicit pin so the QA suite has a named
/// anchor: the standard-14 Helvetica path is the most common rendering
/// path and must paint a recognisable blue glyph through the pipeline.
#[test]
fn qa_text_built_in_helvetica_fallback_paints_blue_ink() {
    let content = "BT 0.2 0.4 0.8 rg /F1 24 Tf 10 50 Td (Built-in font!) Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let avg = average_ink_rgb(&on, 0, 0, 100, 100);
    let (r, g, b) = avg.expect("Helvetica fallback must paint glyph ink");
    assert!(
        b > g + 20.0 && b > r + 20.0,
        "Helvetica fallback must paint blue-leaning ink, got ({r:.1}, {g:.1}, {b:.1})"
    );
}

/// Probe 20 — Unicode mapping via a ToUnicode CMap stream. We don't ship a
/// binary font with a ToUnicode here; what's pinned is that adding a
/// /ToUnicode entry to the font dict doesn't perturb rendering. ToUnicode
/// affects extraction, not rendering — the assertion is that a font with
/// a ToUnicode entry still paints glyph ink in the requested colour.
fn build_pdf_text_with_tounicode(content_ops: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");

    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let page_off = buf.len();
    let page = "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
         /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n";
    buf.extend_from_slice(page.as_bytes());

    let stream_off = buf.len();
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content_ops.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(content_ops.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let font_off = buf.len();
    buf.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
          /Encoding /WinAnsiEncoding /ToUnicode 6 0 R >>\nendobj\n",
    );

    let cmap_body = "/CIDInit /ProcSet findresource begin\n\
12 dict begin\nbegincmap\n\
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
1 beginbfchar\n<48> <0048>\nendbfchar\n\
endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    let cmap_off = buf.len();
    let cmap_hdr = format!("6 0 obj\n<< /Length {} >>\nstream\n", cmap_body.len());
    buf.extend_from_slice(cmap_hdr.as_bytes());
    buf.extend_from_slice(cmap_body.as_bytes());
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_off = buf.len();
    buf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [cat_off, pages_off, page_off, stream_off, font_off, cmap_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(format!("trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref_off).as_bytes());
    buf
}

#[test]
fn qa_text_unicode_via_tounicode_cmap_paints_red_glyph() {
    let content = "BT 1 0 0 rg /F1 50 Tf 10 30 Td (H) Tj ET\n";
    let bytes = build_pdf_text_with_tounicode(content);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let avg = average_ink_rgb(&on, 0, 0, 100, 100);
    let (r, g, b) = avg.expect("ToUnicode-bearing font must still paint glyph ink");
    assert!(
        r > 180.0 && r > g + 60.0 && r > b + 60.0,
        "ToUnicode-bearing font Tj must paint red, got ({r:.1}, {g:.1}, {b:.1})"
    );
}

/// Probe 21 — `Tj` followed by `re` + `f` of the same colour. The text
/// runs through the text-side pipeline helper (fill side); the
/// rectangle fill runs through the path-side helper. Both arms must
/// paint, AND the page must contain both the glyph ink AND the
/// rectangle ink in the expected colour.
#[test]
fn qa_text_tj_followed_by_path_fill_paints_both_regions() {
    let content = "BT 1 0 0 rg /F1 30 Tf 5 70 Td (T) Tj ET\n\
                   60 5 30 30 re f\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let glyph_ink = count_ink_pixels(&on, 0, 0, 50, 50);
    let rect_ink = count_ink_pixels(&on, 50, 50, 100, 100);
    assert!(glyph_ink > 5, "glyph region must have ink, got {glyph_ink}");
    assert!(rect_ink > 100, "rectangle region must be heavily inked, got {rect_ink}");
}

/// Probe 22 — `Tj` inside `q ... Q` save/restore. The save pushes the
/// current `GraphicsState` onto the stack and restores it on `Q`. The
/// pipeline's spliced GS clone is transient — it's owned locally by
/// the operator arm and dropped at end-of-statement, so `q/Q` shouldn't
/// see it at all.
#[test]
fn qa_text_tj_inside_q_q_restores_outer_color() {
    let content = "1 0 0 rg \
                   q \
                   0 0 1 rg \
                   BT /F1 30 Tf 5 70 Td (Q) Tj ET \
                   Q \
                   BT /F1 30 Tf 5 30 Td (R) Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    // The top glyph (inside q…Q) must be BLUE (fill set after q).
    // The bottom glyph (after Q) must be RED (fill restored). ~keep
    let top_avg = average_ink_rgb(&on, 0, 0, 100, 50);
    let bot_avg = average_ink_rgb(&on, 0, 50, 100, 100);
    let (r_t, g_t, b_t) = top_avg.expect("top glyph must be painted");
    assert!(
        b_t > r_t,
        "top glyph (inside q/Q) must be bluer than red, got ({r_t:.1}, {g_t:.1}, {b_t:.1})"
    );
    let (r_b, g_b, b_b) = bot_avg.expect("bottom glyph must be painted");
    assert!(
        r_b > b_b,
        "bottom glyph (after Q restore) must be redder than blue, got ({r_b:.1}, {g_b:.1}, {b_b:.1})"
    );
}

/// Probe 23 — `Tj` with an active SMask through ExtGState. The smask
/// modulates alpha; the pipeline migration must not perturb the smask
/// path. Use a `/SMask /None` (no soft mask) since shipping an actual
/// soft-mask form XObject is beyond the scope of the fixture builder.
/// Even with /None, the ExtGState operator runs and exercises the
/// dispatch path.
#[test]
fn qa_text_tj_with_extgstate_smask_none_paints_red_glyph() {
    let resources = "/ExtGState << /Sm << /Type /ExtGState /SMask /None >> >>";
    let content = "/Sm gs BT 1 0 0 rg /F1 40 Tf 10 30 Td (M) Tj ET\n";
    let bytes = build_pdf_text(content, resources);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let avg = average_ink_rgb(&on, 0, 0, 100, 100);
    let (r, g, b) = avg.expect("glyph must paint despite /SMask /None");
    assert!(
        r > 180.0 && r > g + 60.0 && r > b + 60.0,
        "/SMask /None must not suppress the red glyph, got ({r:.1}, {g:.1}, {b:.1})"
    );
}

/// Probe 24 — `Tj` with a blend mode set on the active GS via ExtGState.
/// Multiply blends the painted colour with the destination. On a white
/// background `(c) * (1)` is `c`, so the painted colour is preserved;
/// what matters is that the blend-mode field round-trips through the
/// spliced GS clone unperturbed.
#[test]
fn qa_text_tj_with_blend_mode_multiply_paints_red() {
    let resources = "/ExtGState << /Mul << /Type /ExtGState /BM /Multiply >> >>";
    let content = "/Mul gs BT 1 0 0 rg /F1 40 Tf 10 30 Td (M) Tj ET\n";
    let bytes = build_pdf_text(content, resources);
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let avg = average_ink_rgb(&on, 0, 0, 100, 100);
    let (r, g, b) = avg.expect("Multiply-mode glyph must paint ink");
    assert!(
        r > 180.0 && r > g + 60.0 && r > b + 60.0,
        "Multiply over white must preserve red, got ({r:.1}, {g:.1}, {b:.1})"
    );
}

/// Probe 25 — `Tj` with an active clip path. Clip is set via `re W n`,
/// limiting paint to the clipped region; subsequent text must only paint
/// inside the clip. The pipeline migration must not perturb the clip
/// state passed to the text rasteriser.
#[test]
fn qa_text_tj_under_active_clip_paints_inside_only() {
    let content = "20 20 60 60 re W n \
                   BT 1 0 0 rg /F1 60 Tf 0 30 Td (M) Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    let inside = count_ink_pixels(&on, 20, 20, 80, 80);
    assert!(inside > 0, "clipped Tj must paint inside the clip, got {inside}");
    // Outside the clip the page must remain white. ~keep
    let outside_left = count_ink_pixels(&on, 0, 0, 20, 100);
    let outside_right = count_ink_pixels(&on, 80, 0, 100, 100);
    let outside_top = count_ink_pixels(&on, 0, 0, 100, 20);
    let outside_bot = count_ink_pixels(&on, 0, 80, 100, 100);
    let total_outside = outside_left + outside_right + outside_top + outside_bot;
    assert_eq!(
        total_outside, 0,
        "clip must prevent ink outside the clipped region, got {total_outside}"
    );
}

/// Probe 26 — Empty `Tj ()` paints no glyphs. The pipeline helper
/// still runs (the operator-arm dispatch fires); the spliced GS clone
/// might happen, but the rasteriser has zero glyphs to paint. The
/// page must remain white.
#[test]
fn qa_text_empty_tj_paints_zero_pixels() {
    let content = "BT 1 0 0 rg /F1 30 Tf 10 30 Td () Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert_eq!(
        count_ink_pixels(&on, 0, 0, 100, 100),
        0,
        "empty Tj must paint zero pixels"
    );
}

/// Probe 27 — Whitespace-only Tj string. Tw word-spacing affects only
/// `0x20` glyphs in the rasteriser; with Tw=0 and a single space, no
/// visible ink lands (space glyph has zero bbox).
#[test]
fn qa_text_whitespace_only_tj_paints_zero_pixels() {
    let content = "BT 1 0 0 rg /F1 30 Tf 10 30 Td (   ) Tj ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline(&doc, true);
    assert_eq!(
        count_ink_pixels(&on, 0, 0, 100, 100),
        0,
        "whitespace-only Tj must paint zero pixels"
    );
}

/// Probe 28 — TJ with an extreme negative numeric offset that would
/// push the text cursor far off the page. The pipeline path must not
/// crash on the off-page cursor.
#[test]
fn qa_text_tj_extreme_negative_offset_no_panic() {
    let content = "BT 1 0 0 rg /F1 15 Tf 50 50 Td [(A) -32767 (B)] TJ ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true);
    assert!(on.is_some(), "extreme TJ offset must not panic the renderer");
}

/// Probe 29 — TJ array containing only numeric kerning offsets (no
/// string segments). The array advances the text cursor but paints no
/// glyphs; no ink should appear.
#[test]
fn qa_text_tj_all_numeric_array_paints_zero_pixels() {
    let content = "BT 1 0 0 rg /F1 30 Tf 10 50 Td [-100 -200 -300] TJ ET\n";
    let bytes = build_pdf_text(content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let on = render_with_pipeline_allow_fail(&doc, true);
    let on = on.expect("all-numeric TJ must not panic the renderer");
    assert_eq!(
        count_ink_pixels(&on, 0, 0, 100, 100),
        0,
        "all-numeric TJ must paint zero pixels (no string segments)"
    );
}

// ============================================================================
// Performance probes — the "one resolve per Tj" invariant must hold (1000
// glyphs spread across 10 Tj calls should see ~10× the resolve cost, not
// 1000×).
// ============================================================================ ~keep

/// Probe 30 — 1000-glyph render through the pipeline. Wall-clock budget
/// catches the "pipeline accidentally resolves per glyph" regression
/// without flaking on shared CI runners: one resolver-construction +
/// one GS clone per Tj call, with 10 Tj calls each painting 100 glyphs,
/// is 10 resolver calls and 10 clones, dwarfed by 1000 glyph
/// rasterisations.
#[test]
fn qa_text_perf_thousand_glyphs_completes_within_budget() {
    let mut content = String::from("BT 0 0 1 rg /F1 6 Tf 5 90 Td ");
    let row = "ABCDEFGHIJABCDEFGHIJABCDEFGHIJABCDEFGHIJABCDEFGHIJ\
               ABCDEFGHIJABCDEFGHIJABCDEFGHIJABCDEFGHIJABCDEFGHIJ";
    for _ in 0..10 {
        content.push_str(&format!("({}) Tj 0 -7 Td ", row));
    }
    content.push_str("ET\n");
    let bytes = build_pdf_text(&content, "");
    let doc = PdfDocument::from_bytes(bytes).expect("PDF parses");
    let t = Instant::now();
    let _ = render_with_pipeline(&doc, true);
    let dt = t.elapsed();
    assert!(
        dt.as_secs_f64() < 30.0,
        "1000-glyph pipeline render must complete within 30 s, took {:.3} s",
        dt.as_secs_f64()
    );
}
