#[test]
fn test_prepress_fixture_combinations() {
    let pdf_bytes = build_pdf_prepress_layers();
    let doc = PdfDocument::from_bytes(pdf_bytes).expect("parse PDF");

    let (data_all, w, h) = render_raw(&doc, HashSet::new());
    let (ar, ag, ab, _) = sample_region(&data_all, w, h, 100, 80, 30, 30);
    assert!(
        ar > 200.0 && ab > 200.0 && ag < 80.0,
        "Artwork (magenta) missing: ({ar}, {ag}, {ab})"
    );
    let (dr, dg, db, _) = sample_region(&data_all, w, h, 270, 80, 30, 30);
    assert!(
        dr < 80.0 && dg > 200.0 && db > 200.0,
        "Dieline (cyan) missing: ({dr}, {dg}, {db})"
    );
    let (vr, vg, vb, _) = sample_region(&data_all, w, h, 370, 80, 30, 30);
    assert!(
        vr > 200.0 && vg > 200.0 && vb < 80.0,
        "Varnish (yellow) missing: ({vr}, {vg}, {vb})"
    );

    let excluded = HashSet::from(["Dieline".to_string(), "Varnish".to_string()]);
    let (data, w, h) = render_raw(&doc, excluded);
    let (ar, ag, ab, _) = sample_region(&data, w, h, 100, 80, 30, 30);
    assert!(
        ar > 200.0 && ab > 200.0 && ag < 80.0,
        "Artwork should remain when only finishing layers dropped: ({ar}, {ag}, {ab})"
    );
    let (dr, dg, db, _) = sample_region(&data, w, h, 270, 80, 30, 30);
    assert!(
        dr > 240.0 && dg > 240.0 && db > 240.0,
        "Dieline must be hidden: ({dr}, {dg}, {db})"
    );
    let (vr, vg, vb, _) = sample_region(&data, w, h, 370, 80, 30, 30);
    assert!(
        vr > 240.0 && vg > 240.0 && vb > 240.0,
        "Varnish must be hidden: ({vr}, {vg}, {vb})"
    );

    let excluded = HashSet::from(["Artwork".to_string()]);
    let (data, w, h) = render_raw(&doc, excluded);
    let (ar, ag, ab, _) = sample_region(&data, w, h, 100, 80, 30, 30);
    assert!(
        ar > 240.0 && ag > 240.0 && ab > 240.0,
        "Artwork must be hidden: ({ar}, {ag}, {ab})"
    );
    let (dr, dg, db, _) = sample_region(&data, w, h, 270, 80, 30, 30);
    assert!(
        dr < 80.0 && dg > 200.0 && db > 200.0,
        "Dieline must remain: ({dr}, {dg}, {db})"
    );
    let (vr, vg, vb, _) = sample_region(&data, w, h, 370, 80, 30, 30);
    assert!(
        vr > 200.0 && vg > 200.0 && vb < 80.0,
        "Varnish must remain: ({vr}, {vg}, {vb})"
    );
}

/// Build a PDF whose OCMD uses a `/VE` visibility expression.
///
/// The expression is `[/<op> <ocg_ref> ...]`. We test:
///   /Not  — visible iff the OCG is excluded (inverted)
///   /And  — visible iff all referenced OCGs are on
///   /Or   — visible iff any referenced OCG is on
/// Two OCGs ("A", "B") are defined. A blue rect is inside the OCMD scope, a
/// green rect is always visible.
fn build_pdf_with_ve(expr: &[u8]) -> Vec<u8> {
    let mut pdf = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();

    pdf.extend_from_slice(b"%PDF-1.4\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R\n\
           /OCProperties << /OCGs [7 0 R 8 0 R] /D << /ON [7 0 R 8 0 R] >> >> >>\nendobj\n\n",
    );
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\n");
    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
           /Contents 4 0 R\n\
           /Resources << /Font << /F1 5 0 R >> /Properties << /MC0 6 0 R >> >> >>\nendobj\n\n",
    );

    let content = b"/OC /MC0 BDC 0 0 1 rg 50 550 200 200 re f EMC \
                    0 1 0 rg 300 550 200 200 re f";
    offsets.push(pdf.len());
    let hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    pdf.extend_from_slice(hdr.as_bytes());
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica\n\
           /Encoding /WinAnsiEncoding >>\nendobj\n\n",
    );

    offsets.push(pdf.len());
    let ocmd = format!(
        "6 0 obj\n<< /Type /OCMD /VE {} >>\nendobj\n\n",
        std::str::from_utf8(expr).unwrap()
    );
    pdf.extend_from_slice(ocmd.as_bytes());

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"7 0 obj\n<< /Type /OCG /Name /A >>\nendobj\n\n");
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"8 0 obj\n<< /Type /OCG /Name /B >>\nendobj\n\n");

    let xref_offset = pdf.len();
    let n_obj = offsets.len() + 1;
    let mut xref = format!("xref\n0 {}\n", n_obj);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{:010} 00000 n \n", off));
    }
    pdf.extend_from_slice(xref.as_bytes());
    let trailer = format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        n_obj, xref_offset
    );
    pdf.extend_from_slice(trailer.as_bytes());
    pdf
}

fn ocmd_scope_is_visible(doc: &PdfDocument, excluded: HashSet<String>) -> bool {
    let (data, w, h) = render_raw(doc, excluded);
    let (r, g, b, _) = sample_region(&data, w, h, 100, 100, 30, 30);
    // Blue rect is inside the OCMD scope. Visible -> blue (b high, r/g low).
    // Hidden -> white background (all high). ~keep
    b > 200.0 && r < 80.0 && g < 80.0
}

#[test]
fn test_ve_not_operator() {
    // /VE = [/Not 7 0 R]
    // Visible iff OCG "A" is off (i.e. "A" is excluded). ~keep
    let doc = PdfDocument::from_bytes(build_pdf_with_ve(b"[/Not 7 0 R]")).unwrap();
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::new()),
        "Not(A): with A on, scope should be hidden"
    );
    assert!(
        ocmd_scope_is_visible(&doc, HashSet::from(["A".to_string()])),
        "Not(A): with A excluded (off), scope should be visible"
    );
}

#[test]
fn test_ve_and_operator() {
    // /VE = [/And 7 0 R 8 0 R]
    // Visible iff both A and B are on. ~keep
    let doc = PdfDocument::from_bytes(build_pdf_with_ve(b"[/And 7 0 R 8 0 R]")).unwrap();
    assert!(
        ocmd_scope_is_visible(&doc, HashSet::new()),
        "And(A,B): both on -> visible"
    );
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::from(["A".to_string()])),
        "And(A,B): A excluded -> hidden"
    );
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::from(["B".to_string()])),
        "And(A,B): B excluded -> hidden"
    );
}

#[test]
fn test_ve_or_operator() {
    // /VE = [/Or 7 0 R 8 0 R]
    // Visible iff A or B is on. ~keep
    let doc = PdfDocument::from_bytes(build_pdf_with_ve(b"[/Or 7 0 R 8 0 R]")).unwrap();
    assert!(
        ocmd_scope_is_visible(&doc, HashSet::new()),
        "Or(A,B): both on -> visible"
    );
    assert!(
        ocmd_scope_is_visible(&doc, HashSet::from(["A".to_string()])),
        "Or(A,B): A excluded but B on -> visible"
    );
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::from(["A".to_string(), "B".to_string()])),
        "Or(A,B): both excluded -> hidden"
    );
}

// ---------------------------------------------------------------------------
// /D default-off respect — ISO 32000-1 §8.11.4
// --------------------------------------------------------------------------- ~keep

/// PDF whose /OCProperties/D entry marks an OCG as off by default.
fn build_pdf_with_default_off_layer() -> Vec<u8> {
    let mut pdf = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();

    pdf.extend_from_slice(b"%PDF-1.4\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R\n\
           /OCProperties << /OCGs [6 0 R] /D << /BaseState /ON /OFF [6 0 R] >> >> >>\nendobj\n\n",
    );
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\n");
    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
           /Contents 4 0 R\n\
           /Resources << /Font << /F1 5 0 R >> /Properties << /MC0 6 0 R >> >> >>\nendobj\n\n",
    );

    let content = b"/OC /MC0 BDC 0 0 1 rg 50 550 200 200 re f EMC \
                    0 1 0 rg 300 550 200 200 re f";
    offsets.push(pdf.len());
    let hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    pdf.extend_from_slice(hdr.as_bytes());
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica\n\
           /Encoding /WinAnsiEncoding >>\nendobj\n\n",
    );
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"6 0 obj\n<< /Type /OCG /Name /Watermark >>\nendobj\n\n");

    let xref_offset = pdf.len();
    let n_obj = offsets.len() + 1;
    let mut xref = format!("xref\n0 {}\n", n_obj);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{:010} 00000 n \n", off));
    }
    pdf.extend_from_slice(xref.as_bytes());
    let trailer = format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        n_obj, xref_offset
    );
    pdf.extend_from_slice(trailer.as_bytes());
    pdf
}

#[test]
fn test_default_off_layer_hidden_with_empty_exclusions() {
    // /D/OFF marks Watermark off — content should be hidden even when the
    // caller passes no excluded_layers. ~keep
    let doc = PdfDocument::from_bytes(build_pdf_with_default_off_layer()).unwrap();
    let (data, w, h) = render_raw(&doc, HashSet::new());
    let (r, g, b, _) = sample_region(&data, w, h, 100, 100, 30, 30);
    assert!(
        r > 240.0 && g > 240.0 && b > 240.0,
        "Default-off OCG content should be hidden: ({r}, {g}, {b})"
    );
    let (r2, g2, b2, _) = sample_region(&data, w, h, 350, 100, 30, 30);
    assert!(
        r2 < 80.0 && g2 > 200.0 && b2 < 80.0,
        "Unrelated content should remain: ({r2}, {g2}, {b2})"
    );
}

#[test]
fn test_ve_nested_expression() {
    // /VE = [/And 7 0 R [/Not 8 0 R]]   — A on AND B off ~keep
    let doc = PdfDocument::from_bytes(build_pdf_with_ve(b"[/And 7 0 R [/Not 8 0 R]]")).unwrap();
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::new()),
        "And(A, Not(B)): A on, B on -> hidden"
    );
    assert!(
        ocmd_scope_is_visible(&doc, HashSet::from(["B".to_string()])),
        "And(A, Not(B)): A on, B excluded -> visible"
    );
    assert!(
        !ocmd_scope_is_visible(&doc, HashSet::from(["A".to_string(), "B".to_string()])),
        "And(A, Not(B)): A excluded -> hidden"
    );
}
