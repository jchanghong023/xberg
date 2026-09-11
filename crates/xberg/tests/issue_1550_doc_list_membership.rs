//! Regression test for #1550: a legacy `.doc` paragraph that Word numbers
//! automatically arrived as prose, indistinguishable from an unnumbered
//! sentence, while the DOCX path emitted a `ListItem` for the same construct.
//!
//! The binding lives in the paragraph's properties (`sprmPIlfo` / `sprmPIlvl`
//! in a `PAPX`), not in the text stream, so only numbers an author typed as
//! characters used to survive. Scope here is membership and nesting depth --
//! the number Word paints (`1.1`, `a.`) is deliberately not resolved, which
//! would need `PlfLst`/`PlfLfo` counter state.
//!
//! `unit_test_lists.doc` is used rather than a synthesized document on
//! purpose. #1551 is the standing proof that a `.doc` suite built only from
//! this codebase's own assumptions about the format will agree with those
//! assumptions even when they are wrong: the `fcClx` index was mirrored in the
//! test helper, so every synthetic test passed while the piece table was dead
//! on every real file. The expected counts below come from an independent
//! parser written against [MS-DOC].

#![cfg(feature = "office")]

mod helpers;

use xberg::{ElementType, ExtractionConfig, ResultFormat};

fn corpus_document() -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/doc/unit_test_lists.doc");
    assert!(
        path.exists(),
        "corpus fixture missing at {}; fetch test_documents rather than skipping -- a skipped \
         test and a passing test are indistinguishable in a summary",
        path.display()
    );
    std::fs::read(&path).expect("read corpus fixture")
}

async fn extract_elements() -> Vec<xberg::Element> {
    let config = ExtractionConfig {
        result_format: ResultFormat::ElementBased,
        ..Default::default()
    };
    let document = helpers::extract_bytes_document(&corpus_document(), "application/msword", &config)
        .await
        .expect("DOC extraction should succeed");
    document.elements.expect("element-based format yields elements")
}

#[tokio::test]
async fn auto_numbered_doc_paragraphs_are_list_items_not_prose() {
    let elements = extract_elements().await;

    let list_items = elements
        .iter()
        .filter(|element| element.element_type == ElementType::ListItem)
        .count();

    assert_eq!(
        list_items,
        25,
        "expected the 25 list-bound paragraphs to arrive as ListItems; got {list_items} \
         out of {} elements",
        elements.len()
    );
}

#[tokio::test]
async fn a_document_without_list_bindings_still_gets_one_element_per_word_paragraph() {
    // Elements follow Word's paragraph structure for every Word97+ document,
    // not only ones with lists. `vendor_renewal_letter.doc` is the case that
    // makes this visible: its 10 paragraphs used to arrive as a SINGLE element
    // because the old path split `content` on "\n\n" and the letter's
    // paragraphs are separated by single marks.
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_documents/doc/vendor_renewal_letter.doc");
    assert!(path.exists(), "corpus fixture missing at {}", path.display());
    let config = ExtractionConfig {
        result_format: ResultFormat::ElementBased,
        ..Default::default()
    };
    let document = helpers::extract_bytes_document(&std::fs::read(&path).unwrap(), "application/msword", &config)
        .await
        .expect("DOC extraction should succeed");
    let elements = document.elements.expect("elements");

    assert_eq!(
        elements.len(),
        10,
        "expected one element per Word paragraph; got {} -- 1 would mean the blank-line \
         chunking path ran and merged them",
        elements.len()
    );
    assert!(
        !elements
            .iter()
            .any(|element| element.element_type == ElementType::ListItem),
        "this document has no list bindings and must not gain list structure"
    );
}
