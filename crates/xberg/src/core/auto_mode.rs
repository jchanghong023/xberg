//! (fork) Engine-level large-document auto-downgrade.
//!
//! Zero-config callers get a speed guard for genuinely huge documents: a light
//! page-count probe runs before the extractor chain, and above
//! [`ExtractionConfig::auto_fast_pages`] the request-local config copy drops to
//! the fast profile (OCR disabled) exactly like an explicit worker
//! `mode:"fast"`. The default threshold sits above the largest
//! standard-corpus document (358 pages), so ordinary documents keep full
//! quality; a probe failure or unknown page count falls back to the normal
//! profile — never errors, never upgrades.

use crate::core::config::extraction::ExtractionConfig;

/// What the auto-downgrade decided, for the observable warning.
pub(crate) struct AutoDowngrade {
    pub pages: u32,
    pub threshold: u32,
}

impl AutoDowngrade {
    pub(crate) fn warning_message(&self) -> String {
        format!(
            "large document ({} pages > auto_fast_pages={}): OCR disabled for speed; \
             set auto_fast_pages=0 to keep full quality",
            self.pages, self.threshold
        )
    }
}

/// Light page-count probe: PDF via the native engine's structure read, OOXML
/// via the zip central directory (`docx` `docProps/app.xml` `<Pages>`, `pptx`
/// slide entries, `xlsx` worksheet entries). Only the `%PDF` / `PK` magic
/// numbers are probed; every other format — and any parse failure — returns
/// `None`, which means "unknown, stay on the normal profile".
pub(crate) fn probe_page_count(content: &[u8]) -> Option<u32> {
    if content.starts_with(b"%PDF") {
        return probe_pdf_pages(content);
    }
    if content.starts_with(b"PK\x03\x04") {
        return probe_ooxml_pages(content);
    }
    None
}

#[cfg(feature = "pdf")]
fn probe_pdf_pages(content: &[u8]) -> Option<u32> {
    crate::pdf::render::pdf_page_count(content, None)
        .ok()
        .and_then(|pages| u32::try_from(pages).ok())
}

#[cfg(not(feature = "pdf"))]
fn probe_pdf_pages(_content: &[u8]) -> Option<u32> {
    None
}

fn probe_ooxml_pages(content: &[u8]) -> Option<u32> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(content)).ok()?;
    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let count_entries = |prefix: &str| {
        names
            .iter()
            .filter(|name| name.starts_with(prefix) && name.ends_with(".xml"))
            .count()
    };
    if names.iter().any(|name| name.starts_with("word/")) {
        let mut entry = zip.by_name("docProps/app.xml").ok()?;
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut entry, &mut xml).ok()?;
        extract_pages_tag(&xml)
    } else if names.iter().any(|name| name.starts_with("ppt/slides/")) {
        u32::try_from(count_entries("ppt/slides/")).ok()
    } else if names.iter().any(|name| name.starts_with("xl/worksheets/")) {
        u32::try_from(count_entries("xl/worksheets/")).ok()
    } else {
        None
    }
}

/// Read `<Pages>N</Pages>` from a `docProps/app.xml` payload. Office writes the
/// field as a plain integer element; a missing tag, non-numeric body, or
/// overflow all return `None` (unknown page count).
fn extract_pages_tag(xml: &str) -> Option<u32> {
    let start = xml.find("<Pages>")? + "<Pages>".len();
    let end = start + xml[start..].find("</Pages>")?;
    xml[start..end].trim().parse().ok()
}

/// Threshold decision: `None` keeps the normal profile (auto-downgrade
/// disabled, page count unknown, or at/below the threshold).
pub(crate) fn evaluate(content: &[u8], config: &ExtractionConfig) -> Option<AutoDowngrade> {
    let threshold = config.auto_fast_pages;
    if threshold == 0 {
        return None;
    }
    let pages = probe_page_count(content)?;
    (pages > threshold).then_some(AutoDowngrade { pages, threshold })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docx_zip_with_pages(pages: Option<u32>) -> Vec<u8> {
        let body = match pages {
            Some(n) => format!(
                "<Properties><Application>Microsoft Office Word</Application>\
                 <Pages>{n}</Pages></Properties>"
            ),
            None => "<Properties><Application>Microsoft Office Word</Application></Properties>".to_string(),
        };
        zip_with_entries(vec![
            ("word/document.xml".to_string(), "<w:document/>".to_string()),
            ("docProps/app.xml".to_string(), body),
        ])
    }

    fn zip_with_entries(entries: Vec<(String, String)>) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            for (name, body) in entries {
                writer.start_file(&name, options).expect("start_file");
                std::io::Write::write_all(&mut writer, body.as_bytes()).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        cursor.into_inner()
    }

    fn pptx_zip_with_slides(slides: usize) -> Vec<u8> {
        let mut entries = vec![("ppt/presentation.xml".to_string(), "<p:presentation/>".to_string())];
        for i in 1..=slides {
            entries.push((format!("ppt/slides/slide{i}.xml"), "<p:sld/>".to_string()));
        }
        zip_with_entries(entries)
    }

    fn xlsx_zip_with_sheets(sheets: usize) -> Vec<u8> {
        let mut entries = vec![("xl/workbook.xml".to_string(), "<workbook/>".to_string())];
        for i in 1..=sheets {
            entries.push((format!("xl/worksheets/sheet{i}.xml"), "<worksheet/>".to_string()));
        }
        zip_with_entries(entries)
    }

    #[test]
    fn probe_reads_docx_pages_from_app_properties() {
        assert_eq!(probe_page_count(&docx_zip_with_pages(Some(358))), Some(358));
    }

    #[test]
    fn probe_counts_pptx_slides() {
        assert_eq!(probe_page_count(&pptx_zip_with_slides(37)), Some(37));
    }

    #[test]
    fn probe_counts_xlsx_worksheets() {
        assert_eq!(probe_page_count(&xlsx_zip_with_sheets(12)), Some(12));
    }

    #[test]
    fn probe_returns_none_for_unknown_or_plain_content() {
        // app.xml without <Pages>: page count unknown, not zero.
        assert_eq!(probe_page_count(&docx_zip_with_pages(None)), None);
        assert_eq!(probe_page_count(b"plain text, no magic"), None);
        assert_eq!(probe_page_count(b""), None);
    }

    #[test]
    fn evaluate_honours_threshold_and_disable() {
        let mut config = ExtractionConfig {
            auto_fast_pages: 200,
            ..Default::default()
        };
        let large = docx_zip_with_pages(Some(358));
        let small = docx_zip_with_pages(Some(46));
        assert!(evaluate(&large, &config).is_some());
        assert!(evaluate(&small, &config).is_none());

        config.auto_fast_pages = 0;
        assert!(evaluate(&large, &config).is_none(), "0 must disable the auto-downgrade");

        // Default threshold keeps the largest corpus document on the normal profile.
        let default_config = ExtractionConfig::default();
        assert_eq!(default_config.auto_fast_pages, 500);
        assert!(evaluate(&large, &default_config).is_none());
    }

    #[test]
    fn warning_message_reports_pages_and_threshold() {
        let decision = AutoDowngrade {
            pages: 358,
            threshold: 200,
        };
        assert_eq!(
            decision.warning_message(),
            "large document (358 pages > auto_fast_pages=200): OCR disabled for speed; \
             set auto_fast_pages=0 to keep full quality"
        );
    }

    #[cfg(all(test, feature = "pdf"))]
    fn build_minimal_pdf_with_n_pages(pages: usize) -> Vec<u8> {
        let mut buf = Vec::<u8>::new();
        buf.extend_from_slice(b"%PDF-1.4\n");
        let obj1 = buf.len();
        buf.extend_from_slice(b"1 0 obj\n<</Type /Catalog /Pages 2 0 R>>\nendobj\n");
        let obj2 = buf.len();
        let kids: Vec<String> = (3..pages + 3).map(|n| format!("{n} 0 R")).collect();
        buf.extend_from_slice(
            format!(
                "2 0 obj\n<</Type /Pages /Kids [{}] /Count {pages}>>\nendobj\n",
                kids.join(" ")
            )
            .as_bytes(),
        );
        let page_offsets: Vec<usize> = (3..pages + 3)
            .map(|n| {
                let offset = buf.len();
                buf.extend_from_slice(
                    format!(
                        "{} 0 obj\n<</Type /Page /MediaBox [0 0 200 200] /Parent 2 0 R>>\nendobj\n",
                        n
                    )
                    .as_bytes(),
                );
                offset
            })
            .collect();
        let xref = buf.len();
        buf.extend_from_slice(b"xref\n");
        buf.extend_from_slice(format!("0 {}\n", pages + 3).as_bytes());
        buf.extend_from_slice(b"0000000000 65535 f \n");
        buf.extend_from_slice(format!("{:010} 00000 n \n", obj1).as_bytes());
        buf.extend_from_slice(format!("{:010} 00000 n \n", obj2).as_bytes());
        for offset in page_offsets {
            buf.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
        }
        buf.extend_from_slice(format!("trailer\n<</Size {} /Root 1 0 R>>\n", pages + 3).as_bytes());
        buf.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        buf
    }

    #[cfg(all(test, feature = "pdf"))]
    #[test]
    fn probe_reads_pdf_structure_page_count() {
        assert_eq!(probe_page_count(&build_minimal_pdf_with_n_pages(3)), Some(3));
    }
}
