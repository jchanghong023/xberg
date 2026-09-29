//! GH#1778 no-ship measurement.
//!
//! The premise under test: `tests/table_cell_word_seam.rs`'s ambiguous-gap
//! fixture declares its space glyph (code 32) at 170/1000 em, narrower than
//! real Helvetica's 278/1000 em (`fonts/font_dict.rs`'s Standard-14 table).
//! The question was whether that unrealistic width is what makes the
//! fragmented-word case ("Cre"/"d"/"it" + " < 21 500 euros", drawn as four
//! `Tm`+`Tj` show ops) glue correctly today, i.e. whether a realistic font
//! would change the outcome.
//!
//! Measured answer: no. `extract_spans`, `extract_tables` and `extract_text`
//! all produce the same correctly-glued "Credit < 21 500 euros" at both
//! 170/1000 em and 278/1000 em. The Tm+Tj continuation arm in
//! `extractors/text/operators.rs` that joins these fragments does not
//! consult the space-glyph width — or any gap magnitude — at all for
//! proportional fonts; it concatenates decoded text purely on
//! baseline/transform geometry. That is by design a *known*, separately
//! tracked defect for gaps that are genuinely two tokens
//! (`known_defect_proportional_tm_per_glyph_gap.rs` pins gluing across a
//! 100pt gap), but it is not a defect for this seam: there is no space glyph
//! at the seam, so blind concatenation is already the correct answer, at any
//! font width. This test pins that finding directly (rather than only via
//! the arithmetic in the 170/1000 em fixture), so it cannot be dismissed as
//! an artifact of the unrealistic width.
//!
//! Confirmed non-vacuous: temporarily bounding the continuation arm's gap
//! (rejecting a continuation when `e` advances more than 1.0pt past the
//! buffer's accumulated width — a stand-in for the kind of gap-magnitude
//! check GH#1778 asked to add) turns `sub_em_fragment_seams_do_not_split`
//! (below) red at both 170/1000 em and 278/1000 em: the seam (1.75pt) is
//! rejected as a continuation and the word splits into "Cre d it" / "Cred it"
//! again. That is the same outcome `known_defect_proportional_tm_per_glyph_gap.rs`
//! already documents as unsafe for the 170/1000 em fixture — this test shows
//! it holds at a realistic width too, so no space-width fix closes the gap
//! without reintroducing that regression.

use xberg_native_pdf::document::PdfDocument;

/// Same layout as `table_cell_word_seam.rs::fragmented_cell_pdf`, with the
/// declared space-glyph width raised from 170/1000 em to real Helvetica's
/// 278/1000 em. Nothing else changes: same fragments, same pen positions,
/// same seam (1.75pt) and word-space (now 2.502pt at 9pt) geometry.
fn fragmented_cell_pdf_realistic_space() -> Vec<u8> {
    let mut content = Vec::new();
    for y in [710.0f32, 690.0, 670.0, 650.0] {
        content.extend_from_slice(format!("0.7 w 95 {y} m 420 {y} l S\n").as_bytes());
    }
    for x in [95.0f32, 300.0, 420.0] {
        content.extend_from_slice(format!("0.7 w {x} 650 m {x} 710 l S\n").as_bytes());
    }
    content.extend_from_slice(b"BT /F1 9 Tf\n");
    content.extend_from_slice(b"1 0 0 1 100 695 Tm (Garanties) Tj\n");
    content.extend_from_slice(b"1 0 0 1 305 695 Tm (Montant) Tj\n");
    content.extend_from_slice(b"1 0 0 1 100 655 Tm (Duree) Tj\n");
    content.extend_from_slice(b"1 0 0 1 305 655 Tm (25 ans) Tj\n");
    // Same fragments and pen positions as table_cell_word_seam.rs: only the
    // font's declared space width differs (see build_minimal_pdf_realistic_space). ~keep
    content.extend_from_slice(b"1 0 0 1 100 675 Tm (Cre) Tj\n");
    content.extend_from_slice(b"1 0 0 1 113.981 675 Tm (d) Tj\n");
    content.extend_from_slice(b"1 0 0 1 119.808 675 Tm (it) Tj\n");
    content.extend_from_slice(b"1 0 0 1 127.962 675 Tm ( < 21 500 euros) Tj\n");
    content.extend_from_slice(b"1 0 0 1 305 675 Tm (Oui) Tj\n");
    content.extend_from_slice(b"ET");
    build_minimal_pdf_realistic_space(&content)
}

/// Identical to `table_cell_word_seam.rs::build_minimal_pdf_raw` except the
/// space glyph (code 32, the first `/Widths` entry) is declared at 278/1000
/// em — real Helvetica's space width — instead of 170/1000 em.
fn build_minimal_pdf_realistic_space(content: &[u8]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();

    let off1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let off2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

    let off3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
          /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n",
    );

    let off4 = pdf.len();
    pdf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len()).as_bytes());
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    // 453/1000 em for every WinAnsi code except the space (code 32, the first
    // entry), which is 278/1000 em — real Helvetica's space width, unlike the
    // sibling fixture's 170/1000 em. ~keep
    let off5 = pdf.len();
    let mut widths_v = vec!["453"; 95];
    widths_v[0] = "278";
    let widths = widths_v.join(" ");
    pdf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica \
             /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 126 /Widths [{widths}] >>\nendobj\n"
        )
        .as_bytes(),
    );

    let xref_pos = pdf.len();
    let offsets = [0usize, off1, off2, off3, off4, off5];
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len()).as_bytes());
    pdf.extend_from_slice(format!("{:010} 65535 f\r\n", 0).as_bytes());
    for &off in &offsets[1..] {
        pdf.extend_from_slice(format!("{off:010} 00000 n\r\n").as_bytes());
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

/// At a realistic space width the fragmented word must still glue correctly
/// through the span, table and text-assembly paths — the same paths
/// `table_cell_word_seam.rs` pins at the (unrealistic) 170/1000 em width.
/// A regression here (a space-width-dependent fix that only works at one
/// width) would show up as the word splitting at 278/1000 em while it stays
/// glued at 170/1000 em, or vice versa.
#[test]
fn sub_em_fragment_seams_do_not_split_at_a_realistic_space_width() {
    let doc = PdfDocument::from_bytes(fragmented_cell_pdf_realistic_space()).expect("open");

    let spans = doc.extract_spans(0).expect("spans");
    let flow = spans
        .iter()
        .find(|s| s.text.contains("euros"))
        .expect("flow span with the fragmented line");
    assert!(
        flow.text.contains("Credit < 21 500 euros"),
        "at a realistic 278/1000 em space width, the span-level merger no longer joins the seams: {:?}",
        flow.text
    );

    let tables = doc.extract_tables(0).expect("tables");
    let cell = tables
        .iter()
        .flat_map(|t| &t.rows)
        .flat_map(|r| &r.cells)
        .find(|c| c.text.contains("euros"))
        .expect("cell with the fragmented line");
    assert_eq!(
        cell.text, "Credit < 21 500 euros",
        "at a realistic 278/1000 em space width, the cell builder re-split a word the span-level merger had joined"
    );

    let text = doc.extract_text(0).expect("text");
    assert!(
        text.contains("Credit"),
        "at a realistic 278/1000 em space width, extracted text lost the fragmented word: {text:?}"
    );
    assert!(
        !text.contains("Cre d it") && !text.contains("Cred it"),
        "at a realistic 278/1000 em space width, extracted text shows the re-split word: {text:?}"
    );
}
