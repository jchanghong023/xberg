//! CFF (Compact Font Format) encoding parser.
//!
//! Parses the built-in encoding table from CFF font programs and resolves
//! PDF content-stream bytes to glyph IDs.
//!
//! # Two byte → GID resolution paths
//!
//! Simple CFF fonts have two potential sources of byte → glyph mapping
//! information, and PDFs decide between them according to ISO 32000-1
//! §9.6.6:
//!
//! 1. **The PDF font dictionary's `/Encoding`** is authoritative for
//!    simple fonts (Type 1 / TrueType / CFF). It supplies byte → glyph
//!    *name*; the CFF Charset then resolves the name → SID → GID.
//!    [`parse_cff_gid_mapping_with_pdf_encoding`] implements this path.
//!
//! 2. **The CFF font program's own Encoding table** (CFF Tech Note #5176
//!    §12) supplies byte → GID directly. This is the *fallback* when no
//!    PDF-level encoding is supplied (e.g. an `Encoding::Identity` caller).
//!    [`parse_cff_gid_mapping`] implements this path.
//!
//! Subsetter-emitted CFF Encoding tables are frequently sparse —
//! some prepress subsetters commonly emit only `0x20 → space` and
//! `0x41 → A` while the Charset enumerates the full subset — so callers
//! that have the PDF `/Encoding` in hand should always go through the
//! `_with_pdf_encoding` entrypoint. The CFF Encoding path is preserved
//! for backwards compatibility and as a final fallback when the PDF side
//! cannot supply a byte → name resolver.
//!
//! Per PDF spec §9.6.6.2, when no `/BaseEncoding` is specified in an
//! encoding dictionary, the implicit base encoding is the font program's
//! built-in encoding — this module also provides [`parse_cff_encoding`]
//! for that legacy lookup.

use std::collections::HashMap;

/// Standard CFF string IDs (SIDs) 0-390 per CFF specification Table A-2.
/// Only the subset needed for encoding (common glyph names).
fn sid_to_name(sid: u16) -> Option<&'static str> {
    // CFF predefined strings (SID 0-390)
    // Full table from Adobe CFF specification, Appendix A ~keep
    static SID_NAMES: &[&str] = &[
        ".notdef",
        "space",
        "exclam",
        "quotedbl",
        "numbersign",
        "dollar",
        "percent",
        "ampersand",
        "quoteright",
        "parenleft",
        "parenright",
        "asterisk",
        "plus",
        "comma",
        "hyphen",
        "period",
        "slash",
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "colon",
        "semicolon",
        "less",
        "equal",
        "greater",
        "question",
        "at",
        "A",
        "B",
        "C",
        "D",
        "E",
        "F",
        "G",
        "H",
        "I",
        "J",
        "K",
        "L",
        "M",
        "N",
        "O",
        "P",
        "Q",
        "R",
        "S",
        "T",
        "U",
        "V",
        "W",
        "X",
        "Y",
        "Z",
        "bracketleft",
        "backslash",
        "bracketright",
        "asciicircum",
        "underscore",
        "quoteleft",
        "a",
        "b",
        "c",
        "d",
        "e",
        "f",
        "g",
        "h",
        "i",
        "j",
        "k",
        "l",
        "m",
        "n",
        "o",
        "p",
        "q",
        "r",
        "s",
        "t",
        "u",
        "v",
        "w",
        "x",
        "y",
        "z",
        "braceleft",
        "bar",
        "braceright",
        "asciitilde",
        "exclamdown",
        "cent",
        "sterling",
        "fraction",
        "yen",
        "florin",
        "section",
        "currency",
        "quotesingle",
        "quotedblleft",
        "guillemotleft",
        "guilsinglleft",
        "guilsinglright",
        "fi",
        "fl",
        "endash",
        "dagger",
        "daggerdbl",
        "periodcentered",
        "paragraph",
        "bullet",
        "quotesinglbase",
        "quotedblbase",
        "quotedblright",
        "guillemotright",
        "ellipsis",
        "perthousand",
        "questiondown",
        "grave",
        "acute",
        "circumflex",
        "tilde",
        "macron",
        "breve",
        "dotaccent",
        "dieresis",
        "ring",
        "cedilla",
        "hungarumlaut",
        "ogonek",
        "caron",
        "emdash",
        "AE",
        "ordfeminine",
        "Lslash",
        "Oslash",
        "OE",
        "ordmasculine",
        "ae",
        "dotlessi",
        "lslash",
        "oslash",
        "oe",
        "germandbls",
        "onesuperior",
        "logicalnot",
        "mu",
        "trademark",
        "Eth",
        "onehalf",
        "plusminus",
        "Thorn",
        "onequarter",
        "divide",
        "brokenbar",
        "degree",
        "thorn",
        "threequarters",
        "twosuperior",
        "registered",
        "minus",
        "eth",
        "multiply",
        "threesuperior",
        "copyright",
        "Aacute",
        "Acircumflex",
        "Adieresis",
        "Agrave",
        "Aring",
        "Atilde",
        "Ccedilla",
        "Eacute",
        "Ecircumflex",
        "Edieresis",
        "Egrave",
        "Iacute",
        "Icircumflex",
        "Idieresis",
        "Igrave",
        "Ntilde",
        "Oacute",
        "Ocircumflex",
        "Odieresis",
        "Ograve",
        "Otilde",
        "Scaron",
        "Uacute",
        "Ucircumflex",
        "Udieresis",
        "Ugrave",
        "Yacute",
        "Ydieresis",
        "Zcaron",
        "aacute",
        "acircumflex",
        "adieresis",
        "agrave",
        "aring",
        "atilde",
        "ccedilla",
        "eacute",
        "ecircumflex",
        "edieresis",
        "egrave",
        "iacute",
        "icircumflex",
        "idieresis",
        "igrave",
        "ntilde",
        "oacute",
        "ocircumflex",
        "odieresis",
        "ograve",
        "otilde",
        "scaron",
        "uacute",
        "ucircumflex",
        "udieresis",
        "ugrave",
        "yacute",
        "ydieresis",
        "zcaron",
        "exclamsmall",
        "Hungarumlautsmall",
        "dollaroldstyle",
        "dollarsuperior",
        "ampersandsmall",
        "Acutesmall",
        "parenleftsuperior",
        "parenrightsuperior",
        "twodotenleader",
        "onedotenleader",
        "zerooldstyle",
        "oneoldstyle",
        "twooldstyle",
        "threeoldstyle",
        "fouroldstyle",
        "fiveoldstyle",
        "sixoldstyle",
        "sevenoldstyle",
        "eightoldstyle",
        "nineoldstyle",
        "commasuperior",
        "threequartersemdash",
        "periodsuperior",
        "questionsmall",
        "asuperior",
        "bsuperior",
        "centsuperior",
        "dsuperior",
        "esuperior",
        "isuperior",
        "lsuperior",
        "msuperior",
        "nsuperior",
        "osuperior",
        "rsuperior",
        "ssuperior",
        "tsuperior",
        "ff",
        "ffi",
        "ffl",
        "parenleftinferior",
        "parenrightinferior",
        "Circumflexsmall",
        "hyphensuperior",
        "Gravesmall",
        "Asmall",
        "Bsmall",
        "Csmall",
        "Dsmall",
        "Esmall",
        "Fsmall",
        "Gsmall",
        "Hsmall",
        "Ismall",
        "Jsmall",
        "Ksmall",
        "Lsmall",
        "Msmall",
        "Nsmall",
        "Osmall",
        "Psmall",
        "Qsmall",
        "Rsmall",
        "Ssmall",
        "Tsmall",
        "Usmall",
        "Vsmall",
        "Wsmall",
        "Xsmall",
        "Ysmall",
        "Zsmall",
        "colonmonetary",
        "onefitted",
        "rupiah",
        "Tildesmall",
        "exclamdownsmall",
        "centoldstyle",
        "Lslashsmall",
        "Scaronsmall",
        "Zcaronsmall",
        "Dieresissmall",
        "Brevesmall",
        "Caronsmall",
        "Dotaccentsmall",
        "Macronsmall",
        "figuredash",
        "hypheninferior",
        "Ogoneksmall",
        "Ringsmall",
        "Cedillasmall",
        "questiondownsmall",
        "oneeighth",
        "threeeighths",
        "fiveeighths",
        "seveneighths",
        "onethird",
        "twothirds",
        "zerosuperior",
        "foursuperior",
        "fivesuperior",
        "sixsuperior",
        "sevensuperior",
        "eightsuperior",
        "ninesuperior",
        "zeroinferior",
        "oneinferior",
        "twoinferior",
        "threeinferior",
        "fourinferior",
        "fiveinferior",
        "sixinferior",
        "seveninferior",
        "eightinferior",
        "nineinferior",
        "centinferior",
        "dollarinferior",
        "periodinferior",
        "commainferior",
        "Agravesmall",
        "Aacutesmall",
        "Acircumflexsmall",
        "Atildesmall",
        "Adieresissmall",
        "Aringsmall",
        "AEsmall",
        "Ccedillasmall",
        "Egravesmall",
        "Eacutesmall",
        "Ecircumflexsmall",
        "Edieresissmall",
        "Igravesmall",
        "Iacutesmall",
        "Icircumflexsmall",
        "Idieresissmall",
        "Ethsmall",
        "Ntildesmall",
        "Ogravesmall",
        "Oacutesmall",
        "Ocircumflexsmall",
        "Otildesmall",
        "Odieresissmall",
        "OEsmall",
        "Oslashsmall",
        "Ugravesmall",
        "Uacutesmall",
        "Ucircumflexsmall",
        "Udieresissmall",
        "Yacutesmall",
        "Thornsmall",
        "Ydieresissmall",
        "001.000",
        "001.001",
        "001.002",
        "001.003",
        "Black",
        "Bold",
        "Book",
        "Light",
        "Medium",
        "Regular",
        "Roman",
        "Semibold",
    ];

    if (sid as usize) < SID_NAMES.len() {
        Some(SID_NAMES[sid as usize])
    } else {
        None
    }
}

/// Look up the SID for a standard glyph name by searching the predefined SID table.
fn glyph_name_to_sid(name: &str) -> Option<u16> {
    for sid in 0u16..391 {
        if sid_to_name(sid) == Some(name) {
            return Some(sid);
        }
    }
    None
}

/// Parse a CFF INDEX structure, returning byte slices for each entry.
fn parse_index(data: &[u8], offset: usize) -> Option<(Vec<&[u8]>, usize)> {
    if offset + 2 > data.len() {
        return None;
    }
    let count = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
    if count == 0 {
        return Some((Vec::new(), offset + 2));
    }

    if offset + 3 > data.len() {
        return None;
    }
    let off_size = data[offset + 2] as usize;
    if off_size == 0 || off_size > 4 {
        return None;
    }

    let offset_array_start = offset + 3;
    let offset_array_len = (count + 1) * off_size;
    if offset_array_start + offset_array_len > data.len() {
        return None;
    }

    let mut offsets = Vec::with_capacity(count + 1);
    for i in 0..=count {
        let pos = offset_array_start + i * off_size;
        let mut val: u32 = 0;
        for j in 0..off_size {
            val = (val << 8) | data[pos + j] as u32;
        }
        offsets.push(val as usize);
    }

    let data_start = offset_array_start + offset_array_len;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let start = data_start + offsets[i] - 1; // CFF offsets are 1-based ~keep
        let end = data_start + offsets[i + 1] - 1;
        if start > data.len() || end > data.len() || start > end {
            return None;
        }
        entries.push(&data[start..end]);
    }

    let next_offset = data_start + offsets[count] - 1;
    Some((entries, next_offset))
}

/// Parse a CFF DICT operand (integer or real).
/// Returns (value, bytes consumed).
fn parse_dict_operand(data: &[u8], pos: usize) -> Option<(i32, usize)> {
    if pos >= data.len() {
        return None;
    }
    let b0 = data[pos] as i32;
    match b0 {
        // Integer: 1 byte ~keep
        32..=246 => Some((b0 - 139, 1)),
        // Integer: 2 bytes ~keep
        247..=250 => {
            if pos + 1 >= data.len() {
                return None;
            }
            let b1 = data[pos + 1] as i32;
            Some(((b0 - 247) * 256 + b1 + 108, 2))
        }
        251..=254 => {
            if pos + 1 >= data.len() {
                return None;
            }
            let b1 = data[pos + 1] as i32;
            Some((-(b0 - 251) * 256 - b1 - 108, 2))
        }
        // Integer: 3 bytes (16-bit) ~keep
        28 => {
            if pos + 2 >= data.len() {
                return None;
            }
            let val = i16::from_be_bytes([data[pos + 1], data[pos + 2]]) as i32;
            Some((val, 3))
        }
        // Integer: 5 bytes (32-bit) ~keep
        29 => {
            if pos + 4 >= data.len() {
                return None;
            }
            let val = i32::from_be_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]]);
            Some((val, 5))
        }
        // Real number (skip, we only need integers for encoding/charset offsets) ~keep
        30 => {
            let mut i = pos + 1;
            while i < data.len() {
                let nibble1 = (data[i] >> 4) & 0x0F;
                let nibble2 = data[i] & 0x0F;
                if nibble1 == 0x0F || nibble2 == 0x0F {
                    return Some((0, i - pos + 1));
                }
                i += 1;
            }
            None
        }
        _ => None,
    }
}

/// Parse a CFF Top DICT to extract encoding and charset offsets.
fn parse_top_dict(dict_data: &[u8]) -> (i32, i32) {
    let mut encoding_offset: i32 = 0; // Default: StandardEncoding ~keep
    let mut charset_offset: i32 = 0; // Default: ISOAdobe charset ~keep

    let mut pos = 0;
    let mut operand_stack: Vec<i32> = Vec::new();

    while pos < dict_data.len() {
        let b0 = dict_data[pos];
        if b0 <= 21 {
            let op = if b0 == 12 {
                pos += 1;
                if pos >= dict_data.len() {
                    break;
                }
                (12u16 << 8) | dict_data[pos] as u16
            } else {
                b0 as u16
            };

            match op {
                16 => {
                    if let Some(&val) = operand_stack.last() {
                        encoding_offset = val;
                    }
                }
                15 => {
                    if let Some(&val) = operand_stack.last() {
                        charset_offset = val;
                    }
                }
                _ => {}
            }

            operand_stack.clear();
            pos += 1;
        } else if let Some((val, consumed)) = parse_dict_operand(dict_data, pos) {
            operand_stack.push(val);
            pos += consumed;
        } else {
            pos += 1;
        }
    }

    (encoding_offset, charset_offset)
}

/// Parse the CFF Top DICT and surface (CharStrings, Encoding, charset)
/// offsets. CharStrings (op 17) points at the CharStrings INDEX whose count
/// field is the real `nGlyphs` for the font — needed to parse the full
/// Charset for subsets enumerating >256 glyphs.
///
/// Returns `(charstrings_offset, encoding_offset, charset_offset)`.
fn parse_top_dict_with_charstrings(dict_data: &[u8]) -> (i32, i32, i32) {
    let mut charstrings_offset: i32 = 0;
    let mut encoding_offset: i32 = 0;
    let mut charset_offset: i32 = 0;

    let mut pos = 0;
    let mut operand_stack: Vec<i32> = Vec::new();

    while pos < dict_data.len() {
        let b0 = dict_data[pos];
        if b0 <= 21 {
            let op = if b0 == 12 {
                pos += 1;
                if pos >= dict_data.len() {
                    break;
                }
                (12u16 << 8) | dict_data[pos] as u16
            } else {
                b0 as u16
            };

            match op {
                15 => {
                    if let Some(&val) = operand_stack.last() {
                        charset_offset = val;
                    }
                }
                16 => {
                    if let Some(&val) = operand_stack.last() {
                        encoding_offset = val;
                    }
                }
                17 => {
                    if let Some(&val) = operand_stack.last() {
                        charstrings_offset = val;
                    }
                }
                _ => {}
            }

            operand_stack.clear();
            pos += 1;
        } else if let Some((val, consumed)) = parse_dict_operand(dict_data, pos) {
            operand_stack.push(val);
            pos += consumed;
        } else {
            pos += 1;
        }
    }

    (charstrings_offset, encoding_offset, charset_offset)
}

/// Read the 2-byte big-endian `count` field at the start of a CFF INDEX
/// header (CFF spec §5). For the CharStrings INDEX, this count is `nGlyphs`.
///
/// Returns `None` if the header is truncated.
fn read_index_count(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]) as u32)
}

/// Parse the CFF charset table.
/// Returns GID → SID mapping (GID 0 is always .notdef).
fn parse_charset(data: &[u8], offset: usize, num_glyphs: usize) -> Option<Vec<u16>> {
    if offset >= data.len() {
        return None;
    }

    let mut sids = Vec::with_capacity(num_glyphs);
    sids.push(0); // GID 0 = .notdef (SID 0) ~keep

    let format = data[offset];
    let mut pos = offset + 1;

    match format {
        0 => {
            // Format 0: array of SIDs ~keep
            for _ in 1..num_glyphs {
                if pos + 1 >= data.len() {
                    break;
                }
                let sid = u16::from_be_bytes([data[pos], data[pos + 1]]);
                sids.push(sid);
                pos += 2;
            }
        }
        1 => {
            // Format 1: ranges with 1-byte count ~keep
            while sids.len() < num_glyphs && pos + 2 < data.len() {
                let first_sid = u16::from_be_bytes([data[pos], data[pos + 1]]);
                let n_left = data[pos + 2] as u16;
                pos += 3;
                for i in 0..=n_left {
                    if sids.len() >= num_glyphs {
                        break;
                    }
                    sids.push(first_sid + i);
                }
            }
        }
        2 => {
            // Format 2: ranges with 2-byte count ~keep
            while sids.len() < num_glyphs && pos + 3 < data.len() {
                let first_sid = u16::from_be_bytes([data[pos], data[pos + 1]]);
                let n_left = u16::from_be_bytes([data[pos + 2], data[pos + 3]]);
                pos += 4;
                for i in 0..=n_left {
                    if sids.len() >= num_glyphs {
                        break;
                    }
                    sids.push(first_sid + i);
                }
            }
        }
        _ => return None,
    }

    Some(sids)
}

/// Parse the CFF encoding table.
/// Returns character code → GID mapping.
fn parse_encoding_table(data: &[u8], offset: usize) -> Option<HashMap<u8, u16>> {
    if offset >= data.len() {
        return None;
    }

    let mut code_to_gid = HashMap::new();
    let format = data[offset] & 0x7F; // Bit 7 is supplement flag ~keep
    let has_supplement = (data[offset] & 0x80) != 0;
    let mut pos = offset + 1;

    match format {
        0 => {
            // Format 0: array of codes ~keep
            if pos >= data.len() {
                return None;
            }
            let n_codes = data[pos] as usize;
            pos += 1;
            for gid in 1..=n_codes {
                if pos >= data.len() {
                    break;
                }
                let code = data[pos];
                code_to_gid.insert(code, gid as u16);
                pos += 1;
            }
        }
        1 => {
            // Format 1: ranges ~keep
            if pos >= data.len() {
                return None;
            }
            let n_ranges = data[pos] as usize;
            pos += 1;
            let mut gid: u16 = 1;
            for _ in 0..n_ranges {
                if pos + 1 >= data.len() {
                    break;
                }
                let first = data[pos];
                let n_left = data[pos + 1] as u16;
                pos += 2;
                for i in 0..=n_left {
                    let code = first.wrapping_add(i as u8);
                    code_to_gid.insert(code, gid);
                    gid += 1;
                }
            }
        }
        _ => return None,
    }

    if has_supplement && pos < data.len() {
        let n_sups = data[pos] as usize;
        pos += 1;
        for _ in 0..n_sups {
            if pos + 2 >= data.len() {
                break;
            }
            let code = data[pos];
            let sid = u16::from_be_bytes([data[pos + 1], data[pos + 2]]);
            pos += 3;
            // For supplements, we use SID directly as a pseudo-GID
            // The caller will need to handle this via the charset ~keep
            code_to_gid.insert(code, sid);
        }
    }

    Some(code_to_gid)
}

/// Resolve a glyph name from a SID, using predefined strings and the
/// String INDEX from the CFF font.
fn resolve_glyph_name<'a>(sid: u16, string_index: &'a [&'a [u8]]) -> Option<String> {
    if sid <= 390 {
        sid_to_name(sid).map(|s| s.to_string())
    } else {
        let idx = (sid - 391) as usize;
        if idx < string_index.len() {
            std::str::from_utf8(string_index[idx]).ok().map(|s| s.to_string())
        } else {
            None
        }
    }
}

/// Extract the CFF table from an OpenType (sfnt) wrapper.
/// Returns the CFF data slice if found, or None if the data isn't an sfnt container.
fn extract_cff_from_opentype(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 12 {
        return None;
    }
    let magic = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    // Check for OpenType "OTTO" or TrueType 0x00010000 ~keep
    if magic != 0x4F54544F && magic != 0x00010000 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    // Table directory starts at offset 12 ~keep
    let mut pos = 12;
    for _ in 0..num_tables {
        if pos + 16 > data.len() {
            return None;
        }
        let tag = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let offset = u32::from_be_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]]) as usize;
        let length = u32::from_be_bytes([data[pos + 12], data[pos + 13], data[pos + 14], data[pos + 15]]) as usize;
        // CFF tag = 0x43464620 ("CFF ") ~keep
        if tag == 0x43464620 && offset + length <= data.len() {
            return Some(&data[offset..offset + length]);
        }
        pos += 16;
    }
    None
}

/// Extract the built-in encoding from a CFF font program.
///
/// Returns a HashMap mapping character codes (u8) to Unicode characters.
/// This implements the CFF encoding → charset → glyph name → Unicode pipeline.
/// Also handles OpenType-wrapped CFF data (FontFile3 with sfnt container).
pub fn parse_cff_encoding(font_data: &[u8]) -> Option<HashMap<u8, char>> {
    if font_data.len() < 4 {
        return None;
    }

    let cff_data = if font_data[0] != 1 {
        if let Some(cff) = extract_cff_from_opentype(font_data) {
            tracing::debug!(
                "Extracted CFF table ({} bytes) from OpenType wrapper ({} bytes)",
                cff.len(),
                font_data.len()
            );
            cff
        } else {
            crate::extractors::recovery_tally::record(|counts| {
                counts
                    .unsupported_cff_versions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            tracing::trace!("CFF version {} not supported (expected 1)", font_data[0]);
            return None;
        }
    } else {
        font_data
    };

    if cff_data.len() < 4 || cff_data[0] != 1 {
        return None;
    }
    let hdr_size = cff_data[2] as usize;

    let (_, after_name) = parse_index(cff_data, hdr_size)?;

    let (top_dicts, after_top_dict) = parse_index(cff_data, after_name)?;
    if top_dicts.is_empty() {
        return None;
    }

    let (string_index, _after_string) = parse_index(cff_data, after_top_dict)?;

    let (encoding_offset, charset_offset) = parse_top_dict(top_dicts[0]);

    if encoding_offset == 1 {
        // ExpertEncoding — rarely used for text ~keep
        tracing::debug!("CFF uses ExpertEncoding (predefined)");
        return None;
    }

    if encoding_offset == 0 {
        // StandardEncoding — for fonts with custom charsets, build a GID-based
        // fallback map. This handles subset fonts where character codes equal
        // GIDs rather than standard encoding positions. ~keep
        if charset_offset > 2 {
            tracing::debug!("CFF StandardEncoding + custom charset; building charset-based map");
            let num_glyphs = 256usize;
            let charset_sids = parse_charset(cff_data, charset_offset as usize, num_glyphs)?;

            let mut encoding_map = HashMap::new();
            for (gid, &sid) in charset_sids.iter().enumerate() {
                if gid == 0 || gid > 255 {
                    continue;
                }
                if let Some(glyph_name) = resolve_glyph_name(sid, &string_index)
                    && let Some(unicode_char) = super::font_dict::glyph_name_to_unicode(&glyph_name)
                {
                    encoding_map.insert(gid as u8, unicode_char);
                }
            }
            if !encoding_map.is_empty() {
                tracing::debug!("CFF charset-based fallback: {} character mappings", encoding_map.len());
                return Some(encoding_map);
            }
        }
        tracing::debug!("CFF uses StandardEncoding (predefined)");
        return None;
    }

    let code_to_gid = parse_encoding_table(cff_data, encoding_offset as usize)?;

    let max_gid = code_to_gid.values().max().copied().unwrap_or(0) as usize;
    let num_glyphs = max_gid + 10;

    let charset_sids = if charset_offset == 0 {
        (0..num_glyphs as u16).collect()
    } else if charset_offset == 1 || charset_offset == 2 {
        tracing::debug!("CFF uses predefined charset {}", charset_offset);
        return None;
    } else {
        parse_charset(cff_data, charset_offset as usize, num_glyphs)?
    };

    let mut encoding_map = HashMap::new();

    for (&code, &gid) in &code_to_gid {
        let sid = if (gid as usize) < charset_sids.len() {
            charset_sids[gid as usize]
        } else {
            continue;
        };

        if let Some(glyph_name) = resolve_glyph_name(sid, &string_index)
            && let Some(unicode_char) = super::font_dict::glyph_name_to_unicode(&glyph_name)
        {
            encoding_map.insert(code, unicode_char);
        }
    }

    if encoding_map.is_empty() {
        None
    } else {
        tracing::debug!(
            "CFF built-in encoding parsed: {} character mappings",
            encoding_map.len()
        );
        Some(encoding_map)
    }
}

/// Parse a CFF font program and return a byte_code → glyph_id mapping.
/// This allows rendering CFF subset fonts by mapping PDF character codes
/// directly to glyph indices without needing a Unicode cmap.
pub fn parse_cff_gid_mapping(font_data: &[u8]) -> Option<HashMap<u8, u16>> {
    if font_data.len() < 4 {
        return None;
    }

    let cff_data = if font_data[0] != 1 {
        extract_cff_from_opentype(font_data)?
    } else {
        font_data
    };

    if cff_data.len() < 4 || cff_data[0] != 1 {
        return None;
    }
    let hdr_size = cff_data[2] as usize;

    let (_, after_name) = parse_index(cff_data, hdr_size)?;
    let (top_dicts, after_top_dict) = parse_index(cff_data, after_name)?;
    if top_dicts.is_empty() {
        return None;
    }

    let (_string_index, _after_string) = parse_index(cff_data, after_top_dict)?;
    let (encoding_offset, charset_offset) = parse_top_dict(top_dicts[0]);

    if encoding_offset == 0 && charset_offset > 2 {
        // StandardEncoding + custom charset:
        // byte_code → SID (via CFF Standard Encoding) → GID (via charset) ~keep
        let num_glyphs = 256usize;
        if let Some(charset_sids) = parse_charset(cff_data, charset_offset as usize, num_glyphs) {
            let mut sid_to_gid: HashMap<u16, u16> = HashMap::new();
            for (gid, &sid) in charset_sids.iter().enumerate() {
                if gid > 0 {
                    sid_to_gid.entry(sid).or_insert(gid as u16);
                }
            }
            let mut map = HashMap::new();
            for byte_code in 0u16..256 {
                let glyph_name = super::font_dict::FontInfo::gid_to_standard_glyph_name(byte_code);
                if let Some(name) = glyph_name
                    && let Some(sid) = glyph_name_to_sid(name)
                    && let Some(&gid) = sid_to_gid.get(&sid)
                {
                    map.insert(byte_code as u8, gid);
                }
            }
            if !map.is_empty() {
                tracing::debug!("CFF StandardEncoding→charset GID mapping: {} entries", map.len());
                return Some(map);
            }
        }
        return None;
    }

    if encoding_offset <= 1 {
        return None;
    }

    parse_encoding_table(cff_data, encoding_offset as usize)
}

/// Build the byte → GID map for a simple CFF font using the PDF font
/// dictionary's `/Encoding` as the byte → glyph-name source and the CFF
/// Charset as the glyph-name → GID resolver, per ISO 32000-1 §9.6.6.
///
/// This is the correct resolution model for simple Type 1 / TrueType / CFF
/// fonts. The CFF font program's *own* Encoding table is only authoritative
/// when there is no PDF-level encoding to consult (an Identity case).
///
/// In practice the bug this fixes is prepress-tool-authored subset CFFs
/// whose internal Encoding lists only `0x20 → space` and `0x41 → A` while
/// the Charset enumerates the full subset (e.g. `A B C D E F G I K M N O R
/// S U V X g`). The previous resolution consulted the CFF Encoding directly
/// and silently dropped every non-A content byte to `.notdef`, producing
/// bare-A glyphs on every separation plate.
///
/// Returns `None` when the result is empty so callers can fall through to
/// the legacy [`parse_cff_gid_mapping`] for fonts where the PDF-level
/// encoding genuinely cannot resolve any byte (e.g. `Encoding::Identity`
/// on a CIDFont — though those normally short-circuit before reaching
/// this path).
pub fn parse_cff_gid_mapping_with_pdf_encoding(
    font_data: &[u8],
    pdf_encoding: &crate::fonts::font_dict::Encoding,
    differences: &HashMap<u8, String>,
) -> Option<HashMap<u8, u16>> {
    use crate::fonts::font_dict::Encoding;

    if matches!(pdf_encoding, Encoding::Identity) {
        // Caller has no byte→name mapping to supply — fall through to the
        // CFF Encoding-driven legacy path. ~keep
        return parse_cff_gid_mapping(font_data);
    }

    if font_data.len() < 4 {
        return None;
    }
    let cff_data = if font_data[0] != 1 {
        extract_cff_from_opentype(font_data)?
    } else {
        font_data
    };
    if cff_data.len() < 4 || cff_data[0] != 1 {
        return None;
    }
    let hdr_size = cff_data[2] as usize;

    let (_, after_name) = parse_index(cff_data, hdr_size)?;
    let (top_dicts, after_top_dict) = parse_index(cff_data, after_name)?;
    if top_dicts.is_empty() {
        return None;
    }
    let (string_index, _after_string) = parse_index(cff_data, after_top_dict)?;
    let (charstrings_offset, _encoding_offset, charset_offset) = parse_top_dict_with_charstrings(top_dicts[0]);

    // §9.6.6 path: build name→GID from the Charset (which always enumerates
    // every subset glyph), then key bytes through the PDF /Encoding +
    // /Differences.
    //
    // nGlyphs comes from the CharStrings INDEX header (CFF spec §9). Simple
    // fonts only address ≤256 codes via /Encoding, but a subset's GID space
    // can exceed 256 — a /Differences entry pointing at a glyph whose CFF
    // Charset GID is >256 must still resolve. Falls back to 256 if the
    // CharStrings offset is missing or the INDEX header is truncated. ~keep
    let num_glyphs = if charstrings_offset > 0 {
        read_index_count(cff_data, charstrings_offset as usize)
            .map(|n| n as usize)
            .unwrap_or(256)
    } else {
        256
    };

    let charset_sids = if charset_offset > 2 {
        parse_charset(cff_data, charset_offset as usize, num_glyphs)?
    } else {
        // charset_offset 0 or 1 = ISOAdobe / Expert / ExpertSubset
        // predefined charsets. The CFF Standard Encoding + charset path in
        // `parse_cff_gid_mapping` handles these; defer. ~keep
        return parse_cff_gid_mapping(font_data);
    };

    let resolved = resolve_bytes_via_pdf_encoding(&charset_sids, &string_index, pdf_encoding, differences);

    if resolved.is_empty() {
        // PDF /Encoding yielded zero hits against the Charset. Fall back to
        // the CFF Encoding-driven path so we never make a working font worse. ~keep
        parse_cff_gid_mapping(font_data)
    } else {
        Some(resolved)
    }
}

/// Pure-input helper: given a parsed CFF Charset + String INDEX, build the
/// byte → GID map driven by the PDF font dictionary's `/Encoding` and
/// `/Differences`. Split out of [`parse_cff_gid_mapping_with_pdf_encoding`]
/// so the name-resolution logic can be tested without constructing a
/// custom CFF binary.
fn resolve_bytes_via_pdf_encoding(
    charset_sids: &[u16],
    string_index: &[&[u8]],
    pdf_encoding: &crate::fonts::font_dict::Encoding,
    differences: &HashMap<u8, String>,
) -> HashMap<u8, u16> {
    use crate::fonts::font_dict::{Encoding, FontInfo};

    // Glyph name → GID (lowest GID wins on duplicate names — first occurrence
    // in the Charset reflects the subsetter's primary mapping). ~keep
    let mut name_to_gid: HashMap<String, u16> = HashMap::new();
    for (gid, &sid) in charset_sids.iter().enumerate() {
        if gid == 0 {
            continue; // .notdef is implicit and not addressable by name ~keep
        }
        if let Some(name) = resolve_glyph_name(sid, string_index) {
            name_to_gid.entry(name).or_insert(gid as u16);
        }
    }

    let resolve_base_byte = |byte: u8| -> Option<&'static str> {
        match pdf_encoding {
            Encoding::Standard(name) => match name.as_str() {
                "MacRomanEncoding" => mac_roman_byte_to_name(byte),
                "StandardEncoding" => standard_encoding_byte_to_name(byte),
                // WinAnsiEncoding, MacExpertEncoding, PDFDocEncoding, and any
                // unrecognised string fall through to WinAnsi. Mac Expert is
                // a non-text variant; PDF Doc Encoding overlaps WinAnsi. ~keep
                _ => FontInfo::gid_to_standard_glyph_name(byte as u16),
            },
            Encoding::Custom(_) => FontInfo::gid_to_standard_glyph_name(byte as u16),
            Encoding::Identity => None, // handled by the outer guard already ~keep
        }
    };

    let mut out: HashMap<u8, u16> = HashMap::new();
    for byte_code in 0u16..256 {
        let byte = byte_code as u8;

        // §9.6.6: /Differences entries override the base predefined encoding. ~keep
        if let Some(diff_name) = differences.get(&byte)
            && let Some(&gid) = name_to_gid.get(diff_name)
        {
            out.insert(byte, gid);
            continue;
        }

        if let Some(name) = resolve_base_byte(byte)
            && let Some(&gid) = name_to_gid.get(name)
        {
            out.insert(byte, gid);
        }
    }
    out
}

/// Annex D Table D.2 (MacRomanEncoding) byte → glyph name.
///
/// ASCII range 0x20-0x7E shares glyph names with WinAnsi; 0x80-0xFF is
/// the Mac OS Roman repertoire (which is *not* ISO-8859-1). The Apple-logo
/// glyph at 0xF0 (PUA U+F8FF) has no portable glyph name and returns `None`.
fn mac_roman_byte_to_name(byte: u8) -> Option<&'static str> {
    use crate::fonts::font_dict::FontInfo;
    if (0x20..=0x7E).contains(&byte) {
        return FontInfo::gid_to_standard_glyph_name(byte as u16);
    }
    match byte {
        0x80 => Some("Adieresis"),
        0x81 => Some("Aring"),
        0x82 => Some("Ccedilla"),
        0x83 => Some("Eacute"),
        0x84 => Some("Ntilde"),
        0x85 => Some("Odieresis"),
        0x86 => Some("Udieresis"),
        0x87 => Some("aacute"),
        0x88 => Some("agrave"),
        0x89 => Some("acircumflex"),
        0x8A => Some("adieresis"),
        0x8B => Some("atilde"),
        0x8C => Some("aring"),
        0x8D => Some("ccedilla"),
        0x8E => Some("eacute"),
        0x8F => Some("egrave"),
        0x90 => Some("ecircumflex"),
        0x91 => Some("edieresis"),
        0x92 => Some("iacute"),
        0x93 => Some("igrave"),
        0x94 => Some("icircumflex"),
        0x95 => Some("idieresis"),
        0x96 => Some("ntilde"),
        0x97 => Some("oacute"),
        0x98 => Some("ograve"),
        0x99 => Some("ocircumflex"),
        0x9A => Some("odieresis"),
        0x9B => Some("otilde"),
        0x9C => Some("uacute"),
        0x9D => Some("ugrave"),
        0x9E => Some("ucircumflex"),
        0x9F => Some("udieresis"),
        0xA0 => Some("dagger"),
        0xA1 => Some("degree"),
        0xA2 => Some("cent"),
        0xA3 => Some("sterling"),
        0xA4 => Some("section"),
        0xA5 => Some("bullet"),
        0xA6 => Some("paragraph"),
        0xA7 => Some("germandbls"),
        0xA8 => Some("registered"),
        0xA9 => Some("copyright"),
        0xAA => Some("trademark"),
        0xAB => Some("acute"),
        0xAC => Some("dieresis"),
        0xAD => Some("notequal"),
        0xAE => Some("AE"),
        0xAF => Some("Oslash"),
        0xB0 => Some("infinity"),
        0xB1 => Some("plusminus"),
        0xB2 => Some("lessequal"),
        0xB3 => Some("greaterequal"),
        0xB4 => Some("yen"),
        0xB5 => Some("mu"),
        0xB6 => Some("partialdiff"),
        0xB7 => Some("summation"),
        0xB8 => Some("product"),
        0xB9 => Some("pi"),
        0xBA => Some("integral"),
        0xBB => Some("ordfeminine"),
        0xBC => Some("ordmasculine"),
        0xBD => Some("Omega"),
        0xBE => Some("ae"),
        0xBF => Some("oslash"),
        0xC0 => Some("questiondown"),
        0xC1 => Some("exclamdown"),
        0xC2 => Some("logicalnot"),
        0xC3 => Some("radical"),
        0xC4 => Some("florin"),
        0xC5 => Some("approxequal"),
        0xC6 => Some("Delta"),
        0xC7 => Some("guillemotleft"),
        0xC8 => Some("guillemotright"),
        0xC9 => Some("ellipsis"),
        0xCA => Some("space"), // nonbreakingspace; the canonical glyph name is "space" ~keep
        0xCB => Some("Agrave"),
        0xCC => Some("Atilde"),
        0xCD => Some("Otilde"),
        0xCE => Some("OE"),
        0xCF => Some("oe"),
        0xD0 => Some("endash"),
        0xD1 => Some("emdash"),
        0xD2 => Some("quotedblleft"),
        0xD3 => Some("quotedblright"),
        0xD4 => Some("quoteleft"),
        0xD5 => Some("quoteright"),
        0xD6 => Some("divide"),
        0xD7 => Some("lozenge"),
        0xD8 => Some("ydieresis"),
        0xD9 => Some("Ydieresis"),
        0xDA => Some("fraction"),
        0xDB => Some("currency"),
        0xDC => Some("guilsinglleft"),
        0xDD => Some("guilsinglright"),
        0xDE => Some("fi"),
        0xDF => Some("fl"),
        0xE0 => Some("daggerdbl"),
        0xE1 => Some("periodcentered"),
        0xE2 => Some("quotesinglbase"),
        0xE3 => Some("quotedblbase"),
        0xE4 => Some("perthousand"),
        0xE5 => Some("Acircumflex"),
        0xE6 => Some("Ecircumflex"),
        0xE7 => Some("Aacute"),
        0xE8 => Some("Edieresis"),
        0xE9 => Some("Egrave"),
        0xEA => Some("Iacute"),
        0xEB => Some("Icircumflex"),
        0xEC => Some("Idieresis"),
        0xED => Some("Igrave"),
        0xEE => Some("Oacute"),
        0xEF => Some("Ocircumflex"),
        0xF0 => None, // Apple-logo PUA glyph; no portable name ~keep
        0xF1 => Some("Ograve"),
        0xF2 => Some("Uacute"),
        0xF3 => Some("Ucircumflex"),
        0xF4 => Some("Ugrave"),
        0xF5 => Some("dotlessi"),
        0xF6 => Some("circumflex"),
        0xF7 => Some("tilde"),
        0xF8 => Some("macron"),
        0xF9 => Some("breve"),
        0xFA => Some("dotaccent"),
        0xFB => Some("ring"),
        0xFC => Some("cedilla"),
        0xFD => Some("hungarumlaut"),
        0xFE => Some("ogonek"),
        0xFF => Some("caron"),
        _ => None,
    }
}

/// Annex D Table D.1 (StandardEncoding, PostScript) byte → glyph name.
///
/// ASCII range 0x20-0x7E shares glyph names with WinAnsi except a handful
/// (0x27 quoteright, 0x60 quoteleft, etc.) — those overlap the existing
/// WinAnsi table's choice of `quoteright` for 0x27. The high-byte range
/// has its own PostScript repertoire (fraction at 0xA4, ligature `fi`/`fl`
/// at 0xAE/0xAF, etc.) which differs sharply from WinAnsi.
///
/// Bytes left unassigned by StandardEncoding return `None`.
fn standard_encoding_byte_to_name(byte: u8) -> Option<&'static str> {
    use crate::fonts::font_dict::FontInfo;
    if (0x20..=0x7E).contains(&byte) {
        return FontInfo::gid_to_standard_glyph_name(byte as u16);
    }
    match byte {
        0xA1 => Some("exclamdown"),
        0xA2 => Some("cent"),
        0xA3 => Some("sterling"),
        0xA4 => Some("fraction"),
        0xA5 => Some("yen"),
        0xA6 => Some("florin"),
        0xA7 => Some("section"),
        0xA8 => Some("currency"),
        0xA9 => Some("quotesingle"),
        0xAA => Some("quotedblleft"),
        0xAB => Some("guillemotleft"),
        0xAC => Some("guilsinglleft"),
        0xAD => Some("guilsinglright"),
        0xAE => Some("fi"),
        0xAF => Some("fl"),
        0xB1 => Some("endash"),
        0xB2 => Some("dagger"),
        0xB3 => Some("daggerdbl"),
        0xB4 => Some("periodcentered"),
        0xB6 => Some("paragraph"),
        0xB7 => Some("bullet"),
        0xB8 => Some("quotesinglbase"),
        0xB9 => Some("quotedblbase"),
        0xBA => Some("quotedblright"),
        0xBB => Some("guillemotright"),
        0xBC => Some("ellipsis"),
        0xBD => Some("perthousand"),
        0xBF => Some("questiondown"),
        0xC1 => Some("grave"),
        0xC2 => Some("acute"),
        0xC3 => Some("circumflex"),
        0xC4 => Some("tilde"),
        0xC5 => Some("macron"),
        0xC6 => Some("breve"),
        0xC7 => Some("dotaccent"),
        0xC8 => Some("dieresis"),
        0xCA => Some("ring"),
        0xCB => Some("cedilla"),
        0xCD => Some("hungarumlaut"),
        0xCE => Some("ogonek"),
        0xCF => Some("caron"),
        0xE1 => Some("AE"),
        0xE3 => Some("ordfeminine"),
        0xE8 => Some("Lslash"),
        0xE9 => Some("Oslash"),
        0xEA => Some("OE"),
        0xEB => Some("ordmasculine"),
        0xF1 => Some("ae"),
        0xF5 => Some("dotlessi"),
        0xF8 => Some("lslash"),
        0xF9 => Some("oslash"),
        0xFA => Some("oe"),
        0xFB => Some("germandbls"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sid_to_name_notdef() {
        assert_eq!(sid_to_name(0), Some(".notdef"));
    }

    #[test]
    fn test_sid_to_name_space() {
        assert_eq!(sid_to_name(1), Some("space"));
    }

    #[test]
    fn test_sid_to_name_letters() {
        assert_eq!(sid_to_name(34), Some("A"));
        assert_eq!(sid_to_name(59), Some("Z"));
        assert_eq!(sid_to_name(66), Some("a"));
        assert_eq!(sid_to_name(91), Some("z"));
    }

    #[test]
    fn test_sid_to_name_digits() {
        assert_eq!(sid_to_name(17), Some("zero"));
        assert_eq!(sid_to_name(26), Some("nine"));
    }

    #[test]
    fn test_sid_to_name_punctuation() {
        assert_eq!(sid_to_name(2), Some("exclam"));
        assert_eq!(sid_to_name(15), Some("period"));
        assert_eq!(sid_to_name(13), Some("comma"));
    }

    #[test]
    fn test_sid_to_name_ligatures() {
        assert_eq!(sid_to_name(109), Some("fi"));
        assert_eq!(sid_to_name(110), Some("fl"));
        assert_eq!(sid_to_name(266), Some("ff"));
        assert_eq!(sid_to_name(267), Some("ffi"));
        assert_eq!(sid_to_name(268), Some("ffl"));
    }

    #[test]
    fn test_sid_to_name_accented() {
        assert_eq!(sid_to_name(171), Some("Aacute"));
        assert_eq!(sid_to_name(200), Some("aacute"));
        assert_eq!(sid_to_name(227), Some("ydieresis"));
    }

    #[test]
    fn test_sid_to_name_last_entries() {
        assert_eq!(sid_to_name(388), Some("Regular"));
        assert_eq!(sid_to_name(389), Some("Roman"));
        assert_eq!(sid_to_name(390), Some("Semibold"));
    }

    #[test]
    fn test_sid_to_name_out_of_range() {
        assert_eq!(sid_to_name(391), None);
        assert_eq!(sid_to_name(500), None);
        assert_eq!(sid_to_name(u16::MAX), None);
    }

    #[test]
    fn test_parse_index_too_short() {
        assert_eq!(parse_index(&[0x00], 0), None);
    }

    #[test]
    fn test_parse_index_empty() {
        let data = [0x00, 0x00];
        let result = parse_index(&data, 0);
        assert!(result.is_some());
        let (entries, next) = result.unwrap();
        assert!(entries.is_empty());
        assert_eq!(next, 2);
    }

    #[test]
    fn test_parse_index_single_entry() {
        let data = vec![0x00, 0x01, 0x01, 0x01, 0x04, b'A', b'B', b'C'];
        let result = parse_index(&data, 0);
        assert!(result.is_some());
        let (entries, _next) = result.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], b"ABC");
    }

    #[test]
    fn test_parse_index_multiple_entries() {
        let data = vec![0x00, 0x02, 0x01, 0x01, 0x03, 0x05, b'H', b'i', b'O', b'K'];
        let result = parse_index(&data, 0);
        assert!(result.is_some());
        let (entries, _next) = result.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], b"Hi");
        assert_eq!(entries[1], b"OK");
    }

    #[test]
    fn test_parse_index_invalid_off_size_zero() {
        let data = vec![0x00, 0x01, 0x00];
        assert_eq!(parse_index(&data, 0), None);
    }

    #[test]
    fn test_parse_index_invalid_off_size_too_large() {
        let data = vec![0x00, 0x01, 0x05];
        assert_eq!(parse_index(&data, 0), None);
    }

    #[test]
    fn test_parse_index_truncated_offset_array() {
        let data = vec![0x00, 0x01, 0x01, 0x01];
        assert_eq!(parse_index(&data, 0), None);
    }

    #[test]
    fn test_parse_index_with_offset() {
        let data = vec![0xFF, 0xFF, 0xFF, 0x00, 0x01, 0x01, 0x01, 0x02, b'X'];
        let result = parse_index(&data, 3);
        assert!(result.is_some());
        let (entries, _) = result.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], b"X");
    }

    #[test]
    fn test_parse_index_off_size_2() {
        let data = vec![0x00, 0x01, 0x02, 0x00, 0x01, 0x00, 0x03, b'A', b'B'];
        let result = parse_index(&data, 0);
        assert!(result.is_some());
        let (entries, _) = result.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], b"AB");
    }

    #[test]
    fn test_parse_index_data_out_of_bounds() {
        let data = vec![0x00, 0x01, 0x01, 0x01, 0xFF];
        assert_eq!(parse_index(&data, 0), None);
    }

    #[test]
    fn test_parse_dict_operand_empty() {
        assert_eq!(parse_dict_operand(&[], 0), None);
    }

    #[test]
    fn test_parse_dict_operand_single_byte_zero() {
        assert_eq!(parse_dict_operand(&[139], 0), Some((0, 1)));
    }

    #[test]
    fn test_parse_dict_operand_single_byte_positive() {
        assert_eq!(parse_dict_operand(&[246], 0), Some((107, 1)));
    }

    #[test]
    fn test_parse_dict_operand_single_byte_negative() {
        assert_eq!(parse_dict_operand(&[32], 0), Some((-107, 1)));
    }

    #[test]
    fn test_parse_dict_operand_two_byte_positive() {
        assert_eq!(parse_dict_operand(&[247, 0], 0), Some((108, 2)));
        assert_eq!(parse_dict_operand(&[250, 255], 0), Some((1131, 2)));
    }

    #[test]
    fn test_parse_dict_operand_two_byte_negative() {
        assert_eq!(parse_dict_operand(&[251, 0], 0), Some((-108, 2)));
        assert_eq!(parse_dict_operand(&[254, 255], 0), Some((-1131, 2)));
    }

    #[test]
    fn test_parse_dict_operand_two_byte_truncated() {
        assert_eq!(parse_dict_operand(&[247], 0), None);
        assert_eq!(parse_dict_operand(&[251], 0), None);
    }

    #[test]
    fn test_parse_dict_operand_three_byte_int16() {
        assert_eq!(parse_dict_operand(&[28, 0x00, 0x01], 0), Some((1, 3)));
        assert_eq!(parse_dict_operand(&[28, 0xFF, 0xFF], 0), Some((-1, 3)));
        assert_eq!(parse_dict_operand(&[28, 0x7F, 0xFF], 0), Some((32767, 3)));
    }

    #[test]
    fn test_parse_dict_operand_three_byte_truncated() {
        assert_eq!(parse_dict_operand(&[28, 0x00], 0), None);
    }

    #[test]
    fn test_parse_dict_operand_five_byte_int32() {
        assert_eq!(parse_dict_operand(&[29, 0x00, 0x00, 0x00, 0x01], 0), Some((1, 5)));
        assert_eq!(parse_dict_operand(&[29, 0xFF, 0xFF, 0xFF, 0xFF], 0), Some((-1, 5)));
    }

    #[test]
    fn test_parse_dict_operand_five_byte_truncated() {
        assert_eq!(parse_dict_operand(&[29, 0x00, 0x00, 0x00], 0), None);
    }

    #[test]
    fn test_parse_dict_operand_real_number() {
        let data = [30, 0x1A, 0x5F];
        let result = parse_dict_operand(&data, 0);
        assert!(result.is_some());
        let (val, consumed) = result.unwrap();
        assert_eq!(val, 0);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn test_parse_dict_operand_real_nibble1_end() {
        let data = [30, 0xF0];
        let result = parse_dict_operand(&data, 0);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), (0, 2));
    }

    #[test]
    fn test_parse_dict_operand_real_unterminated() {
        let data = [30, 0x12, 0x34];
        assert_eq!(parse_dict_operand(&data, 0), None);
    }

    #[test]
    fn test_parse_dict_operand_unknown_byte() {
        assert_eq!(parse_dict_operand(&[0], 0), None);
        assert_eq!(parse_dict_operand(&[21], 0), None);
        assert_eq!(parse_dict_operand(&[255], 0), None);
    }

    #[test]
    fn test_parse_dict_operand_with_offset() {
        let data = [0x00, 0x00, 139];
        assert_eq!(parse_dict_operand(&data, 2), Some((0, 1)));
    }

    #[test]
    fn test_parse_top_dict_empty() {
        let (enc, charset) = parse_top_dict(&[]);
        assert_eq!(enc, 0);
        assert_eq!(charset, 0);
    }

    #[test]
    fn test_parse_top_dict_encoding_offset() {
        let data = [181, 16];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 42);
        assert_eq!(charset, 0);
    }

    #[test]
    fn test_parse_top_dict_charset_offset() {
        let data = [238, 15];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 0);
        assert_eq!(charset, 99);
    }

    #[test]
    fn test_parse_top_dict_both_offsets() {
        let data = [189, 16, 239, 15];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 50);
        assert_eq!(charset, 100);
    }

    #[test]
    fn test_parse_top_dict_two_byte_operator() {
        let data = [139, 12, 0];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 0);
        assert_eq!(charset, 0);
    }

    #[test]
    fn test_parse_top_dict_unknown_operator() {
        let data = [181, 17];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 0);
        assert_eq!(charset, 0);
    }

    #[test]
    fn test_parse_top_dict_skip_unparseable() {
        let data = [255, 181, 16];
        let (enc, charset) = parse_top_dict(&data);
        assert_eq!(enc, 42);
        assert_eq!(charset, 0);
    }

    #[test]
    fn test_parse_charset_out_of_bounds() {
        assert_eq!(parse_charset(&[0x00], 5, 10), None);
    }

    #[test]
    fn test_parse_charset_format0() {
        let data = vec![0x00, 0x00, 0x01, 0x00, 0x22, 0x00, 0x42];
        let result = parse_charset(&data, 0, 4);
        assert!(result.is_some());
        let sids = result.unwrap();
        assert_eq!(sids.len(), 4);
        assert_eq!(sids[0], 0);
        assert_eq!(sids[1], 1);
        assert_eq!(sids[2], 34);
        assert_eq!(sids[3], 66);
    }

    #[test]
    fn test_parse_charset_format1() {
        let data = vec![0x01, 0x00, 0x22, 0x02];
        let result = parse_charset(&data, 0, 4);
        assert!(result.is_some());
        let sids = result.unwrap();
        assert_eq!(sids.len(), 4);
        assert_eq!(sids[0], 0);
        assert_eq!(sids[1], 34);
        assert_eq!(sids[2], 35);
        assert_eq!(sids[3], 36);
    }

    #[test]
    fn test_parse_charset_format2() {
        let data = vec![0x02, 0x00, 0x42, 0x00, 0x03];
        let result = parse_charset(&data, 0, 5);
        assert!(result.is_some());
        let sids = result.unwrap();
        assert_eq!(sids.len(), 5);
        assert_eq!(sids[0], 0);
        assert_eq!(sids[1], 66);
        assert_eq!(sids[2], 67);
        assert_eq!(sids[3], 68);
        assert_eq!(sids[4], 69);
    }

    #[test]
    fn test_parse_charset_unknown_format() {
        let data = vec![0x03];
        assert_eq!(parse_charset(&data, 0, 2), None);
    }

    #[test]
    fn test_parse_charset_format0_truncated() {
        let data = vec![0x00, 0x00];
        let result = parse_charset(&data, 0, 3);
        assert!(result.is_some());
        let sids = result.unwrap();
        assert_eq!(sids[0], 0);
    }

    #[test]
    fn test_parse_charset_format1_limits_to_num_glyphs() {
        let data = vec![0x01, 0x00, 0x01, 0xFF];
        let result = parse_charset(&data, 0, 3);
        assert!(result.is_some());
        let sids = result.unwrap();
        assert_eq!(sids.len(), 3);
    }

    #[test]
    fn test_parse_encoding_table_out_of_bounds() {
        assert_eq!(parse_encoding_table(&[0x00], 5), None);
    }

    #[test]
    fn test_parse_encoding_table_format0() {
        let data = vec![0x00, 0x03, 0x41, 0x42, 0x43];
        let result = parse_encoding_table(&data, 0);
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.get(&0x41), Some(&1));
        assert_eq!(map.get(&0x42), Some(&2));
        assert_eq!(map.get(&0x43), Some(&3));
    }

    #[test]
    fn test_parse_encoding_table_format1() {
        let data = vec![0x01, 0x01, 0x41, 0x02];
        let result = parse_encoding_table(&data, 0);
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.get(&0x41), Some(&1));
        assert_eq!(map.get(&0x42), Some(&2));
        assert_eq!(map.get(&0x43), Some(&3));
    }

    #[test]
    fn test_parse_encoding_table_unknown_format() {
        let data = vec![0x02];
        assert_eq!(parse_encoding_table(&data, 0), None);
    }

    #[test]
    fn test_parse_encoding_table_format0_truncated() {
        let data = vec![0x00, 0x05, 0x41, 0x42];
        let result = parse_encoding_table(&data, 0);
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_parse_encoding_table_with_supplement() {
        let data = vec![0x80, 0x01, 0x41, 0x01, 0x42, 0x00, 0x22];
        let result = parse_encoding_table(&data, 0);
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.get(&0x41), Some(&1));
        assert_eq!(map.get(&0x42), Some(&34));
    }

    #[test]
    fn test_parse_encoding_table_format1_truncated() {
        let data = vec![0x01, 0x02, 0x41, 0x01];
        let result = parse_encoding_table(&data, 0);
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_parse_encoding_table_format0_empty_pos() {
        let data = vec![0x00];
        assert_eq!(parse_encoding_table(&data, 0), None);
    }

    #[test]
    fn test_parse_encoding_table_format1_empty_pos() {
        let data = vec![0x01];
        assert_eq!(parse_encoding_table(&data, 0), None);
    }

    #[test]
    fn test_resolve_glyph_name_predefined() {
        let string_index: Vec<&[u8]> = vec![];
        assert_eq!(resolve_glyph_name(0, &string_index), Some(".notdef".to_string()));
        assert_eq!(resolve_glyph_name(1, &string_index), Some("space".to_string()));
        assert_eq!(resolve_glyph_name(34, &string_index), Some("A".to_string()));
        assert_eq!(resolve_glyph_name(390, &string_index), Some("Semibold".to_string()));
    }

    #[test]
    fn test_resolve_glyph_name_custom_string() {
        let custom: Vec<&[u8]> = vec![b"MyGlyph", b"AnotherGlyph"];
        assert_eq!(resolve_glyph_name(391, &custom), Some("MyGlyph".to_string()));
        assert_eq!(resolve_glyph_name(392, &custom), Some("AnotherGlyph".to_string()));
    }

    #[test]
    fn test_resolve_glyph_name_custom_out_of_range() {
        let custom: Vec<&[u8]> = vec![b"OnlyOne"];
        assert_eq!(resolve_glyph_name(393, &custom), None);
    }

    #[test]
    fn test_resolve_glyph_name_custom_invalid_utf8() {
        let invalid_utf8: Vec<&[u8]> = vec![&[0xFF, 0xFE]];
        assert_eq!(resolve_glyph_name(391, &invalid_utf8), None);
    }

    #[test]
    fn test_extract_cff_from_opentype_too_short() {
        assert_eq!(extract_cff_from_opentype(&[0; 8]), None);
    }

    #[test]
    fn test_extract_cff_from_opentype_not_opentype() {
        let data = vec![0x00; 16];
        assert_eq!(extract_cff_from_opentype(&data), None);
    }

    #[test]
    fn test_extract_cff_from_opentype_otto_no_cff_table() {
        let data = vec![0x4F, 0x54, 0x54, 0x4F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(extract_cff_from_opentype(&data), None);
    }

    #[test]
    fn test_extract_cff_from_opentype_with_cff_table() {
        let cff_data = b"\x01\x00\x04\x01";
        let cff_offset: u32 = 28;
        let cff_length: u32 = cff_data.len() as u32;

        let mut data = vec![0x4F, 0x54, 0x54, 0x4F, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(b"CFF ");
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend_from_slice(&cff_offset.to_be_bytes());
        data.extend_from_slice(&cff_length.to_be_bytes());
        data.extend_from_slice(cff_data);

        let result = extract_cff_from_opentype(&data);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), cff_data);
    }

    #[test]
    fn test_extract_cff_from_opentype_truncated_table_dir() {
        let data = vec![
            0x4F, 0x54, 0x54, 0x4F, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, b'C', b'F',
        ];
        assert_eq!(extract_cff_from_opentype(&data), None);
    }

    #[test]
    fn test_parse_cff_encoding_too_short() {
        assert_eq!(parse_cff_encoding(&[0, 1, 2]), None);
    }

    #[test]
    fn test_parse_cff_encoding_wrong_version() {
        let data = vec![0x02, 0x00, 0x04, 0x01, 0x00];
        assert_eq!(parse_cff_encoding(&data), None);
    }

    #[test]
    fn test_parse_cff_encoding_version1_too_short_after_check() {
        let data = vec![0x01, 0x00, 0x04];
        assert_eq!(parse_cff_encoding(&data), None);
    }

    #[test]
    fn test_parse_cff_encoding_expert_encoding() {
        let data = build_minimal_cff(1, 0);
        let result = parse_cff_encoding(&data);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_cff_encoding_standard_encoding_default_charset() {
        let data = build_minimal_cff(0, 0);
        let result = parse_cff_encoding(&data);
        assert_eq!(result, None);
    }

    /// Helper: builds a minimal CFF font binary with specified encoding and charset offsets.
    fn build_minimal_cff(encoding_offset: i32, charset_offset: i32) -> Vec<u8> {
        let mut data = vec![1, 0, 4, 1];

        append_index(&mut data, &[b"Test"]);

        let top_dict = build_top_dict(encoding_offset, charset_offset);
        append_index(&mut data, &[&top_dict]);

        append_index(&mut data, &[]);

        append_index(&mut data, &[]);

        data
    }

    /// Encode a CFF DICT with encoding (op 16) and charset (op 15) operands.
    fn build_top_dict(encoding_offset: i32, charset_offset: i32) -> Vec<u8> {
        let mut dict = Vec::new();
        encode_dict_int(&mut dict, encoding_offset);
        dict.push(16);
        encode_dict_int(&mut dict, charset_offset);
        dict.push(15);
        dict
    }

    /// Encode a CFF integer operand into DICT format.
    fn encode_dict_int(out: &mut Vec<u8>, val: i32) {
        if (-107..=107).contains(&val) {
            out.push((val + 139) as u8);
        } else if (108..=1131).contains(&val) {
            let v = val - 108;
            out.push((v / 256 + 247) as u8);
            out.push((v % 256) as u8);
        } else if (-1131..=-108).contains(&val) {
            let v = -val - 108;
            out.push((v / 256 + 251) as u8);
            out.push((v % 256) as u8);
        } else if (-32768..=32767).contains(&val) {
            out.push(28);
            let bytes = (val as i16).to_be_bytes();
            out.push(bytes[0]);
            out.push(bytes[1]);
        } else {
            out.push(29);
            let bytes = val.to_be_bytes();
            out.extend_from_slice(&bytes);
        }
    }

    /// Append a CFF INDEX to a data vector.
    fn append_index(data: &mut Vec<u8>, entries: &[&[u8]]) {
        let count = entries.len() as u16;
        data.extend_from_slice(&count.to_be_bytes());
        if count == 0 {
            return;
        }
        data.push(1);

        let mut offset: u8 = 1;
        data.push(offset);
        for entry in entries {
            offset += entry.len() as u8;
            data.push(offset);
        }
        for entry in entries {
            data.extend_from_slice(entry);
        }
    }

    #[test]
    fn test_glyph_name_to_sid_known_names() {
        assert_eq!(glyph_name_to_sid(".notdef"), Some(0));
        assert_eq!(glyph_name_to_sid("space"), Some(1));
        assert_eq!(glyph_name_to_sid("A"), Some(34));
        assert_eq!(glyph_name_to_sid("B"), Some(35));
        assert_eq!(glyph_name_to_sid("Z"), Some(59));
        assert_eq!(glyph_name_to_sid("a"), Some(66));
        assert_eq!(glyph_name_to_sid("z"), Some(91));
        assert_eq!(glyph_name_to_sid("zero"), Some(17));
        assert_eq!(glyph_name_to_sid("nine"), Some(26));
    }

    #[test]
    fn test_glyph_name_to_sid_unknown() {
        assert_eq!(glyph_name_to_sid("nonexistent_glyph_xyz"), None);
        assert_eq!(glyph_name_to_sid(""), None);
    }

    #[test]
    fn test_glyph_name_to_sid_roundtrip() {
        for sid in 0u16..391 {
            if let Some(name) = sid_to_name(sid) {
                assert_eq!(
                    glyph_name_to_sid(name),
                    Some(sid),
                    "Roundtrip failed for SID {} (name '{}')",
                    sid,
                    name
                );
            }
        }
    }

    #[test]
    fn test_parse_cff_gid_mapping_invalid_data() {
        assert!(parse_cff_gid_mapping(&[]).is_none());
        assert!(parse_cff_gid_mapping(&[0, 1, 2]).is_none());
        assert!(parse_cff_gid_mapping(&[2, 0, 4, 2]).is_none());
    }

    // ==========================================
    // resolve_bytes_via_pdf_encoding tests
    //
    // These exercise the name-resolution layer in isolation — the layer
    // that was missing in `parse_cff_gid_mapping` and is the substantive
    // fix here. The CFF binary parser is reused unchanged, so its tests
    // remain authoritative for the parsing path.
    // ========================================== ~keep

    use crate::fonts::font_dict::Encoding;

    /// Pin a real-world sparse-CFF subset pattern: charset enumerates
    /// space, A, B, C, O, V, N (SIDs 1, 34, 35, 36, 48, 55, 47) on GIDs
    /// 1..=7. PDF /Encoding is WinAnsiEncoding. The resolver must produce
    /// GIDs for every charset entry — not just for byte 0x41 ("A") as the
    /// sparse CFF Encoding table would have implied.
    #[test]
    fn resolve_via_pdf_encoding_recovers_all_charset_glyphs() {
        let charset = [0u16, 1, 34, 35, 36, 48, 55, 47];
        let string_index: Vec<&[u8]> = Vec::new();
        let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
        let differences: HashMap<u8, String> = HashMap::new();

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);

        assert_eq!(map.get(&0x20), Some(&1), "0x20 (space) → GID 1");
        assert_eq!(map.get(&0x41), Some(&2), "0x41 (A) → GID 2");
        assert_eq!(map.get(&0x42), Some(&3), "0x42 (B) → GID 3");
        assert_eq!(map.get(&0x43), Some(&4), "0x43 (C) → GID 4");
        assert_eq!(map.get(&0x4f), Some(&5), "0x4f (O) → GID 5");
        assert_eq!(map.get(&0x56), Some(&6), "0x56 (V) → GID 6");
        assert_eq!(map.get(&0x4e), Some(&7), "0x4e (N) → GID 7");

        assert!(!map.contains_key(&0x7e), "0x7e (asciitilde) not in charset");
    }

    /// /Differences entries override the base predefined encoding.
    #[test]
    fn resolve_via_pdf_encoding_honors_differences_array() {
        let charset = [0u16, 116];
        let string_index: Vec<&[u8]> = Vec::new();
        let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
        let mut differences = HashMap::new();
        differences.insert(0x95u8, "bullet".to_string());

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert_eq!(map.get(&0x95), Some(&1));
    }

    /// Identity encoding short-circuits via the outer function; the
    /// helper itself is a no-op for Identity.
    #[test]
    fn resolve_via_pdf_encoding_skips_identity() {
        let charset = [0u16, 34];
        let string_index: Vec<&[u8]> = Vec::new();
        let pdf_enc = Encoding::Identity;
        let differences: HashMap<u8, String> = HashMap::new();
        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert!(map.is_empty(), "Identity → no base byte→name resolution");
    }

    /// Custom-string SIDs (>=391) resolved through the String INDEX
    /// land in the name→GID map.
    #[test]
    fn resolve_via_pdf_encoding_resolves_custom_string_sids() {
        let charset = [0u16, 391];
        let custom: &[u8] = b"customGlyph";
        let string_index: Vec<&[u8]> = vec![custom];
        let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());
        let mut differences = HashMap::new();
        differences.insert(0x21u8, "customGlyph".to_string());

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert_eq!(map.get(&0x21), Some(&1));
    }

    // ==========================================
    // Base-encoding selection (§9.6.6.1 / Annex D)
    //
    // /BaseEncoding picks which byte→glyph-name table resolves high bytes
    // 0x80-0xFF. The three predefined bases diverge:
    //   - WinAnsi 0x80   = "euro"      (Windows-1252)
    //   - MacRoman 0x80  = "Adieresis" (Annex D Table D.2)
    //   - Standard 0xA4  = "fraction"  (Annex D Table D.1; WinAnsi = "currency")
    // Resolving the wrong table for a non-WinAnsi base would mis-route bytes.
    // ========================================== ~keep

    /// MacRomanEncoding: byte 0x80 must resolve via "Adieresis", not "euro".
    #[test]
    fn resolve_via_pdf_encoding_uses_mac_roman_table_for_mac_base() {
        let charset: Vec<u16> = vec![0, 173, 391];
        let string_index: Vec<&[u8]> = vec![b"euro"];
        let differences: HashMap<u8, String> = HashMap::new();
        let pdf_enc = Encoding::Standard("MacRomanEncoding".to_string());

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert_eq!(map.get(&0x80), Some(&1), "MacRoman 0x80 → Adieresis (GID 1)");
    }

    /// StandardEncoding: byte 0xA4 must resolve via "fraction", not "currency".
    #[test]
    fn resolve_via_pdf_encoding_uses_standard_encoding_table_for_standard_base() {
        let charset: Vec<u16> = vec![0, 99, 103];
        let string_index: Vec<&[u8]> = vec![];
        let differences: HashMap<u8, String> = HashMap::new();
        let pdf_enc = Encoding::Standard("StandardEncoding".to_string());

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert_eq!(map.get(&0xA4), Some(&1), "StandardEncoding 0xA4 → fraction (GID 1)");
    }

    /// WinAnsiEncoding regression guard: byte 0x80 still resolves to "euro".
    #[test]
    fn resolve_via_pdf_encoding_uses_winansi_table_for_winansi_base() {
        let charset: Vec<u16> = vec![0, 173, 391];
        let string_index: Vec<&[u8]> = vec![b"euro"];
        let differences: HashMap<u8, String> = HashMap::new();
        let pdf_enc = Encoding::Standard("WinAnsiEncoding".to_string());

        let map = resolve_bytes_via_pdf_encoding(&charset, &string_index, &pdf_enc, &differences);
        assert_eq!(map.get(&0x80), Some(&2), "WinAnsi 0x80 → euro (GID 2)");
    }

    /// `parse_charset` already handles arbitrary nGlyphs; the cap lives in the
    /// caller (`parse_cff_gid_mapping_with_pdf_encoding`). This pins the
    /// parser's tolerance for >256 entries so a regression there would
    /// surface independently of the caller refactor.
    #[test]
    fn parse_charset_format0_handles_more_than_256_entries() {
        let mut data = vec![0x00u8];
        for gid in 1u16..=299u16 {
            data.extend_from_slice(&gid.to_be_bytes());
        }
        let sids = parse_charset(&data, 0, 300).expect("parse_charset returned None");
        assert_eq!(sids.len(), 300, "300 entries (GID 0 + 299 enumerated)");
        assert_eq!(sids[0], 0, "GID 0 is .notdef (SID 0)");
        assert_eq!(sids[1], 1, "GID 1 → SID 1");
        assert_eq!(sids[256], 256, "GID 256 → SID 256 (past the old 256 cap)");
        assert_eq!(sids[299], 299, "GID 299 → SID 299 (last entry)");
    }

    /// CFF Top DICT must surface the CharStrings INDEX offset (op 17) so
    /// callers can read the real nGlyphs from the INDEX header instead of
    /// guessing 256.
    #[test]
    fn parse_top_dict_surfaces_charstrings_offset() {
        let mut dict = vec![28u8];
        dict.extend_from_slice(&1234i16.to_be_bytes());
        dict.push(17u8);

        let (charstrings_offset, _enc, _charset) = parse_top_dict_with_charstrings(&dict);
        assert_eq!(charstrings_offset, 1234, "Top DICT op 17 → CharStrings offset");
    }

    /// Read the count field of a CFF INDEX header. nGlyphs is the count of
    /// the CharStrings INDEX.
    #[test]
    fn read_index_count_returns_header_count() {
        let data = [0x01u8, 0x2C];
        assert_eq!(read_index_count(&data, 0), Some(300));
    }
}
