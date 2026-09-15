//! Regression tests for xberg-io/xberg#1223: the text-layer table heuristic must
//! accept a dense unruled ledger that its density guard wrongly rejected (row
//! count alone must not disqualify a real table), while still rejecting genuine
//! multi-column prose.
//!
//! An all-text roster is deliberately NOT recovered: a roster and a scanned
//! multi-column prose page (nougat pattern) are indistinguishable to the
//! row-coherence guard, so the conservative alpha-ratio check is kept and both
//! stay rejected — precision over recall for the ambiguous all-text case.
//!
//! These build unruled (text-only, no ruling lines) synthetic PDFs so they
//! exercise the heuristic fallback tier, not the ruled-table detectors.

#![cfg(feature = "pdf")]

mod helpers;
use helpers::extract_bytes_document_blocking;

use xberg::core::config::ExtractionConfig;

/// Escape a PDF literal-string show-text operand: only `(`, `)`, and `\`
/// need a backslash per ISO 32000-1:2008 §7.3.4.2.
fn pdf_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '(' || ch == ')' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Assemble a one-page PDF from a raw content stream and a Standard-14
/// Helvetica font resource. Hand-built rather than via the (now-removed)
/// `xberg_native_pdf::writer::DocumentBuilder`: these heuristics key off
/// column/row alignment and text density, not exact glyph placement, so a
/// minimal `Tj`-only content stream (no ruling lines -- these fixtures are
/// deliberately unruled) reproduces the same geometry the writer used to
/// emit.
fn build_pdf_with_content(content: &str) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0usize];

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
          /Contents 4 0 R /Resources << /Font << /Helvetica 5 0 R >> >> >>\nendobj\n",
    );

    offsets.push(pdf.len());
    pdf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    pdf.extend_from_slice(content.as_bytes());
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
          /Encoding /WinAnsiEncoding >>\nendobj\n",
    );

    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for &off in &offsets[1..] {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            offsets.len(),
            xref_pos
        )
        .as_bytes(),
    );
    pdf
}

fn text_pdf(rows: &[Vec<(f32, f32, String)>]) -> Vec<u8> {
    let (top, row_h) = (760.0_f32, 16.0_f32);
    let mut content = String::new();
    for (i, row) in rows.iter().enumerate() {
        let y = top - row_h * i as f32;
        let baseline = y + 4.0;
        for (x, _w, text) in row {
            content.push_str(&format!(
                "BT\n/Helvetica 10 Tf\n1 0 0 1 {x} {baseline} Tm\n({}) Tj\nET\n",
                pdf_escape(text)
            ));
        }
    }
    build_pdf_with_content(&content)
}

/// Like [`text_pdf`] but with an explicit baseline per line, for fixtures whose
/// leading and paragraph spacing are the thing under test.
fn text_pdf_at_baselines(lines: &[(f32, f32, &str)]) -> Vec<u8> {
    let mut content = String::new();
    for (x, baseline, text) in lines {
        content.push_str(&format!(
            "BT\n/Helvetica 10 Tf\n1 0 0 1 {x} {baseline} Tm\n({}) Tj\nET\n",
            pdf_escape(text)
        ));
    }
    build_pdf_with_content(&content)
}

fn table_count(bytes: &[u8]) -> usize {
    extract_bytes_document_blocking(bytes, "application/pdf", &ExtractionConfig::default())
        .expect("extraction must succeed")
        .tables
        .len()
}

/// A dense 3-column unruled ledger (Account | Amount | Note, 30 rows). Real
/// table; the density guard rejected it before.
#[test]
fn dense_unruled_ledger_is_detected() {
    let cols = [(50.0_f32, 150.0_f32), (210.0, 90.0), (310.0, 200.0)];
    let mut rows = vec![vec![
        (cols[0].0, cols[0].1, "Account".to_string()),
        (cols[1].0, cols[1].1, "Amount".to_string()),
        (cols[2].0, cols[2].1, "Note".to_string()),
    ]];
    for n in 1..=30 {
        rows.push(vec![
            (cols[0].0, cols[0].1, format!("Account {n:04}")),
            (cols[1].0, cols[1].1, format!("${}.00", n * 137)),
            (cols[2].0, cols[2].1, format!("ref {n}")),
        ]);
    }
    assert!(
        table_count(&text_pdf(&rows)) >= 1,
        "a 30-row 3-column ledger must be detected as a table"
    );
}

/// An all-text roster (Name | City | Role). Ambiguous against scanned columned
/// prose, so it is conservatively NOT detected — the alpha-ratio guard that
/// protects against the nougat pattern also catches this. This pins that the
/// ledger relaxation does not accidentally reopen the all-text prose hole.
#[test]
fn all_text_roster_is_conservatively_rejected() {
    let cols = [(50.0_f32, 150.0_f32), (210.0, 120.0), (340.0, 160.0)];
    let people = [
        ("Alice Johnson", "New York", "Manager"),
        ("Bob Smith", "Chicago", "Analyst"),
        ("Carol White", "Boston", "Director"),
        ("David Brown", "Seattle", "Engineer"),
        ("Eve Davis", "Austin", "Designer"),
        ("Frank Moore", "Denver", "Recruiter"),
    ];
    let mut rows = vec![vec![
        (cols[0].0, cols[0].1, "Name".to_string()),
        (cols[1].0, cols[1].1, "City".to_string()),
        (cols[2].0, cols[2].1, "Role".to_string()),
    ]];
    for (name, city, role) in people {
        rows.push(vec![
            (cols[0].0, cols[0].1, name.to_string()),
            (cols[1].0, cols[1].1, city.to_string()),
            (cols[2].0, cols[2].1, role.to_string()),
        ]);
    }
    assert_eq!(
        table_count(&text_pdf(&rows)),
        0,
        "an all-text roster stays rejected — indistinguishable from columned prose"
    );
}

/// Two-column prose (an article laid out in columns). Must NOT be a table —
/// the guard against columned prose must still fire.
#[test]
fn columned_prose_is_not_a_table() {
    let left = [
        "The quick brown fox jumps over",
        "the lazy dog and then continues",
        "running across the wide green",
        "field toward the distant hills",
        "where the sun was slowly setting",
        "behind the ancient oak trees that",
    ];
    let right = [
        "In addition to that it should be",
        "noted that the weather was quite",
        "pleasant throughout the entire",
        "afternoon which made the long",
        "walk considerably more enjoyable",
        "for everyone who was present there",
    ];
    let rows: Vec<Vec<(f32, f32, String)>> = (0..left.len())
        .map(|i| {
            vec![
                (50.0_f32, 240.0_f32, left[i].to_string()),
                (300.0, 240.0, right[i].to_string()),
            ]
        })
        .collect();
    assert_eq!(
        table_count(&text_pdf(&rows)),
        0,
        "columned prose must not be detected as a table"
    );
}

/// xberg-io/xberg#1542: three ordinary body paragraphs, wrapped over three lines
/// each at a single-space word pitch. Paragraphs two and three share their first
/// two lines verbatim and differ only in the last line — numeric in one,
/// alphabetic in the other.
///
/// At v1.0.14 the numeric one alone came back as a six-column `Table` with its
/// words reordered column-major: `find_data_start` promotes leading rows into the
/// header as soon as a later row is digit-heavy, so both prose lines were folded
/// into row 0, where no `grid[1..]` prose guard and no numeric-exemption
/// denominator can see them. Merging multi-word cells before detecting columns
/// closed it — a prose line at one-space pitch is now a single cell token, so the
/// region is one column wide and never reaches a grid.
///
/// The digits are the whole variable, so the assertion is per paragraph: any of
/// the three becoming a table is the defect.
#[test]
fn prose_paragraph_ending_in_a_phone_number_is_not_a_table() {
    const LEFT: f32 = 48.24;
    const LEADING: f32 = 13.8;
    const PARAGRAPH_GAP: f32 = 27.6;

    let paragraphs = [
        [
            "This handbook lists the replacement parts for the appliances covered by the current range. The",
            "exploded views on the following pages show how each assembly is put together, and the position",
            "numbers in the drawings correspond to the numbers in the table printed beside each drawing.",
        ],
        [
            "Replacement parts are available from your regional distributor. Consult the online catalogue under",
            "parts for current addresses. For technical assistance you can contact the service desk on working",
            "Customerservice, telephone 0000 - 00 00 00 (option 0)",
        ],
        [
            "Replacement parts are available from your regional distributor. Consult the online catalogue under",
            "parts for current addresses. For technical assistance you can contact the service desk on working",
            "Customerservice, via the switchboard (see the back cover)",
        ],
    ];

    let mut lines: Vec<(f32, f32, &str)> = Vec::new();
    let mut baseline = 758.40_f32;
    for paragraph in &paragraphs {
        for line in paragraph {
            lines.push((LEFT, baseline, line));
            baseline -= LEADING;
        }
        baseline -= PARAGRAPH_GAP - LEADING;
    }

    let document = extract_bytes_document_blocking(
        &text_pdf_at_baselines(&lines),
        "application/pdf",
        &ExtractionConfig::default(),
    )
    .expect("extraction must succeed");

    assert_eq!(
        document.tables.len(),
        0,
        "no paragraph on an unruled prose page may be reconstructed as a table, got {:?}",
        document.tables.iter().map(|table| &table.markdown).collect::<Vec<_>>()
    );
    assert!(
        document
            .content
            .contains("Customerservice, telephone 0000 - 00 00 00 (option 0)"),
        "the numeric last line must survive in reading order, got:\n{}",
        document.content
    );
    assert!(
        document
            .content
            .contains("Replacement parts are available from your regional distributor."),
        "the paragraph's opening clause must survive in reading order, got:\n{}",
        document.content
    );
}
