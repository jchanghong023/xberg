//! Extractor for legacy Visio binary drawings.

use crate::Result;
use crate::core::config::ExtractionConfig;
use crate::core::mime::VISIO_MIME_TYPE;
use crate::extraction::visio::extract_visio_text;
use crate::plugins::{InternalDocumentExtractor, Plugin};
use crate::types::Metadata;
use crate::types::internal::{ElementKind, InternalDocument, InternalElement};
use ahash::AHashMap;
use async_trait::async_trait;
use std::borrow::Cow;

#[cfg_attr(alef, alef(skip))]
/// Native text extractor for Microsoft Visio 97-2013 binary drawings (`.vsd`).
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
        "Native Visio text extraction via OLE/CFB parsing"
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

        let max_stream_size = config.security_limits.clone().unwrap_or_default().max_archive_size;
        let text = {
            #[cfg(feature = "tokio-runtime")]
            if crate::core::batch_mode::is_batch_mode() {
                let content_owned = content.to_vec();
                let span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    let _guard = span.entered();
                    extract_visio_text(&content_owned, max_stream_size)
                })
                .await
                .map_err(|error| {
                    crate::error::XbergError::parsing(format!("Visio extraction task failed: {error}"))
                })??
            } else {
                extract_visio_text(content, max_stream_size)?
            }

            #[cfg(not(feature = "tokio-runtime"))]
            {
                extract_visio_text(content, max_stream_size)?
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
        &[VISIO_MIME_TYPE]
    }

    fn priority(&self) -> i32 {
        60
    }
}
