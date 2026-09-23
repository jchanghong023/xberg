//! Embedded object extraction from OOXML (DOCX/PPTX) archives.
//!
//! OOXML files are ZIP archives that may contain embedded objects in:
//! - DOCX: `word/embeddings/` directory
//! - PPTX: `ppt/embeddings/` directory
//!
//! This module extracts those embedded files, detects their MIME type,
//! and recursively processes them through the extraction pipeline.

use crate::core::config::ExtractionConfig;
use crate::types::{ArchiveEntry, ProcessingWarning};
use std::borrow::Cow;
use std::io::{Cursor, Read};

/// Build a `ProcessingWarning` tagged with this module's conventional `<source_label>_embedded_objects` source.
fn embedded_objects_warning(source_label: &str, message: String) -> ProcessingWarning {
    ProcessingWarning {
        source: Cow::Owned(format!("{}_embedded_objects", source_label)),
        message: Cow::Owned(message),
    }
}

/// Collect ZIP entry names under `embeddings_prefix` (entries whose name is strictly
/// longer than the prefix -- i.e. the directory entry itself is excluded).
fn collect_embedding_names(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, embeddings_prefix: &str) -> Vec<String> {
    (0..archive.len())
        .filter_map(|i| {
            let file = archive.by_index(i).ok()?;
            let name = file.name().to_string();
            if name.starts_with(embeddings_prefix) && name.len() > embeddings_prefix.len() {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

/// Truncate `embedding_names` to `max_files_in_archive`, pushing a warning naming how many
/// entries were dropped when the cap was hit.
fn enforce_max_files_in_archive(
    embedding_names: &mut Vec<String>,
    max_files_in_archive: usize,
    embeddings_prefix: &str,
    source_label: &str,
    warnings: &mut Vec<ProcessingWarning>,
) {
    if embedding_names.len() <= max_files_in_archive {
        return;
    }
    let skipped = embedding_names.len() - max_files_in_archive;
    warnings.push(embedded_objects_warning(
        source_label,
        format!(
            "Skipped {} embedded object(s) under '{}': max_files_in_archive ({}) reached",
            skipped, embeddings_prefix, max_files_in_archive
        ),
    ));
    embedding_names.truncate(max_files_in_archive);
}

/// Upper bound for both the initial allocation hint and the actual read of a single
/// embedded file. `file.size()` (read at the call site) is the *declared* uncompressed size
/// from the ZIP central directory: it is attacker-controlled and is not verified against the
/// real decompressed byte count before we use it. A forged declaration (e.g. a
/// multi-terabyte value backed by a few bytes of real compressed data) must not
/// translate into an equally large `Vec::with_capacity` call, which allocates before a
/// single byte is read.
///
/// Prefers the caller's configured `max_embedded_file_bytes` (default 50 MiB, see
/// `ExtractionConfig::default_max_embedded_file_bytes`) since that is the limit this
/// module already enforces on the *actual* extracted size below -- one cap governs
/// both the hint and the acceptance check. If the caller has explicitly disabled the
/// per-file cap (`None`), fall back to the archive-wide `SecurityLimits::max_archive_size`
/// (default 500 MiB) as a hard backstop: no single embedded member should be allowed to
/// force a larger up-front allocation than the whole-archive budget the caller already
/// agreed to.
fn embedded_file_capacity_cap(
    config: &ExtractionConfig,
    security_limits: &crate::extractors::security::SecurityLimits,
) -> u64 {
    config
        .max_embedded_file_bytes
        .unwrap_or(security_limits.max_archive_size as u64)
}

/// Clamp an untrusted declared size to at most `cap` bytes.
///
/// `declared` is meant to be a size read straight from archive metadata the caller does not
/// control (e.g. a ZIP central-directory uncompressed-size field), so it must never be used
/// as-is to size an allocation: a forged multi-terabyte declaration would otherwise translate
/// directly into an equally large `Vec::with_capacity` request before a single byte is read.
/// Pulled out as its own function so the clamp itself -- not just its effect once wired into
/// the extraction loop -- has a direct, allocation-free unit test.
fn clamp_declared_size(declared: u64, cap: u64) -> u64 {
    declared.min(cap)
}

/// Append non-empty embedded-object content into the parent document body.
///
/// `extract_ooxml_embedded_objects` only attaches children on
/// [`crate::types::internal::InternalDocument::children`]. Markdown/`content`
/// is rendered from `elements`, so a successfully extracted legacy Word OLE
/// (and any other embedded document with text) was searchable in JSON children
/// but invisible in the Markdown the CLI writes. Email already merges
/// attachment text into the body the same way; this mirrors that for OOXML.
///
/// Each child contributes a plain-text caption and its own Markdown as a raw
/// block, with `](image_N.ext)` references renumbered onto the parent's image
/// table (see [`renumber_embedded_image_refs`]). A raw block — rather than
/// heading + paragraph — keeps the child's own structure: a paragraph element
/// flattens every line break into one line and escapes the child's Markdown
/// markers, which turned an embedded spreadsheet's tables into a single
/// 78k-character paragraph of `\#`-escaped text.
///
/// Children stay on `children` for structured consumers. Graphical OLE
/// payloads that never identified as a document never become children and are
/// unaffected.
pub(crate) fn append_embedded_object_text(document: &mut crate::types::internal::InternalDocument) {
    use crate::types::internal::{ElementKind, InternalElement};

    let Some(children) = document.children.as_ref() else {
        return;
    };
    // Collect first: pushing elements (and images) needs a mutable borrow of
    // `document` while children still borrow it immutably.
    let mut staged_images = Vec::new();
    let mut merged: Vec<(String, String)> = Vec::new();
    // Next free slot in the parent's image table. Advanced by one past the
    // highest child index rather than by the child's image count, so a child
    // whose indices are not dense cannot collide with the next child's range.
    let mut next_image_base = document.images.len() as u32;
    for child in children {
        let content = child.result.content.trim();
        if content.is_empty() {
            continue;
        }
        let title = child
            .path
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(child.path.as_str())
            .to_string();
        // `images` is optional on an extraction result: a caller that asked for no
        // image data (or an extractor that produces none) leaves it empty, and the
        // body's references then simply keep their alt text.
        let child_images: &[crate::types::ExtractedImage] = child.result.images.as_deref().unwrap_or_default();
        let span = child_images
            .iter()
            .map(|image| image.image_index)
            .max()
            .map_or(0, |highest| highest.saturating_add(1));
        let (body, referenced) = renumber_embedded_image_refs(content, next_image_base, child_images);
        if body.trim().is_empty() {
            // Nothing of the child's body survives (e.g. it was only image
            // references whose alt text was empty): advancing the base and
            // staging the images would export files the body never refers to.
            // The images stay on the child for structured consumers.
            continue;
        }
        for image in child_images {
            // Stage only the images the rewritten body actually points at: a
            // child asset its own Markdown never referenced would otherwise be
            // exported as an orphan file next to the parent document. The full
            // set stays on the child for structured consumers.
            if !referenced.contains(&image.image_index) {
                continue;
            }
            let mut image = image.clone();
            image.image_index = next_image_base.saturating_add(image.image_index);
            staged_images.push(image);
        }
        next_image_base = next_image_base.saturating_add(span);
        merged.push((title, body));
    }
    document.images.extend(staged_images);
    for (title, content) in merged {
        if content.trim().is_empty() {
            continue;
        }
        // A filename is not a section of the host document: emit it as a plain
        // caption so the host's outline keeps only real headings, and keep the
        // child body as a raw block so its own Markdown (headings, tables,
        // lists, code) survives verbatim instead of being escaped into one
        // flattened paragraph.
        let caption = InternalElement::text(ElementKind::Paragraph, format!("Embedded object: {title}"), 0);
        document.push_element(caption);
        let raw = InternalElement::text(ElementKind::RawBlock, format!("\n{content}\n"), 0);
        document.push_element(raw);
    }
}

/// Copy a nested document's Markdown into the parent body, moving its
/// `](image_N.ext)` references onto the parent's image table.
///
/// A child extractor names its images by its own `ExtractedImage::image_index`
/// and exports them beside itself; the parent only ever writes *its* image
/// table, so a reference left as-is would resolve to an unrelated picture of
/// the same number (or to nothing at all). `base` is the parent slot the
/// child's image 0 was moved to. A reference with no matching child image is
/// dropped, keeping its alt text — the same rule the export path applies to
/// assets it cannot carry over. Returns the rewritten body and the child image
/// indices the body still points at, so the caller stages exactly the assets
/// the merged output references.
fn renumber_embedded_image_refs(text: &str, base: u32, images: &[crate::types::ExtractedImage]) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(text.len());
    let mut referenced: Vec<u32> = Vec::new();
    let bytes = text.as_bytes();
    // Fenced code is literal text: an `image_N` example inside a fence keeps
    // its numbering and stages nothing, the same contract the pipeline's own
    // rewriters (`rewrite_content_image_extensions`) apply via `FenceTracker`.
    let mut fences = crate::extraction::markdown_utils::FenceTracker::default();
    let mut fenced: Vec<(usize, usize)> = Vec::new();
    let mut line_offset = 0usize;
    for line in text.split_inclusive('\n') {
        // `FenceTracker` expects a bare line: a closer is only "marker run,
        // nothing else", so the `\n`/`\r\n` terminator must be stripped here
        // the way the pipeline's and renderer's callers strip it — a closer
        // carrying its `\n` would never be recognized and the first fence
        // would swallow the rest of the text. The ranges below still cover
        // the raw line including its terminator.
        let bare = line
            .strip_suffix('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .unwrap_or(line);
        if fences.fenced(bare) {
            fenced.push((line_offset, line_offset + line.len()));
        }
        line_offset += line.len();
    }
    let mut fenced_at = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        // The ranges are line-aligned and `i` only ever grows, so a monotonic
        // cursor answers "is this byte inside a fenced line?" without rescanning.
        while fenced_at < fenced.len() && fenced[fenced_at].1 <= i {
            fenced_at += 1;
        }
        let in_fenced_range = fenced_at < fenced.len() && fenced[fenced_at].0 <= i;
        if bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[' && !in_fenced_range {
            // A writer-escaped literal (`\![a](b)` renders as literal text) is
            // not an opener: an odd run of backslashes before the `!` unescapes
            // to a literal `!` under CommonMark, so the two bytes pass through
            // and the scan resumes after them.
            let mut backslashes = 0usize;
            while backslashes < i && bytes[i - 1 - backslashes] == b'\\' {
                backslashes += 1;
            }
            if backslashes % 2 == 1 {
                out.push_str(&text[i..i + 2]);
                i += 2;
                continue;
            }
            match find_markdown_image_parts(&text[i..]) {
                ImageScan::Parts(alt, target, after) => {
                    // A candidate whose span runs into a fenced line is not a
                    // real reference — its `)` belongs to fence content — and
                    // consuming it whole would swallow the fence's opener:
                    // degrade to literal bytes the way Malformed does.
                    let end = i + after;
                    if fenced_at < fenced.len() && fenced[fenced_at].0 < end {
                        out.push_str(&text[i..i + 2]);
                        i += 2;
                        continue;
                    }
                    if target.starts_with("data:") {
                        // Self-contained payload: nothing to renumber, and dropping
                        // it would discard the only copy of the picture.
                        out.push_str(&text[i..i + after]);
                    } else if let Some((index, suffix)) = parse_image_ref(target)
                        && images.iter().any(|image| image.image_index == index)
                    {
                        out.push_str("![");
                        out.push_str(alt);
                        out.push_str("](image_");
                        out.push_str(&base.saturating_add(index).to_string());
                        out.push_str(suffix);
                        out.push(')');
                        referenced.push(index);
                    } else if !alt.is_empty() {
                        // Not one of this child's exported images: keep the alt text
                        // rather than emit a reference that would resolve to an
                        // unrelated picture of the same number in the parent.
                        out.push_str(alt);
                    }
                    i += after;
                    continue;
                }
                // No unescaped closing bracket or paren follows anywhere: no image
                // reference can start later in this text either, so copying the
                // rest verbatim reproduces the byte-for-byte output of rescanning
                // — without paying an end-of-text scan per stray opener on a body
                // full of unclosed `![`.
                ImageScan::NoTerminator => {
                    out.push_str(&text[i..]);
                    break;
                }
                // The closing bracket exists but is not followed by `(`: this
                // opener is not an image, but later ones may still be.
                ImageScan::Malformed => {
                    out.push_str(&text[i..i + 2]);
                    i += 2;
                    continue;
                }
            }
        }
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    (out, referenced)
}

/// Split an `image_N.ext` reference target into its index and its `.ext` suffix.
fn parse_image_ref(target: &str) -> Option<(u32, &str)> {
    let rest = target.strip_prefix("image_")?;
    let digits_len = rest.chars().take_while(char::is_ascii_digit).count();
    if digits_len == 0 || !rest[digits_len..].starts_with('.') {
        return None;
    }
    let index = rest[..digits_len].parse().ok()?;
    Some((index, &rest[digits_len..]))
}

/// Outcome of scanning one `![` opener.
enum ImageScan<'a> {
    /// A well-formed `![alt](target)`; the index is relative to the opener's slice.
    Parts(&'a str, &'a str, usize),
    /// No unescaped closing bracket (or, past a `(`, closing paren) follows
    /// anywhere in the rest of the text — no image reference can start later.
    NoTerminator,
    /// A closing bracket exists but is not followed by `(`: this opener is not
    /// an image, but later ones may still be well-formed.
    Malformed,
}

/// Scan a slice starting at `![` for a well-formed image reference.
///
/// The scan skips backslash-escaped characters: the CommonMark writer escapes a
/// literal `]` in alt text and a literal `)` in a target as `\]`/`\)`, and
/// stopping at those folded an escaped reference's parse and left the child's
/// numbering in the parent's body.
///
/// A closer search never crosses the next unescaped `![` opener: an opener
/// whose `)` never comes (malformed text before a later, well-formed
/// reference) would otherwise swallow that reference — its parse fails, the
/// span is replaced by the outer alt text, and the later image silently loses
/// its reference and its staged file. CommonMark recovers at the next opener;
/// so does this scan, by reporting the opener as [`ImageScan::Malformed`]
/// (two bytes consumed) and letting the loop rescan from there.
fn find_markdown_image_parts(s: &str) -> ImageScan<'_> {
    debug_assert!(s.starts_with("!["));
    let rest = &s[2..];
    let Some(close_bracket) = find_unescaped(rest, b']') else {
        return ImageScan::NoTerminator;
    };
    let alt = &rest[..close_bracket];
    let after = &rest[close_bracket + 1..];
    if !after.starts_with('(') {
        return ImageScan::Malformed;
    }
    let Some(close_paren) = find_unescaped(after, b')') else {
        return ImageScan::NoTerminator;
    };
    if let Some(next_opener) = find_unescaped_opener(after)
        && next_opener < close_paren
    {
        return ImageScan::Malformed;
    }
    ImageScan::Parts(alt, &after[1..close_paren], 2 + close_bracket + 1 + close_paren + 1)
}

/// Index of the first unescaped `![` in `s`, or `None`.
fn find_unescaped_opener(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut from = 0usize;
    loop {
        let bang = find_unescaped(&s[from..], b'!')? + from;
        let next = bang + 1;
        if next < bytes.len() && bytes[next] == b'[' {
            return Some(bang);
        }
        from = next;
    }
}

/// Index of the first unescaped `needle` byte in `s`. A `\` escapes the character
/// after it, which is how the CommonMark writer emits a literal bracket.
fn find_unescaped(s: &str, needle: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Extract embedded objects from an OOXML ZIP archive and recursively process them.
///
/// Scans the given `embeddings_prefix` directory (e.g. `word/embeddings/` or
/// `ppt/embeddings/`) inside the ZIP archive for embedded files. Known formats
/// (.xlsx, .pdf, .docx, .pptx, etc.) are recursively extracted. OLE compound
/// files (oleObject*.bin) are skipped with a warning unless their format can be
/// identified.
///
/// Returns `(children, warnings)` suitable for attaching to `InternalDocument`.
pub(crate) async fn extract_ooxml_embedded_objects(
    zip_bytes: &[u8],
    embeddings_prefix: &str,
    source_label: &str,
    config: &ExtractionConfig,
) -> (Vec<ArchiveEntry>, Vec<ProcessingWarning>) {
    let mut children = Vec::new();
    let mut warnings = Vec::new();

    let cursor = Cursor::new(zip_bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(_) => return (children, warnings),
    };

    let mut embedding_names = collect_embedding_names(&mut archive, embeddings_prefix);
    // A malformed archive can list the same embeddings path twice; `by_name`
    // would return the same entry for both and the object's text would land in
    // the output twice. Same hygiene as the xlsx media walk.
    embedding_names.sort_unstable();
    embedding_names.dedup();
    if embedding_names.is_empty() {
        return (children, warnings);
    }

    let security_limits = config.security_limits.clone().unwrap_or_default();
    enforce_max_files_in_archive(
        &mut embedding_names,
        security_limits.max_files_in_archive,
        embeddings_prefix,
        source_label,
        &mut warnings,
    );

    if config.max_archive_depth == 0 {
        warnings.push(embedded_objects_warning(
            source_label,
            format!(
                "Skipped {} embedded object(s) under '{}': max_archive_depth reached",
                embedding_names.len(),
                embeddings_prefix
            ),
        ));
        return (children, warnings);
    }

    let mut child_config = config.clone();
    child_config.max_archive_depth = config.max_archive_depth.saturating_sub(1);

    let embedded_capacity_cap = embedded_file_capacity_cap(config, &security_limits);

    for entry_name in &embedding_names {
        let (child, mut entry_warnings) = process_embedded_entry(
            &mut archive,
            entry_name,
            embeddings_prefix,
            source_label,
            embedded_capacity_cap,
            &child_config,
        )
        .await;
        warnings.append(&mut entry_warnings);
        if let Some(child) = child {
            children.push(child);
        }
    }

    (children, warnings)
}

/// Read, classify, and recursively extract a single embedded-object archive entry.
///
/// Returns the resulting `ArchiveEntry` (`None` when the entry was skipped or failed) plus
/// any warnings raised along the way, mirroring the original inline `for` loop body of
/// `extract_ooxml_embedded_objects` one entry at a time.
async fn process_embedded_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    entry_name: &str,
    embeddings_prefix: &str,
    source_label: &str,
    embedded_capacity_cap: u64,
    child_config: &ExtractionConfig,
) -> (Option<ArchiveEntry>, Vec<ProcessingWarning>) {
    let mut warnings = Vec::new();
    let filename = entry_name
        .strip_prefix(embeddings_prefix)
        .unwrap_or(entry_name)
        .to_string();

    let Some(data) = read_embedded_entry_bytes(
        archive,
        entry_name,
        &filename,
        embedded_capacity_cap,
        source_label,
        &mut warnings,
    ) else {
        return (None, warnings);
    };

    let is_ole_binary = data.len() >= 4 && data[0..4] == [0xD0, 0xCF, 0x11, 0xE0];
    let ole_offset = if is_ole_binary {
        Some(0)
    } else if filename.to_ascii_lowercase().starts_with("oleobject") {
        embedded_payload_start(&data).filter(|&offset| {
            data.get(offset..)
                .is_some_and(|payload| payload.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]))
        })
    } else {
        None
    };
    let child = if let Some(ole_offset) = ole_offset {
        extract_ole_entry(&data[ole_offset..], filename, embedded_capacity_cap, child_config, source_label, &mut warnings)
            .await
    } else {
        extract_regular_entry(&data, filename, child_config, source_label, &mut warnings).await
    };

    (child, warnings)
}

/// Read one archive entry's bytes, enforcing `embedded_capacity_cap` on both the
/// allocation hint and the actual read, and skipping (with a pushed warning where
/// applicable) an unreadable, empty, or oversized entry.
fn read_embedded_entry_bytes(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    entry_name: &str,
    filename: &str,
    embedded_capacity_cap: u64,
    source_label: &str,
    warnings: &mut Vec<ProcessingWarning>,
) -> Option<Vec<u8>> {
    let data = match archive.by_name(entry_name) {
        Ok(file) => {
            // `file.size()` is attacker-controlled declared metadata (see the comment
            // on `embedded_capacity_cap` above); clamp the allocation hint so a forged
            // value cannot force an immediate huge allocation. `Vec::with_capacity` is
            // only a hint -- it does not by itself bound how far `read_to_end` can grow
            // the buffer -- so the read itself is bounded via `.take()` below too.
            let capacity_hint = clamp_declared_size(file.size(), embedded_capacity_cap) as usize;
            let mut buf = Vec::with_capacity(capacity_hint);
            // Read at most one byte past the cap: this lets the size check below still
            // detect and report an oversized entry (it observes `cap + 1` bytes), while
            // guaranteeing `buf` itself can never grow past `embedded_capacity_cap + 1`
            // regardless of what the archive's central directory claims or what the
            // entry actually decompresses to.
            let read_cap = embedded_capacity_cap.saturating_add(1);
            if file.take(read_cap).read_to_end(&mut buf).is_err() {
                warnings.push(embedded_objects_warning(
                    source_label,
                    format!("Failed to read embedded file '{}'", filename),
                ));
                return None;
            }
            buf
        }
        Err(_) => return None,
    };

    if data.is_empty() {
        return None;
    }

    if data.len() as u64 > embedded_capacity_cap {
        warnings.push(embedded_objects_warning(
            source_label,
            format!(
                "Skipped embedded file '{}': size {} bytes exceeds cap {} bytes",
                filename,
                data.len(),
                embedded_capacity_cap
            ),
        ));
        return None;
    }

    Some(data)
}

/// Unwrap and recursively extract an OLE (CFB) compound-file embedded object.
async fn extract_ole_entry(
    data: &[u8],
    filename: String,
    embedded_capacity_cap: u64,
    child_config: &ExtractionConfig,
    source_label: &str,
    warnings: &mut Vec<ProcessingWarning>,
) -> Option<ArchiveEntry> {
    match extract_ole_embedded_object(data, &filename, embedded_capacity_cap) {
        Some((inner_bytes, inner_mime)) => {
            match crate::core::extractor::extract_bytes(&inner_bytes, &inner_mime, child_config).await {
                Ok(result) => Some(ArchiveEntry {
                    path: filename,
                    mime_type: inner_mime,
                    result: Box::new(result),
                }),
                Err(e) => {
                    warnings.push(embedded_objects_warning(
                        source_label,
                        format!("Failed to extract embedded OLE object '{}': {}", filename, e),
                    ));
                    None
                }
            }
        }
        None => {
            warnings.push(embedded_objects_warning(
                source_label,
                format!(
                    "Skipped OLE compound file '{}': format identification not supported",
                    filename
                ),
            ));
            None
        }
    }
}

/// Detect a non-OLE embedded entry's MIME type and recursively extract it.
async fn extract_regular_entry(
    data: &[u8],
    filename: String,
    child_config: &ExtractionConfig,
    source_label: &str,
    warnings: &mut Vec<ProcessingWarning>,
) -> Option<ArchiveEntry> {
    let detected_mime = crate::core::mime::detect_mime_type_from_bytes(data).ok().or_else(|| {
        std::path::Path::new(&filename)
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| mime_guess::from_ext(ext).first())
            .map(|m| m.to_string())
    });

    let file_mime = match detected_mime {
        Some(m) if m != "application/octet-stream" => m,
        _ => {
            warnings.push(embedded_objects_warning(
                source_label,
                format!(
                    "Skipped embedded file '{}': MIME type could not be determined",
                    filename
                ),
            ));
            return None;
        }
    };

    match crate::core::extractor::extract_bytes(data, &file_mime, child_config).await {
        Ok(result) => Some(ArchiveEntry {
            path: filename,
            mime_type: file_mime,
            result: Box::new(result),
        }),
        Err(e) => {
            warnings.push(embedded_objects_warning(
                source_label,
                format!("Failed to extract embedded '{}': {}", filename, e),
            ));
            None
        }
    }
}

/// Attempt to identify and unwrap an OLE (CFB) compound-file embedded object.
///
/// OLE embeds modern packages in a `Package` stream, native files in an
/// `Ole10Native` stream, and legacy Office/Visio documents in their own root
/// streams. Stream names are not consistently rooted or cased across
/// producers, so discovery also walks the bounded stream list.
///
/// Returns `None` when the container can't be opened or none of the supported
/// streams contains a recognizable payload.
#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
pub(crate) fn extract_ole_embedded_object(data: &[u8], source_name: &str, max_bytes: u64) -> Option<(Vec<u8>, String)> {
    let mut compound_file = cfb::CompoundFile::open(Cursor::new(data)).ok()?;
    let stream_paths = collect_ole_stream_paths(&compound_file);

    let native_names = ["/\x01Ole10Native", "\x01Ole10Native", "Ole10Native"];
    if let Some(native) = read_ole_stream(&mut compound_file, &stream_paths, &native_names, max_bytes) {
        if let Some((payload, name_hint)) = parse_ole10_native(&native)
            && let Some(result) = identify_ole_payload(payload, name_hint.as_deref().or(Some(source_name)), max_bytes)
        {
            return Some(result);
        }
        if let Some(start) = embedded_payload_start(&native)
            && let Some(payload) = native.get(start..)
            && let Some(result) = identify_ole_payload(payload.to_vec(), Some(source_name), max_bytes)
        {
            return Some(result);
        }
    }

    let package_names = ["Package", "/Package"];
    if let Some(package) = read_ole_stream(&mut compound_file, &stream_paths, &package_names, max_bytes) {
        if let Some((payload, name_hint)) = parse_ole_package(&package)
            && let Some(result) = identify_ole_payload(payload, name_hint.as_deref().or(Some(source_name)), max_bytes)
        {
            return Some(result);
        }
        if let Some(result) = identify_ole_payload(package, Some(source_name), max_bytes) {
            return Some(result);
        }
    }

    let legacy_mime = if has_ole_stream(&compound_file, &stream_paths, &["VisioDocument", "/VisioDocument"]) {
        Some(crate::core::mime::VISIO_MIME_TYPE)
    } else if has_ole_stream(&compound_file, &stream_paths, &["WordDocument", "/WordDocument"]) {
        Some(crate::core::mime::LEGACY_WORD_MIME_TYPE)
    } else if has_ole_stream(
        &compound_file,
        &stream_paths,
        &["PowerPoint Document", "/PowerPoint Document"],
    ) {
        Some(crate::core::mime::LEGACY_POWERPOINT_MIME_TYPE)
    } else if has_ole_stream(&compound_file, &stream_paths, &["Workbook", "/Workbook"])
        || has_ole_stream(&compound_file, &stream_paths, &["Book", "/Book"])
    {
        Some("application/vnd.ms-excel")
    } else {
        None
    };
    if let Some(mime) = legacy_mime {
        return Some((data.to_vec(), mime.to_string()));
    }

    // Some producers omit the conventional root stream name and only leave a
    // `\x01CompObj` class descriptor. Use that descriptor to classify the
    // complete CFB, so the native extractor still receives the container. The
    // descriptor names the *editing application*, not the container layout: one
    // such object claimed Visio while carrying no `VisioDocument` stream, and
    // handing it to the Visio reader only produced a dead-end warning. Trust the
    // descriptor only when the native stream it implies is really there;
    // otherwise the signature scan below still gets the actual streams.
    let compobj_names = ["\x01CompObj", "/\x01CompObj", "CompObj"];
    if let Some(compobj) = read_ole_stream(&mut compound_file, &stream_paths, &compobj_names, max_bytes)
        && let Some(mime) = classify_ole_program(&compobj)
        && native_stream_present(&compound_file, &stream_paths, mime)
    {
        return Some((data.to_vec(), mime.to_string()));
    }

    // A few wrappers store a recognizable payload in a non-standard stream.
    // Only inspect streams carrying a file signature; property streams cannot
    // be mistaken for arbitrary text or metadata.
    for path in &stream_paths {
        if ole_path_matches(
            path,
            &[
                "/\x01Ole10Native",
                "\x01Ole10Native",
                "Ole10Native",
                "Package",
                "/Package",
                "\x01CompObj",
                "/\x01CompObj",
                "CompObj",
            ],
        ) {
            continue;
        }
        let Some(stream) = read_ole_stream_path(&mut compound_file, path, max_bytes) else {
            continue;
        };
        if !has_embedded_payload_signature(&stream) {
            continue;
        }
        if let Some(result) = identify_ole_payload(stream, Some(source_name), max_bytes) {
            return Some(result);
        }
    }

    None
}

/// Whether the legacy root stream a legacy MIME type implies actually exists.
///
/// A native Office container is identified by its root stream (`VisioDocument`,
/// `WordDocument`, …); a class descriptor alone does not make the container
/// readable by the matching extractor.
#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn native_stream_present<F: Read + std::io::Seek>(
    compound_file: &cfb::CompoundFile<F>,
    stream_paths: &[std::path::PathBuf],
    mime: &str,
) -> bool {
    match mime {
        crate::core::mime::VISIO_MIME_TYPE => {
            has_ole_stream(compound_file, stream_paths, &["VisioDocument", "/VisioDocument"])
        }
        crate::core::mime::LEGACY_WORD_MIME_TYPE => {
            has_ole_stream(compound_file, stream_paths, &["WordDocument", "/WordDocument"])
        }
        crate::core::mime::LEGACY_POWERPOINT_MIME_TYPE => has_ole_stream(
            compound_file,
            stream_paths,
            &["PowerPoint Document", "/PowerPoint Document"],
        ),
        "application/vnd.ms-excel" => {
            has_ole_stream(compound_file, stream_paths, &["Workbook", "/Workbook"])
                || has_ole_stream(compound_file, stream_paths, &["Book", "/Book"])
        }
        _ => true,
    }
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn collect_ole_stream_paths<F: Read + std::io::Seek>(compound_file: &cfb::CompoundFile<F>) -> Vec<std::path::PathBuf> {
    compound_file
        .walk()
        .filter(|entry| entry.is_stream())
        .take(256)
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn ole_path_matches(path: &std::path::Path, names: &[&str]) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.trim_start_matches('/');
    names.iter().any(|name| {
        name.rsplit('/')
            .next()
            .unwrap_or(name)
            .trim_start_matches('/')
            .eq_ignore_ascii_case(file_name)
    })
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_ole_stream(
    compound_file: &mut cfb::CompoundFile<Cursor<&[u8]>>,
    stream_paths: &[std::path::PathBuf],
    names: &[&str],
    max_bytes: u64,
) -> Option<Vec<u8>> {
    for name in names {
        if let Some(data) = read_ole_stream_named(compound_file, name, max_bytes) {
            return Some(data);
        }
    }
    for path in stream_paths {
        if ole_path_matches(path, names)
            && let Some(data) = read_ole_stream_path(compound_file, path, max_bytes)
        {
            return Some(data);
        }
    }
    None
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_ole_stream_named(
    compound_file: &mut cfb::CompoundFile<Cursor<&[u8]>>,
    name: &str,
    max_bytes: u64,
) -> Option<Vec<u8>> {
    let stream = compound_file.open_stream(name).ok()?;
    read_bounded_ole_stream(stream, max_bytes)
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_ole_stream_path(
    compound_file: &mut cfb::CompoundFile<Cursor<&[u8]>>,
    path: &std::path::Path,
    max_bytes: u64,
) -> Option<Vec<u8>> {
    let stream = compound_file.open_stream(path).ok()?;
    read_bounded_ole_stream(stream, max_bytes)
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_bounded_ole_stream<R: Read>(stream: R, max_bytes: u64) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    if stream.take(max_bytes.saturating_add(1)).read_to_end(&mut data).is_ok() && data.len() as u64 <= max_bytes {
        return (!data.is_empty()).then_some(data);
    }
    None
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn has_ole_stream<F: Read + std::io::Seek>(
    compound_file: &cfb::CompoundFile<F>,
    stream_paths: &[std::path::PathBuf],
    names: &[&str],
) -> bool {
    names.iter().any(|name| compound_file.exists(name)) || stream_paths.iter().any(|path| ole_path_matches(path, names))
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn identify_ole_payload(mut payload: Vec<u8>, name_hint: Option<&str>, max_bytes: u64) -> Option<(Vec<u8>, String)> {
    if payload.is_empty() {
        return None;
    }

    if let Some(mime) = identify_ole_container_mime(&payload, max_bytes) {
        return Some((payload, mime.to_string()));
    }

    let detected = crate::core::mime::detect_mime_type_from_bytes(&payload)
        .ok()
        .filter(|mime| mime != "application/octet-stream");
    if let Some(detected) = detected {
        let mime = crate::core::mime::validate_mime_type(&detected).ok()?;
        return Some((std::mem::take(&mut payload), mime));
    }

    // `Package` and native wrappers may prepend a small header before the
    // actual file. Strip only up to the first bounded, known file signature;
    // this avoids guessing offsets for arbitrary binary data. Do this before
    // consulting the filename or class descriptor, because both can describe
    // the wrapper rather than the bytes that must be handed to the extractor.
    if let Some(start) = embedded_payload_start(&payload)
        && start > 0
    {
        let candidate = payload.get(start..)?.to_vec();
        return identify_ole_payload(candidate, name_hint, max_bytes);
    }

    let detected = name_hint
        .and_then(|name| std::path::Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .and_then(|extension| mime_guess::from_ext(extension).first())
        .map(|mime| mime.to_string())
        .filter(|mime| mime != "application/octet-stream")
        .or_else(|| classify_ole_program(&payload).map(str::to_string))?;
    let mime = crate::core::mime::validate_mime_type(&detected).ok()?;
    Some((std::mem::take(&mut payload), mime))
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn identify_ole_container_mime(data: &[u8], max_bytes: u64) -> Option<&'static str> {
    if !data.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return None;
    }
    let compound_file = cfb::CompoundFile::open(Cursor::new(data)).ok()?;
    let stream_paths = collect_ole_stream_paths(&compound_file);
    if has_ole_stream(&compound_file, &stream_paths, &["VisioDocument", "/VisioDocument"]) {
        return Some(crate::core::mime::VISIO_MIME_TYPE);
    }
    if has_ole_stream(&compound_file, &stream_paths, &["WordDocument", "/WordDocument"]) {
        return Some(crate::core::mime::LEGACY_WORD_MIME_TYPE);
    }
    if has_ole_stream(
        &compound_file,
        &stream_paths,
        &["PowerPoint Document", "/PowerPoint Document"],
    ) {
        return Some(crate::core::mime::LEGACY_POWERPOINT_MIME_TYPE);
    }
    if has_ole_stream(&compound_file, &stream_paths, &["Workbook", "/Workbook"])
        || has_ole_stream(&compound_file, &stream_paths, &["Book", "/Book"])
    {
        return Some("application/vnd.ms-excel");
    }

    let compobj_names = ["\x01CompObj", "/\x01CompObj", "CompObj"];
    let mut compound_file = compound_file;
    let compobj = read_ole_stream(&mut compound_file, &stream_paths, &compobj_names, max_bytes)?;
    let mime = classify_ole_program(&compobj)?;
    // Same guard as the container-level CompObj branch: a descriptor naming an
    // application whose native root stream is absent must not label the CFB.
    native_stream_present(&compound_file, &stream_paths, mime).then_some(mime)
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn classify_ole_program(data: &[u8]) -> Option<&'static str> {
    if contains_ole_text(data, b"microsoft excel")
        || contains_ole_text(data, b"excel.sheet")
        || contains_ole_text(data, b"excel worksheet")
    {
        return Some("application/vnd.ms-excel");
    }
    if contains_ole_text(data, b"microsoft word")
        || contains_ole_text(data, b"word.document")
        || contains_ole_text(data, b"word document")
    {
        return Some(crate::core::mime::LEGACY_WORD_MIME_TYPE);
    }
    if contains_ole_text(data, b"microsoft powerpoint")
        || contains_ole_text(data, b"powerpoint.presentation")
        || contains_ole_text(data, b"powerpoint presentation")
    {
        return Some(crate::core::mime::LEGACY_POWERPOINT_MIME_TYPE);
    }
    if contains_ole_text(data, b"microsoft visio")
        || contains_ole_text(data, b"visio.drawing")
        || contains_ole_text(data, b"visio drawing")
    {
        return Some(crate::core::mime::VISIO_MIME_TYPE);
    }
    None
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn contains_ole_text(data: &[u8], needle: &[u8]) -> bool {
    let scan = &data[..data.len().min(64 * 1024)];
    scan.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(actual, expected)| actual.to_ascii_lowercase() == *expected)
    }) || scan.windows(needle.len() * 2).any(|window| {
        window
            .chunks_exact(2)
            .zip(needle)
            .all(|(pair, expected)| pair[0].to_ascii_lowercase() == *expected && pair[1] == 0)
    })
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn has_embedded_payload_signature(data: &[u8]) -> bool {
    embedded_payload_start(data).is_some()
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn embedded_payload_start(data: &[u8]) -> Option<usize> {
    [
        &[0xD0, 0xCF, 0x11, 0xE0][..],
        &[0x50, 0x4B, 0x03, 0x04][..],
        b"%PDF-",
        &[0x89, 0x50, 0x4E, 0x47][..],
        &[0xFF, 0xD8, 0xFF][..],
        b"GIF8",
        b"{\\rtf",
    ]
    .iter()
    .filter_map(|signature| data.windows(signature.len()).position(|window| window == *signature))
    .min()
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn parse_ole10_native(data: &[u8]) -> Option<(Vec<u8>, Option<String>)> {
    let parsed = (|| {
        let mut offset = 4usize;
        let _native_data_size = read_u32_le(data, 0)?;
        let _flags = read_u16_le(data, offset)?;
        offset += 2;
        let filename = read_ole_c_string(data, &mut offset)?;
        let _source_path = read_ole_c_string(data, &mut offset)?;
        offset = offset.checked_add(8)?;
        let _temporary_path = read_ole_c_string(data, &mut offset)?;
        let data_len = read_u32_le(data, offset)? as usize;
        offset += 4;
        let end = offset.checked_add(data_len)?;
        let payload = data.get(offset..end)?;
        (!payload.is_empty()).then(|| (payload.to_vec(), (!filename.is_empty()).then_some(filename)))
    })();
    if parsed.is_some() {
        return parsed;
    }

    let start = embedded_payload_start(data)?;
    let payload = data.get(start..)?;
    (!payload.is_empty()).then(|| (payload.to_vec(), None))
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn parse_ole_package(data: &[u8]) -> Option<(Vec<u8>, Option<String>)> {
    for base in [0usize, 4] {
        let Some(mut offset) = base.checked_add(4) else {
            continue;
        };
        if read_u32_le(data, base).is_none() {
            continue;
        }
        let Some(label) = read_ole_c_string(data, &mut offset) else {
            continue;
        };
        let Some(original_path) = read_ole_c_string(data, &mut offset) else {
            continue;
        };
        let Some(after_format) = offset.checked_add(4) else {
            continue;
        };
        if read_u32_le(data, after_format).is_none() {
            continue;
        }
        let Some(mut offset) = after_format.checked_add(4) else {
            continue;
        };
        if read_u32_le(data, offset).is_none() {
            continue;
        }
        offset += 4;
        if read_ole_c_string(data, &mut offset).is_none() {
            continue;
        }
        let Some(data_len) = read_u32_le(data, offset).map(|length| length as usize) else {
            continue;
        };
        let Some(payload_start) = offset.checked_add(4) else {
            continue;
        };
        let Some(payload_end) = payload_start.checked_add(data_len) else {
            continue;
        };
        let Some(payload) = data.get(payload_start..payload_end) else {
            continue;
        };
        if payload.is_empty() {
            continue;
        }
        let name_hint = if !original_path.is_empty() {
            Some(original_path)
        } else if !label.is_empty() {
            Some(label)
        } else {
            None
        };
        return Some((payload.to_vec(), name_hint));
    }

    let start = embedded_payload_start(data)?;
    let payload = data.get(start..)?;
    (!payload.is_empty()).then(|| (payload.to_vec(), None))
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_ole_c_string(data: &[u8], offset: &mut usize) -> Option<String> {
    let rest = data.get(*offset..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    let value = String::from_utf8_lossy(&rest[..end]).into_owned();
    *offset = offset.checked_add(end + 1)?;
    Some(value)
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

#[cfg(any(feature = "office", feature = "hwp", feature = "email"))]
fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Fallback used when the `cfb` dependency isn't active for the enabled feature set
/// (e.g. `excel` without `office`/`hwp`/`email`): OLE objects are always reported
/// as unidentifiable rather than attempting extraction.
#[cfg(not(any(feature = "office", feature = "hwp", feature = "email")))]
pub(crate) fn extract_ole_embedded_object(
    _data: &[u8],
    _source_name: &str,
    _max_bytes: u64,
) -> Option<(Vec<u8>, String)> {
    None
}
#[cfg(all(test, feature = "office"))]
mod tests;
