// ---------------------------------------------------------------------------
// Probe 32/33 — Rotated CTM with WMode 0 (horizontal text rotated 90°)
// --------------------------------------------------------------------------- ~keep

/// Horizontal-encoding text inside a content stream with a 90° rotation
/// CTM (`0 1 -1 0 0 0 cm`) — visually vertical on the page but logically
/// horizontal (WMode 0). The advance_text_matrix helper should route this
/// through the horizontal arm (uses `(tm.a, tm.b)`), so the rotated CTM
/// alone determines the on-page direction.
#[test]
fn probe32_horizontal_text_with_90deg_rotated_ctm_advances_perpendicularly() {
    // The CTM rotation: applied via `0 1 -1 0 0 0 cm` rotates 90° CCW.
    // Text matrix Tm stays identity. Each Tj displaces text matrix in x
    // (horizontal mode), which after CTM becomes user-space y. ~keep
    let content = b"q 0 1 -1 0 100 100 cm BT /F1 12 Tf 0 0 Td <0001> Tj <0002> Tj ET Q";
    let pdf = build_pdf("Identity-H", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse rotated horizontal");
    let chars = doc.extract_chars(0).expect("extract_chars");
    if let (Some(a), Some(b)) = (
        chars.iter().find(|c| c.char == 'A'),
        chars.iter().find(|c| c.char == 'B'),
    ) {
        // Under a 90° CCW CTM, the horizontal Tj advance becomes a +Y user
        // space advance. So B.y > A.y, and A.x ≈ B.x. ~keep
        let dx = (a.bbox.x - b.bbox.x).abs();
        let dy = b.bbox.y - a.bbox.y;
        // Document the observed behavior. A 90° rotation means horizontal
        // mode under a rotated CTM produces ~12-unit Y advances per glyph. ~keep
        assert!(
            dy > 5.0,
            "rotated-CTM horizontal text should advance in user-space Y; dy={}, dx={}",
            dy,
            dx
        );
    } else {
        panic!("could not find A and B in rotated-CTM horizontal extraction");
    }
}

// ---------------------------------------------------------------------------
// Probe 33 — Vertical text with rotated CTM (composite case)
// --------------------------------------------------------------------------- ~keep

#[test]
fn probe33_vertical_text_with_rotated_ctm_does_not_panic() {
    let content = b"q 0 1 -1 0 100 100 cm BT /F1 12 Tf 0 0 Td <0001> Tj <0002> Tj ET Q";
    let pdf = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse rotated vertical");
    let _ = doc.extract_chars(0);
    let _ = doc.extract_spans(0);
}

// ---------------------------------------------------------------------------
// Probe 34 — q ... Tf vertical ... Q restores wmode correctly
// --------------------------------------------------------------------------- ~keep

/// Inside `q ... Q` the graphics state stack saves and restores. Verify
/// that selecting a vertical font, then `Q`, restores the outer state's
/// (horizontal) wmode. With a single-font document we can't easily
/// observe the post-Q state; but we CAN observe that an extraction that
/// crosses save/restore boundaries doesn't lose track of wmode.
#[test]
fn probe34_save_restore_preserves_outer_wmode() {
    // Single font (vertical). q...Q does not switch fonts here, but the
    // wmode is saved-and-restored as part of GraphicsState. Pin: any glyphs
    // emitted before, during, and after q...Q all carry wmode=1. ~keep
    let content = b"BT /F1 12 Tf 100 700 Td <0001> Tj ET \
                    q BT /F1 12 Tf 100 600 Td <0002> Tj ET Q \
                    BT /F1 12 Tf 100 500 Td <0003> Tj ET";
    let pdf = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let spans = doc.extract_spans(0).expect("extract_spans");
    for s in &spans {
        assert_eq!(
            s.wmode, 1,
            "wmode=1 must survive save/restore boundary; span {:?} has wmode={}",
            s.text, s.wmode
        );
    }
}

// (The mid-BT Tf bug is exercised by probe36_mid_bt_tf_font_switch_drops_subsequent_spans
// further down — kept as a dedicated bug-finder near the cache-poisoning tests.) ~keep

// ---------------------------------------------------------------------------
// Probe 38 — Separation renderer with vertical text smoke test
// --------------------------------------------------------------------------- ~keep

/// Verify the separation renderer doesn't panic when given a vertical
/// content stream. Cannot easily assert pixel positions without a full
/// rendering pipeline, but pin no-panic.
///
/// Uses the `render_separations` free function on the separation
/// renderer; gated on the `rendering` feature so this only compiles in
/// configurations that ship the renderer.
#[test]
fn probe38_separation_renderer_vertical_text_does_not_panic() {
    let content = b"BT /F1 12 Tf 100 700 Td <0001> Tj <0002> Tj ET";
    let pdf = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    // Pin: separation rendering on a vertical-mode page must not panic.
    // A page without /Separation colorspaces returns an empty Vec — that
    // is expected on this synthetic PDF and still proves the operator
    // walk handled the vertical Tj path without crashing. ~keep
    let _ = xberg_native_pdf::rendering::render_separations(&doc, 0, 150);
}

// ---------------------------------------------------------------------------
// Probe 1 — Japanese packaging tategaki short brand + horizontal ingredients
// --------------------------------------------------------------------------- ~keep

/// Mixed page: 2 vertical brand-name spans, 1 horizontal ingredients-label
/// span. Vertical majority → tategaki sort.
#[test]
fn probe01_mixed_packaging_tategaki_brand_with_horizontal_label() {
    let spans = vec![
        span_with_wmode("Brand1", 500.0, 700.0, 1),
        span_with_wmode("Brand2", 500.0, 600.0, 1),
        span_with_wmode("Ingredients", 100.0, 100.0, 0),
    ];
    let pipeline = TextPipeline::new();
    let ordered = pipeline.process(spans, ReadingOrderContext::new()).unwrap();
    // 2/3 vertical → tategaki. The horizontal label still ends up in the
    // sort but the vertical brand goes first. ~keep
    let first = &ordered[0].span.text;
    assert!(
        first.starts_with("Brand"),
        "tategaki page must place vertical brand first; got {} first",
        first
    );
}

// ---------------------------------------------------------------------------
// Probe 4 — Mongolian vertical-only document (see probe29)
// --------------------------------------------------------------------------- ~keep

/// Mongolian is exclusively vertical TOP-to-BOTTOM, columns LEFT-to-RIGHT.
/// See probe29 for the LTR-column-direction probe. This probe pins the
/// per-character Y advance behavior.
#[test]
fn probe04_mongolian_vertical_y_advance_works_synthetic() {
    // Synthetic: an Identity-V font for the advance math, even though
    // the visual reading order is wrong for Mongolian. ~keep
    let content = b"BT /F1 12 Tf 100 700 Td <0001> Tj <0002> Tj <0003> Tj ET";
    let pdf = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let chars = doc.extract_chars(0).expect("extract_chars");
    assert_eq!(chars.len(), 3);
    // Per-glyph Y advance must be exactly 12.0 in synthetic Identity-V at fs=12. ~keep
    let mut sorted = chars.clone();
    sorted.sort_by(|a, b| b.bbox.y.partial_cmp(&a.bbox.y).unwrap());
    for w in sorted.windows(2) {
        let dy = w[0].bbox.y - w[1].bbox.y;
        assert!((dy - 12.0).abs() < 0.05, "Mongolian/CJK vertical dy = {}", dy);
    }
}

// ---------------------------------------------------------------------------
// Probe 35 — Form XObject containing vertical text
// --------------------------------------------------------------------------- ~keep

/// A page Do's a Form XObject. The XObject's content stream is rendered
/// with the outer graphics state. Pin: WMode is part of GraphicsState.text_wmode,
/// which is set by Tf — and Tf inside the XObject should set wmode.
#[test]
fn probe35_form_xobject_vertical_text_no_panic() {
    let cmap = b"\
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
2 beginbfchar
<0001> <0041>
<0002> <0042>
endbfchar
endcmap
end
end";
    let xobj_content = b"BT /F1 12 Tf 50 50 Td <0001> Tj <0002> Tj ET";
    let page_content = b"q /FXO Do Q";

    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let o1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
    let o2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
    let o3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> /XObject << /FXO 8 0 R >> >> >> endobj\n",
    );
    let o4 = pdf.len();
    pdf.extend_from_slice(format!("4 0 obj << /Length {} >> stream\n", page_content.len()).as_bytes());
    pdf.extend_from_slice(page_content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let o5 = pdf.len();
    pdf.extend_from_slice(
        b"5 0 obj << /Type /Font /Subtype /Type0 /BaseFont /TestFont /Encoding /Identity-V /DescendantFonts [6 0 R] /ToUnicode 7 0 R >> endobj\n",
    );
    let o6 = pdf.len();
    pdf.extend_from_slice(
        b"6 0 obj << /Type /Font /Subtype /CIDFontType2 /BaseFont /TestFont /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 1000 /DW2 [880 -1000] >> endobj\n",
    );
    let o7 = pdf.len();
    pdf.extend_from_slice(format!("7 0 obj << /Length {} >> stream\n", cmap.len()).as_bytes());
    pdf.extend_from_slice(cmap);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let o8 = pdf.len();
    pdf.extend_from_slice(
        format!(
            "8 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 600 800] /Resources << /Font << /F1 5 0 R >> >> /Length {} >> stream\n",
            xobj_content.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(xobj_content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
    for off in [o1, o2, o3, o4, o5, o6, o7, o8] {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    pdf.extend_from_slice(format!("trailer << /Size 9 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref).as_bytes());

    let doc = PdfDocument::from_bytes(pdf).expect("parse XObject PDF");
    // Pin: extracting the page should pick up the XObject's spans and tag
    // them with wmode=1 (the XObject's Tf selects Identity-V). ~keep
    let spans = doc.extract_spans(0).expect("extract_spans");
    // If the XObject recursion threads wmode correctly, every glyph A/B
    // should have wmode=1. ~keep
    if let Some(a) = spans.iter().find(|s| s.text.contains('A')) {
        assert_eq!(
            a.wmode, 1,
            "Form XObject vertical text must carry wmode=1; got wmode={} for {:?}",
            a.wmode, a.text
        );
    }
}

// ---------------------------------------------------------------------------
// Probe 37 — Performance: horizontal-only path is still cheap
// --------------------------------------------------------------------------- ~keep

// ---------------------------------------------------------------------------
// BUG: /W2 and /DW2 NOT included in font cache identity hash
// --------------------------------------------------------------------------- ~keep

/// **CRITICAL BUG** — exposed by parallel-test failures of probe15/19/20/25.
///
/// `PdfDocument::font_identity_hash_cheap` (src/document.rs:13212) hashes
/// BaseFont, Subtype, Encoding, ToUnicode (by reference), FontDescriptor
/// presence, DescendantFonts (by reference), FirstChar/LastChar, /Widths,
/// /DW — but **NOT /DW2 or /W2**. Two synthetic test PDFs with identical
/// BaseFont/Subtype/Encoding/DescendantFonts byte structure but different
/// `/DW2` arrays will produce the SAME cache key.
///
/// This is the same bug class as commit a327bcd (ToUnicode-stream cache
/// poisoning) but for vertical metrics. When test A loads a font with
/// /DW2 [880 -1000] and test B loads what looks like the same font with
/// /DW2 [900 -800], test B silently gets test A's parsed FontInfo,
/// causing vertical advances to be computed against the wrong w1y.
///
/// This test reproduces the bug deterministically by parsing two PDFs
/// in sequence, both using BaseFont /TestFont and DescendantFonts [6 0 R]
/// but with different /DW2 arrays. The second document's vertical advance
/// will incorrectly use the FIRST document's w1y.
#[test]
fn probe_cache_poison_dw2_collision_across_documents() {
    use xberg_native_pdf::fonts::global_cache::clear_global_font_cache;
    clear_global_font_cache();

    let content = b"BT /F1 12 Tf 100 700 Td <0001> Tj <0002> Tj ET";
    // PDF #1: DW2 = [880 -1000] (default; w1y=-1000 → dy=12.0). ~keep
    let pdf1 = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    // PDF #2: DW2 = [900 -800] (override; w1y=-800 → dy=9.6). ~keep
    let pdf2 = build_pdf("Identity-V", content, 1000, (900, -800), None, None);

    let doc1 = PdfDocument::from_bytes(pdf1).expect("parse 1");
    let chars1 = doc1.extract_chars(0).expect("chars1");
    let a1 = chars1.iter().find(|c| c.char == 'A').unwrap();
    let b1 = chars1.iter().find(|c| c.char == 'B').unwrap();
    let dy1 = a1.bbox.y - b1.bbox.y;
    assert!((dy1 - 12.0).abs() < 0.1, "doc1 dy = {}", dy1);

    let doc2 = PdfDocument::from_bytes(pdf2).expect("parse 2");
    let chars2 = doc2.extract_chars(0).expect("chars2");
    let a2 = chars2.iter().find(|c| c.char == 'A').unwrap();
    let b2 = chars2.iter().find(|c| c.char == 'B').unwrap();
    let dy2 = a2.bbox.y - b2.bbox.y;
    // FAILING ASSERTION: dy2 should be 9.6 per its own /DW2, but it will be
    // 12.0 (doc1's cached w1y) because the cache key did not include /DW2. ~keep
    assert!(
        (dy2 - 9.6).abs() < 0.1,
        "BUG: doc2's /DW2 ignored due to cache poisoning. dy2={} (expected 9.6, doc1's value 12.0 means cache key omits /DW2/W2)",
        dy2
    );
}

/// Companion: the same bug for /W2. Two PDFs with identical font dicts
/// except one has a per-CID /W2 override and the other doesn't. The
/// second-parsed document will inherit the first's /W2 from the cache.
#[test]
fn probe_cache_poison_w2_collision_across_documents() {
    use xberg_native_pdf::fonts::global_cache::clear_global_font_cache;
    clear_global_font_cache();

    let content = b"BT /F1 12 Tf 100 700 Td <0001> Tj <0002> Tj ET";
    // PDF A: no /W2 override → CID 1 advances per /DW2 → dy=12. ~keep
    let pdf_a = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    // PDF B: explicit /W2 forcing CID 1 to w1y=-500 → dy=6. ~keep
    let pdf_b = build_pdf(
        "Identity-V",
        content,
        1000,
        (880, -1000),
        Some("[1 [-500 250 600]]"),
        None,
    );

    // Force PDF A to land in the cache first. ~keep
    let doc_a = PdfDocument::from_bytes(pdf_a).expect("parse A");
    let _ = doc_a.extract_chars(0).unwrap();

    let doc_b = PdfDocument::from_bytes(pdf_b).expect("parse B");
    let chars_b = doc_b.extract_chars(0).expect("chars_b");
    let a_b = chars_b.iter().find(|c| c.char == 'A').unwrap();
    let b_b = chars_b.iter().find(|c| c.char == 'B').unwrap();
    let dy_b = a_b.bbox.y - b_b.bbox.y;
    assert!(
        (dy_b - 6.0).abs() < 0.1,
        "BUG: doc B's /W2 override ignored due to cache poisoning. dy_b={} (expected 6.0; got 12.0 means cache served doc A's FontInfo)",
        dy_b
    );
}

// ---------------------------------------------------------------------------
// BUG: mid-BT Tf font switch loses subsequent spans in extract_spans
// --------------------------------------------------------------------------- ~keep

/// **BUG** — within a single BT...ET block, switching fonts via `Tf`
/// silently drops every glyph emitted by subsequent Tj operators in the
/// `extract_spans` path. `extract_chars` still emits each glyph (with
/// suspicious coordinates), but `extract_spans` produces only the FIRST
/// glyph's span.
///
/// Reproducer:
///   BT /F1 12 Tf 100 700 Td <0001> Tj
///      /F2 12 Tf 200 700 Td <0002> Tj
///      /F1 12 Tf 300 700 Td <0003> Tj
///      /F2 12 Tf 400 700 Td <0004> Tj ET
///
/// Expected: 4 spans (A,B,C,D). Actual: 1 span ("A"). The Tf-induced
/// flush path apparently abandons the buffered span state without re-
/// emitting the in-flight glyphs of the alternative font.
///
/// The existing `mid_stream_tf_h_to_v_switches_span_wmode` test in
/// test_vertical_writing_mode_fixes.rs sidesteps this by splitting into
/// TWO BT/ET blocks — explicitly noted in its comments as a workaround.
/// This probe pins the actual mid-BT-Tf failure mode as a bug.
#[test]
fn probe36_mid_bt_tf_font_switch_drops_subsequent_spans() {
    use xberg_native_pdf::fonts::global_cache::clear_global_font_cache;
    clear_global_font_cache();

    let cmap = b"\
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
4 beginbfchar
<0001> <0041>
<0002> <0042>
<0003> <0043>
<0004> <0044>
endbfchar
endcmap
end
end";
    // Single BT/ET, position set ONCE via Td, then alternate Tf/Tj. This is
    // the actual mid-BT Tf reproducer: every Tf inside the BT block must
    // flush the pending Tj span buffer for its previous font and start a
    // fresh buffer for the new font. (The earlier draft used cumulative Td
    // between Tjs, which placed B/C/D far off the MediaBox and ran them
    // straight into the postprocess off-page filter — incidentally
    // unrelated to Tf flushing. The advance inside the block stays
    // on-page: F1 is /Identity-V so its glyph advance is vertical;
    // F2 is /Identity-H so it advances horizontally.) ~keep
    let content = b"BT 100 700 Td \
                    /F1 12 Tf <0001> Tj \
                    /F2 12 Tf <0002> Tj \
                    /F1 12 Tf <0003> Tj \
                    /F2 12 Tf <0004> Tj ET";

    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let o1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
    let o2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
    let o3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Contents 4 0 R /Resources << /Font << /F1 5 0 R /F2 8 0 R >> >> >> endobj\n",
    );
    let o4 = pdf.len();
    pdf.extend_from_slice(format!("4 0 obj << /Length {} >> stream\n", content.len()).as_bytes());
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let o5 = pdf.len();
    pdf.extend_from_slice(
        b"5 0 obj << /Type /Font /Subtype /Type0 /BaseFont /TestV /Encoding /Identity-V /DescendantFonts [6 0 R] /ToUnicode 7 0 R >> endobj\n",
    );
    let o6 = pdf.len();
    pdf.extend_from_slice(
        b"6 0 obj << /Type /Font /Subtype /CIDFontType2 /BaseFont /TestV /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 1000 /DW2 [880 -1000] >> endobj\n",
    );
    let o7 = pdf.len();
    pdf.extend_from_slice(format!("7 0 obj << /Length {} >> stream\n", cmap.len()).as_bytes());
    pdf.extend_from_slice(cmap);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
    let o8 = pdf.len();
    pdf.extend_from_slice(
        b"8 0 obj << /Type /Font /Subtype /Type0 /BaseFont /TestH /Encoding /Identity-H /DescendantFonts [9 0 R] /ToUnicode 7 0 R >> endobj\n",
    );
    let o9 = pdf.len();
    pdf.extend_from_slice(
        b"9 0 obj << /Type /Font /Subtype /CIDFontType2 /BaseFont /TestH /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 1000 /DW2 [880 -1000] >> endobj\n",
    );
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 10\n0000000000 65535 f \n");
    for off in [o1, o2, o3, o4, o5, o6, o7, o8, o9] {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    pdf.extend_from_slice(format!("trailer << /Size 10 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", xref).as_bytes());

    let doc = PdfDocument::from_bytes(pdf).expect("parse two-font PDF");
    let spans = doc.extract_spans(0).expect("extract_spans");
    // EXPECTED behavior: 4 spans, one per glyph, with alternating wmode.
    // ACTUAL behavior (bug): only span "A" comes out. ~keep
    let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
    let combined: String = texts.iter().copied().collect();
    assert!(
        combined.contains('A') && combined.contains('B') && combined.contains('C') && combined.contains('D'),
        "mid-BT Tf font switch must emit all four glyphs as spans; got {:?}",
        texts
    );
}

// ---------------------------------------------------------------------------
// Probe 37 — Performance regression: horizontal-only path
// --------------------------------------------------------------------------- ~keep

/// Smoke benchmark: extract 1000-glyph horizontal page and assert wall
/// clock under a generous bound. Not a strict perf test but pins that the
/// hot path hasn't gained an obviously expensive op.
#[test]
fn probe37_horizontal_extraction_bulk_within_bound() {
    let mut content = Vec::new();
    content.extend_from_slice(b"BT /F1 12 Tf 100 700 Td <");
    for _ in 0..200 {
        content.extend_from_slice(b"0001");
    }
    content.extend_from_slice(b"> Tj ET");
    let pdf = build_pdf("Identity-H", &content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");
    let start = std::time::Instant::now();
    let _spans = doc.extract_spans(0).expect("extract_spans");
    let elapsed = start.elapsed();
    // A 200-glyph horizontal page should extract well under 200ms on any
    // reasonable machine. This is generous; the goal is to catch ~100x
    // regressions, not micro-regressions. ~keep
    assert!(
        elapsed.as_millis() < 1000,
        "horizontal extraction took {}ms — significant regression?",
        elapsed.as_millis()
    );
}

// ---------------------------------------------------------------------------
// Probe 39 — a vertical (WMode 1) column positioned with a rotated per-glyph
// Tm must stay one run.
//
// ISO 32000-1 §9.4.4 puts the glyph displacement along the text matrix's
// (a, b) row only for WMode 0; for WMode 1 it runs along (c, d) — the branch
// `GraphicsState::advance_text_matrix` already makes. Any same-line test that
// assumes the WMode-0 axis therefore reads a vertical column's advance as a
// perpendicular offset, refuses to continue the run, and emits one span per
// glyph. Downstream cannot repair that: `rotation_compatible` in the span
// merge refuses to re-join quadrant-vertical runs, so each glyph reaches the
// output separated — spaces injected between CJK glyphs, i.e. wrong text.
// --------------------------------------------------------------------------- ~keep

#[test]
fn probe39_vertical_column_with_rotated_tm_stays_one_run() {
    // Four CIDs (A..D) drawn as a 90° tategaki column, one Tm per glyph, `e`
    // ascending — the coherent handedness for a negative vertical advance. ~keep
    let content = b"BT /F1 12 Tf \
0 1 -1 0 100 700 Tm <0001> Tj \
0 1 -1 0 112 700 Tm <0002> Tj \
0 1 -1 0 124 700 Tm <0003> Tj \
0 1 -1 0 136 700 Tm <0004> Tj ET";
    let pdf = build_pdf("Identity-V", content, 1000, (880, -1000), None, None);
    let doc = PdfDocument::from_bytes(pdf).expect("parse");

    let text = doc.extract_text(0).expect("extract_text");
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join("");
    assert!(flat.contains("ABCD"), "vertical column split glyph-by-glyph: {text:?}");
    assert!(
        !text.contains("A B C D"),
        "spaces injected between vertical glyphs: {text:?}"
    );
}
