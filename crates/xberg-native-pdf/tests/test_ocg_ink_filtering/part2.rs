#[test]
fn test_filtering_unrelated_layer_produces_identical_output() {
    let pdf_bytes = build_pdf_with_table_and_unrelated_layer();
    let doc = PdfDocument::from_bytes(pdf_bytes).expect("parse PDF");

    let text_normal = doc.extract_text(0).expect("unfiltered");
    let text_filtered = doc
        .extract_text_filtered(0, HashSet::from(["Dieline".to_string()]), HashSet::new())
        .expect("filtered with non-matching layer");

    assert_eq!(
        text_normal, text_filtered,
        "Excluding a layer with no content must produce identical output.\n\
         Unfiltered: {:?}\n\
         Filtered:   {:?}",
        text_normal, text_filtered
    );
}

// ============================================================================
// Bug 4: DeviceN names array as indirect reference is not resolved during
// ink-filter classification — mirrors a real-world shape
// `/CS6 [/DeviceN 4 0 R /DeviceCMYK <attrs>]` where the names list is a
// separate indirect object shared across multiple colour spaces.
// ============================================================================ ~keep

/// Build a PDF where a Form XObject paints text in a DeviceN colour space
/// whose names array is an **indirect reference** to another object,
/// matching the banana-label `/CS6 [/DeviceN 4 0 R …]` shape.
///
/// Page content :  BT (BEFORE) Tj ET  /Fm0 Do  BT (AFTER) Tj ET
/// Form content :  /CS1 cs 1 0 0 0 scn BT (NESTED_SPOT_TEXT) Tj ET
/// Form resources : /CS1 → 7 0 R
/// Obj 7 : [/DeviceN 8 0 R /DeviceCMYK 9 0 R]   ← names slot is INDIRECT
/// Obj 8 : [/Cyan /Magenta /Yellow /SpotRed]    ← the names list
fn build_pdf_with_indirect_devicen_names_in_form() -> Vec<u8> {
    let mut pdf = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
           /Contents 4 0 R\n\
           /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> >>\nendobj\n\n",
    );

    let page_content = b"BT /F1 12 Tf 50 700 Td (BEFORE) Tj ET /Fm0 Do BT /F1 12 Tf 50 600 Td (AFTER) Tj ET";
    offsets.push(pdf.len());
    let hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", page_content.len());
    pdf.extend_from_slice(hdr.as_bytes());
    pdf.extend_from_slice(page_content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    // Obj 5: Form XObject — selects /CS1 (DeviceN), sets all 4 components,
    // then paints (NESTED_SPOT_TEXT). Because /CS1's names array includes
    // /SpotRed, exclude_inks={SpotRed} must suppress this text. ~keep
    let form_stream = b"/CS1 cs 1 0 0 0 scn BT /F1 12 Tf 50 650 Td (NESTED_SPOT_TEXT) Tj ET";
    offsets.push(pdf.len());
    let form_hdr = format!(
        "5 0 obj\n<< /Type /XObject /Subtype /Form /BBox [0 0 612 792]\n\
            /Resources << /Font << /F1 6 0 R >>\n\
            /ColorSpace << /CS1 7 0 R >> >>\n\
            /Length {} >>\nstream\n",
        form_stream.len()
    );
    pdf.extend_from_slice(form_hdr.as_bytes());
    pdf.extend_from_slice(form_stream);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"6 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica\n\
           /Encoding /WinAnsiEncoding >>\nendobj\n\n",
    );

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"7 0 obj\n[/DeviceN 8 0 R /DeviceCMYK 9 0 R]\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"8 0 obj\n[/Cyan /Magenta /Yellow /SpotRed]\nendobj\n\n");

    let tint_fn = b"{ 0 0 0 0 }";
    offsets.push(pdf.len());
    let fn_hdr = format!(
        "9 0 obj\n<< /FunctionType 4 /Domain [0.0 1.0 0.0 1.0 0.0 1.0 0.0 1.0]\n\
         /Range [0.0 1.0 0.0 1.0 0.0 1.0 0.0 1.0] /Length {} >>\nstream\n",
        tint_fn.len()
    );
    pdf.extend_from_slice(fn_hdr.as_bytes());
    pdf.extend_from_slice(tint_fn);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    let xref_offset = pdf.len();
    let n_obj = offsets.len() + 1;
    let mut xref = format!("xref\n0 {}\n", n_obj);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{:010} 00000 n \n", off));
    }
    pdf.extend_from_slice(xref.as_bytes());
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {n_obj} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n").as_bytes(),
    );
    pdf
}

#[test]
fn test_ink_filter_resolves_indirect_devicen_names_in_nested_form() {
    let pdf_bytes = build_pdf_with_indirect_devicen_names_in_form();
    let doc = PdfDocument::from_bytes(pdf_bytes).expect("parse PDF");

    let excluded_inks = HashSet::from(["SpotRed".to_string()]);
    let text = doc
        .extract_text_filtered(0, HashSet::new(), excluded_inks)
        .expect("filtered extract");

    assert!(
        !text.contains("NESTED_SPOT_TEXT"),
        "NESTED_SPOT_TEXT should be suppressed — SpotRed is one of the DeviceN \
         colorants (via indirect names array). Got: {:?}",
        text
    );

    assert!(
        text.contains("BEFORE"),
        "BEFORE should be visible (DeviceGray, before form invocation). Got: {:?}",
        text
    );
    assert!(
        text.contains("AFTER"),
        "AFTER should be visible (DeviceGray, after form returns). Got: {:?}",
        text
    );
}
