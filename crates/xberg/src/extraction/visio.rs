//! Native extraction of text from legacy Visio binary documents.
//!
//! Legacy `.vsd` files are OLE compound documents. Their `VisioDocument` stream
//! contains a pointer tree whose leaf chunk records carry shape text. This module
//! implements the bounded v5/v6+ pointer, chunk, and Visio-LZW readers needed to
//! recover that text without delegating to an external office converter.

use crate::Result;
use crate::XbergError;
use crate::extractors::security::SecurityLimits;
use std::collections::HashSet;
use std::io::{Cursor, Read, Seek};

const VISIO_HEADER: &[u8] = b"Visio (TM) Drawing\r\n";
const VISIO_DOCUMENT_OFFSET: usize = 0x24;
const MAX_CHILD_POINTERS: usize = 100_000;
const MAX_CHILD_DEPTH: usize = 512;
const MAX_TEXT_CHUNKS: usize = 100_000;
const LZW_DICTIONARY_SIZE: usize = 4096;

/// Extract the individual shape-text records from a legacy Visio document.
///
/// `max_stream_size` limits both the OLE `VisioDocument` stream and the total
/// decompressed Visio stream data the parse may materialize. It is supplied by
/// the caller's archive/security budget, so malformed files cannot grow an
/// unbounded allocation: the per-stream cap alone bounds one read, not a deep
/// pointer tree that keeps a decompressed copy per level, so every read is
/// charged against the one budget and exhausting it fails the extraction.
pub(crate) fn extract_visio_text(content: &[u8], max_stream_size: usize) -> Result<Vec<String>> {
    let mut compound_file = cfb::CompoundFile::open(Cursor::new(content))
        .map_err(|error| XbergError::parsing(format!("Failed to open VSD as OLE container: {error}")))?;

    // Embedded wrappers (an OLE object inside another compound file) can carry the
    // VisioDocument stream inside a substorage rather than at the root, while the
    // classification side recognises it at any depth — so the reader has to search
    // the tree too, or such containers are misrouted here and fail on the root name.
    let candidates: Vec<std::path::PathBuf> = {
        let mut found = vec![std::path::PathBuf::from("/VisioDocument")];
        for entry in compound_file.walk() {
            if !entry.is_stream() {
                continue;
            }
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("VisioDocument"))
            {
                found.push(path.to_path_buf());
            }
        }
        found
    };
    let mut stream = None;
    let mut open_error = None;
    for path in candidates {
        match compound_file.open_stream(&path) {
            Ok(opened) => {
                stream = Some(opened);
                break;
            }
            Err(error) => open_error = Some(error),
        }
    }
    let stream = stream.ok_or_else(|| {
        XbergError::parsing(format!(
            "Failed to open VisioDocument stream: {}",
            open_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no such stream".to_string())
        ))
    })?;

    let read_limit = max_stream_size.saturating_add(1) as u64;
    let mut document_stream = Vec::with_capacity(content.len().min(max_stream_size));
    stream
        .take(read_limit)
        .read_to_end(&mut document_stream)
        .map_err(|error| XbergError::parsing(format!("Failed to read VisioDocument stream: {error}")))?;
    if document_stream.len() > max_stream_size {
        return Err(XbergError::parsing(format!(
            "VisioDocument stream exceeds configured limit of {max_stream_size} bytes"
        )));
    }

    if document_stream.len() < VISIO_DOCUMENT_OFFSET || !document_stream.starts_with(VISIO_HEADER) {
        return Err(XbergError::parsing("VisioDocument stream has an invalid Visio header"));
    }

    let version = read_u16(&document_stream, 0x1a)
        .ok_or_else(|| XbergError::parsing("VisioDocument stream is missing its version"))?;
    if version < 5 {
        return Err(XbergError::parsing(format!(
            "Visio file version {version} is older than the supported v5 pointer format"
        )));
    }

    // A v11+ document stores shape text as UTF-16; anything older stores the ANSI
    // codepage the file was written in, which the OLE property set declares.
    let ansi_encoding = if version >= 11 {
        encoding_rs::WINDOWS_1252
    } else {
        summary_information_codepage(&mut compound_file)
            .map(crate::text::windows_codepage::encoding_for_windows_codepage)
            .unwrap_or(encoding_rs::WINDOWS_1252)
    };

    let root_pointer = parse_pointer(&document_stream, VISIO_DOCUMENT_OFFSET, version)
        .ok_or_else(|| XbergError::parsing("VisioDocument stream has an invalid trailer pointer"))?;
    if root_pointer.kind != 20 {
        return Err(XbergError::parsing(format!(
            "VisioDocument trailer pointer has unexpected type {}",
            root_pointer.kind
        )));
    }

    let mut parser = VisioParser {
        document: &document_stream,
        version,
        ansi_encoding,
        max_stream_size,
        remaining_stream_bytes: max_stream_size,
        stream_budget_exhausted: false,
        depth_exhausted: false,
        pointer_limit_exhausted: false,
        text_truncated: false,
        visited: HashSet::new(),
        text: Vec::new(),
    };
    parser.scan_stream(root_pointer, 0)?;
    // A descendant whose read the budget rejected is tolerated during the descent (a damaged
    // child must not hide its siblings), but the result would then be silently truncated text.
    // Report it instead, the way the archive readers reject rather than truncate.
    if parser.stream_budget_exhausted {
        return Err(XbergError::parsing(format!(
            "Visio stream data exceeds the configured budget of {max_stream_size} bytes"
        )));
    }
    if parser.depth_exhausted {
        return Err(XbergError::parsing("Visio stream nesting exceeds the safety limit"));
    }
    if parser.pointer_limit_exhausted {
        return Err(XbergError::parsing(
            "Visio pointer container exceeds the child safety limit",
        ));
    }
    if parser.text_truncated {
        return Err(XbergError::parsing(format!(
            "Visio document carries more than the safety limit of {MAX_TEXT_CHUNKS} text chunks"
        )));
    }
    Ok(parser.text)
}

/// Reject a Visio Drawing package whose ZIP container violates the caller's
/// `SecurityLimits` before any part is read: entry count, aggregate declared
/// size, and compression ratio all use the configured values — the same three
/// checks `extraction::excel::validate_zip_container` runs ahead of calamine.
/// The legacy tolerance that function keeps for `.xls` does not apply here: a
/// Visio Drawing package that reaches this point was already identified as a
/// readable ZIP by its magic bytes.
fn validate_package_container<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, limits: &SecurityLimits) -> Result<()> {
    if archive.len() > limits.max_files_in_archive {
        return Err(XbergError::validation(format!(
            "Visio package declares {} entries, which exceeds the configured limit of {} \
             (SecurityLimits::max_files_in_archive); reduce the archive's entry count or raise the limit",
            archive.len(),
            limits.max_files_in_archive
        )));
    }
    crate::extractors::security::ZipBombValidator::new(limits.clone())
        .validate(archive)
        .map_err(XbergError::from)
}

/// Extract shape text from a Visio Drawing package (`.vsdx`/`.vsdm`).
///
/// A drawing package is an OPC (ZIP) container; the shape text lives in the
/// `<Text>` elements of its `visio/pages/*.xml` and `visio/masters/*.xml`
/// parts. This is the OOXML counterpart of [`extract_visio_text`], which reads
/// the binary `.vsd` container.
///
/// The container is validated against the caller's `limits` before any part is
/// read (see [`validate_package_container`]); `limits.max_archive_size` is then
/// one budget across all package parts: a single part over it aborts the
/// extraction, and so does the running total once it is spent, mirroring the
/// binary reader's behavior (a per-part cap alone does not bound a container
/// with many parts).
pub(crate) fn extract_visio_package_text(content: &[u8], limits: &SecurityLimits) -> Result<Vec<String>> {
    let max_stream_size = limits.max_archive_size;
    let mut archive = zip::ZipArchive::new(Cursor::new(content))
        .map_err(|error| XbergError::parsing(format!("Failed to open VSDX as ZIP package: {error}")))?;
    validate_package_container(&mut archive, limits)?;

    let mut text = Vec::new();
    // A per-part cap does not bound the package: a container with many small parts still makes
    // the reader decompress (and parse) arithmetically more than `max_stream_size`. Charge every
    // part against one budget, as the binary reader now does, so a crafted package cannot exceed
    // the limit the caller thinks it set.
    let mut remaining = max_stream_size;
    for index in 0..archive.len() {
        let file = match archive.by_index(index) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let name = file.name().to_string();
        let is_text_part = name.ends_with(".xml")
            && (name.starts_with("visio/pages/")
                || name.starts_with("visio/masters/")
                || name.starts_with("pages/")
                || name.starts_with("masters/"));
        if !is_text_part {
            continue;
        }
        if file.size() as usize > max_stream_size {
            return Err(XbergError::parsing(format!(
                "Visio package part '{name}' exceeds configured limit of {max_stream_size} bytes"
            )));
        }
        // The ZIP central directory's declared size is attacker-controlled and only
        // checked against the full cap above; clamp the preallocation to what the
        // shared budget could still admit (same idiom as the CFB reader), so a tiny
        // part lying about its size cannot make the reader reserve the whole cap.
        let mut xml = String::with_capacity((file.size() as usize).min(remaining));
        // A part that cannot be read (I/O error, non-UTF-8 bytes) must not hide
        // its siblings: the part failures above and below both skip the part,
        // and a damaged master must not cost the pages' text. Only the budget
        // checks abort the whole extraction — that is the caller's explicit cap.
        if file
            .take(max_stream_size.saturating_add(1) as u64)
            .read_to_string(&mut xml)
            .is_err()
        {
            tracing::debug!("Skipping unreadable or non-UTF-8 Visio package part '{name}'");
            continue;
        }
        if xml.len() > max_stream_size {
            return Err(XbergError::parsing(format!(
                "Visio package part '{name}' exceeds configured limit of {max_stream_size} bytes"
            )));
        }
        if xml.len() > remaining {
            return Err(XbergError::parsing(format!(
                "Visio package parts exceed the configured budget of {max_stream_size} bytes"
            )));
        }
        remaining -= xml.len();
        let Ok(document) = roxmltree::Document::parse(&xml) else {
            tracing::debug!("Skipping malformed XML in Visio package part '{name}'");
            continue;
        };
        for node in document.descendants().filter(|node| node.has_tag_name("Text")) {
            // EDDX (Edraw) parts nest the same tag name —
            // `<Text><TextBlock>…<Text><tp>…</tp></Text></TextBlock></Text>` — so a
            // plain `Text` filter visits the outer and the inner element and pushes
            // every string twice. The outer pass already collects all descendant
            // text, so a node that itself has a `Text` ancestor adds nothing new.
            // Real VSDX parts never nest `Text`, so this changes nothing for them.
            // roxmltree's `ancestors()` STARTS AT THE NODE ITSELF (`node: Some(*self)`),
            // so the self-match must be excluded — without `.skip(1)` every `Text`
            // node sees itself as its own `Text` ancestor and the part yields no text
            // at all (that bug silently emptied every embedded EDDX section).
            if node.ancestors().skip(1).any(|ancestor| ancestor.has_tag_name("Text")) {
                continue;
            }
            let mut buffer = String::new();
            for descendant in node.descendants() {
                if descendant.is_text()
                    && let Some(value) = descendant.text()
                {
                    buffer.push_str(value);
                }
            }
            let normalized = buffer.replace("\r\n", "\n").replace('\r', "\n");
            let trimmed = normalized.trim();
            if !trimmed.is_empty() {
                text.push(trimmed.to_string());
            }
        }
    }
    Ok(text)
}

struct VisioParser<'a> {
    document: &'a [u8],
    version: u16,
    /// Encoding of the pre-v11 8-bit text chunks, resolved once from the OLE property set.
    ansi_encoding: &'static encoding_rs::Encoding,
    max_stream_size: usize,
    /// Decompressed stream bytes the parse may still materialize, shared by every stream read.
    ///
    /// The per-stream cap bounds one stream, not the pointer tree: a deep chain of distinct keys
    /// keeps one decompressed copy alive per recursion level, and each key re-decodes the same
    /// region. Charging every read keeps the module's promise ("malformed files cannot grow an
    /// unbounded allocation") true for the whole descent.
    remaining_stream_bytes: usize,
    /// Set when a stream read was rejected by the budget.
    ///
    /// Descendant reads are tolerated on purpose (`let _ = self.scan_stream(child, ..)`), so a
    /// budget failure there would otherwise degrade into silently truncated text. The caller
    /// checks this flag after the descent and reports the failure instead.
    stream_budget_exhausted: bool,
    /// Set when a pointer chain ran past [`MAX_CHILD_DEPTH`]. The same tolerance applies
    /// — the `let _ =` at the recursion site would swallow the error and return a
    /// "clean" conversion of silently truncated text.
    depth_exhausted: bool,
    /// Set when a pointer container declared more children than [`MAX_CHILD_POINTERS`]:
    /// the parse refuses to return a table, the `if let Ok` at the recursion site swallows
    /// that error, and the subtree's text would be silently lost without the report.
    pointer_limit_exhausted: bool,
    /// Set when the shape-text collection hit [`MAX_TEXT_CHUNKS`] — same silent-truncation
    /// discipline as the other safety caps.
    text_truncated: bool,
    visited: HashSet<StreamKey>,
    text: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StreamKey {
    kind: u32,
    offset: usize,
    length: usize,
    format: u16,
}

#[derive(Debug, Clone, Copy)]
struct Pointer {
    kind: u32,
    offset: usize,
    length: usize,
    format: u16,
}

struct StreamData {
    contents: Vec<u8>,
    block_header: Option<[u8; 4]>,
}

impl<'a> VisioParser<'a> {
    fn scan_stream(&mut self, pointer: Pointer, depth: usize) -> Result<()> {
        if depth > MAX_CHILD_DEPTH {
            self.depth_exhausted = true;
            return Ok(());
        }

        let key = StreamKey {
            kind: pointer.kind,
            offset: pointer.offset,
            length: pointer.length,
            format: pointer.format,
        };
        if !self.visited.insert(key) {
            return Ok(());
        }

        let stream = self.read_stream(pointer)?;

        if pointer_has_pointers(pointer, self.version) {
            match self.parse_child_pointers(pointer, &stream.contents) {
                Ok(children) => {
                    for child in children {
                        // A damaged child must not hide valid siblings. The root stream
                        // remains fatal when it cannot be read, while malformed descendants
                        // are skipped after the surrounding document has been recovered.
                        let _ = self.scan_stream(child, depth + 1);
                    }
                }
                // An unparseable ROOT pointer table means the document structure
                // itself is unreadable — returning an empty success would be a
                // silent failure. Descendant tables keep the recovery above.
                Err(error) if depth == 0 => return Err(error),
                Err(_) => {}
            }
        }

        if pointer_has_chunks(pointer, self.version) {
            self.scan_chunks(&stream);
        }

        Ok(())
    }

    fn read_stream(&mut self, pointer: Pointer) -> Result<StreamData> {
        let end = pointer
            .offset
            .checked_add(pointer.length)
            .ok_or_else(|| XbergError::parsing("Visio stream range overflowed"))?;
        if end > self.document.len() {
            return Err(XbergError::parsing(format!(
                "Visio stream range {}..{} exceeds VisioDocument stream length {}",
                pointer.offset,
                end,
                self.document.len()
            )));
        }

        // Copied out of `self` first: the slice borrows the document, not the parser, so the
        // budget charge below can take `&mut self` while `raw` is still alive.
        let document = self.document;
        let raw = &document[pointer.offset..end];
        if !pointer_compressed(pointer) {
            self.charge_stream_bytes(raw.len())?;
            return Ok(StreamData {
                contents: raw.to_vec(),
                block_header: None,
            });
        }

        // Also limited by the parse-wide budget, so a decompression bomb cannot expand past it
        // even when this is the deepest stream in the chain.
        let cap = self.max_stream_size.min(self.remaining_stream_bytes.max(1));
        let decompressed = match decode_visio_lzw(raw, cap) {
            Ok(decompressed) => decompressed,
            Err(VisioLzwError::TooLarge) => {
                // The stream's decompressed size violates its cap. When the budget lowered
                // the cap this read was refused by the budget; when the cap is still the
                // full `max_stream_size`, the stream alone exceeds what the parse allows.
                // The flag must be set in both cases — returning `Err` without it would let
                // a caller's descendant read swallow the error and report truncated text.
                self.stream_budget_exhausted = true;
                return Err(XbergError::parsing(
                    "Decompressed Visio stream exceeds its safety limit",
                ));
            }
            Err(VisioLzwError::Malformed(error)) => return Err(error),
        };
        self.charge_stream_bytes(decompressed.len())?;
        if decompressed.len() < 4 {
            return Err(XbergError::parsing("Compressed Visio stream has no block header"));
        }
        let mut block_header = [0u8; 4];
        block_header.copy_from_slice(&decompressed[..4]);
        Ok(StreamData {
            contents: decompressed[4..].to_vec(),
            block_header: Some(block_header),
        })
    }

    /// Charge `bytes` against the parse-wide decompressed-stream budget.
    fn charge_stream_bytes(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.remaining_stream_bytes {
            self.stream_budget_exhausted = true;
            return Err(XbergError::parsing(format!(
                "Visio stream data exceeds the configured budget of {} bytes",
                self.max_stream_size
            )));
        }
        self.remaining_stream_bytes -= bytes;
        Ok(())
    }

    fn parse_child_pointers(&mut self, parent: Pointer, contents: &[u8]) -> Result<Vec<Pointer>> {
        let pointer_size = pointer_size(self.version);
        let (count_offset, count, post_count_skip) = if self.version >= 6 {
            let count_offset = read_u32(contents, 0)
                .ok_or_else(|| XbergError::parsing("Visio pointer container has no count"))?
                as usize;
            let count = read_u32(contents, count_offset)
                .ok_or_else(|| XbergError::parsing("Visio pointer container count is truncated"))?
                as usize;
            (count_offset, count, 8usize)
        } else {
            // Count-offset table from libvisio's `VSD5Parser::readPointerInfo`
            // (VSDDocumentStructure.h constants): each pointer kind carries its
            // count at a different offset into the container.
            let count_offset = match parent.kind {
                0x14 => 130,       // VSD_TRAILER_STREAM
                0x15 => 66,        // VSD_PAGE
                0x18 => 46,        // VSD_FONT_LIST
                0x1a => 18,        // VSD_STYLES
                0x1d | 0x4e => 30, // VSD_STENCILS / VSD_SHAPE_FOREIGN
                0x1e => 54,        // VSD_STENCIL_PAGE
                kind if kind > 0x45 => 30,
                _ => 10,
            };
            let count = read_u16(contents, count_offset)
                .ok_or_else(|| XbergError::parsing("Visio pointer container count is truncated"))?
                as usize;
            (count_offset, count, 2usize)
        };

        if count > MAX_CHILD_POINTERS {
            // The recursion site swallows descendant errors, but the flag set here
            // still fails the conversion at the top level — a pointer table this
            // damaged must not end in a "clean" conversion that silently lost text.
            self.pointer_limit_exhausted = true;
            return Err(XbergError::parsing(format!(
                "Visio pointer container declares {count} children, over the safety limit"
            )));
        }

        let start = count_offset
            .checked_add(post_count_skip)
            .ok_or_else(|| XbergError::parsing("Visio pointer table offset overflowed"))?;
        let table_len = count
            .checked_mul(pointer_size)
            .ok_or_else(|| XbergError::parsing("Visio pointer table size overflowed"))?;
        let end = start
            .checked_add(table_len)
            .ok_or_else(|| XbergError::parsing("Visio pointer table end overflowed"))?;
        if end > contents.len() {
            return Err(XbergError::parsing("Visio pointer table is truncated"));
        }

        let mut pointers = Vec::with_capacity(count);
        let mut offset = start;
        for _ in 0..count {
            let child = parse_pointer(contents, offset, self.version)
                .ok_or_else(|| XbergError::parsing("Visio child pointer is truncated"))?;
            pointers.push(child);
            offset += pointer_size;
        }
        Ok(pointers)
    }

    fn scan_chunks(&mut self, stream: &StreamData) {
        if self.text.len() >= MAX_TEXT_CHUNKS {
            // Same discipline as the budget and depth caps: the caller must see the
            // truncation instead of receiving a "clean" conversion that silently
            // dropped text.
            self.text_truncated = true;
            return;
        }

        let mut contents =
            Vec::with_capacity(stream.contents.len() + stream.block_header.map_or(0, |header| header.len()));
        if let Some(header) = stream.block_header {
            contents.extend_from_slice(&header);
        }
        contents.extend_from_slice(&stream.contents);

        let header_size = if self.version >= 6 { 19 } else { 12 };
        let mut offset = 0usize;
        while offset.checked_add(header_size).is_some_and(|end| end <= contents.len()) {
            let Some((chunk_type, declared_length, unknown1, unknown2, unknown3)) =
                parse_chunk_header(&contents, offset, self.version)
            else {
                break;
            };
            let body_start = offset + header_size;
            let Some(body_end) = body_start.checked_add(declared_length) else {
                break;
            };
            if body_end > contents.len() {
                break;
            }

            if chunk_type == 14 && body_end >= body_start + 8 {
                let text_start = body_start + 8;
                let text = decode_visio_text(&contents[text_start..body_end], self.version >= 11, self.ansi_encoding);
                // The pointer walk reaches some byte ranges twice — the same offset and
                // length carry two pointer formats, and both are chunk-bearing — which
                // emitted every shape text twice: a 24-label drawing produced 182 text
                // entries, each label in an adjacent run of exactly two. An adjacent
                // repeat is that artifact, so it is dropped here. The cost is a shape
                // whose label also appears on a shape stored immediately after it, whose
                // runs merge into one entry.
                let repeated = self.text.last().map(String::as_str) == Some(text.as_str());
                if !text.is_empty() && text != "\n" && !repeated {
                    self.text.push(text);
                    if self.text.len() >= MAX_TEXT_CHUNKS {
                        self.text_truncated = true;
                        break;
                    }
                }
            }

            let trailer_len = chunk_trailer_len(chunk_type, unknown1, unknown2, unknown3, self.version);
            let Some(next) = body_end.checked_add(trailer_len) else {
                break;
            };
            if next > contents.len() || next <= offset {
                break;
            }
            offset = next;
        }
    }
}

fn pointer_size(version: u16) -> usize {
    if version >= 6 { 18 } else { 16 }
}

fn parse_pointer(data: &[u8], offset: usize, version: u16) -> Option<Pointer> {
    let end = offset.checked_add(pointer_size(version))?;
    if end > data.len() {
        return None;
    }

    if version >= 6 {
        Some(Pointer {
            kind: read_u32(data, offset)?,
            offset: read_u32(data, offset + 8)? as usize,
            length: read_u32(data, offset + 12)? as usize,
            format: read_u16(data, offset + 16)?,
        })
    } else {
        // libvisio's `VSD5Parser::readPointer` masks Type and Format to their low
        // bytes — real v5 files carry noise in the high byte, and an unmasked
        // value both fails the root-pointer kind check and misses every
        // dispatch range below.
        Some(Pointer {
            kind: (read_u16(data, offset)? & 0x00ff) as u32,
            offset: read_u32(data, offset + 8)? as usize,
            length: read_u32(data, offset + 12)? as usize,
            format: read_u16(data, offset + 2)? & 0x00ff,
        })
    }
}

fn pointer_has_pointers(pointer: Pointer, version: u16) -> bool {
    if version >= 6 {
        pointer.kind == 20 || (0x1d..0x1f).contains(&pointer.format) || (0x50..0x60).contains(&pointer.format)
    } else {
        pointer.kind == 20
            || (pointer.kind != 22
                && ((0x1d..0x1f).contains(&pointer.format) || (0x50..0x60).contains(&pointer.format)))
    }
}

fn pointer_has_chunks(pointer: Pointer, version: u16) -> bool {
    if version >= 6 {
        (0xd0..0xdf).contains(&pointer.format)
    } else {
        pointer.kind == 21 || pointer.kind == 24 || (0xd0..0xdf).contains(&pointer.format)
    }
}

fn pointer_compressed(pointer: Pointer) -> bool {
    pointer.format & 2 != 0
}

fn parse_chunk_header(data: &[u8], offset: usize, version: u16) -> Option<(u32, usize, u32, u16, u8)> {
    if version >= 6 {
        let end = offset.checked_add(19)?;
        if end > data.len() {
            return None;
        }
        Some((
            read_u32(data, offset)?,
            read_u32(data, offset + 12)? as usize,
            read_u32(data, offset + 8)?,
            read_u16(data, offset + 16)?,
            data[offset + 18],
        ))
    } else {
        let end = offset.checked_add(12)?;
        if end > data.len() {
            return None;
        }
        Some((
            read_u16(data, offset)? as u32,
            read_u32(data, offset + 8)? as usize,
            read_u16(data, offset + 6)? as u32,
            0,
            0,
        ))
    }
}

/// The trailer computation of libvisio's `getChunkHeader` — the reference
/// implementation this chunk walk transcribes — per format version. The
/// advance lands the cursor on the next chunk header; being off by 4 or 8
/// bytes desynchronizes the walk and silently drops every later text chunk in
/// the stream, so both variants mirror the reference condition for condition
/// (VSD6Parser.cpp for v6, VSDParser.cpp for v11+).
fn chunk_trailer_len(chunk_type: u32, list: u32, level: u16, unknown: u8, version: u16) -> usize {
    if version < 6 {
        // VSD5Parser (v5 and below): `getChunkHeader` sets the trailer to zero
        // unconditionally — the older formats carry no chunk trailer at all.
        return 0;
    }
    if version == 6 {
        // VSD6Parser: an 8-byte trailer for list chunks and a wide type set;
        // 0x1f (OLE data) and 0xc9 (Name ID) never have one.
        if matches!(chunk_type, 0x1f | 0xc9) {
            return 0;
        }
        if list != 0 || matches!(chunk_type, 0x64..=0x73 | 0x76 | 0x2c | 0x0d) {
            return 8;
        }
        return 0;
    }
    // VSDParser (v11+): an 8-byte stage, a 4-byte stage gated on
    // list/level/unknown, an array of types that take the extra word only
    // when the stages did not already fire, and four never-trailer types
    // that zero the whole thing at the end.
    let mut trailer = 0usize;
    if list != 0 || matches!(chunk_type, 0x2c | 0x65 | 0x66 | 0x69 | 0x6a | 0x6b | 0x70 | 0x71) {
        trailer += 8;
    }
    if list != 0
        || (level == 2 && unknown == 0x55)
        || (level == 2 && unknown == 0x54 && chunk_type == 0xaa)
        || (level == 3 && unknown != 0x50 && unknown != 0x54)
    {
        trailer += 4;
    }
    const TRAILER_CHUNKS: [u32; 14] = [
        0x64, 0x65, 0x66, 0x69, 0x6a, 0x6b, 0x6f, 0x71, 0x92, 0xa9, 0xb4, 0xb6, 0xb9, 0xc7,
    ];
    if trailer != 12 && trailer != 4 && TRAILER_CHUNKS.contains(&chunk_type) {
        trailer += 4;
    }
    if matches!(chunk_type, 0x1f | 0xc9 | 0x2d | 0xd1) {
        trailer = 0;
    }
    trailer
}

fn decode_visio_text(data: &[u8], utf16: bool, ansi_encoding: &'static encoding_rs::Encoding) -> String {
    if utf16 {
        let mut units = Vec::with_capacity(data.len() / 2);
        for pair in data.chunks_exact(2) {
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16_lossy(&units)
    } else {
        let (decoded, _, _) = ansi_encoding.decode(data);
        decoded.trim_end_matches('\0').to_string()
    }
}

/// `PIDSI_CODEPAGE`: the codepage of the property set's 8-bit strings (MS-OLEPS).
const PIDSI_CODEPAGE: u32 = 1;

/// `VT_I2`, the value type `PIDSI_CODEPAGE` is declared with.
const VT_I2: u32 = 2;
/// `VT_UI2`, which some writers use for the same property.
const VT_UI2: u32 = 18;

/// The ANSI codepage a legacy Visio document declares in its OLE
/// `\x05SummaryInformation` property set.
///
/// Pre-v11 Visio stores shape text as bytes in the codepage of the machine that
/// wrote the file, and the property set's `PIDSI_CODEPAGE` is where that is
/// recorded — the same field Office's own streams use (MS-OLEPS). Decoding those
/// bytes as Windows-1252 turned every label of a Simplified-Chinese drawing into
/// mojibake (`不推荐` → `²»ÍÆ¼ö`): the bytes are GBK, and a codepage of 936 says so.
///
/// Returns `None` when the stream is missing, unreadable, or carries no codepage;
/// the caller then keeps Windows-1252.
fn summary_information_codepage<R: Read + std::io::Seek>(compound_file: &mut cfb::CompoundFile<R>) -> Option<u32> {
    /// A property set is a few hundred bytes of header and directory; the strings
    /// (which this only walks past) are what grow. Bound the read so a container
    /// that declares a huge stream cannot turn this metadata probe into a second
    /// copy of the file.
    const MAX_PROPERTY_SET_BYTES: u64 = 64 * 1024;

    let stream = compound_file.open_stream("/\u{5}SummaryInformation").ok()?;
    let mut data = Vec::with_capacity(1024);
    stream.take(MAX_PROPERTY_SET_BYTES).read_to_end(&mut data).ok()?;

    // Header: byte order (2) + version (2) + OS (4) + CLSID (16) + count (4); then one
    // FMTID (16) + offset (4) per set.
    let set_count = read_u32(&data, 24)?;
    if set_count == 0 {
        return None;
    }
    let set_offset = read_u32(&data, 28 + 16)? as usize;
    let property_count = read_u32(&data, set_offset.checked_add(4)?)? as usize;

    for index in 0..property_count {
        let entry_offset = set_offset.checked_add(8)?.checked_add(index.checked_mul(8)?)?;
        let property_id = read_u32(&data, entry_offset)?;
        if property_id != PIDSI_CODEPAGE {
            continue;
        }
        let value_offset = set_offset.checked_add(read_u32(&data, entry_offset + 4)? as usize)?;
        if !matches!(read_u32(&data, value_offset), Some(VT_I2 | VT_UI2)) {
            return None;
        }
        return read_u16(&data, value_offset + 4).map(u32::from);
    }
    None
}

/// Why a Visio LZW decode failed. The distinction matters to the parse-wide
/// budget: an over-large output means decompression is still expanding past
/// its allowance (the bomb must stay flagged even when a parent's recovery
/// swallows this stream's error), while malformed input merely loses this
/// stream's text.
enum VisioLzwError {
    TooLarge,
    Malformed(XbergError),
}

fn decode_visio_lzw(data: &[u8], max_size: usize) -> std::result::Result<Vec<u8>, VisioLzwError> {
    let mut dictionary = [0u8; LZW_DICTIONARY_SIZE];
    let mut output = Vec::with_capacity(data.len().min(max_size));
    let mut output_position = 0usize;
    let mut input_position = 0usize;
    let mut truncated = false;

    'flags: while input_position < data.len() {
        let flags = data[input_position];
        input_position += 1;
        let mut mask = 1u16;
        while mask < 0x100 {
            if flags & mask as u8 != 0 {
                let Some(&value) = data.get(input_position) else {
                    truncated = true;
                    break 'flags;
                };
                input_position += 1;
                if output.len() >= max_size {
                    return Err(VisioLzwError::TooLarge);
                }
                dictionary[output_position & (LZW_DICTIONARY_SIZE - 1)] = value;
                output.push(value);
                output_position += 1;
            } else {
                let Some(&first) = data.get(input_position) else {
                    truncated = true;
                    break 'flags;
                };
                let Some(&second) = data.get(input_position + 1) else {
                    truncated = true;
                    break 'flags;
                };
                input_position += 2;

                let length = (second & 0x0f) as usize + 3;
                if output.len().checked_add(length).is_none_or(|end| end > max_size) {
                    return Err(VisioLzwError::TooLarge);
                }

                let pointer = if first as usize + ((second as usize & 0xf0) << 4) > 4078 {
                    first as usize + ((second as usize & 0xf0) << 4) - 4078
                } else {
                    first as usize + ((second as usize & 0xf0) << 4) + 18
                };
                // Back-references may overlap bytes written during this same match.
                // Read and write one byte at a time so a newly emitted byte is
                // available to the next source position, as required by the
                // LZW sliding-window semantics.
                for index in 0..length {
                    let byte = dictionary[(pointer + index) & (LZW_DICTIONARY_SIZE - 1)];
                    dictionary[output_position & (LZW_DICTIONARY_SIZE - 1)] = byte;
                    output.push(byte);
                    output_position += 1;
                }
            }
            mask <<= 1;
        }
    }

    // Truncated input is refused even when some bytes already decoded: partial
    // output would flow into the chunk scanner as if it were the whole stream
    // and come out as silently truncated text. A truncated descendant stream is
    // dropped by the caller's recovery; a truncated root stream fails the read.
    if truncated {
        return Err(VisioLzwError::Malformed(XbergError::parsing(
            "Truncated Visio LZW stream",
        )));
    }
    if output.len() < 4 {
        return Err(VisioLzwError::Malformed(XbergError::parsing(
            "Visio LZW stream has no block header",
        )));
    }
    Ok(output)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v6 trailer computation mirrors `VSD6Parser::getChunkHeader`: a wide
    /// type set plus any list gets 8 bytes, 0x1f/0xc9 always zero, everything
    /// else nothing.
    #[test]
    fn v6_chunk_trailer_matches_the_reference() {
        assert_eq!(
            chunk_trailer_len(0x64, 0, 0, 0, 6),
            8,
            "0x64 is in the v6 set (the old set8 missed it)"
        );
        assert_eq!(chunk_trailer_len(0x73, 0, 0, 0, 6), 8);
        assert_eq!(chunk_trailer_len(0x76, 0, 0, 0, 6), 8);
        assert_eq!(chunk_trailer_len(0x2c, 0, 0, 0, 6), 8);
        assert_eq!(chunk_trailer_len(0x0d, 0, 0, 0, 6), 8);
        assert_eq!(
            chunk_trailer_len(0x0e, 0, 0, 0, 6),
            0,
            "a text chunk with no list has no trailer"
        );
        assert_eq!(
            chunk_trailer_len(0x0e, 7, 0, 0, 6),
            8,
            "a non-zero list always carries one"
        );
        assert_eq!(chunk_trailer_len(0x1f, 9, 0, 0, 6), 0, "OLE data never has a trailer");
        assert_eq!(chunk_trailer_len(0xc9, 0, 0, 0, 6), 0);
        assert_eq!(
            chunk_trailer_len(0x74, 0, 0, 0, 6),
            0,
            "0x74/0x75 are outside the v6 set"
        );
    }

    /// The v11+ computation mirrors `VSDParser::getChunkHeader`: the 8-byte
    /// stage, the gated 4-byte stage, the array types that take 4 only when the
    /// stages did not already fire, and the four never-trailer types zeroing it
    /// all. These cases are the shapes the old two-predicate version got wrong.
    #[test]
    fn v11_chunk_trailer_matches_the_reference() {
        // Array type, no stage fired: the array adds the 4 the old code dropped.
        assert_eq!(chunk_trailer_len(0x64, 0, 1, 0x00, 11), 4);
        assert_eq!(chunk_trailer_len(0x64, 0, 0, 0x00, 11), 4);
        assert_eq!(chunk_trailer_len(0x92, 0, 1, 0x00, 11), 4);
        assert_eq!(chunk_trailer_len(0x6f, 0, 1, 0x00, 11), 4);
        // Set8 type outside the array: stays at 8 (the old fallback over-advanced).
        assert_eq!(chunk_trailer_len(0x2c, 0, 1, 0x00, 11), 8);
        assert_eq!(chunk_trailer_len(0x70, 0, 1, 0x00, 11), 8);
        // Level 3 with unknown 0x54: the reference excludes it (the old code didn't).
        assert_eq!(chunk_trailer_len(0x0e, 0, 3, 0x54, 11), 0);
        assert_eq!(chunk_trailer_len(0x0e, 0, 3, 0x50, 11), 0);
        // 0x1f/0xc9/0x2d/0xd1 zero out even when earlier stages fired.
        assert_eq!(chunk_trailer_len(0x1f, 5, 2, 0x55, 11), 0);
        assert_eq!(chunk_trailer_len(0xd1, 5, 2, 0x55, 11), 0);
        assert_eq!(chunk_trailer_len(0x2d, 0, 3, 0x10, 11), 0);
        // Known-good shapes the old code already handled.
        assert_eq!(chunk_trailer_len(0x69, 0, 1, 0x00, 11), 12);
        assert_eq!(chunk_trailer_len(0xaa, 0, 2, 0x54, 11), 4);
        assert_eq!(chunk_trailer_len(0x71, 3, 2, 0x55, 11), 12);
        assert_eq!(chunk_trailer_len(0x0e, 0, 0, 0x00, 11), 0);
    }

    /// Build a minimal ZIP in memory with `count` page parts.
    fn make_zip_with_entries(count: usize) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for index in 0..count {
            zip.start_file(format!("visio/pages/page{index}.xml"), options).unwrap();
            zip.write_all(b"<pages/>").unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn validate_package_container_accepts_an_ordinary_package() {
        let bytes = make_zip_with_entries(2);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert!(
            validate_package_container(&mut archive, &SecurityLimits::default()).is_ok(),
            "a two-entry package under every default limit must validate"
        );
    }

    /// The entry-count ceiling comes from the caller's `SecurityLimits`, not a
    /// built-in default: a three-entry package against a configured limit of two
    /// must be rejected with the limit named.
    #[test]
    fn validate_package_container_rejects_package_over_the_configured_entry_limit() {
        let bytes = make_zip_with_entries(3);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        let limits = SecurityLimits {
            max_files_in_archive: 2,
            ..Default::default()
        };

        let error = validate_package_container(&mut archive, &limits)
            .expect_err("a three-entry package against a limit of two must be rejected");
        assert!(
            error.to_string().contains("2"),
            "the error must name the configured limit: {error}"
        );
    }

    /// Build a flat "VDX-style" package (`document.xml` + `pages/page1.xml`, the
    /// layout an OLE `Package` stream or an EDDX part uses) whose page nests the
    /// same `Text` tag the way EDDX writes it.
    fn make_eddx_style_package(page1: &str) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        zip.start_file("document.xml", options).unwrap();
        zip.write_all(b"<document/>").unwrap();
        zip.start_file("pages/page1.xml", options).unwrap();
        zip.write_all(page1.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    /// EDDX nests `<Text>` inside `<Text>`; the reader must emit each string once,
    /// not once per matching element (the outer pass already collects everything).
    #[test]
    fn eddx_nested_text_elements_are_collected_once() {
        let page1 = r##"<page>
            <Text>
                <TextBlock>
                    <Text>
                        <pp><tp>ARCH41_CORE4</tp></pp>
                    </Text>
                </TextBlock>
            </Text>
            <Text>
                <TextBlock>
                    <Text><pp><tp>macro join</tp></pp></Text>
                </TextBlock>
            </Text>
        </page>"##;
        let bytes = make_eddx_style_package(page1);
        let text = extract_visio_package_text(&bytes, &SecurityLimits::default())
            .expect("a well-formed flat package must extract");

        assert_eq!(
            text.iter().filter(|t| t.as_str() == "ARCH41_CORE4").count(),
            1,
            "each nested-Text string must appear exactly once, got {text:?}"
        );
        assert_eq!(
            text.iter().filter(|t| t.as_str() == "macro join").count(),
            1,
            "each nested-Text string must appear exactly once, got {text:?}"
        );
    }

    /// Ordinary VSDX parts (no `Text` nesting) keep their exact per-shape output.
    #[test]
    fn vsdx_flat_text_elements_are_unchanged() {
        let page1 = r##"<page>
            <Shapes>
                <Shape><Text>Alpha<x>br</x></Text></Shape>
                <Shape><Text>Beta</Text></Shape>
            </Shapes>
        </page>"##;
        let bytes = make_eddx_style_package(page1);
        let text = extract_visio_package_text(&bytes, &SecurityLimits::default())
            .expect("a well-formed flat package must extract");

        assert_eq!(text, vec!["Alphabr".to_string(), "Beta".to_string()], "got {text:?}");
    }
}
