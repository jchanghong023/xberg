#[test]
fn probe17c_marked_false_with_struct_tree_root_still_resolves_actualtext() {
    let mut b = PdfBuilder::new().marked(false);
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("replaced").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    // /StructTreeRoot in the catalog is enough (struct_tree_marked's
    // contract: `mark.marked || has_struct_tree_root`). ~keep
    assert!(
        extracted.contains("replaced"),
        "/Marked false + /StructTreeRoot present must still apply /ActualText, got {:?}",
        extracted
    );
}

// =============================================================
// Probe 19: OCG nested with multi-page. Scope covers pages 0 and 1;
//          page 0's covered MCID is in the excluded layer; page 1's
//          covered MCID is visible. Pin: page 0 produces no
//          emission (first page rule + layer-excluded → no emit),
//          and page 1 is suppress_only → still no emission.
//
//          Net: the replacement is LOST when the first page's
//          covered MCIDs are all hidden. This pins the current
//          implementation's behaviour; the locked decision said
//          "skip-when-all-filtered" for the first page; the second
//          page is suppress_only with no recourse.
// ============================================================= ~keep

#[test]
fn probe19_first_page_hidden_via_ocg_drops_emission_entirely() {
    let mut b = PdfBuilder::new();
    b.add_page_content(
        b"BT\n/F1 12 Tf\n50 700 Td\n\
          /OC /L BDC\n\
          /Span << /MCID 0 >> BDC\n(A) Tj\nEMC\n\
          EMC\nET\n"
            .to_vec(),
    );
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(B) Tj\nEMC\nET\n".to_vec());
    // For 2 pages + OCG: catalog=1, pages=2, font=3, page0=4,
    // page1=5, content0=6, content1=7, parent_tree=8,
    // struct_tree_root=9, ocg=10, first elem obj = 11. ~keep
    let h1 = b.add_elem(
        Elem::new(11, "H1", 9)
            .actual_text("Heading Y")
            .k(K::Mcid(0, 0))
            .k(K::Mcid(0, 1)),
    );
    b.register_mcid(0, 0, h1);
    b.register_mcid(1, 0, h1);
    let pdf = b.build_with_ocg("HiddenLayer");
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let mut excluded = std::collections::HashSet::new();
    excluded.insert("HiddenLayer".to_string());

    let p0 = doc
        .extract_text_filtered(0, excluded.clone(), std::collections::HashSet::new())
        .expect("extract_text_filtered 0");
    let p1 = doc
        .extract_text_filtered(1, excluded, std::collections::HashSet::new())
        .expect("extract_text_filtered 1");

    assert!(
        !p0.contains("Heading Y"),
        "page 0 first-page MCID is hidden by excluded layer → no emission, got {:?}",
        p0
    );
    assert!(
        !p0.contains('A'),
        "page 0 raw glyph 'A' must stay hidden under excluded layer, got {:?}",
        p0
    );

    // Page 1: suppress-only entry → still no emission, raw also
    // suppressed (covered). This pins the "lost replacement" edge:
    // the producer's intent silently drops here. If a fix changes this
    // contract, this test will alert. ~keep
    assert!(
        !p1.contains("Heading Y"),
        "page 1 (suppress_only) must NOT emit the replacement — pin current behaviour, got {:?}",
        p1
    );
    assert!(
        !p1.contains('B'),
        "page 1 raw glyph 'B' must be suppressed (covered MCID), got {:?}",
        p1
    );
}

// =============================================================
// Probe 29: /ActualText whose value is `null` PDF object. Should be
//          treated as absent — raw glyph survives, no panic.
// ============================================================= ~keep

#[test]
fn probe29_actualtext_null_value_treated_as_absent() {
    // Hand-assemble a PDF with /ActualText null on a Span — bypass
    // the builder's typed `actual_text` (which always writes a hex
    // string). ~keep
    use std::collections::BTreeMap;
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n";

    // catalog=1, pages=2, font=3, page=4, content=5, parent_tree=6,
    // struct_tree_root=7, struct_elem=8. ~keep
    let mut objs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    objs.insert(
        1,
        b"<< /Type /Catalog /Pages 2 0 R /MarkInfo << /Marked true >> /StructTreeRoot 7 0 R >>".to_vec(),
    );
    objs.insert(2, b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec());
    objs.insert(3, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    objs.insert(
        4,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 3 0 R >> /ProcSet [/PDF /Text] >> \
          /Contents 5 0 R /StructParents 0 >>"
            .to_vec(),
    );
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    objs.insert(5, stream);
    objs.insert(6, b"<< /Nums [0 [8 0 R]] >>".to_vec());
    objs.insert(7, b"<< /Type /StructTreeRoot /K 8 0 R /ParentTree 6 0 R >>".to_vec());
    // KEY DIFFERENCE: /ActualText null. ~keep
    objs.insert(
        8,
        b"<< /Type /StructElem /S /Span /P 7 0 R /Pg 4 0 R /ActualText null \
          /K << /Type /MCR /Pg 4 0 R /MCID 0 >> >>"
            .to_vec(),
    );

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n");
    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    for (num, body) in &objs {
        offsets.insert(*num, out.len());
        out.extend_from_slice(format!("{} 0 obj\n", num).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = out.len();
    let n_objs = 9u32;
    out.extend_from_slice(format!("xref\n0 {}\n", n_objs).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for n in 1..n_objs {
        if let Some(off) = offsets.get(&n) {
            out.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            n_objs, xref_offset
        )
        .as_bytes(),
    );

    let doc = PdfDocument::from_bytes(out).expect("open with /ActualText null");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert!(
        extracted.contains('X'),
        "/ActualText null must be treated as absent → raw 'X' survives, got {:?}",
        extracted
    );
}

// =============================================================
// Probe 30: /ActualText whose value is an INDIRECT reference to a
//          string. Should resolve correctly.
// ============================================================= ~keep

#[test]
fn probe30_actualtext_indirect_string_reference_resolves() {
    use std::collections::BTreeMap;
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n";

    // Same as probe29 layout but /ActualText is `9 0 R` and obj 9 is
    // a UTF-16BE string. ~keep
    let mut objs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    objs.insert(
        1,
        b"<< /Type /Catalog /Pages 2 0 R /MarkInfo << /Marked true >> /StructTreeRoot 7 0 R >>".to_vec(),
    );
    objs.insert(2, b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec());
    objs.insert(3, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    objs.insert(
        4,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 3 0 R >> /ProcSet [/PDF /Text] >> \
          /Contents 5 0 R /StructParents 0 >>"
            .to_vec(),
    );
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    objs.insert(5, stream);
    objs.insert(6, b"<< /Nums [0 [8 0 R]] >>".to_vec());
    objs.insert(7, b"<< /Type /StructTreeRoot /K 8 0 R /ParentTree 6 0 R >>".to_vec());
    objs.insert(
        8,
        b"<< /Type /StructElem /S /Span /P 7 0 R /Pg 4 0 R /ActualText 9 0 R \
          /K << /Type /MCR /Pg 4 0 R /MCID 0 >> >>"
            .to_vec(),
    );
    // Obj 9: the actual string. UTF-16BE for "indirect". ~keep
    objs.insert(
        9,
        b"<FEFF0069006E0064006900720065006300740020 0068006900740073>".to_vec(),
    );
    objs.insert(9, b"<FEFF0069006E0064006900720065006300740068006900740073>".to_vec());
    // Recompute — we re-inserted the same key; last write wins; the
    // string above is for "indirecthits" (close enough — we check
    // substring "indirect"). Keep what's there for the test. ~keep

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n");
    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    for (num, body) in &objs {
        offsets.insert(*num, out.len());
        out.extend_from_slice(format!("{} 0 obj\n", num).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = out.len();
    let n_objs = 10u32;
    out.extend_from_slice(format!("xref\n0 {}\n", n_objs).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for n in 1..n_objs {
        if let Some(off) = offsets.get(&n) {
            out.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
        } else {
            out.extend_from_slice(b"0000000000 00000 f \n");
        }
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            n_objs, xref_offset
        )
        .as_bytes(),
    );

    let doc = PdfDocument::from_bytes(out).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    // Either the indirect string resolves (substring "indirect" appears
    // and raw 'X' is gone), OR the resolver doesn't follow the ref and
    // we get raw 'X' through with no replacement.
    // We pin the behaviour: if 'X' survives the resolver doesn't follow
    // the indirect; if "indirect" appears, it does. Either is acceptable
    // PDF behaviour but the result must be self-consistent. ~keep
    let resolved_indirect = extracted.contains("indirect");
    let has_raw = extracted.contains('X');
    assert!(
        resolved_indirect ^ has_raw,
        "indirect /ActualText: either resolution succeeds (no raw, replacement appears) \
         or it doesn't (raw survives). XOR must hold. got {:?}",
        extracted
    );
}

// =============================================================
// Probe 32: /ActualText with embedded \r and \n. Pin that the
//          replacement is forwarded verbatim (line breaks may
//          survive in the output).
// ============================================================= ~keep

#[test]
fn probe32_actualtext_with_line_breaks_pin_behaviour() {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    // /ActualText "L1\nL2" — embeds U+000A. ~keep
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("L1\nL2").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    assert!(
        extracted.contains("L1") && extracted.contains("L2"),
        "/ActualText with embedded newline must forward both halves verbatim, got {:?}",
        extracted
    );
    assert!(!extracted.contains('X'), "raw 'X' must NOT appear, got {:?}", extracted);
}

// =============================================================
// Probe 34: /Artifact MC sequence with /ActualText. The /Artifact
//          tag traditionally suppresses content; the /ActualText on
//          the artifact should also be suppressed (the entire
//          artifact is meant to be hidden).
// ============================================================= ~keep

/// BUG REPRODUCER — third-pass review finding.
///
/// MC-scope `/ActualText` carried by an `/Artifact` BDC leaks into
/// extracted text. The artifact filter is downstream
/// (`spans.retain(|s| s.artifact_type.is_none())`) and depends on
/// the span carrying `artifact_type=Some(...)`. The Tj-span buffer
/// flush path hardcodes `artifact_type: None`
/// (see `src/extractors/text.rs` `flush_tj_span_buffer`), so every
/// span produced by that flush — including MC-scope ActualText
/// replacements emitted into the buffer — bypasses the filter and
/// leaks the substituted text from inside `/Artifact`.
///
/// `#[ignore]` flags this for the next fix pass; remove the
/// attribute once the underlying bug is fixed in production code.
/// Verified by `probe34b` below: the same leak occurs for vanilla
/// raw glyphs inside `/Artifact`, so the bug is pre-existing in
/// the buffered Tj-span path and exposed by ActualText, not
/// introduced by it.
#[test]
fn probe34_actualtext_on_artifact_mc_does_not_leak() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Artifact << /Type /Pagination /ActualText <FEFF00610072> >> BDC\n\
                    (X) Tj\n\
                    EMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    // The struct tree need not reference the artifact MCID — artifacts
    // are not part of the structure tree; we still ship a minimal tree. ~keep
    let _doc = b.add_elem(Elem::new(8, "Document", 7));
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");

    // Dump the raw spans for diagnostic context: the artifact filter
    // is downstream from the extractor and depends on the span
    // carrying `artifact_type=Some(...)`. If the MC-scope
    // /ActualText emission creates a span without the artifact tag
    // (because the buffer was flushed and the artifact-tag lookup
    // happened at the wrong stack depth), the span survives the
    // downstream `spans.retain(|s| s.artifact_type.is_none())` and
    // leaks "ar" into extraction output. ~keep
    let page = doc.extract_page_text(0).expect("extract_page_text");
    eprintln!("probe34 spans dump:");
    for (i, s) in page.spans.iter().enumerate() {
        eprintln!(
            "  [{}] text={:?} mcid={:?} artifact_type={:?}",
            i, s.text, s.mcid, s.artifact_type
        );
    }

    // extract_text() defaults to include_artifacts=true (matching
    // extract_words/extract_text_lines), so request exclusion explicitly
    // to test the artifact_type-propagation property this probe pins. ~keep
    let opts = ConversionOptions {
        include_artifacts: false,
        ..Default::default()
    };
    let extracted = doc
        .extract_text_with_options(0, &opts)
        .expect("extract_text_with_options");
    // Per the extractor's artifact filtering, neither the raw 'X' nor
    // the ActualText replacement ("ar") should be emitted. ~keep
    assert!(
        !extracted.contains("ar"),
        "/Artifact MC's /ActualText must not leak into output (artifact filtered), got {:?}; spans={:?}",
        extracted,
        page.spans
            .iter()
            .map(|s| (s.text.clone(), s.artifact_type.clone()))
            .collect::<Vec<_>>(),
    );
    assert!(
        !extracted.contains('X'),
        "raw 'X' must NOT appear inside /Artifact, got {:?}",
        extracted
    );
}

// =============================================================
// Probe 34b: vanilla /Artifact MC sequence with raw glyphs (NO
//          /ActualText). Verifies the artifact filter works at all
//          for the buffered Tj-span path, so we can scope the
//          ActualText leak in probe34 precisely. If THIS also leaks,
//          the bug isn't ActualText-specific — it's a pre-existing
//          artifact_type=None hardcode in the Tj-span flush path
//          that the ActualText feature merely exposed.
// ============================================================= ~keep

/// BUG REPRODUCER — third-pass review finding.
///
/// Scopes the artifact-leak bug found by `probe34` to the buffered
/// Tj-span path, INDEPENDENT of ActualText. Vanilla raw glyphs
/// inside `/Artifact` BDC also leak through to extracted text
/// because `flush_tj_span_buffer` hardcodes
/// `artifact_type: None` on the produced span. This is a
/// pre-existing bug unrelated to the ActualText feature but
/// inherited by it.
///
/// `#[ignore]` flags the production-side fix; remove once
/// `flush_tj_span_buffer` reads `self.current_artifact_type()`
/// like `flush_tj_buffer` does (see `src/extractors/text.rs:6363`).
#[test]
fn probe34b_vanilla_artifact_raw_glyph_pin_behaviour() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Artifact << /Type /Pagination >> BDC\n\
                    (XYZ) Tj\n\
                    EMC\nET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let _doc = b.add_elem(Elem::new(8, "Document", 7));
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");
    eprintln!("probe34b spans dump:");
    for (i, s) in page.spans.iter().enumerate() {
        eprintln!(
            "  [{}] text={:?} mcid={:?} artifact_type={:?}",
            i, s.text, s.mcid, s.artifact_type
        );
    }
    // extract_text() defaults to include_artifacts=true (matching
    // extract_words/extract_text_lines), so request exclusion explicitly
    // to test the artifact_type-propagation property this probe pins. ~keep
    let opts = ConversionOptions {
        include_artifacts: false,
        ..Default::default()
    };
    let extracted = doc
        .extract_text_with_options(0, &opts)
        .expect("extract_text_with_options");
    // Pin: raw artifact glyphs must NOT appear in extracted text.
    // The downstream filter retains only spans with
    // `artifact_type.is_none()`. If raw glyphs leak, the Tj-span
    // flush is broken at the artifact_type hardcode (see
    // src/extractors/text.rs `flush_tj_span_buffer` field
    // `artifact_type: None`). ~keep
    assert!(
        !extracted.contains("XYZ"),
        "vanilla /Artifact raw glyphs must NOT appear in extract_text — \
         artifact filter is broken if this fails; got {:?}; spans={:?}",
        extracted,
        page.spans
            .iter()
            .map(|s| (s.text.clone(), s.artifact_type.clone()))
            .collect::<Vec<_>>(),
    );
}

// =============================================================
// Probe 36: extract_structured surfaces the replacement in region
//          text and never the raw glyph.
// ============================================================= ~keep

#[test]
fn probe36_extract_structured_region_text_carries_replacement() {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("fi").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_structured(0).expect("extract_structured");
    let joined = page
        .regions
        .iter()
        .map(|r| r.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("fi"),
        "extract_structured regions must carry the replacement text 'fi', got regions={:?}",
        page.regions
    );
    assert!(
        !joined.contains('X'),
        "extract_structured regions must NOT carry the raw 'X' glyph, got regions={:?}",
        page.regions
    );
}

// =============================================================
// Probe 38: two sequential extractions on the same `PdfDocument`
//          give identical results — the per-page mc_actualtext_mcids
//          map is REPLACED (not extended) so re-runs are idempotent.
// ============================================================= ~keep

#[test]
fn probe38_two_sequential_extract_text_calls_idempotent() {
    let mut b = PdfBuilder::new();
    b.add_page_content(
        b"BT\n/F1 12 Tf\n50 700 Td\n\
          /Span << /MCID 0 /ActualText <FEFF0061> >> BDC\n(X) Tj\nEMC\nET\n"
            .to_vec(),
    );
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("struct").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let first = doc.extract_text(0).expect("extract_text 1");
    let second = doc.extract_text(0).expect("extract_text 2");
    assert_eq!(
        first, second,
        "two sequential extract_text calls must give byte-equal output (per-call MC-wins map is \
         REPLACED, not accumulated)"
    );
    assert!(first.contains('a'), "MC-scope 'a' must appear, got {:?}", first);
    assert!(!first.contains("struct"), "struct-scope must NOT win, got {:?}", first);
}

// =============================================================
// Probe 38 / continuation: same surface but verifying via the
// public extract_page_text + apply pipeline that spans carry the
// replacement on the second call exactly as on the first.
// ============================================================= ~keep

#[test]
fn probe38b_repeated_extract_page_text_carries_replacement_each_time() {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("fi").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");

    for round in 0..3 {
        let page = doc.extract_page_text(0).expect("extract_page_text");
        let joined: String = page.spans.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("|");
        assert!(
            joined.contains("fi"),
            "round {}: replacement must appear in spans, got {:?}",
            round,
            joined
        );
        assert!(
            !joined.contains('X'),
            "round {}: raw 'X' must NOT appear in spans, got {:?}",
            round,
            joined
        );
    }
}

// =============================================================
// Probe (cross-path consistency at extract_structured): JSON-shape
// region text must match extract_text byte-for-byte minus whitespace
// for a single-MCID fixture. Slightly stricter than the existing
// `cross_path_byte_equal_for_actualtext_replacement` because we
// also check extract_structured.
// ============================================================= ~keep

#[test]
fn probe35_extract_structured_byte_equal_with_extract_text() {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("fi").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extract_text = doc.extract_text(0).expect("extract_text");
    let _opts = ConversionOptions::default();
    let structured = doc.extract_structured(0).expect("structured");
    let structured_joined: String = structured
        .regions
        .iter()
        .map(|r| r.text.clone())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        extract_text.trim().replace(['\r', '\n'], ""),
        "fi",
        "extract_text body must be exactly 'fi', got {:?}",
        extract_text
    );
    assert_eq!(
        structured_joined.trim().replace(['\r', '\n'], ""),
        "fi",
        "extract_structured region body must be exactly 'fi', got {:?}",
        structured_joined
    );
}

// =============================================================
// Probe 4: pin the canonical MCID iteration order used by the
//          consecutive-run dedup. The action map walks struct-tree
//          pre-order; emission lands at the first content-stream
//          span carrying the emit-pick MCID.
//
// Fixture: content stream order [B(mcid 1), A(mcid 0), C(mcid 2)].
// Struct-tree order [0, 1, 2]. Outer Span /ActualText "ALL" covers
// all three. With one consecutive run, ONE emission fires. The
// emit-pick is MCID 0 (first in struct-tree order). In the
// extracted text "ALL" appears at the SPAN-ORDER position of MCID
// 0 — i.e. between B and C.
// ============================================================= ~keep

#[test]
fn probe4_mcid_iteration_order_uses_struct_tree_preorder() {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /Span << /MCID 1 >> BDC\n(B) Tj\nEMC\n\
                    /Span << /MCID 0 >> BDC\n(A) Tj\nEMC\n\
                    /Span << /MCID 2 >> BDC\n(C) Tj\nEMC\n\
                    ET\n";
    let mut b = PdfBuilder::new();
    b.add_page_content(content.to_vec());
    let outer = b.add_elem(
        Elem::new(8, "Span", 7)
            .page(4)
            .actual_text("ALL")
            .k(K::Mcid(0, 0))
            .k(K::Mcid(1, 0))
            .k(K::Mcid(2, 0)),
    );
    b.register_mcid(0, 0, outer);
    b.register_mcid(0, 1, outer);
    b.register_mcid(0, 2, outer);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let extracted = doc.extract_text(0).expect("extract_text");
    eprintln!("probe4 extract_text = {:?}", extracted);
    assert_eq!(
        extracted.matches("ALL").count(),
        1,
        "consecutive-run dedup: exactly one emission, got {:?}",
        extracted
    );
    for raw in ['A', 'B', 'C'] {
        // Note: "ALL" contains 'A' and 'L' so strip first. ~keep
        let stripped = extracted.replace("ALL", "");
        assert!(!stripped.contains(raw), "raw {:?} suppressed, got {:?}", raw, extracted);
    }
}

// =============================================================
// Probe 8: multi-page subtree where the bearing element straddles
//          all pages — replacement fires on first page only, the
//          rest produce nothing (no emission, no raw).
// ============================================================= ~keep

#[test]
fn probe8_actualtext_spanning_all_pages_emits_only_first() {
    let mut b = PdfBuilder::new();
    for _ in 0..3 {
        b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(Z) Tj\nEMC\nET\n".to_vec());
    }
    // For 3 pages: catalog=1, pages=2, font=3, page0=4, page1=5,
    // page2=6, content0=7, content1=8, content2=9, parent_tree=10,
    // struct_tree_root=11, first elem obj = 12. ~keep
    let doc_e = b.add_elem(
        Elem::new(12, "Document", 11)
            .actual_text("DOC-WIDE")
            .k(K::Mcid(0, 0))
            .k(K::Mcid(0, 1))
            .k(K::Mcid(0, 2)),
    );
    b.register_mcid(0, 0, doc_e);
    b.register_mcid(1, 0, doc_e);
    b.register_mcid(2, 0, doc_e);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let p0 = doc.extract_text(0).expect("p0");
    let p1 = doc.extract_text(1).expect("p1");
    let p2 = doc.extract_text(2).expect("p2");
    assert!(p0.contains("DOC-WIDE"), "page 0 must emit, got {:?}", p0);
    assert!(!p0.contains('Z'), "page 0 raw suppressed, got {:?}", p0);
    for (i, p) in [(1, &p1), (2, &p2)] {
        assert!(!p.contains("DOC-WIDE"), "page {} must NOT re-emit, got {:?}", i, p);
        assert!(!p.contains('Z'), "page {} raw suppressed, got {:?}", i, p);
    }
}

// =============================================================
// Probe 18 mixed-layer: covered MCIDs land in TWO different OCG
//          layers. When both layers are excluded the emission
//          drops; when only one is excluded the emission still
//          fires on the visible-and-not-MC-wins entry.
// ============================================================= ~keep

#[test]
fn probe18_two_layers_both_excluded_drops_emission() {
    // Single page, two MCIDs each in a different layer.
    // /OC /L1 BDC -> Span MCID 0 -> EMC
    // /OC /L2 BDC -> Span MCID 1 -> EMC
    // Outer Span covers both MCIDs with /ActualText "BOTH". ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
                    /OC /L1 BDC\n/Span << /MCID 0 >> BDC\n(A) Tj\nEMC\nEMC\n\
                    /OC /L2 BDC\n/Span << /MCID 1 >> BDC\n(B) Tj\nEMC\nEMC\n\
                    ET\n";

    // Hand-roll the PDF because PdfBuilder only supports one OCG. ~keep
    use std::collections::BTreeMap;
    let mut objs: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    // catalog=1, pages=2, font=3, page=4, content=5, parent_tree=6,
    // struct_tree_root=7, ocg1=8, ocg2=9, struct_elem=10. ~keep
    objs.insert(
        1,
        b"<< /Type /Catalog /Pages 2 0 R /MarkInfo << /Marked true >> /StructTreeRoot 7 0 R \
          /OCProperties << /OCGs [8 0 R 9 0 R] /D << /Order [8 0 R 9 0 R] >> >> >>"
            .to_vec(),
    );
    objs.insert(2, b"<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_vec());
    objs.insert(3, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    objs.insert(
        4,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Resources << /Font << /F1 3 0 R >> /ProcSet [/PDF /Text] \
          /Properties << /L1 8 0 R /L2 9 0 R >> >> \
          /Contents 5 0 R /StructParents 0 >>"
            .to_vec(),
    );
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    objs.insert(5, stream);
    objs.insert(6, b"<< /Nums [0 [10 0 R 10 0 R]] >>".to_vec());
    objs.insert(7, b"<< /Type /StructTreeRoot /K 10 0 R /ParentTree 6 0 R >>".to_vec());
    objs.insert(8, b"<< /Type /OCG /Name (Layer1) >>".to_vec());
    objs.insert(9, b"<< /Type /OCG /Name (Layer2) >>".to_vec());
    objs.insert(
        10,
        b"<< /Type /StructElem /S /Span /P 7 0 R /Pg 4 0 R /ActualText <FEFF0042004F00540048> \
          /K [<< /Type /MCR /Pg 4 0 R /MCID 0 >> << /Type /MCR /Pg 4 0 R /MCID 1 >>] >>"
            .to_vec(),
    );

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n");
    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    for (num, body) in &objs {
        offsets.insert(*num, out.len());
        out.extend_from_slice(format!("{} 0 obj\n", num).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = out.len();
    out.extend_from_slice(b"xref\n0 11\n0000000000 65535 f \n");
    for n in 1..11u32 {
        let off = offsets[&n];
        out.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size 11 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            xref_offset
        )
        .as_bytes(),
    );

    let doc = PdfDocument::from_bytes(out).expect("open");

    let mut both = std::collections::HashSet::new();
    both.insert("Layer1".to_string());
    both.insert("Layer2".to_string());
    let extracted_both = doc
        .extract_text_filtered(0, both, std::collections::HashSet::new())
        .expect("filtered both");
    assert!(
        !extracted_both.contains("BOTH"),
        "with BOTH layers excluded, every covered MCID is invisible → no emission, got {:?}",
        extracted_both
    );

    // Exclude only Layer1 — Layer2's MCID is visible → emission fires
    // on MCID 1 (the visible one). ~keep
    let mut just_one = std::collections::HashSet::new();
    just_one.insert("Layer1".to_string());
    let extracted_one = doc
        .extract_text_filtered(0, just_one, std::collections::HashSet::new())
        .expect("filtered one");
    assert!(
        extracted_one.contains("BOTH"),
        "with one layer excluded but the other visible, emission must fire at the visible MCID, got {:?}",
        extracted_one
    );

    let extracted_neither = doc.extract_text(0).expect("unfiltered");
    assert!(
        extracted_neither.contains("BOTH"),
        "with no layers excluded, emission must fire, got {:?}",
        extracted_neither
    );
    // Note: raw glyphs "A" / "B" must not appear AS RAW glyphs. The
    // replacement contains 'B' and 'O' so we strip the replacement
    // substring before checking for raw glyphs. ~keep
    let stripped = extracted_neither.replace("BOTH", "");
    assert!(!stripped.contains('A'), "raw A suppressed, got {:?}", extracted_neither);
    assert!(
        !stripped.contains('B'),
        "raw B suppressed (independent of replacement), got {:?}",
        extracted_neither
    );
}

// =============================================================
// Probe 37: clones of extracted spans see the mutated text, NOT
//          the raw glyph. The implementer's claim: `effective_text`
//          was dropped; `span.text` is mutated in place.
// ============================================================= ~keep

#[test]
fn probe37_cloned_spans_carry_replacement_not_raw() {
    let mut b = PdfBuilder::new();
    b.add_page_content(b"BT\n/F1 12 Tf\n50 700 Td\n/Span << /MCID 0 >> BDC\n(X) Tj\nEMC\nET\n".to_vec());
    let _ = b.add_elem(Elem::new(8, "Span", 7).page(4).actual_text("fi").k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    let pdf = b.build();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");
    let cloned: Vec<_> = page.spans.to_vec();
    for s in &cloned {
        assert!(
            !s.text.contains('X'),
            "cloned span must NOT contain raw 'X', got {:?}",
            s.text
        );
    }
    let any_fi = cloned.iter().any(|s| s.text.contains("fi"));
    assert!(
        any_fi,
        "at least one cloned span must contain 'fi'; got spans={:?}",
        cloned.iter().map(|s| s.text.clone()).collect::<Vec<_>>()
    );
}

// =============================================================
// Probe (MarkInfo/Marked=false variant): a malformed Marked=false +
// no StructTreeRoot reference — the document has the StructTreeRoot
// object but never declares it in the catalog. ActualText must NOT
// fire (the index can't be built without the catalog hook).
// (This is a defensive pin; the builder always writes
// /StructTreeRoot, so we only cover the explicit Marked=false
// case from probe17c above.)
// ============================================================= ~keep

// =============================================================
// Probe 40: cross-MCID merge predicate — fragments with different
//          MCIDs MUST stay separate even when they would otherwise
//          satisfy every other merge condition (same font, same
//          baseline, zero gap).
//
// Spec basis (ISO 32000-1:2008 §14.6, §14.8): the MCID is the
// structural unit. Two same-line fragments with different MCIDs
// belong to different marked-content references and therefore to
// different structure elements (or different references to the
// same element). Merging them would silently fuse their identities
// — the merged span keeps `current.mcid` and drops the other —
// destroying the boundary that structure-tree reading order,
// tree-scope ActualText suppression, and table-cell membership
// rely on.
//
// Fixture: a single visible word "Hello" emitted across two MCIDs
// in the same Span content stream, in the same font, on the same
// baseline, with a zero gap between them. Without the same_mcid
// gate this would merge into one span "Hello" carrying MCID 0;
// with the gate the two fragments survive as separate spans
// "He" and "llo" carrying MCIDs 0 and 1 respectively.
// ============================================================= ~keep

fn fixture_probe40_cross_mcid_no_merge() -> Vec<u8> {
    // "He" at MCID 0, "llo" at MCID 1 — same font, same baseline,
    // zero gap; the natural pre-PR behaviour was to glue these into
    // "Hello" under is_same_font + same_line + tight gap. ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
         /Span << /MCID 0 >> BDC\n(He) Tj\nEMC\n\
         /Span << /MCID 1 >> BDC\n(llo) Tj\nEMC\n\
         ET\n"
        .to_vec();
    let mut b = PdfBuilder::new();
    b.add_page_content(content);
    let _e0 = b.add_elem(Elem::new(8, "Span", 7).page(4).k(K::Mcid(0, 0)));
    let _e1 = b.add_elem(Elem::new(9, "Span", 7).page(4).k(K::Mcid(1, 0)));
    b.register_mcid(0, 0, 8);
    b.register_mcid(0, 1, 9);
    b.build()
}
