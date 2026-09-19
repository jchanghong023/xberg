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

    let mut embedding_names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let file = archive.by_index(i).ok()?;
            let name = file.name().to_string();
            if name.starts_with(embeddings_prefix) && name.len() > embeddings_prefix.len() {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // A malformed archive can list the same embeddings path twice; `by_name`
    // would return the same entry for both and the object's text would land in
    // the output twice. Same hygiene as the xlsx media walk.
    embedding_names.sort_unstable();
    embedding_names.dedup();

    if embedding_names.is_empty() {
        return (children, warnings);
    }

    let security_limits = config.security_limits.clone().unwrap_or_default();
    let max_files_in_archive = security_limits.max_files_in_archive;
    if embedding_names.len() > max_files_in_archive {
        let skipped = embedding_names.len() - max_files_in_archive;
        warnings.push(ProcessingWarning {
            source: Cow::Owned(format!("{}_embedded_objects", source_label)),
            message: Cow::Owned(format!(
                "Skipped {} embedded object(s) under '{}': max_files_in_archive ({}) reached",
                skipped, embeddings_prefix, max_files_in_archive
            )),
        });
        embedding_names.truncate(max_files_in_archive);
    }

    if config.max_archive_depth == 0 {
        warnings.push(ProcessingWarning {
            source: Cow::Owned(format!("{}_embedded_objects", source_label)),
            message: Cow::Owned(format!(
                "Skipped {} embedded object(s) under '{}': max_archive_depth reached",
                embedding_names.len(),
                embeddings_prefix
            )),
        });
        return (children, warnings);
    }

    let mut child_config = config.clone();
    child_config.max_archive_depth = config.max_archive_depth.saturating_sub(1);

    // Upper bound for both the initial allocation hint and the actual read of a single
    // embedded file. `file.size()` (used below) is the *declared* uncompressed size from
    // the ZIP central directory: it is attacker-controlled and is not verified against the
    // real decompressed byte count before we use it. A forged declaration (e.g. a
    // multi-terabyte value backed by a few bytes of real compressed data) must not
    // translate into an equally large `Vec::with_capacity` call, which allocates before a
    // single byte is read.
    //
    // Prefers the caller's configured `max_embedded_file_bytes` (default 50 MiB, see
    // `ExtractionConfig::default_max_embedded_file_bytes`) since that is the limit this
    // function already enforces on the *actual* extracted size below -- one cap governs
    // both the hint and the acceptance check. If the caller has explicitly disabled the
    // per-file cap (`None`), fall back to the archive-wide `SecurityLimits::max_archive_size`
    // (default 500 MiB) as a hard backstop: no single embedded member should be allowed to
    // force a larger up-front allocation than the whole-archive budget the caller already
    // agreed to.
    let embedded_capacity_cap: u64 = config
        .max_embedded_file_bytes
        .unwrap_or(security_limits.max_archive_size as u64);

    for entry_name in &embedding_names {
        let filename = entry_name
            .strip_prefix(embeddings_prefix)
            .unwrap_or(entry_name)
            .to_string();

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
                    warnings.push(ProcessingWarning {
                        source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                        message: Cow::Owned(format!("Failed to read embedded file '{}'", filename)),
                    });
                    continue;
                }
                buf
            }
            Err(_) => continue,
        };

        if data.is_empty() {
            continue;
        }

        if data.len() as u64 > embedded_capacity_cap {
            warnings.push(ProcessingWarning {
                source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                message: Cow::Owned(format!(
                    "Skipped embedded file '{}': size {} bytes exceeds cap {} bytes",
                    filename,
                    data.len(),
                    embedded_capacity_cap
                )),
            });
            continue;
        }

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
        if let Some(ole_offset) = ole_offset {
            let ole_data = &data[ole_offset..];
            match extract_ole_embedded_object(ole_data, &filename, embedded_capacity_cap) {
                Some((inner_bytes, inner_mime)) => {
                    match crate::core::extractor::extract_bytes(&inner_bytes, &inner_mime, &child_config).await {
                        Ok(result) => {
                            children.push(ArchiveEntry {
                                path: filename,
                                mime_type: inner_mime,
                                result: Box::new(result),
                            });
                        }
                        Err(e) => {
                            warnings.push(ProcessingWarning {
                                source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                                message: Cow::Owned(format!(
                                    "Failed to extract embedded OLE object '{}': {}",
                                    filename, e
                                )),
                            });
                        }
                    }
                }
                None => {
                    warnings.push(ProcessingWarning {
                        source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                        message: Cow::Owned(format!(
                            "Skipped OLE compound file '{}': format identification not supported",
                            filename
                        )),
                    });
                }
            }
            continue;
        }

        let detected_mime = crate::core::mime::detect_mime_type_from_bytes(&data).ok().or_else(|| {
            std::path::Path::new(&filename)
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(|ext| mime_guess::from_ext(ext).first())
                .map(|m| m.to_string())
        });

        let file_mime = match detected_mime {
            Some(m) if m != "application/octet-stream" => m,
            _ => {
                warnings.push(ProcessingWarning {
                    source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                    message: Cow::Owned(format!(
                        "Skipped embedded file '{}': MIME type could not be determined",
                        filename
                    )),
                });
                continue;
            }
        };

        match crate::core::extractor::extract_bytes(&data, &file_mime, &child_config).await {
            Ok(result) => {
                children.push(ArchiveEntry {
                    path: filename,
                    mime_type: file_mime,
                    result: Box::new(result),
                });
            }
            Err(e) => {
                warnings.push(ProcessingWarning {
                    source: Cow::Owned(format!("{}_embedded_objects", source_label)),
                    message: Cow::Owned(format!("Failed to extract embedded '{}': {}", filename, e)),
                });
            }
        }
    }

    (children, warnings)
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
fn extract_ole_embedded_object(data: &[u8], source_name: &str, max_bytes: u64) -> Option<(Vec<u8>, String)> {
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
fn extract_ole_embedded_object(_data: &[u8], _source_name: &str, _max_bytes: u64) -> Option<(Vec<u8>, String)> {
    None
}

#[cfg(all(test, feature = "office"))]
mod tests {
    use super::*;
    use std::io::Write;

    /// Embedded document text must land in `elements` so Markdown/`content` is
    /// searchable, not only on `children`.
    #[test]
    fn append_embedded_object_text_merges_child_content_into_body() {
        use crate::types::ExtractedDocument;
        use crate::types::internal::{ElementKind, InternalDocument};

        let child = ExtractedDocument {
            content: "SCAN设计流程介绍\n\n拟制".to_string(),
            mime_type: crate::core::mime::LEGACY_WORD_MIME_TYPE.into(),
            ..Default::default()
        };
        let mut doc = InternalDocument::default();
        doc.children = Some(vec![ArchiveEntry {
            path: "oleObject15.bin".to_string(),
            mime_type: crate::core::mime::LEGACY_WORD_MIME_TYPE.into(),
            result: Box::new(child),
        }]);

        append_embedded_object_text(&mut doc);

        let texts: Vec<&str> = doc.elements.iter().map(|element| element.text.as_str()).collect();
        assert!(
            texts.iter().any(|text| text.contains("SCAN设计流程介绍")),
            "expected child body in elements, got {texts:?}"
        );
        // The child's own Markdown has to survive verbatim: as a paragraph, its line
        // breaks collapse into one line and its markers get escaped.
        assert!(
            doc.elements
                .iter()
                .any(|element| matches!(element.kind, ElementKind::RawBlock) && element.text.contains("拟制")),
            "expected the child body as a raw block, got {texts:?}"
        );
        // A filename is not a heading of the host document.
        assert!(
            !doc.elements
                .iter()
                .any(|element| matches!(element.kind, ElementKind::Heading { .. })),
            "expected no heading for the embedded object, got {texts:?}"
        );
    }

    /// A child's images are renumbered into the parent's image table, and a
    /// reference with no matching child image keeps only its alt text.
    #[test]
    fn renumber_embedded_image_refs_moves_refs_onto_the_parent_table() {
        let image = |index: u32| crate::types::ExtractedImage {
            image_index: index,
            ..Default::default()
        };
        let images = vec![image(0), image(1)];
        let input = "前言\n![流程图](image_1.png)\n后记 ![x](image_7.png) 结束";
        let (rewritten, referenced) = renumber_embedded_image_refs(input, 4, &images);
        assert!(rewritten.contains("![流程图](image_5.png)"), "got {rewritten}");
        // Index 7 is not one of the child's images: the alt text stays, the
        // reference (which would resolve to an unrelated picture) does not.
        assert!(!rewritten.contains("image_7.png"), "got {rewritten}");
        assert!(rewritten.contains("后记 x 结束"), "got {rewritten}");
        // Only the image the body still points at is reported for staging.
        assert_eq!(referenced, vec![1]);
    }

    /// Alt text the CommonMark writer escaped (`\]`) must not end the parse:
    /// the old `find(']')` stopped at the escape and the reference kept the
    /// child's numbering in the parent's body.
    #[test]
    fn renumber_embedded_image_refs_parses_escaped_alt_text() {
        let images = vec![crate::types::ExtractedImage {
            image_index: 0,
            ..Default::default()
        }];
        let (rewritten, referenced) = renumber_embedded_image_refs("![flow \\] chart](image_0.png)", 2, &images);
        assert!(
            rewritten.contains("![flow \\] chart](image_2.png)"),
            "the escaped reference is renumbered, got {rewritten}"
        );
        assert_eq!(
            referenced,
            vec![0],
            "the escaped reference counts as a staging candidate"
        );
    }

    /// A fenced `image_N` reference is example text, not a file reference: it
    /// keeps the child's numbering verbatim and stages nothing, while the
    /// unfenced reference after the fence still renumbers and stages.
    #[test]
    fn renumber_embedded_image_refs_leaves_fenced_examples_alone() {
        let image = |index: u32| crate::types::ExtractedImage {
            image_index: index,
            ..Default::default()
        };
        let images = vec![image(0), image(1)];
        let input = "```text\n![示例](image_0.png)\n```\n![流程图](image_1.png)\n";
        let (rewritten, referenced) = renumber_embedded_image_refs(input, 4, &images);
        assert!(
            rewritten.contains("```text\n![示例](image_0.png)\n```"),
            "the fenced example stays verbatim, got {rewritten}"
        );
        assert!(
            rewritten.contains("![流程图](image_5.png)"),
            "the unfenced reference renumbers, got {rewritten}"
        );
        assert_eq!(
            referenced,
            vec![1],
            "only the unfenced reference is a staging candidate"
        );
    }

    /// A candidate whose first `)` lands inside a later fence is not a real
    /// reference: consuming it whole would swallow the fence's opener line,
    /// so the scan degrades to literal bytes and the fence survives whole —
    /// and a real reference after that fence still renumbers and stages.
    #[test]
    fn renumber_embedded_image_refs_does_not_swallow_a_fence_with_an_unclosed_opener() {
        let images = vec![crate::types::ExtractedImage {
            image_index: 0,
            ..Default::default()
        }];
        let input = "![a](x\n```text\ny)\n```\n![flow](image_0.png)\n";
        let (rewritten, referenced) = renumber_embedded_image_refs(input, 4, &images);
        assert!(
            rewritten.contains("```text\ny)\n```"),
            "the fence survives whole, got {rewritten}"
        );
        assert!(
            rewritten.contains("![flow](image_4.png)"),
            "the later real reference still renumbers, got {rewritten}"
        );
        assert_eq!(referenced, vec![0]);
    }

    /// A malformed opener whose `)` never comes must not swallow the next
    /// well-formed reference: the closer search stops at the next `![`, the
    /// malformed opener degrades to two literal bytes, and the later reference
    /// is rescanned from its own start — its image stays a staging candidate.
    #[test]
    fn renumber_embedded_image_refs_recovers_at_the_next_opener() {
        let images = vec![crate::types::ExtractedImage {
            image_index: 0,
            ..Default::default()
        }];
        let (rewritten, referenced) = renumber_embedded_image_refs("![a](unclosed ![c](image_0.png)", 3, &images);
        assert!(
            rewritten.contains("![c](image_3.png)"),
            "the later reference must be renumbered, got {rewritten}"
        );
        assert_eq!(referenced, vec![0], "the later reference is staged");
        // The malformed opener's own text stays verbatim (Malformed keeps two
        // literal bytes); what must NOT survive is the inner reference in its
        // un-renumbered form — that would mean the span was parsed as one
        // reference and the image silently lost.
        assert!(
            !rewritten.contains("image_0.png"),
            "the swallowed inner reference must not survive un-renumbered, got {rewritten}"
        );
    }

    /// A child whose body is empty after renumbering (a reference that matches
    /// no child image and has no alt text) must not stage its images either:
    /// the parent would export picture files that nothing in the body refers
    /// to. The base must also not advance past the unused slots.
    #[test]
    fn append_embedded_object_text_skips_orphan_images_of_empty_children() {
        use crate::types::ExtractedDocument;
        use crate::types::internal::InternalDocument;

        let mut child_result = ExtractedDocument::default();
        child_result.content = "![](image_7.png)".to_string();
        child_result.images = Some(vec![crate::types::ExtractedImage {
            image_index: 0,
            ..Default::default()
        }]);

        let mut doc = InternalDocument::default();
        doc.children = Some(vec![ArchiveEntry {
            path: "oleObject1.bin".to_string(),
            mime_type: "application/octet-stream".to_string(),
            result: Box::new(child_result),
        }]);

        append_embedded_object_text(&mut doc);
        assert!(
            doc.images.is_empty(),
            "orphan images must not be staged: {:?}",
            doc.images
        );
        assert!(doc.elements.is_empty(), "an empty child adds no elements");
    }

    /// Blank child payloads must not inject empty captions/raw blocks.
    #[test]
    fn append_embedded_object_text_skips_blank_children() {
        use crate::types::ExtractedDocument;
        use crate::types::internal::InternalDocument;

        let mut doc = InternalDocument::default();
        doc.children = Some(vec![ArchiveEntry {
            path: "oleObject1.bin".to_string(),
            mime_type: "application/octet-stream".to_string(),
            result: Box::new(ExtractedDocument::default()),
        }]);

        append_embedded_object_text(&mut doc);
        assert!(doc.elements.is_empty(), "blank children must not add elements");
    }

    /// Build a minimal ZIP in memory with one file at the given path and contents.
    fn make_zip_with_file(entry_path: &str, entry_data: &[u8]) -> Vec<u8> {
        make_zip_with_files(&[(entry_path, entry_data)])
    }

    /// Build a minimal ZIP in memory with several files at the given paths and contents.
    fn make_zip_with_files(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let buf = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let options = zip::write::FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        for (entry_path, entry_data) in entries {
            zip.start_file(*entry_path, options).unwrap();
            zip.write_all(entry_data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    /// Bit-by-bit CRC-32 (IEEE 802.3 / zlib / ZIP polynomial 0xEDB88320).
    ///
    /// The hand-forged archive below cannot go through `zip::ZipWriter` (it needs a
    /// central-directory uncompressed-size that the writer's public API has no way to
    /// misstate), so the CRC the reader checks at end-of-stream has to be computed here
    /// too, matching exactly what any standard ZIP implementation would produce.
    fn crc32_ieee(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Hand-construct a single-entry ZIP archive whose central-directory record declares an
    /// enormous uncompressed size via a Zip64 extended-information extra field, while the
    /// real stored payload (and the compressed-size field that bounds the actual read) stays
    /// tiny.
    ///
    /// This forges exactly the shape described for the vulnerability: `zip::ZipWriter`'s
    /// public API has no method to write a declared size that disagrees with the real
    /// payload, so the archive is built byte-by-byte instead, matching the `zip` crate's own
    /// on-disk layout (`ZipLocalEntryBlock`, `ZipCentralEntryBlock`, the Zip64 extended-info
    /// extra field, and `Zip32CDEBlock`/EOCD -- see `zip-8.6.0/src/spec.rs` and
    /// `zip-8.6.0/src/extra_fields/zip64_extended_information.rs`).
    ///
    /// The central-directory `uncompressed_size` 32-bit field is set to the ZIP64 sentinel
    /// (`0xFFFFFFFF`), which the reader ignores in favor of an 8-byte Zip64 extra field
    /// carrying `forged_uncompressed_size`. The `compressed_size` field is left at the real,
    /// honest payload length -- the reader's `find_content` bounds the *actual* on-disk read
    /// to `compressed_size`, so this is what makes the entry parse and read successfully at
    /// all despite the forged size, exactly like a forged real-world OOXML attachment would.
    fn make_forged_zip64_entry(entry_name: &str, payload: &[u8], forged_uncompressed_size: u64) -> Vec<u8> {
        let name_bytes = entry_name.as_bytes();
        let crc = crc32_ieee(payload);
        let compressed_size = payload.len() as u32;

        let mut out = Vec::new();

        // -- Local File Header (ZipLocalEntryBlock, spec.rs) --
        let local_header_start = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header signature
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // compression method: Stored
        out.extend_from_slice(&0u16.to_le_bytes()); // last mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // last mod date
        out.extend_from_slice(&crc.to_le_bytes()); // crc32
        out.extend_from_slice(&compressed_size.to_le_bytes()); // compressed size (honest)
        out.extend_from_slice(&compressed_size.to_le_bytes()); // uncompressed size (local; unused by the reader)
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes()); // file name length
        out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        out.extend_from_slice(name_bytes);
        // -- file data (Stored, verbatim) --
        out.extend_from_slice(payload);

        // -- Zip64 extended-information extra field (only the uncompressed-size slot is
        // populated; kept under 24 bytes so the reader's parser does not also expect a
        // compressed-size or header-start slot to follow) --
        let mut zip64_extra = Vec::new();
        zip64_extra.extend_from_slice(&0x0001u16.to_le_bytes()); // Zip64 extended info tag
        zip64_extra.extend_from_slice(&8u16.to_le_bytes()); // this field's data length: one u64
        zip64_extra.extend_from_slice(&forged_uncompressed_size.to_le_bytes());
        assert_eq!(zip64_extra.len(), 12);

        // -- Central Directory File Header (ZipCentralEntryBlock, spec.rs) --
        let central_header_start = out.len() as u32;
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central file header signature
        out.extend_from_slice(&45u16.to_le_bytes()); // version made by (45 = zip64 support)
        out.extend_from_slice(&45u16.to_le_bytes()); // version needed to extract
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // compression method: Stored
        out.extend_from_slice(&0u16.to_le_bytes()); // last mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // last mod date
        out.extend_from_slice(&crc.to_le_bytes()); // crc32
        out.extend_from_slice(&compressed_size.to_le_bytes()); // compressed size (honest)
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // uncompressed size: ZIP64 sentinel
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes()); // file name length
        out.extend_from_slice(&(zip64_extra.len() as u16).to_le_bytes()); // extra field length
        out.extend_from_slice(&0u16.to_le_bytes()); // file comment length
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0u16.to_le_bytes()); // internal file attributes
        out.extend_from_slice(&0u32.to_le_bytes()); // external file attributes
        out.extend_from_slice(&local_header_start.to_le_bytes()); // relative offset of local header
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&zip64_extra);

        let central_directory_size = out.len() as u32 - central_header_start;

        // -- End Of Central Directory record (Zip32CDEBlock, spec.rs) --
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD signature
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with central directory
        out.extend_from_slice(&1u16.to_le_bytes()); // number of files on this disk
        out.extend_from_slice(&1u16.to_le_bytes()); // total number of files
        out.extend_from_slice(&central_directory_size.to_le_bytes());
        out.extend_from_slice(&central_header_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length

        out
    }

    /// Bytes with no recognizable magic and no valid UTF-8, so both the byte-sniffing and
    /// extension-based MIME fallbacks fail deterministically regardless of which optional
    /// extractor features are compiled in. Used to make "how many embeddings were processed"
    /// observable purely by counting "MIME type could not be determined" warnings.
    const UNDETECTABLE_MIME_BYTES: &[u8] = &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87];

    #[tokio::test]
    async fn test_embedded_file_over_cap_skipped_with_warning() {
        let data = b"Hello world! This is a test document.";
        let zip_bytes = make_zip_with_file("word/embeddings/doc.txt", data);

        let config = ExtractionConfig {
            max_embedded_file_bytes: Some(10),
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(
            children.is_empty(),
            "oversized embedded file must not produce a child entry"
        );
        assert_eq!(warnings.len(), 1, "exactly one warning expected");
        assert!(
            warnings[0].message.contains("exceeds cap"),
            "warning must mention cap: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("doc.txt"),
            "warning must name the file: {}",
            warnings[0].message
        );
    }

    #[tokio::test]
    async fn test_embedded_file_under_cap_proceeds_to_extraction() {
        let data = b"Hello";
        let zip_bytes = make_zip_with_file("word/embeddings/note.txt", data);

        let config = ExtractionConfig {
            max_embedded_file_bytes: Some(1024 * 1024),
            ..Default::default()
        };

        let (_children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        let cap_warnings: Vec<_> = warnings.iter().filter(|w| w.message.contains("exceeds cap")).collect();
        assert!(cap_warnings.is_empty(), "no size-cap warning expected for small file");
    }

    #[tokio::test]
    async fn test_embedded_file_no_cap_proceeds() {
        let data = b"some content";
        let zip_bytes = make_zip_with_file("word/embeddings/file.txt", data);

        let config = ExtractionConfig {
            max_embedded_file_bytes: None,
            ..Default::default()
        };

        let (_children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        let cap_warnings: Vec<_> = warnings.iter().filter(|w| w.message.contains("exceeds cap")).collect();
        assert!(cap_warnings.is_empty(), "no size-cap warning when cap is None");
    }

    /// Build a CFB (OLE compound file) with a single "Package" stream holding `payload`,
    /// the shape OLE object wrappers use to embed a modern Office (OPC/ZIP) document
    /// verbatim.
    // Only consumer is `test_ole_package_stream_extracted_as_embedded_xlsx`, which is
    // `#[cfg(feature = "excel")]` for the reason documented on it. Matching that gate here
    // keeps an `office`-without-`excel` build warning-free.
    #[cfg(feature = "excel")]
    fn make_ole_package(payload: &[u8]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut comp = cfb::CompoundFile::create(cursor).expect("create CFB container");
        {
            let mut stream = comp.create_stream("Package").expect("create Package stream");
            stream.write_all(payload).unwrap();
        }
        comp.into_inner().into_inner()
    }

    /// Build a CFB with a single named stream (e.g. "WordDocument"), simulating a legacy
    /// binary Office document embedded directly as an OLE compound file.
    fn make_ole_with_stream(stream_name: &str, payload: &[u8]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut comp = cfb::CompoundFile::create(cursor).expect("create CFB container");
        {
            let mut stream = comp.create_stream(stream_name).expect("create stream");
            stream.write_all(payload).unwrap();
        }
        comp.into_inner().into_inner()
    }

    /// Path to the shared `test_documents/` corpus (two levels up from this crate).
    #[cfg(feature = "excel")]
    fn test_documents_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("test_documents")
    }

    /// Gated on `excel` as well as `office`: the payload is an .xlsx, so without the
    /// excel extractor registered the recursive extraction correctly reports
    /// `UnsupportedFormat` and produces no child. The test would then fail for a reason
    /// that has nothing to do with OLE Package unwrapping, which is what it exists to
    /// cover. Observed under `--features "email,office,ocr,transcription"`.
    #[cfg(feature = "excel")]
    #[tokio::test]
    async fn test_ole_package_stream_extracted_as_embedded_xlsx() {
        let fixture = test_documents_dir().join("xlsx/excel_tiny_excel.xlsx");
        if !fixture.exists() {
            eprintln!(
                "Skipping test: test_documents/ fixture not found at {}",
                fixture.display()
            );
            return;
        }
        let xlsx_bytes = std::fs::read(&fixture).expect("read fixture xlsx");

        let ole_bytes = make_ole_package(&xlsx_bytes);
        let zip_bytes = make_zip_with_file("word/embeddings/oleObject1.bin", &ole_bytes);

        let config = ExtractionConfig::default();
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert_eq!(
            children.len(),
            1,
            "the OLE Package stream must be unwrapped and recursively extracted; warnings: {:?}",
            warnings
        );
        assert!(
            children[0].mime_type.contains("spreadsheet") || children[0].mime_type.contains("excel"),
            "expected an Excel MIME type, got '{}'",
            children[0].mime_type
        );
    }

    #[tokio::test]
    async fn test_legacy_word_document_ole_stream_is_identified_not_skipped() {
        // The WordDocument content doesn't need to be a well-formed FIB for this test: we
        // only assert that the OLE container was recognized and routed to the legacy
        // `.doc` MIME type instead of being reported as unidentifiable outright. Whether
        // the FIB itself parses is covered by `extraction::doc` tests.
        let ole_bytes = make_ole_with_stream("WordDocument", b"not-a-real-fib-but-present");
        let zip_bytes = make_zip_with_file("word/embeddings/oleObject2.bin", &ole_bytes);

        let config = ExtractionConfig::default();
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty());
        assert_eq!(warnings.len(), 1, "expected exactly one warning: {:?}", warnings);
        assert!(
            !warnings[0].message.contains("format identification not supported"),
            "a recognized WordDocument stream must not be reported as unidentifiable: {}",
            warnings[0].message
        );
    }

    #[tokio::test]
    async fn test_unidentifiable_ole_container_still_warns() {
        let ole_bytes = make_ole_with_stream("SomeUnknownStream", b"opaque binary data");
        let zip_bytes = make_zip_with_file("word/embeddings/oleObject3.bin", &ole_bytes);

        let config = ExtractionConfig::default();
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("format identification not supported"));
        assert!(warnings[0].message.contains("oleObject3.bin"));
    }

    #[tokio::test]
    async fn test_undetectable_mime_now_warns_instead_of_silent_skip() {
        // Bytes with no recognizable magic, invalid as UTF-8 (so the plain-text fallback
        // doesn't kick in either), and no file extension: MIME detection must fail for
        // both the byte-sniffing and extension-based fallback paths.
        let data = vec![0x80u8, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87];
        let zip_bytes = make_zip_with_file("word/embeddings/mystery_blob", &data);

        let config = ExtractionConfig::default();
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty());
        assert_eq!(warnings.len(), 1, "expected exactly one warning: {:?}", warnings);
        assert!(
            warnings[0].message.contains("mystery_blob"),
            "warning must name the file: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("MIME type could not be determined"),
            "warning must explain why the file was skipped: {}",
            warnings[0].message
        );
    }

    #[tokio::test]
    async fn test_depth_exhausted_with_embeddings_present_warns() {
        let data = b"Hello world! This is a test document.";
        let zip_bytes = make_zip_with_file("word/embeddings/doc.txt", data);

        let config = ExtractionConfig {
            max_archive_depth: 0,
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty());
        assert_eq!(warnings.len(), 1, "expected exactly one warning: {:?}", warnings);
        assert!(
            warnings[0].message.contains("max_archive_depth"),
            "warning must explain why embeddings were skipped: {}",
            warnings[0].message
        );
    }

    #[tokio::test]
    async fn test_depth_exhausted_with_no_embeddings_does_not_warn() {
        let zip_bytes = make_zip_with_file("word/document.xml", b"<w:document/>");

        let config = ExtractionConfig {
            max_archive_depth: 0,
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty());
        assert!(
            warnings.is_empty(),
            "no embeddings exist, so no depth warning should be emitted: {:?}",
            warnings
        );
    }

    #[tokio::test]
    async fn test_embedded_objects_exceeding_max_files_in_archive_are_rejected() {
        // 5 embedded entries, each undetectable by MIME so every processed entry produces
        // exactly one "MIME type could not be determined" warning. Unfixed code reads no
        // count limit at all, so it would process and warn on all 5; the fixed code must
        // stop after `max_files_in_archive` (2) and report the remaining 3 as skipped.
        let entries: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("word/embeddings/blob{i}"), UNDETECTABLE_MIME_BYTES.to_vec()))
            .collect();
        let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let zip_bytes = make_zip_with_files(&entry_refs);

        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 2,
                ..Default::default()
            }),
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty(), "undetectable-MIME entries never produce children");

        let cap_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| w.message.contains("max_files_in_archive"))
            .collect();
        assert_eq!(
            cap_warnings.len(),
            1,
            "expected exactly one cap warning: {:?}",
            warnings
        );
        assert!(
            cap_warnings[0].message.contains("Skipped 3"),
            "warning must report the 3 skipped entries: {}",
            cap_warnings[0].message
        );
        assert!(
            cap_warnings[0].message.contains("max_files_in_archive (2)"),
            "warning must name the limit that was hit: {}",
            cap_warnings[0].message
        );

        let processed_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| w.message.contains("MIME type could not be determined"))
            .collect();
        assert_eq!(
            processed_warnings.len(),
            2,
            "only max_files_in_archive (2) entries must be processed, not all 5: {:?}",
            warnings
        );
    }

    #[tokio::test]
    async fn test_embedded_objects_just_under_max_files_in_archive_all_process() {
        // 4 entries against a cap of 5: every entry must still be attempted and no cap
        // warning should fire. A fix that rejects everything (e.g. off-by-one, or clamping
        // to 0) would fail this.
        let entries: Vec<(String, Vec<u8>)> = (0..4)
            .map(|i| (format!("word/embeddings/blob{i}"), UNDETECTABLE_MIME_BYTES.to_vec()))
            .collect();
        let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let zip_bytes = make_zip_with_files(&entry_refs);

        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 5,
                ..Default::default()
            }),
            ..Default::default()
        };

        let (_children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        let cap_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| w.message.contains("max_files_in_archive"))
            .collect();
        assert!(
            cap_warnings.is_empty(),
            "no cap warning expected when the count is under the limit: {:?}",
            warnings
        );

        let processed_warnings: Vec<_> = warnings
            .iter()
            .filter(|w| w.message.contains("MIME type could not be determined"))
            .collect();
        assert_eq!(
            processed_warnings.len(),
            4,
            "all 4 entries must be processed when under the cap: {:?}",
            warnings
        );
    }

    #[tokio::test]
    async fn test_legitimate_document_under_max_files_in_archive_extracts_successfully() {
        // A real, extractable payload (plain text) under the cap must still produce a
        // child entry — proving the fix does not merely suppress warnings but leaves
        // legitimate extraction intact.
        let zip_bytes = make_zip_with_file("word/embeddings/note.txt", b"Hello, world!");

        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 10,
                ..Default::default()
            }),
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert_eq!(
            children.len(),
            1,
            "the single embedded file, well under the cap, must be extracted: {:?}",
            warnings
        );
        assert!(
            !warnings.iter().any(|w| w.message.contains("max_files_in_archive")),
            "no cap warning expected: {:?}",
            warnings
        );
    }

    #[tokio::test]
    async fn test_nested_container_enforces_max_files_in_archive_independently() {
        // Per-container accounting: `extract_ooxml_embedded_objects` is invoked once per
        // container (the outer DOCX/PPTX/XLSX, and again recursively for any embedded
        // OOXML container found inside it, via `extract_bytes`). This test proves the cap
        // is applied fresh to each container's own embeddings directory rather than
        // decremented from some shared, cumulative counter: an outer container with 2
        // embeddings (under a cap of 2) and, independently, an inner container with 5
        // embeddings (over the same cap of 2) each get judged solely against their own
        // entry count.
        let outer_entries: Vec<(String, Vec<u8>)> = (0..2)
            .map(|i| (format!("word/embeddings/outer{i}"), UNDETECTABLE_MIME_BYTES.to_vec()))
            .collect();
        let outer_refs: Vec<(&str, &[u8])> = outer_entries.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let outer_zip_bytes = make_zip_with_files(&outer_refs);

        let inner_entries: Vec<(String, Vec<u8>)> = (0..5)
            .map(|i| (format!("word/embeddings/inner{i}"), UNDETECTABLE_MIME_BYTES.to_vec()))
            .collect();
        let inner_refs: Vec<(&str, &[u8])> = inner_entries.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        let inner_zip_bytes = make_zip_with_files(&inner_refs);

        let config = ExtractionConfig {
            security_limits: Some(crate::extractors::security::SecurityLimits {
                max_files_in_archive: 2,
                ..Default::default()
            }),
            ..Default::default()
        };

        let (_outer_children, outer_warnings) =
            extract_ooxml_embedded_objects(&outer_zip_bytes, "word/embeddings/", "outer", &config).await;
        let (_inner_children, inner_warnings) =
            extract_ooxml_embedded_objects(&inner_zip_bytes, "word/embeddings/", "inner", &config).await;

        assert!(
            !outer_warnings
                .iter()
                .any(|w| w.message.contains("max_files_in_archive")),
            "outer container is exactly at the cap and must not warn: {:?}",
            outer_warnings
        );
        let inner_cap_warnings: Vec<_> = inner_warnings
            .iter()
            .filter(|w| w.message.contains("max_files_in_archive"))
            .collect();
        assert_eq!(
            inner_cap_warnings.len(),
            1,
            "inner container independently exceeds the same cap: {:?}",
            inner_warnings
        );
        assert!(
            inner_cap_warnings[0].message.contains("Skipped 3"),
            "inner container's own 5 entries against a cap of 2 must skip 3: {}",
            inner_cap_warnings[0].message
        );
    }

    /// Direct, allocation-free test of the clamp itself: a forged multi-terabyte declared
    /// size (an attacker-controlled ZIP central-directory uncompressed-size field) must be
    /// clamped down to the configured cap, never passed through as-is.
    #[test]
    fn test_clamp_declared_size_bounds_forged_declaration_to_cap() {
        let forged_declared_size = 4u64 * 1024 * 1024 * 1024 * 1024; // 4 TiB
        let cap = 50 * 1024 * 1024; // the default max_embedded_file_bytes
        assert_eq!(
            clamp_declared_size(forged_declared_size, cap),
            cap,
            "a forged multi-terabyte declared size must be clamped to the configured cap"
        );
    }

    /// Boundary: a declared size exactly at the cap must pass through unchanged (proves the
    /// clamp isn't off-by-one and doesn't needlessly shrink a legitimately-sized file).
    #[test]
    fn test_clamp_declared_size_passes_through_value_at_cap() {
        let cap = 50 * 1024 * 1024;
        assert_eq!(clamp_declared_size(cap, cap), cap);
    }

    /// An honest, small declared size well under the cap must pass through unchanged.
    #[test]
    fn test_clamp_declared_size_passes_through_honest_value_under_cap() {
        let cap = 50 * 1024 * 1024;
        assert_eq!(clamp_declared_size(1024, cap), 1024);
    }

    /// End-to-end reproduction of the vulnerability: a DOCX embedding whose ZIP
    /// central-directory record declares an uncompressed size of `u64::MAX` (via a forged
    /// Zip64 extended-information extra field) while the real stored payload is a few bytes.
    ///
    /// `u64::MAX` is deliberately chosen over a merely large value like "4 TB": on unfixed
    /// code (`Vec::with_capacity(file.size() as usize)`), any capacity request whose byte
    /// count exceeds `isize::MAX` makes `Vec::with_capacity` panic with "capacity overflow"
    /// -- unconditionally, on any platform, regardless of available RAM or virtual-memory
    /// overcommit settings. A merely-large-but-representable value (a few TB) would not give
    /// this guarantee: 64-bit operating systems can often satisfy a multi-terabyte
    /// `with_capacity` as a lazy virtual-memory reservation without touching a single page,
    /// so such a test could pass "by accident" on unfixed code and prove nothing. Choosing a
    /// declared size just past `isize::MAX` instead makes the unfixed behavior a deterministic
    /// panic (this `#[tokio::test]` would fail with "capacity overflow"), not a
    /// platform-dependent maybe-OOM-maybe-not.
    ///
    /// Against the fixed code, `clamp_declared_size` bounds the allocation hint to
    /// `embedded_capacity_cap` (here the default 50 MiB) before `Vec::with_capacity` is ever
    /// called, so no such request is made; the tiny real payload is read normally and (being
    /// undetectable-MIME junk) is reported exactly like any other unidentifiable embedding.
    #[tokio::test]
    async fn test_forged_multi_terabyte_declared_size_does_not_overflow_allocation() {
        let zip_bytes = make_forged_zip64_entry("word/embeddings/huge.bin", UNDETECTABLE_MIME_BYTES, u64::MAX);

        let config = ExtractionConfig::default();
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(
            children.is_empty(),
            "undetectable-MIME entry must never produce a child: {:?}",
            children.len()
        );
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one warning, proving the entry was read and processed rather \
             than rejected outright: {:?}",
            warnings
        );
        assert!(
            warnings[0].message.contains("huge.bin"),
            "warning must name the file: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("MIME type could not be determined"),
            "the tiny real payload must reach the normal MIME-detection path, not be rejected \
             for its (forged) declared size: {}",
            warnings[0].message
        );
    }

    /// Same forged declaration, but the real payload is legitimate small text. Proves the fix
    /// doesn't merely avoid crashing -- the embedding is still correctly extracted, with its
    /// real content intact, despite the archive's central directory lying about its size.
    #[tokio::test]
    async fn test_forged_declared_size_still_extracts_real_small_payload() {
        let payload = b"Hello, world!";
        let zip_bytes = make_forged_zip64_entry("word/embeddings/note.txt", payload, u64::MAX);

        // The default output format is Markdown, which escapes the payload's `!`. These tests
        // pin that the *bytes* survived the forged size, so ask for the plain rendering. ~keep
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Plain,
            ..Default::default()
        };
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert_eq!(
            children.len(),
            1,
            "the real (small) payload behind the forged declaration must still be extracted: {:?}",
            warnings
        );
        assert_eq!(
            children[0].result.content.trim(),
            "Hello, world!",
            "extracted content must match the real payload bytes, not be corrupted by the \
             forged declared size"
        );
    }

    /// Positive control: an ordinary embedded object (no forged metadata at all) with a real
    /// small payload must extract with exactly the same content as before this fix -- proving
    /// the clamp does not affect legitimate, honestly-declared embeddings.
    #[tokio::test]
    async fn test_legitimate_small_embedded_object_extracts_unchanged_bytes() {
        let payload = b"Hello, world!";
        let zip_bytes = make_zip_with_file("word/embeddings/note.txt", payload);

        // Plain rendering for the same reason as the forged-size test above. ~keep
        let config = ExtractionConfig {
            output_format: crate::core::config::OutputFormat::Plain,
            ..Default::default()
        };
        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert_eq!(
            children.len(),
            1,
            "a legitimate small embedded file must still be extracted: {:?}",
            warnings
        );
        assert!(warnings.is_empty(), "no warnings expected: {:?}", warnings);
        assert_eq!(
            children[0].result.content.trim(),
            "Hello, world!",
            "extracted content must be exactly the original bytes"
        );
    }

    /// Boundary: a real (honest) embedded file whose size is exactly at the configured cap
    /// must be extracted, not rejected. Proves the `> cap` comparison (not `>=`).
    #[tokio::test]
    async fn test_embedded_file_exactly_at_cap_is_extracted() {
        let payload = b"Hello, world!"; // 13 bytes
        let zip_bytes = make_zip_with_file("word/embeddings/note.txt", payload);

        let config = ExtractionConfig {
            max_embedded_file_bytes: Some(payload.len() as u64),
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(
            !warnings.iter().any(|w| w.message.contains("exceeds cap")),
            "a file exactly at the cap must not be treated as oversized: {:?}",
            warnings
        );
        assert_eq!(
            children.len(),
            1,
            "a file exactly at the cap must still be extracted: {:?}",
            warnings
        );
    }

    /// Boundary: one byte over the configured cap must be rejected with the size-exceeded
    /// warning and produce no child.
    #[tokio::test]
    async fn test_embedded_file_one_byte_over_cap_is_rejected() {
        let payload = b"Hello, world!!"; // 14 bytes
        let zip_bytes = make_zip_with_file("word/embeddings/note.txt", payload);

        let config = ExtractionConfig {
            max_embedded_file_bytes: Some((payload.len() - 1) as u64),
            ..Default::default()
        };

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(
            children.is_empty(),
            "a file one byte over the cap must not produce a child"
        );
        assert_eq!(warnings.len(), 1, "expected exactly one warning: {:?}", warnings);
        assert!(
            warnings[0].message.contains("exceeds cap"),
            "warning must mention the cap: {}",
            warnings[0].message
        );
    }

    #[tokio::test]
    async fn test_embedded_objects_fall_back_to_default_max_files_in_archive_when_unset() {
        // `security_limits: None` must mean "the `SecurityLimits` default", not "no limit".
        // One entry past the default ceiling must be skipped and reported. The entries are
        // empty so the loop skips each processed one before extraction; the test costs one
        // ZIP central directory, not ten thousand extractions.
        let default_limit = crate::extractors::security::SecurityLimits::default().max_files_in_archive;
        let entries: Vec<String> = (0..=default_limit)
            .map(|i| format!("word/embeddings/blob{i}"))
            .collect();
        let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|p| (p.as_str(), &[][..])).collect();
        let zip_bytes = make_zip_with_files(&entry_refs);

        let config = ExtractionConfig::default();
        assert!(
            config.security_limits.is_none(),
            "this test must exercise the unset fallback, not an explicit limit"
        );

        let (children, warnings) =
            extract_ooxml_embedded_objects(&zip_bytes, "word/embeddings/", "test", &config).await;

        assert!(children.is_empty(), "empty entries never produce children");
        assert_eq!(
            warnings.len(),
            1,
            "exactly one cap warning expected, nothing else: {:?}",
            warnings
        );
        assert!(
            warnings[0].message.contains("Skipped 1 "),
            "exactly one entry past the default ceiling must be skipped: {}",
            warnings[0].message
        );
        assert!(
            warnings[0]
                .message
                .contains(&format!("max_files_in_archive ({default_limit})")),
            "warning must name the default limit that was hit: {}",
            warnings[0].message
        );
    }
}
