//! Extractor for Visio drawings (both legacy binary `.vsd` and OPC `.vsdx`).

use crate::Result;
use crate::core::config::ExtractionConfig;
use crate::core::mime::{VISIO_DRAWING_ML_MIME_TYPE, VISIO_MIME_TYPE};
use crate::extraction::visio::{extract_visio_package_text, extract_visio_text};
use crate::extractors::security::SecurityLimits;
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::Metadata;
use crate::types::internal::{ElementKind, InternalDocument, InternalElement};
use ahash::AHashMap;
use async_trait::async_trait;
use std::borrow::Cow;

#[cfg_attr(alef, alef(skip))]
/// Native text extractor for Microsoft Visio drawings: legacy binary `.vsd`
/// (OLE/CFB) and OPC drawing packages `.vsdx`/`.vsdm`.
pub struct VisioExtractor;

impl VisioExtractor {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Default for VisioExtractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for VisioExtractor {
    fn name(&self) -> &str {
        "visio-extractor"
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    fn initialize(&self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    fn description(&self) -> &str {
        "Native Visio text extraction (legacy OLE/CFB `.vsd` and OPC `.vsdx` packages)"
    }

    fn author(&self) -> &str {
        "Xberg Team"
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl InternalDocumentExtractor for VisioExtractor {
    async fn extract_content(
        &self,
        content: &[u8],
        mime_type: &str,
        config: &ExtractionConfig,
    ) -> Result<InternalDocument> {
        if config.cancel_token.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err(crate::error::XbergError::Cancelled);
        }

        let security_limits = config.security_limits.clone().unwrap_or_default();
        let text = {
            #[cfg(feature = "tokio-runtime")]
            if crate::core::batch_mode::is_batch_mode() {
                let content_owned = content.to_vec();
                let limits_owned = security_limits.clone();
                let span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    let _guard = span.entered();
                    extract_visio_drawing(&content_owned, &limits_owned)
                })
                .await
                .map_err(|error| {
                    crate::error::XbergError::parsing(format!("Visio extraction task failed: {error}"))
                })??
            } else {
                extract_visio_drawing(content, &security_limits)?
            }

            #[cfg(not(feature = "tokio-runtime"))]
            {
                extract_visio_drawing(content, &security_limits)?
            }
        };

        let mut document = InternalDocument::new("visio");
        document.mime_type = mime_type.to_string();
        let mut metadata = AHashMap::new();
        metadata.insert(
            Cow::Borrowed("extraction_method"),
            serde_json::Value::String("native_visio".to_string()),
        );
        document.metadata = Metadata {
            additional: metadata,
            ..Default::default()
        };

        for shape_text in text {
            let normalized = shape_text.replace("\r\n", "\n").replace('\r', "\n");
            if normalized.trim().is_empty() {
                continue;
            }
            document.push_element(InternalElement::text(ElementKind::Paragraph, normalized.trim(), 0));
        }

        Ok(document)
    }

    fn supported_mime_types(&self) -> &[&str] {
        &[VISIO_MIME_TYPE, VISIO_DRAWING_ML_MIME_TYPE]
    }

    fn priority(&self) -> i32 {
        60
    }
}

/// Dispatch a Visio drawing to the binary (`.vsd`) or package (`.vsdx`) reader
/// based on the container's magic bytes. The package reader enforces the
/// caller's full `SecurityLimits`; the binary reader needs only the stream-size
/// budget, which it takes from `limits.max_archive_size`.
fn extract_visio_drawing(content: &[u8], limits: &SecurityLimits) -> Result<Vec<String>> {
    if content.starts_with(b"PK\x03\x04") {
        extract_visio_package_text(content, limits)
    } else {
        extract_visio_text(content, limits.max_archive_size)
    }
}
