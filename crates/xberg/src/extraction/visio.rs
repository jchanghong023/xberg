//! Native extraction of text from legacy Visio binary documents.
//!
//! Legacy `.vsd` files are OLE compound documents. Their `VisioDocument` stream
//! contains a pointer tree whose leaf chunk records carry shape text. This module
//! implements the bounded v5/v6+ pointer, chunk, and Visio-LZW readers needed to
//! recover that text without delegating to an external office converter.

use crate::Result;
use crate::XbergError;
use std::collections::HashSet;
use std::io::{Cursor, Read};

const VISIO_HEADER: &[u8] = b"Visio (TM) Drawing\r\n";
const VISIO_DOCUMENT_OFFSET: usize = 0x24;
const MAX_CHILD_POINTERS: usize = 100_000;
const MAX_CHILD_DEPTH: usize = 512;
const MAX_TEXT_CHUNKS: usize = 100_000;
const LZW_DICTIONARY_SIZE: usize = 4096;

/// Extract the individual shape-text records from a legacy Visio document.
///
/// `max_stream_size` limits both the OLE `VisioDocument` stream and every
/// decompressed Visio stream. It is supplied by the caller's archive/security
/// budget so malformed files cannot grow an unbounded allocation.
pub(crate) fn extract_visio_text(content: &[u8], max_stream_size: usize) -> Result<Vec<String>> {
    let mut compound_file = cfb::CompoundFile::open(Cursor::new(content))
        .map_err(|error| XbergError::parsing(format!("Failed to open VSD as OLE container: {error}")))?;

    let mut stream = compound_file
        .open_stream("/VisioDocument")
        .or_else(|_| compound_file.open_stream("VisioDocument"))
        .map_err(|error| XbergError::parsing(format!("Failed to open VisioDocument stream: {error}")))?;

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
        max_stream_size,
        visited: HashSet::new(),
        text: Vec::new(),
    };
    parser.scan_stream(root_pointer, 0)?;
    Ok(parser.text)
}

struct VisioParser<'a> {
    document: &'a [u8],
    version: u16,
    max_stream_size: usize,
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
            return Err(XbergError::parsing("Visio stream nesting exceeds the safety limit"));
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
            if let Ok(children) = self.parse_child_pointers(pointer, &stream.contents) {
                for child in children {
                    // A damaged child must not hide valid siblings. The root stream
                    // remains fatal when it cannot be read, while malformed descendants
                    // are skipped after the surrounding document has been recovered.
                    let _ = self.scan_stream(child, depth + 1);
                }
            }
        }

        if pointer_has_chunks(pointer, self.version) {
            self.scan_chunks(&stream);
        }

        Ok(())
    }

    fn read_stream(&self, pointer: Pointer) -> Result<StreamData> {
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

        let raw = &self.document[pointer.offset..end];
        if !pointer_compressed(pointer) {
            return Ok(StreamData {
                contents: raw.to_vec(),
                block_header: None,
            });
        }

        let decompressed = decode_visio_lzw(raw, self.max_stream_size)?;
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

    fn parse_child_pointers(&self, parent: Pointer, contents: &[u8]) -> Result<Vec<Pointer>> {
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
            let count_offset = match parent.kind {
                0x1d | 0x4e => 30,
                0x1e => 54,
                0x14 => 130,
                _ => 10,
            };
            let count = read_u16(contents, count_offset)
                .ok_or_else(|| XbergError::parsing("Visio pointer container count is truncated"))?
                as usize;
            (count_offset, count, 2usize)
        };

        if count > MAX_CHILD_POINTERS {
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
                let text = decode_visio_text(&contents[text_start..body_end], self.version >= 11);
                if !text.is_empty() && text != "\n" {
                    self.text.push(text);
                    if self.text.len() >= MAX_TEXT_CHUNKS {
                        break;
                    }
                }
            }

            let trailer_len = if has_chunk_trailer(chunk_type, unknown1, self.version) {
                8
            } else {
                0
            };
            let separator_len = if has_chunk_separator(chunk_type, unknown2, unknown3, self.version, trailer_len != 0) {
                4
            } else {
                0
            };
            let Some(next) = body_end
                .checked_add(trailer_len)
                .and_then(|end| end.checked_add(separator_len))
            else {
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
        Some(Pointer {
            kind: read_u16(data, offset)? as u32,
            offset: read_u32(data, offset + 8)? as usize,
            length: read_u32(data, offset + 12)? as usize,
            format: read_u16(data, offset + 2)?,
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

fn has_chunk_trailer(chunk_type: u32, unknown1: u32, version: u16) -> bool {
    version >= 6 && matches!(chunk_type, 0x2c | 0x65 | 0x66 | 0x69 | 0x6a | 0x6b | 0x70 | 0x71)
        || (version >= 6 && unknown1 != 0)
}

fn has_chunk_separator(chunk_type: u32, unknown2: u16, unknown3: u8, version: u16, has_trailer: bool) -> bool {
    if version <= 6 {
        return false;
    }
    if matches!(chunk_type, 0x1f | 0xc9) {
        return false;
    }
    if chunk_type == 0x69 {
        return true;
    }
    if matches!(chunk_type, 0xa9 | 0xaa | 0xb4 | 0xb6) && unknown2 == 2 && unknown3 == 0x54 {
        return true;
    }
    if (unknown2 == 2 && unknown3 == 0x55) || (unknown2 == 3 && unknown3 != 0x50) {
        return true;
    }
    has_trailer
}

fn decode_visio_text(data: &[u8], utf16: bool) -> String {
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
        let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(data);
        decoded.trim_end_matches('\0').to_string()
    }
}

fn decode_visio_lzw(data: &[u8], max_size: usize) -> Result<Vec<u8>> {
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
                    return Err(XbergError::parsing(
                        "Decompressed Visio stream exceeds its safety limit",
                    ));
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
                    return Err(XbergError::parsing(
                        "Decompressed Visio stream exceeds its safety limit",
                    ));
                }

                let pointer = if first as usize + ((second as usize & 0xf0) << 4) > 4078 {
                    first as usize + ((second as usize & 0xf0) << 4) - 4078
                } else {
                    first as usize + ((second as usize & 0xf0) << 4) + 18
                };
                let mut copied = [0u8; 18];
                for (index, byte) in copied.iter_mut().take(length).enumerate() {
                    *byte = dictionary[(pointer + index) & (LZW_DICTIONARY_SIZE - 1)];
                }
                for &byte in copied.iter().take(length) {
                    dictionary[output_position & (LZW_DICTIONARY_SIZE - 1)] = byte;
                    output.push(byte);
                    output_position += 1;
                }
            }
            mask <<= 1;
        }
    }

    if truncated && output.len() < 4 {
        return Err(XbergError::parsing("Truncated Visio LZW stream"));
    }
    if output.len() < 4 {
        return Err(XbergError::parsing("Visio LZW stream has no block header"));
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
