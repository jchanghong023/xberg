#[test]
fn probe40_cross_mcid_fragments_do_not_merge() {
    let pdf = fixture_probe40_cross_mcid_no_merge();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");

    let texts: Vec<String> = page.spans.iter().map(|s| s.text.clone()).collect();
    assert!(
        texts.iter().any(|t| t == "He"),
        "fragment 'He' missing from spans: {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "llo"),
        "fragment 'llo' missing from spans: {:?}",
        texts
    );

    // The pre-PR fused form must NOT appear. ~keep
    assert!(
        !texts.iter().any(|t| t == "Hello"),
        "fragments must not merge across MCIDs; found 'Hello' in {:?}",
        texts
    );

    let mcids: Vec<Option<u32>> = page.spans.iter().map(|s| s.mcid).collect();
    assert!(
        mcids.contains(&Some(0)) && mcids.contains(&Some(1)),
        "expected MCIDs 0 and 1 each on their own span; got {:?}",
        mcids
    );
}

// =============================================================
// Probe 41: regression sentry — same-MCID fragments still merge
//          when they should. Pins that the same_mcid gate does
//          not break the common case where a producer emits
//          multiple Tj operators inside one /Span /MCID BDC ... EMC
//          envelope (e.g. cross-font glue, small-caps glue, decimal
//          merges within one marked-content reference). Without
//          this pin a future refactor could turn same_mcid into
//          "always require different objects" and silently
//          fragment every tagged word.
// ============================================================= ~keep

fn fixture_probe41_same_mcid_still_merges() -> Vec<u8> {
    // "He" and "llo" both inside MCID 0 — same font, same baseline,
    // zero gap. Must merge to "Hello". ~keep
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n\
         /Span << /MCID 0 >> BDC\n(He) Tj\n(llo) Tj\nEMC\n\
         ET\n"
        .to_vec();
    let mut b = PdfBuilder::new();
    b.add_page_content(content);
    let _e = b.add_elem(Elem::new(8, "Span", 7).page(4).k(K::Mcid(0, 0)));
    b.register_mcid(0, 0, 8);
    b.build()
}

#[test]
fn probe41_same_mcid_fragments_merge_to_single_word() {
    let pdf = fixture_probe41_same_mcid_still_merges();
    let doc = PdfDocument::from_bytes(pdf).expect("open");
    let page = doc.extract_page_text(0).expect("extract_page_text");
    let texts: Vec<String> = page.spans.iter().map(|s| s.text.clone()).collect();
    assert!(
        texts.iter().any(|t| t == "Hello"),
        "same-MCID fragments must still merge to 'Hello'; got spans={:?}",
        texts
    );
}

// =============================================================
// extract_text / to_markdown / to_plain_text default to including
// /Artifact-tagged content.
//
// extract_words()/extract_text_lines() already defaulted
// include_artifacts=true for backward compatibility; extract_text(),
// to_markdown(), and to_plain_text() unconditionally dropped artifact
// content instead, with no override — silently losing real content on
// PDFs that tag a repeated footer (e.g. a section identifier) as an
// artifact. A tagged page took the structure-order drop
// (`extract_text_structure_order_cached_with_spans`); an untagged page
// took the geometric drop (`assemble_text_from_spans`'s else arm) —
// both are now gated on `ConversionOptions::include_artifacts`
// (default true).
// ============================================================= ~keep

fn fixture_tagged_page_with_artifact_footer() -> Vec<u8> {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n(Body text) Tj\nET\n\
                    BT\n/F1 12 Tf\n50 50 Td\n\
                    /Artifact << /Type /Pagination /Subtype /Footer >> BDC\n\
                    (SECTION-01-79-00) Tj\n\
                    EMC\nET\n"
        .to_vec();
    let mut b = PdfBuilder::new();
    b.add_page_content(content);
    // Minimal structure tree (no MCID references needed by the artifact
    // itself — artifacts are not part of the logical structure tree) —
    // just enough to make `struct_tree_trustworthy()` return `Some`, so ~keep
    // this fixture exercises the structure-order path.
    let _doc = b.add_elem(Elem::new(8, "Document", 7));
    b.build()
}

fn fixture_untagged_page_with_artifact_footer() -> Vec<u8> {
    let content = b"BT\n/F1 12 Tf\n50 700 Td\n(Body text) Tj\nET\n\
                    BT\n/F1 12 Tf\n50 50 Td\n\
                    /Artifact << /Type /Pagination /Subtype /Footer >> BDC\n\
                    (SECTION-01-79-00) Tj\n\
                    EMC\nET\n"
        .to_vec();
    // `PdfBuilder` always emits a `/StructTreeRoot`, so a genuinely
    // struct-tree-free fixture isn't expressible here — `.suspects()`
    // is the spec-correct way (§14.7.1 /MarkInfo /Suspects) to force
    // `struct_tree_trustworthy()` to fall back to the geometric path,
    // which is the branch this fixture needs to exercise. ~keep
    let mut b = PdfBuilder::new().suspects();
    b.add_page_content(content);
    let _doc = b.add_elem(Elem::new(8, "Document", 7));
    b.build()
}

#[test]
fn extract_text_includes_artifact_footer_by_default_tagged() {
    let doc = PdfDocument::from_bytes(fixture_tagged_page_with_artifact_footer()).expect("open");
    let text = doc.extract_text(0).expect("extract_text");
    assert!(
        text.contains("SECTION-01-79-00"),
        "extract_text() must include /Artifact content by default, got {:?}",
        text
    );
}

#[test]
fn extract_text_includes_artifact_footer_by_default_untagged() {
    let doc = PdfDocument::from_bytes(fixture_untagged_page_with_artifact_footer()).expect("open");
    let text = doc.extract_text(0).expect("extract_text");
    assert!(
        text.contains("SECTION-01-79-00"),
        "extract_text() must include /Artifact content by default on an \
         untagged PDF too, got {:?}",
        text
    );
}

#[test]
fn extract_text_with_options_can_still_exclude_artifacts() {
    let opts = ConversionOptions {
        include_artifacts: false,
        ..Default::default()
    };
    for pdf in [
        fixture_tagged_page_with_artifact_footer(),
        fixture_untagged_page_with_artifact_footer(),
    ] {
        let doc = PdfDocument::from_bytes(pdf).expect("open");
        let text = doc
            .extract_text_with_options(0, &opts)
            .expect("extract_text_with_options");
        assert!(
            !text.contains("SECTION-01-79-00"),
            "include_artifacts=false must still exclude /Artifact content, got {:?}",
            text
        );
    }
}

#[test]
fn extract_text_includes_artifact_footer_by_default() {
    for pdf in [
        fixture_tagged_page_with_artifact_footer(),
        fixture_untagged_page_with_artifact_footer(),
    ] {
        let doc = PdfDocument::from_bytes(pdf).expect("open");
        let text = doc.extract_text(0).expect("extract_text");
        assert!(
            text.contains("SECTION-01-79-00"),
            "extract_text() must include /Artifact content by default, got {:?}",
            text
        );
    }
}

#[test]
fn artifact_footer_survives_on_both_tagged_and_untagged_paths() {
    // The trustworthy-tagged page takes the structure-order path and the
    // untagged page the geometric one; the /Artifact drop must be absent
    // from both, not accidentally skipped by only one. ~keep
    for pdf in [
        fixture_tagged_page_with_artifact_footer(),
        fixture_untagged_page_with_artifact_footer(),
    ] {
        let doc = PdfDocument::from_bytes(pdf).expect("open");
        let text = doc.extract_text(0).expect("extract_text");
        assert!(
            text.contains("SECTION-01-79-00"),
            "/Artifact content must be included by default on \
             both tagged and untagged PDFs, got {:?}",
            text
        );
    }
}
