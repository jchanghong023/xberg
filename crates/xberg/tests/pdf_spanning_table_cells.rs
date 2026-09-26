//! Text inside a row-spanning cell must survive public extraction (#1802).
#![cfg(feature = "pdf")]

mod helpers;
use helpers::extract_bytes_document_blocking;
use xberg::{ExtractionConfig, PdfConfig};

fn table_pdf(label_y: u32) -> Vec<u8> {
    let content = format!(
        "0.5 w 50 600 m 450 600 l S 50 660 m 450 660 l S \
         50 620 m 450 620 l S 150 640 m 450 640 l S \
         50 600 m 50 660 l S 150 600 m 150 660 l S \
         300 600 m 300 660 l S 450 600 m 450 660 l S \
         BT /F1 10 Tf 60 {label_y} Td (SPAN) Tj ET \
         BT /F1 10 Tf 160 645 Td (Alpha) Tj ET BT /F1 10 Tf 310 645 Td (10) Tj ET \
         BT /F1 10 Tf 160 625 Td (Beta) Tj ET BT /F1 10 Tf 310 625 Td (20) Tj ET \
         BT /F1 10 Tf 60 605 Td (Anchor) Tj ET BT /F1 10 Tf 160 605 Td (Gamma) Tj ET BT /F1 10 Tf 310 605 Td (30) Tj ET \
         BT /F1 10 Tf 60 690 Td (Outside the table) Tj ET"
    );
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    pdf
}

#[test]
fn row_spanning_label_is_retained_once_at_every_vertical_position() {
    for y in [645, 635, 625] {
        let config = ExtractionConfig {
            disable_ocr: true,
            use_cache: false,
            enable_quality_processing: false,
            pdf_options: Some(PdfConfig {
                extract_tables: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = extract_bytes_document_blocking(&table_pdf(y), "application/pdf", &config).unwrap();
        assert_eq!(result.tables.len(), 1, "label y={y}: {:?}", result.tables);
        let cells = &result.tables[0].cells;
        assert_eq!(
            cells
                .iter()
                .filter(|row| row.first().is_some_and(|c| c == "SPAN"))
                .count(),
            1,
            "label y={y}: {cells:?}"
        );
        for text in ["SPAN", "Anchor", "Alpha", "Beta", "Gamma", "10", "20", "30"] {
            assert_eq!(
                cells.iter().flatten().filter(|c| c.as_str() == text).count(),
                1,
                "{text}, label y={y}: {cells:?}"
            );
        }
        assert!(!cells.iter().flatten().any(|c| c.contains("Outside")));
    }
}
