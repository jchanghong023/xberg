//! Regression test for xberg-io/xberg#1542 — the reporter's own reproducer.
//!
//! An unruled preface page with three three-line body paragraphs. Paragraphs two
//! and three share their first two lines verbatim and differ only in the last
//! line, numeric in one and alphabetic in the other, so the page is a
//! self-contained A/B: the only thing that turned prose into a table was whether
//! the last line held digits.
//!
//! At v1.0.14 the numeric paragraph came back as a six-column `Table` with its
//! words reordered column-major. `find_data_start` promotes leading rows into the
//! header the moment a later row is digit-heavy, so both prose lines were folded
//! into row 0 — where no `grid[1..]` prose guard, and none of the numeric
//! exemptions' denominators, can see them. Merging multi-word cells before
//! detecting columns closed it upstream of the guards: at a one-space word pitch
//! a prose line is a single cell token, so the region is one column wide.
//!
//! `pdf_table_heuristics::prose_paragraph_ending_in_a_phone_number_is_not_a_table`
//! pins the same behaviour on a synthetic page and runs without the corpus; this
//! one pins the actual bytes that were reported.

#![cfg(feature = "pdf")]
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

mod helpers;
use helpers::{extract_uri_document_blocking, get_test_file_path, skip_if_missing};

use xberg::core::config::ExtractionConfig;

const FIXTURE: &str = "pdf/issue-1542-prose-paragraph-shredded-into-table.pdf";

#[test]
fn should_not_reconstruct_a_prose_paragraph_as_a_table_when_its_last_line_holds_digits() {
    if skip_if_missing(FIXTURE) {
        eprintln!("SKIP: fixture {FIXTURE} not available; run python3 test_documents/scripts/fetch_corpus.py");
        return;
    }

    let document = extract_uri_document_blocking(
        get_test_file_path(FIXTURE),
        Some("application/pdf"),
        &ExtractionConfig::default(),
    )
    .expect("extraction must succeed");

    assert_eq!(
        document.tables.len(),
        0,
        "the page has no table, no ruling line and no vector path; got {:?}",
        document.tables.iter().map(|table| &table.markdown).collect::<Vec<_>>()
    );

    // The defect destroyed reading order rather than losing text, so assert the
    // sentences, not the character count. ~keep
    for expected in [
        "numbers in the drawings correspond to the numbers in the table printed beside each drawing.",
        "Customerservice, telephone 0000 - 00 00 00 (option 0)",
        "Customerservice, via the switchboard (see the back cover)",
    ] {
        assert!(
            document.content.contains(expected),
            "expected {expected:?} intact in reading order, got:\n{}",
            document.content
        );
    }
}
