use super::*;

struct DocumentProcessingOcrBackend {
    name: String,
    image_call_count: AtomicUsize,
    document_call_count: AtomicUsize,
    supports_doc_override: bool,
}

impl Plugin for DocumentProcessingOcrBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> String {
        "1.0.0".to_string()
    }

    fn initialize(&self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl OcrBackend for DocumentProcessingOcrBackend {
    async fn process_image(&self, _image_bytes: &[u8], _config: &OcrConfig) -> Result<ExtractedDocument> {
        self.image_call_count.fetch_add(1, Ordering::SeqCst);

        use std::borrow::Cow;
        let mut document = ExtractedDocument::default();
        document.content = "Processed via image extraction".to_string();
        document.mime_type = Cow::Borrowed("text/plain");
        Ok(document)
    }

    fn supports_document_processing(&self) -> bool {
        self.supports_doc_override
    }

    async fn process_document(
        &self,
        _document_path: &std::path::Path,
        _config: &OcrConfig,
    ) -> Result<ExtractedDocument> {
        self.document_call_count.fetch_add(1, Ordering::SeqCst);

        use std::borrow::Cow;
        let mut document = ExtractedDocument::default();
        document.content = "Processed natively as document".to_string();
        document.mime_type = Cow::Borrowed("text/plain");
        Ok(document)
    }

    fn supports_language(&self, _lang: &str) -> bool {
        true
    }

    fn backend_type(&self) -> OcrBackendType {
        OcrBackendType::Custom
    }
}

#[serial]
#[test]
fn test_ocr_backend_document_processing_fallback() {
    let _guard = BackendRegistryGuard;
    let test_document = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_documents/pdf/ocr_test.pdf");
    let registry = get_ocr_backend_registry();

    {
        let mut reg = registry.write();
        reg.shutdown_all().expect("Operation failed");
    }

    let backend = Arc::new(DocumentProcessingOcrBackend {
        name: "fallback-ocr".to_string(),
        image_call_count: AtomicUsize::new(0),
        document_call_count: AtomicUsize::new(0),
        supports_doc_override: false,
    });

    {
        let mut reg = registry.write();
        reg.register(Arc::clone(&backend) as Arc<dyn OcrBackend>)
            .expect("Operation failed");
    }

    let ocr_config = OcrConfig {
        backend: "fallback-ocr".to_string(),
        language: vec!["eng".to_string()],
        ..Default::default()
    };

    let config = ExtractionConfig {
        ocr: Some(ocr_config),
        force_ocr: true,
        ..Default::default()
    };

    let result = extract_uri_document_blocking(test_document, None, &config);

    assert!(result.is_ok(), "Extraction failed: {:?}", result.err());

    let extraction_result = result.expect("Operation failed");
    assert!(
        extraction_result.content.contains("Processed via image extraction"),
        "Custom OCR fallback was not used. Content: {}",
        extraction_result.content
    );

    assert!(
        backend.image_call_count.load(Ordering::SeqCst) > 0,
        "OCR fallback to image extraction was not called"
    );
    assert_eq!(
        backend.document_call_count.load(Ordering::SeqCst),
        0,
        "Native process_document was called unexpectedly"
    );

    {
        let mut reg = registry.write();
        reg.shutdown_all().expect("Operation failed");
    }
}

// Exercises the document-level OCR override against a PDF, so it needs the `pdf` feature
// that supplies `pdf_options`. ~keep
#[cfg(feature = "pdf")]
#[serial]
#[test]
fn test_ocr_backend_document_processing_override() {
    let _guard = BackendRegistryGuard;
    let test_document = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_documents/pdf/ocr_test.pdf");
    let registry = get_ocr_backend_registry();

    {
        let mut reg = registry.write();
        reg.shutdown_all().expect("Operation failed");
    }

    let backend = Arc::new(DocumentProcessingOcrBackend {
        name: "override-ocr".to_string(),
        image_call_count: AtomicUsize::new(0),
        document_call_count: AtomicUsize::new(0),
        supports_doc_override: true,
    });

    {
        let mut reg = registry.write();
        reg.register(Arc::clone(&backend) as Arc<dyn OcrBackend>)
            .expect("Operation failed");
    }

    let ocr_config = OcrConfig {
        backend: "override-ocr".to_string(),
        language: vec!["eng".to_string()],
        ..Default::default()
    };

    // Document-level OCR is only taken when the effective page margins are zero: it processes
    // the whole file at once and so cannot crop per-page headers/footers. The defaults are
    // non-zero (0.06 / 0.05), which routes to the per-page image path -- see
    // `should_use_per_page_ocr_only_when_effective_margins_are_nonzero`. Opt out explicitly so
    // this test exercises the document override it is named for. ~keep
    let config = ExtractionConfig {
        ocr: Some(ocr_config),
        force_ocr: true,
        pdf_options: Some(PdfConfig {
            top_margin_fraction: Some(0.0),
            bottom_margin_fraction: Some(0.0),
            ..Default::default()
        }),
        ..Default::default()
    };

    let result = extract_uri_document_blocking(test_document, None, &config);

    assert!(result.is_ok(), "Extraction failed: {:?}", result.err());

    let extraction_result = result.expect("Operation failed");
    assert!(
        extraction_result.content.contains("Processed natively as document"),
        "Custom OCR document override was not used. Content: {}",
        extraction_result.content
    );

    assert_eq!(
        backend.image_call_count.load(Ordering::SeqCst),
        0,
        "process_image was called unexpectedly"
    );
    assert_eq!(
        backend.document_call_count.load(Ordering::SeqCst),
        1,
        "process_document was not called exactly once"
    );

    {
        let mut reg = registry.write();
        reg.shutdown_all().expect("Operation failed");
    }
}

#[serial]
#[test]
fn test_ocr_backend_document_processing_missing_path_fallback() {
    let _guard = BackendRegistryGuard;
    let test_document = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_documents/pdf/ocr_test.pdf");

    let bytes = std::fs::read(test_document).expect("Failed to read test document");

    let backend = std::sync::Arc::new(DocumentProcessingOcrBackend {
        name: "missing-path-ocr".to_string(),
        image_call_count: std::sync::atomic::AtomicUsize::new(0),
        document_call_count: std::sync::atomic::AtomicUsize::new(0),
        supports_doc_override: true,
    });

    {
        let registry = get_ocr_backend_registry();
        let mut reg = registry.write();
        reg.register(std::sync::Arc::clone(&backend) as std::sync::Arc<dyn OcrBackend>)
            .expect("Operation failed");
    }

    let ocr_config = OcrConfig {
        backend: "missing-path-ocr".to_string(),
        language: vec!["eng".to_string()],
        ..Default::default()
    };

    let config = ExtractionConfig {
        ocr: Some(ocr_config),
        force_ocr: true,
        ..Default::default()
    };

    let result = extract_bytes_document_blocking(&bytes, "application/pdf", &config);

    assert!(result.is_ok(), "Extraction failed: {:?}", result.err());

    let extraction_result = result.expect("Operation failed");
    assert!(
        extraction_result.content.contains("Processed via image extraction"),
        "Custom OCR fallback was not used. Content: {}",
        extraction_result.content
    );

    assert!(
        backend.image_call_count.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "OCR fallback to image extraction was not called"
    );
    assert_eq!(
        backend.document_call_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "Native process_document was called unexpectedly on memory bytes"
    );
}
