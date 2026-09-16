//! Embedded picture discovery for ZIP-backed spreadsheets.
//!
//! calamine models cells, formulas and hyperlinks but not drawings, so the
//! `xl/media/*` parts an `.xlsx`/`.xlsm` package carries were never surfaced:
//! the pictures were neither extracted nor handed to the OCR pass. This module
//! reads them straight out of the package — the same ZIP `read_excel_bytes`
//! already validated — by enumerating every media part and then walking
//! `xl/workbook.xml` -> `xl/_rels/workbook.xml.rels` -> per-sheet rels -> the
//! DrawingML (`xl/drawings/drawingN.xml`) and legacy VML
//! (`xl/drawings/vmlDrawingN.vml`) parts that anchor a picture to a cell. That
//! yields the 1-based sheet index and the anchor cell for each picture and
//! lets the extractor place its marker inside the owning sheet's element
//! stream instead of at the end of the document.
//!
//! Nothing here is fatal. A missing, malformed, oversized or empty part is
//! skipped and logged; a picture the package never anchors to a sheet is still
//! returned without a sheet rather than dropped, so the OCR pass sees
//! every picture the archive holds.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read, Seek};

use crate::extractors::security::SecurityLimits;

/// Office Document relationships namespace (`r:`).
const RELATIONSHIPS_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Legacy VML office namespace (`o:`), where `v:imagedata` carries `o:relid`.
const VML_OFFICE_NS: &str = "urn:schemas-microsoft-com:office:office";

/// Package prefix holding every embedded picture of a ZIP-backed spreadsheet.
const MEDIA_PREFIX: &str = "xl/media/";

/// One picture read out of `xl/media/`, with the placement its drawing part
/// resolves to.
pub(crate) struct XlsxPicture {
    /// Original bytes, exactly as stored in the package.
    pub(crate) data: Vec<u8>,
    /// Format detected from the magic bytes, falling back to the part's file
    /// extension when the magic is not one the shared detector knows.
    pub(crate) format: Cow<'static, str>,
    /// The archive-relative part name, e.g. `xl/media/image4.emf`.
    pub(crate) source_path: String,
    /// Name of the sheet the picture is anchored to, as written in
    /// `xl/workbook.xml`. The extractor resolves it to a 1-based page number
    /// against the sheets it actually read; resolving by position here would
    /// shift every later sheet's pictures onto the wrong page once any sheet is
    /// skipped (a chartsheet part, for example, is not a worksheet).
    pub(crate) sheet_name: Option<String>,
    /// 1-based index of the sheet the picture is anchored to, resolved from
    /// `sheet_name` by the extractor's workbook pass. `None` while unresolved or
    /// when no drawing part names a sheet.
    pub(crate) sheet_index: Option<u32>,
    /// Zero-based `(row, col)` of the anchor's top-left cell, when the drawing
    /// part records one.
    pub(crate) anchor: Option<(u32, u32)>,
    /// Alt text from the drawing part, when present.
    pub(crate) description: Option<String>,
}

/// Where a picture sits in the workbook: which sheet, and where on it.
#[derive(Clone)]
struct Placement {
    sheet_name: String,
    cell: Option<(u32, u32)>,
}

/// One floating text shape (`xdr:sp` with a `xdr:txBody`) from a drawing part.
///
/// A workbook's drawings carry text as well as pictures: the labels of a flow
/// chart drawn in Excel, the callouts and titles of a dashboard sheet. calamine
/// models cells and formulas only, so without this a sheet whose content is a
/// diagram extracted as an empty table.
pub(crate) struct XlsxShapeText {
    /// The shape's paragraphs, joined with a single space.
    pub(crate) text: String,
    /// Name of the sheet whose drawing part holds the shape, as written in
    /// `xl/workbook.xml`.
    pub(crate) sheet_name: String,
    /// Zero-based `(row, col)` of the shape's anchor, for reading order.
    pub(crate) anchor: Option<(u32, u32)>,
}

/// Pictures plus the alt text of the anchor they were first seen under, keyed by
/// media part name.
type Placements = HashMap<String, (Placement, Option<String>)>;

/// One packaged relationship (`_rels/*.rels` entry).
struct Rel {
    target: String,
    kind: String,
}

/// Read every `xl/media/*` picture, and every floating shape's text, from an
/// in-memory spreadsheet package.
///
/// A blob that is not a readable ZIP (a legacy `.xls`, an ODS, a corrupt file)
/// yields no pictures; the caller has already reported the format's real
/// problem, so this stays a debug log rather than a second error. Shape text is
/// read from the same drawing parts either way — `include_pictures` only skips
/// loading the media bytes for callers that have no use for them.
pub(crate) fn read_xlsx_drawings_from_bytes(
    data: &[u8],
    limits: &SecurityLimits,
    include_pictures: bool,
) -> (Vec<XlsxPicture>, Vec<XlsxShapeText>) {
    match zip::ZipArchive::new(std::io::Cursor::new(data)) {
        Ok(mut archive) => read_archive(&mut archive, limits, include_pictures),
        Err(e) => {
            tracing::debug!(error = %e, "spreadsheet is not a readable ZIP; no pictures read");
            (Vec::new(), Vec::new())
        }
    }
}

/// Read every `xl/media/*` picture from a spreadsheet file on disk.
///
/// Same semantics as [`read_xlsx_drawings_from_bytes`].
pub(crate) fn read_xlsx_drawings_from_file(
    path: &std::path::Path,
    limits: &SecurityLimits,
    include_pictures: bool,
) -> (Vec<XlsxPicture>, Vec<XlsxShapeText>) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "spreadsheet file could not be opened; no pictures read");
            return (Vec::new(), Vec::new());
        }
    };
    match zip::ZipArchive::new(file) {
        Ok(mut archive) => read_archive(&mut archive, limits, include_pictures),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "spreadsheet file is not a readable ZIP; no pictures read");
            (Vec::new(), Vec::new())
        }
    }
}

/// Enumerate the media parts, then attach each one's placement.
///
/// The media parts come first and independently of the drawing walk: a picture
/// whose anchor cannot be resolved is still returned, so a package that trips
/// the anchor parser never silently loses its pictures.
fn read_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    limits: &SecurityLimits,
    include_pictures: bool,
) -> (Vec<XlsxPicture>, Vec<XlsxShapeText>) {
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|name| name.starts_with(MEDIA_PREFIX))
        .map(str::to_string)
        .collect();
    names.sort_unstable();
    names.dedup();

    let (placements, shapes) = collect_drawing_data(archive);
    if !include_pictures {
        return (Vec::new(), shapes);
    }

    let mut pictures = Vec::with_capacity(names.len());
    let mut total_bytes: usize = 0;
    for name in names {
        let Some(data) = read_member(archive, &name, super::MAX_EXCEL_ZIP_MEMBER_SIZE) else {
            continue;
        };
        if data.is_empty() {
            tracing::warn!(part = %name, "skipping empty spreadsheet media part");
            continue;
        }
        // `max_content_size` is documented as the "decoded image allocation per
        // document" cap; the media parts of one workbook are exactly that.
        if total_bytes.saturating_add(data.len()) > limits.max_content_size {
            tracing::warn!(
                part = %name,
                limit = limits.max_content_size,
                "spreadsheet media exceeds SecurityLimits::max_content_size; remaining pictures skipped"
            );
            break;
        }
        total_bytes += data.len();

        let (placement, description) = match placements.get(&name) {
            Some((placement, description)) => (Some(placement.clone()), description.clone()),
            None => (None, None),
        };
        pictures.push(XlsxPicture {
            format: detect_format(&data, &name),
            data,
            source_path: name,
            sheet_name: placement.as_ref().map(|placement| placement.sheet_name.clone()),
            sheet_index: None,
            anchor: placement.as_ref().and_then(|placement| placement.cell),
            description: description.filter(|text| !text.trim().is_empty()),
        });
    }
    (pictures, shapes)
}

/// Detect the picture's format from its magic bytes, falling back to the part's
/// extension when the shared detector does not recognize the magic.
fn detect_format(data: &[u8], part: &str) -> Cow<'static, str> {
    if let Some(format) = magic_format(data) {
        return format;
    }
    match part.rsplit_once('.').map(|(_, extension)| extension) {
        Some(extension) if !extension.is_empty() => Cow::Owned(extension.to_ascii_lowercase()),
        _ => Cow::Borrowed("bin"),
    }
}

/// Format from magic bytes, via the detector shared with DOCX/PPTX.
///
/// `image_format` lives behind `any(office, ocr)`, so an Excel-only build
/// without either falls back to the extension instead of duplicating the
/// signature table; with `ocr` alone the shared detector is skipped anyway —
/// `xl/media/` part names always carry an extension, so the fallback is exact.
#[cfg(feature = "office")]
fn magic_format(data: &[u8]) -> Option<Cow<'static, str>> {
    let format = crate::extraction::image_format::detect_image_format(data);
    (format != "unknown").then_some(format)
}

#[cfg(not(feature = "office"))]
fn magic_format(_data: &[u8]) -> Option<Cow<'static, str>> {
    None
}

/// Read one ZIP member, bounded by its declared size.
///
/// The `zip` crate verifies the CRC only at EOF and puts no `Take` on the
/// decompressed side, so a member that declares a small size while carrying a
/// large deflate stream would inflate without bound here. The declared size is
/// checked against `limit` first, then the read is capped at declared + 1 so a
/// member that outgrows its own declaration is dropped instead of kept (the
/// same rule the DOCX image path applies).
fn read_member<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, name: &str, limit: u64) -> Option<Vec<u8>> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::debug!(part = name, error = %e, "spreadsheet part could not be opened");
            return None;
        }
    };
    let declared = entry.size();
    if declared > limit {
        tracing::warn!(
            part = name,
            size = declared,
            limit,
            "spreadsheet part exceeds the per-member size cap; skipped"
        );
        return None;
    }
    let mut data = Vec::with_capacity(usize::try_from(declared).unwrap_or(0));
    let mut bounded = entry.take(declared.saturating_add(1));
    if bounded.read_to_end(&mut data).is_err() {
        return None;
    }
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > declared {
        tracing::warn!(part = name, declared, "spreadsheet part outgrew its declared size; skipped");
        return None;
    }
    Some(data)
}

/// Walk the workbook's sheet list and every drawing part each sheet references,
/// recording where each media part is anchored and the text of every floating
/// shape. First placement wins, and the walk is sheet-ordered then
/// part-name-ordered, so the result is deterministic.
fn collect_drawing_data<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> (Placements, Vec<XlsxShapeText>) {
    let mut placements = Placements::new();
    let mut shapes = Vec::new();
    let mut visited_parts = std::collections::HashSet::new();
    let Some(workbook_xml) = read_member(archive, "xl/workbook.xml", super::MAX_EXCEL_ZIP_MEMBER_SIZE) else {
        return (placements, shapes);
    };
    let Some(workbook_rels_xml) =
        read_member(archive, "xl/_rels/workbook.xml.rels", super::MAX_EXCEL_ZIP_MEMBER_SIZE)
    else {
        return (placements, shapes);
    };
    let workbook_rels = parse_rels(&workbook_rels_xml);

    for (sheet_name, rel_id) in sheet_relationship_ids(&workbook_xml) {
        let Some(rel) = workbook_rels.get(&rel_id) else {
            continue;
        };
        let Some(worksheet) = resolve_relative("xl", &rel.target) else {
            continue;
        };
        collect_sheet_placements(
            archive,
            &worksheet,
            &sheet_name,
            &mut placements,
            &mut shapes,
            &mut visited_parts,
        );
    }
    (placements, shapes)
}

/// Collect the placements of every drawing/vmlDrawing part one worksheet
/// references, and the text of every shape those drawing parts carry.
///
/// `visited_parts` carries the DrawingML parts earlier sheets already consumed:
/// a hand-built package can hang one drawing part on two worksheets, and a
/// second walk would re-collect its shape text verbatim under the other sheet's
/// name, where the placements' first-wins rule cannot help.
fn collect_sheet_placements<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    worksheet: &str,
    sheet_name: &str,
    placements: &mut Placements,
    shapes: &mut Vec<XlsxShapeText>,
    visited_parts: &mut std::collections::HashSet<String>,
) {
    let Some(rels_path) = rels_path_for(worksheet) else {
        return;
    };
    let Some(rels_xml) = read_member(archive, &rels_path, super::MAX_EXCEL_ZIP_MEMBER_SIZE) else {
        return;
    };
    let rels = parse_rels(&rels_xml);
    if rels.is_empty() {
        return;
    }

    // A `HashMap` iterates in an arbitrary order; sort the referenced parts so
    // the first-wins rule never depends on hashing.
    let mut parts: Vec<(&str, bool)> = rels
        .values()
        .filter_map(|rel| {
            let is_vml = if rel.kind.ends_with("/drawing") {
                false
            } else if rel.kind.ends_with("/vmlDrawing") {
                true
            } else {
                return None;
            };
            Some((rel.target.as_str(), is_vml))
        })
        .collect();
    parts.sort_unstable();
    parts.dedup();

    for (target, is_vml) in parts {
        let Some(part) = resolve_relative(parent_dir(worksheet), target) else {
            continue;
        };
        // First sheet to reference this drawing part owns its walk, and the
        // gate sits before the ZIP reads so a later sheet sharing the part pays
        // nothing for it. The placements such a walk records are deduped
        // first-wins anyway; this is what keeps a shared part's shape text from
        // appearing twice. (VML writes only first-wins placements, so a repeat
        // walk is harmless there and needs no gate.)
        if !is_vml && !visited_parts.insert(part.clone()) {
            continue;
        }
        let Some(part_rels_path) = rels_path_for(&part) else {
            continue;
        };
        let Some(part_rels_xml) = read_member(archive, &part_rels_path, super::MAX_EXCEL_ZIP_MEMBER_SIZE) else {
            continue;
        };
        let part_rels = parse_rels(&part_rels_xml);
        if part_rels.is_empty() {
            continue;
        }
        let Some(xml) = read_member(archive, &part, super::MAX_EXCEL_ZIP_MEMBER_SIZE) else {
            continue;
        };
        if is_vml {
            collect_vml_placements(&xml, parent_dir(&part), sheet_name, &part_rels, placements);
        } else {
            collect_drawing_placements(&xml, parent_dir(&part), sheet_name, &part_rels, placements, shapes);
        }
    }
}

/// Record the placement of every `a:blip` in one DrawingML drawing part. The
/// enclosing cell anchor (`xdr:twoCellAnchor`/`oneCellAnchor`) carries the
/// `from` cell; a group's blips inherit the group anchor, which is the cell the
/// group starts at. The text of every `xdr:sp` in the same part is collected
/// alongside, since a shape is anchored the same way and both are read from the
/// one parse.
fn collect_drawing_placements(
    xml: &[u8],
    directory: &str,
    sheet_name: &str,
    rels: &HashMap<String, Rel>,
    placements: &mut Placements,
    shapes: &mut Vec<XlsxShapeText>,
) {
    let Some(text) = xml_text(xml) else {
        return;
    };
    let Ok(document) = roxmltree::Document::parse(text) else {
        tracing::debug!("spreadsheet drawing part is not well-formed XML; no anchors read");
        return;
    };

    for blip in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "blip")
    {
        let Some(rel_id) = attribute(&blip, RELATIONSHIPS_NS, "embed") else {
            // `r:link` is an externally referenced image: no media part to read.
            continue;
        };
        let Some(media) = rels
            .get(rel_id)
            .and_then(|rel| resolve_relative(directory, &rel.target))
        else {
            continue;
        };
        let anchor = blip.ancestors().find(|node| {
            node.is_element()
                && matches!(
                    node.tag_name().name(),
                    "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor"
                )
        });
        let cell = anchor.and_then(from_cell);
        let description = anchor
            .and_then(|anchor| {
                anchor
                    .descendants()
                    .find(|node| node.is_element() && node.tag_name().name() == "cNvPr")
            })
            .and_then(|name| name.attribute("descr"))
            .map(str::to_string);
        placements
            .entry(media)
            .or_insert((
                Placement {
                    sheet_name: sheet_name.to_string(),
                    cell,
                },
                description,
            ));
    }

    for shape in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "sp")
    {
        let Some(text) = shape_text(shape) else {
            continue;
        };
        let anchor = shape.ancestors().find(|node| {
            node.is_element()
                && matches!(
                    node.tag_name().name(),
                    "twoCellAnchor" | "oneCellAnchor" | "absoluteAnchor"
                )
        });
        shapes.push(XlsxShapeText {
            text,
            sheet_name: sheet_name.to_string(),
            anchor: anchor.and_then(from_cell),
        });
    }
}

/// The text of one DrawingML shape, or `None` when it carries none.
///
/// A `xdr:txBody` holds paragraphs (`a:p`) of runs (`a:t`); runs of the same
/// paragraph are contiguous text, so they are concatenated without a separator
/// while paragraphs are joined by a space.
///
/// `<`, `>` and `&` are kept raw: shape text becomes paragraph text nodes in
/// the internal document, and the CommonMark writer escapes those characters
/// itself (`\<`, `\>`) so they round-trip. Pre-encoding them as named entities
/// (`&lt;`) backfired — the writer escapes the `&` of the entity too, leaving
/// a literal `\&lt;` in the Markdown and a visible `&gt;` in plain output.
fn shape_text(shape: roxmltree::Node<'_, '_>) -> Option<String> {
    let body = shape
        .children()
        .find(|node| node.is_element() && node.tag_name().name() == "txBody")?;
    let mut paragraphs: Vec<String> = Vec::new();
    for paragraph in body
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "p")
    {
        let line: String = paragraph
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "t")
            .filter_map(|run| run.text())
            .collect();
        let line = line.trim();
        if !line.is_empty() {
            paragraphs.push(line.to_string());
        }
    }
    if paragraphs.is_empty() {
        return None;
    }
    Some(paragraphs.join(" "))
}

/// Record the placement of every legacy VML picture shape (`v:shape` holding a
/// `v:imagedata`) on one sheet. The VML `x:Anchor` lists eight comma-separated
/// values ordered `left column, left offset, top row, top offset, ...`.
fn collect_vml_placements(
    xml: &[u8],
    directory: &str,
    sheet_name: &str,
    rels: &HashMap<String, Rel>,
    placements: &mut Placements,
) {
    let Some(text) = xml_text(xml) else {
        return;
    };
    let Ok(document) = roxmltree::Document::parse(text) else {
        tracing::debug!("spreadsheet VML part is not well-formed XML; no anchors read");
        return;
    };

    for shape in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "shape")
    {
        let Some(image) = shape
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == "imagedata")
        else {
            continue;
        };
        let Some(rel_id) = attribute(&image, VML_OFFICE_NS, "relid") else {
            continue;
        };
        let Some(media) = rels
            .get(rel_id)
            .and_then(|rel| resolve_relative(directory, &rel.target))
        else {
            continue;
        };
        let cell = shape
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == "Anchor")
            .and_then(|anchor| anchor.text())
            .and_then(vml_cell);
        let description = shape.attribute("alt").map(str::to_string);
        placements
            .entry(media)
            .or_insert((
                Placement {
                    sheet_name: sheet_name.to_string(),
                    cell,
                },
                description,
            ));
    }
}

/// The `<xdr:from>` cell of a DrawingML anchor, as zero-based `(row, col)`.
fn from_cell(anchor: roxmltree::Node<'_, '_>) -> Option<(u32, u32)> {
    let from = anchor
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == "from")?;
    let col = child_number(&from, "col")?;
    let row = child_number(&from, "row")?;
    Some((row, col))
}

/// The parsed integer held by the named child element.
fn child_number(parent: &roxmltree::Node<'_, '_>, name: &str) -> Option<u32> {
    parent
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == name)?
        .text()?
        .trim()
        .parse()
        .ok()
}

/// The cell named by a legacy VML `<x:Anchor>` value.
fn vml_cell(text: &str) -> Option<(u32, u32)> {
    let mut fields = text.split(',').map(|field| field.trim().parse::<u32>().ok());
    let col = fields.next()??;
    let _left_offset = fields.next()??;
    let row = fields.next()??;
    Some((row, col))
}

/// The `(name, r:id)` of every `<sheet>` in `xl/workbook.xml`, in the workbook's
/// own sheet order. Pictures are keyed by name: the extractor only sees the
/// sheets it could read, so a positional index cannot survive a skipped sheet.
fn sheet_relationship_ids(workbook_xml: &[u8]) -> Vec<(String, String)> {
    let Some(text) = xml_text(workbook_xml) else {
        return Vec::new();
    };
    let Ok(document) = roxmltree::Document::parse(text) else {
        return Vec::new();
    };
    document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "sheet")
        .filter_map(|sheet| {
            let id = attribute(&sheet, RELATIONSHIPS_NS, "id")?;
            let name = sheet.attribute("name").unwrap_or_default();
            Some((name.to_string(), id.to_string()))
        })
        .collect()
}

/// Parse a `.rels` part into relationship id -> relationship.
fn parse_rels(xml: &[u8]) -> HashMap<String, Rel> {
    let mut rels = HashMap::new();
    let Some(text) = xml_text(xml) else {
        return rels;
    };
    let Ok(document) = roxmltree::Document::parse(text) else {
        tracing::debug!("spreadsheet relationships part is not well-formed XML; no targets read");
        return rels;
    };
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Relationship")
    {
        // An external target is a URL, not a package part, so it can never name media.
        if node.attribute("TargetMode") == Some("External") {
            continue;
        }
        let (Some(id), Some(target)) = (node.attribute("Id"), node.attribute("Target")) else {
            continue;
        };
        rels.insert(
            id.to_string(),
            Rel {
                target: target.to_string(),
                kind: node.attribute("Type").unwrap_or_default().to_string(),
            },
        );
    }
    rels
}

/// `xl/worksheets/sheet3.xml` -> `xl/worksheets/_rels/sheet3.xml.rels`.
fn rels_path_for(part: &str) -> Option<String> {
    let (directory, file) = part.rsplit_once('/')?;
    Some(format!("{directory}/_rels/{file}.rels"))
}

/// The directory holding a part, e.g. `xl/drawings` for `xl/drawings/drawing1.xml`.
fn parent_dir(part: &str) -> &str {
    part.rsplit_once('/').map(|(directory, _)| directory).unwrap_or("")
}

/// Resolve an OPC relationship target against the directory holding its source
/// part. Targets are package-relative (`../media/image1.png`); a target that
/// climbs above the package root resolves to `None` rather than escaping it.
fn resolve_relative(base_dir: &str, target: &str) -> Option<String> {
    let target = target.replace('\\', "/");
    let mut stack: Vec<&str> = Vec::new();
    let effective = match target.strip_prefix('/') {
        Some(root_relative) => root_relative,
        None => {
            for segment in base_dir.split('/') {
                if !segment.is_empty() && segment != "." {
                    stack.push(segment);
                }
            }
            target.as_str()
        }
    };
    for segment in effective.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            name => stack.push(name),
        }
    }
    if stack.is_empty() {
        None
    } else {
        Some(stack.join("/"))
    }
}

/// Decode a part as UTF-8 text for `roxmltree`, tolerating a leading BOM.
fn xml_text(xml: &[u8]) -> Option<&str> {
    let xml = xml.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(xml);
    std::str::from_utf8(xml).ok()
}

/// Read an attribute by namespace and local name, independent of the document's
/// prefix choice.
fn attribute<'a, 'input>(
    node: &roxmltree::Node<'a, 'input>,
    namespace: &str,
    local: &str,
) -> Option<&'a str> {
    node.attribute((namespace, local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_resolve_parent_relative_relationship_targets() {
        assert_eq!(
            resolve_relative("xl/worksheets", "../drawings/drawing1.xml").as_deref(),
            Some("xl/drawings/drawing1.xml")
        );
        assert_eq!(
            resolve_relative("xl/drawings", "../media/image4.emf").as_deref(),
            Some("xl/media/image4.emf")
        );
    }

    #[test]
    fn should_reject_relationship_targets_that_escape_the_package_root() {
        assert_eq!(resolve_relative("xl/drawings", "../../../../etc/passwd"), None);
    }

    #[test]
    fn should_read_the_from_cell_of_a_two_cell_anchor() {
        let xml = br#"<xdr:wsDr xmlns:xdr="urn:x" xmlns:r="urn:r"><xdr:twoCellAnchor><xdr:from><xdr:col>9</xdr:col><xdr:colOff>1</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>2</xdr:rowOff></xdr:from><xdr:pic><xdr:blipFill><a:blip xmlns:a="urn:a" r:embed="rId1"/></xdr:blipFill></xdr:pic></xdr:twoCellAnchor></xdr:wsDr>"#;
        let document = roxmltree::Document::parse(xml_text(xml).unwrap()).unwrap();
        let blip = document
            .descendants()
            .find(|node| node.tag_name().name() == "blip")
            .unwrap();
        let anchor = blip
            .ancestors()
            .find(|node| node.tag_name().name() == "twoCellAnchor")
            .unwrap();
        assert_eq!(from_cell(anchor), Some((4, 9)));
    }

    #[test]
    fn should_read_the_anchor_of_a_legacy_vml_picture() {
        // `LEFT COLUMN, LEFT OFFSET, TOP ROW, TOP OFFSET, ...` from the VML spec.
        assert_eq!(vml_cell("7, 326, 2, 10, 8, 129, 3, 237"), Some((2, 7)));
        assert_eq!(vml_cell("not an anchor"), None);
    }

    #[test]
    fn should_read_shape_text_with_runs_joined_and_paragraphs_spaced() {
        let xml = "<xdr:wsDr xmlns:xdr=\"urn:x\" xmlns:a=\"urn:a\"><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><xdr:sp><xdr:txBody><a:p><a:r><a:t>自动</a:t></a:r><a:r><a:t>编译链接</a:t></a:r></a:p><a:p><a:r><a:t>源码</a:t></a:r></a:p></xdr:txBody></xdr:sp></xdr:twoCellAnchor></xdr:wsDr>";
        let mut placements = Placements::new();
        let mut shapes = Vec::new();
        collect_drawing_placements(
            xml.as_bytes(),
            "xl/drawings",
            "流程图",
            &HashMap::new(),
            &mut placements,
            &mut shapes,
        );
        assert!(placements.is_empty(), "the part holds no picture relationship");
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].text, "自动编译链接 源码");
        assert_eq!(shapes[0].sheet_name, "流程图");
        assert_eq!(shapes[0].anchor, Some((2, 1)));
    }

    #[test]
    fn keeps_shape_text_that_is_only_punctuation_raw() {
        let xml = "<xdr:wsDr xmlns:xdr=\"urn:x\" xmlns:a=\"urn:a\"><xdr:sp><xdr:txBody><a:p><a:r><a:t>&gt;</a:t></a:r></a:p></xdr:txBody></xdr:sp></xdr:wsDr>";
        let mut placements = Placements::new();
        let mut shapes = Vec::new();
        collect_drawing_placements(
            xml.as_bytes(),
            "xl/drawings",
            "Sheet1",
            &HashMap::new(),
            &mut placements,
            &mut shapes,
        );
        assert_eq!(shapes.len(), 1);
        // Raw `>` in a paragraph text node: the CommonMark writer escapes it
        // (`\>`) on render; a stored `&gt;` would surface as a literal entity.
        assert_eq!(shapes[0].text, ">", "a connector glyph stays a raw `>`");
    }
}
