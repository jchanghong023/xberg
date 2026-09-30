//! Format dispatch and read entry points (file/bytes -> parsed workbook) for the
//! Excel/ODS extraction module, split out of `excel.rs` to keep that file under the
//! line-count limit.

use super::*;

/// Reject a ZIP-backed spreadsheet (XLSX/XLSM/XLTM/XLAM/XLSB/ODS) whose ZIP central
/// directory declares more entries than the caller's configured
/// `SecurityLimits::max_files_in_archive`, or whose declared aggregate uncompressed
/// size or compression ratio exceeds `SecurityLimits::max_archive_size` /
/// `max_compression_ratio` (`ZipBombValidator`).
///
/// `calamine::Xlsx`/`Xlsb`/`Ods` own the `zip::ZipArchive` internally end-to-end
/// (`Reader::new` opens it and immediately reads workbook/style/shared-string parts
/// before returning) and never expose the archive, an entry count, or per-entry
/// sizes, so none of these limits can be enforced from inside calamine's read path.
/// This reads the archive's central directory once, ahead of the calamine open,
/// purely to enforce them — parsing the central directory does not decompress any
/// entry, so this is not "after the bomb has already been expanded." Errors out
/// rather than truncating, matching the DOCX/PPTX top-level containers
/// (`extraction::docx::parser::validate_archive_security`,
/// `extraction::pptx::container::check_entry_count` +
/// `extractors::security::ZipBombValidator`): a workbook depends on specific named
/// parts (`xl/workbook.xml`, `xl/_rels/workbook.xml.rels`, per-sheet XML) that cannot
/// survive an arbitrary truncation of the ZIP's central directory.
///
/// The entry-count check runs first and keeps its own specific error message
/// (existing tests assert on it); `ZipBombValidator::validate` also re-checks entry
/// count using the same `limits.max_files_in_archive`, which is a no-op here since
/// the explicit check above already returned on that condition, but keeps this
/// function equivalent to calling the validator directly.
///
/// Silently returns `Ok(())` when `reader` is not a readable ZIP at all (e.g. a legacy
/// `.xls`/`.xla` OLE2 file misrouted here) — the subsequent calamine open then reports a
/// format error with clearer context than this pre-check could.
///
/// `declared_legacy` names a format calamine reads as an OLE2/CFB container, never as a ZIP
/// (`.xls`/`.xla`). Only those may tolerate an unreadable entry header — what a stray central
/// directory inside an OLE2 container produces: the ZIP validator stops at the first entry it
/// cannot read, so tolerating it on a real ZIP would stop the accounting for every entry after
/// it. The flag comes from the declared format: `.xls`/`.xla` 走 CFB `Xls` reader，
/// 从不解压 ZIP 条目。
#[cfg(feature = "excel")]
fn validate_zip_container<R: Read + Seek>(reader: R, limits: &SecurityLimits, declared_legacy: bool) -> Result<()> {
    let mut archive = match zip::ZipArchive::new(reader) {
        Ok(archive) => archive,
        Err(_) => return Ok(()),
    };
    if archive.len() > limits.max_files_in_archive {
        return Err(XbergError::validation(format!(
            "Spreadsheet ZIP archive declares {} entries, which exceeds the configured limit of {} \
             (SecurityLimits::max_files_in_archive); reduce the archive's entry count or raise the limit",
            archive.len(),
            limits.max_files_in_archive
        )));
    }
    match crate::extractors::security::ZipBombValidator::new(limits.clone()).validate(&mut archive) {
        Ok(()) => Ok(()),
        Err(crate::extractors::security::SecurityError::UnreadableEntry { .. }) if declared_legacy => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether `validate_zip_container`'s unreadable-entry tolerance applies to this
/// spreadsheet: `.xls`/`.xla`（任意大小写）在下方直接派发 calamine 的 CFB `Xls`
/// reader（扩展名决定，不做内容嗅探，与 [`read_excel_bytes`] 同一契约），因此
/// OLE 容器后被追加的杂散 central directory 永远不会进入 ZIP 条目核算；其余
/// 扩展名全额核算。被改名为 `*.xls` 的 ZIP 会在 XLS 解析处失败，同样不会有任何
/// ZIP 条目被解压。
#[cfg(feature = "excel")]
pub(super) fn xls_zip_tolerance(raw_extension: &str) -> bool {
    raw_extension.eq_ignore_ascii_case("xls") || raw_extension.eq_ignore_ascii_case("xla")
}

pub(crate) fn read_excel_file(file_path: &str, limits: &SecurityLimits) -> Result<ExcelReadResult> {
    let lower_path = file_path.to_lowercase();
    let mut warnings: Vec<ProcessingWarning> = Vec::new();

    #[cfg(feature = "excel")]
    {
        // `.xls`/`.xla`（任意大小写）由下方显式派发到 calamine 的 CFB `Xls`
        // reader（扩展名决定，无内容嗅探），所以 OLE 容器携带的杂散 central
        // directory 不能触发 ZIP 条目核算；其余扩展名全额核算。
        let raw_extension = Path::new(file_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default();
        let declared_legacy = xls_zip_tolerance(raw_extension);
        let check_file = std::fs::File::open(file_path)?;
        validate_zip_container(std::io::BufReader::new(check_file), limits, declared_legacy)?;
    }
    #[cfg(not(feature = "excel"))]
    let _ = limits;

    #[cfg(feature = "office")]
    let office_metadata = if lower_path.ends_with(".xlsx")
        || lower_path.ends_with(".xlsm")
        || lower_path.ends_with(".xlam")
        || lower_path.ends_with(".xltm")
    {
        extract_xlsx_office_metadata_from_file(file_path).ok()
    } else if lower_path.ends_with(".ods") {
        extract_ods_office_metadata_from_file(file_path).ok()
    } else {
        None
    };

    #[cfg(not(feature = "office"))]
    let office_metadata: Option<HashMap<String, String>> = None;

    if lower_path.ends_with(".xlsx") || lower_path.ends_with(".xlsm") || lower_path.ends_with(".xltm") {
        return read_xlsx_family_file(file_path, office_metadata, warnings);
    }
    if lower_path.ends_with(".xlam") {
        return read_xlam_file(file_path, office_metadata, warnings);
    }
    if lower_path.ends_with(".xla") {
        return read_xla_file(file_path, office_metadata, warnings);
    }
    if lower_path.ends_with(".xlsb") {
        return read_xlsb_file(file_path, office_metadata, warnings);
    }
    if lower_path.ends_with(".xls") {
        return read_xls_file(file_path, office_metadata, warnings);
    }

    let workbook = match open_workbook_auto(Path::new(file_path)) {
        Ok(wb) => wb,
        Err(calamine::Error::Io(io_err)) => {
            if io_err.kind() == std::io::ErrorKind::InvalidData {
                return Err(XbergError::parsing(format!(
                    "Cannot detect Excel file format: {}",
                    io_err
                )));
            }
            // Real IO error - bubble up unchanged ~keep
            return Err(io_err.into());
        }
        Err(e) => return Err(XbergError::parsing(format!("Failed to parse Excel file: {}", e))),
    };

    let result = process_workbook(workbook, office_metadata, &mut warnings)?;
    Ok((result, warnings))
}

/// Read an XLSX/XLSM/XLTM file: parse via calamine's XLSX reader, then merge sheet
/// revisions and comments — the extra sidecar data only this primary XLSX-family format
/// carries (XLAM shares the same reader but not this sidecar data, see [`read_xlam_file`]).
fn read_xlsx_family_file(
    file_path: &str,
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let file = std::fs::File::open(file_path)?;
    let workbook = calamine::Xlsx::new(std::io::BufReader::new(file))
        .map_err(|e| XbergError::parsing(format!("Failed to parse XLSX: {}", e)))?;
    let mut result = process_xlsx_workbook(workbook, office_metadata, &mut warnings)?;
    result.revisions = extract_xlsx_revisions_from_file(file_path);
    if let Some(comments) = extract_xlsx_comments_from_file(file_path) {
        result.metadata.insert("comments".to_owned(), comments);
    }
    Ok((result, warnings))
}

/// Read a `.xlam` add-in file via the XLSX reader; on failure return an empty workbook with
/// a warning instead of failing the whole read (an add-in commonly carries no sheet data
/// worth extracting, so a parse failure here is not fatal).
fn read_xlam_file(
    file_path: &str,
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let file = std::fs::File::open(file_path)?;
    match calamine::Xlsx::new(std::io::BufReader::new(file)) {
        Ok(workbook) => {
            let result = process_xlsx_workbook(workbook, office_metadata, &mut warnings)?;
            Ok((result, warnings))
        }
        Err(e) => {
            push_warning(
                &mut warnings,
                "excel",
                format!("Workbook could not be parsed as XLSX and no sheets were extracted ({e})"),
            );
            Ok((
                ExcelWorkbook {
                    sheets: vec![],
                    metadata: office_metadata.unwrap_or_default(),
                    revisions: None,
                },
                warnings,
            ))
        }
    }
}

/// Read a legacy `.xla` add-in file via the XLS reader, with the same
/// fall-back-to-empty-workbook-on-parse-failure behavior as [`read_xlam_file`].
fn read_xla_file(
    file_path: &str,
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let file = std::fs::File::open(file_path)?;
    match calamine::Xls::new(std::io::BufReader::new(file)) {
        Ok(workbook) => {
            let result = process_workbook(workbook, office_metadata, &mut warnings)?;
            Ok((result, warnings))
        }
        Err(e) => {
            push_warning(
                &mut warnings,
                "excel",
                format!("Workbook could not be parsed as XLS and no sheets were extracted ({e})"),
            );
            Ok((
                ExcelWorkbook {
                    sheets: vec![],
                    metadata: office_metadata.unwrap_or_default(),
                    revisions: None,
                },
                warnings,
            ))
        }
    }
}

/// 把「构造 calamine 二进制 reader → 解析失败转 parsing 错误 → `process_workbook`」
/// 这一骨架收敛到一处：XLS/XLSB/ODS 的 file/bytes 五个入口仅差 reader 构造方式与
/// 错误文案 label，其余语句逐行相同。xlsx 家族与 xla/xlam 带 sidecar/fallback
/// 逻辑，不经此处。错误文案由 `label` 拼出，与原字面量逐字节一致。
fn read_binary_workbook<RS, W>(
    open: impl FnOnce(RS) -> std::result::Result<W, W::Error>,
    label: &str,
    input: RS,
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult>
where
    RS: std::io::Read + std::io::Seek,
    W: Reader<RS>,
    W::Error: std::fmt::Display,
{
    let workbook = open(input).map_err(|e| XbergError::parsing(format!("Failed to parse {label}: {}", e)))?;
    let result = process_workbook(workbook, office_metadata, &mut warnings)?;
    Ok((result, warnings))
}

/// Read a legacy `.xls` binary workbook through calamine's CFB reader directly —
/// the file-path counterpart of [`read_xls_bytes`]. The extension decides (any
/// casing), so a ZIP renamed `.xls` fails the XLS parse instead of being sniffed
/// into the Xlsx reader.
fn read_xls_file(
    file_path: &str,
    office_metadata: Option<HashMap<String, String>>,
    warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let file = std::fs::File::open(file_path)?;
    read_binary_workbook(
        calamine::Xls::new,
        "XLS",
        std::io::BufReader::new(file),
        office_metadata,
        warnings,
    )
}

/// Read a `.xlsb` binary workbook file.
fn read_xlsb_file(
    file_path: &str,
    office_metadata: Option<HashMap<String, String>>,
    warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let file = std::fs::File::open(file_path)?;
    read_binary_workbook(
        calamine::Xlsb::new,
        "XLSB",
        std::io::BufReader::new(file),
        office_metadata,
        warnings,
    )
}

pub(crate) fn read_excel_bytes(data: &[u8], file_extension: &str, limits: &SecurityLimits) -> Result<ExcelReadResult> {
    let warnings: Vec<ProcessingWarning> = Vec::new();

    #[cfg(feature = "excel")]
    {
        // `.xls`/`.xla` are dispatched straight to calamine's CFB reader below (no content
        // sniffing on this bytes path), so an unreadable entry header in a stray central
        // directory must not reject them. Every other extension — including unknown ones the
        // auto-detector may read as a ZIP — is accounted for in full.
        let extension = file_extension.to_lowercase();
        validate_zip_container(Cursor::new(data), limits, extension == ".xls" || extension == ".xla")?;
    }
    #[cfg(not(feature = "excel"))]
    let _ = limits;

    #[cfg(feature = "office")]
    let office_metadata = match file_extension.to_lowercase().as_str() {
        ".xlsx" | ".xlsm" | ".xlam" | ".xltm" => extract_xlsx_office_metadata_from_bytes(data).ok(),
        ".ods" => extract_ods_office_metadata_from_bytes(data).ok(),
        _ => None,
    };

    #[cfg(not(feature = "office"))]
    let office_metadata: Option<HashMap<String, String>> = None;

    match file_extension.to_lowercase().as_str() {
        ".xlsx" | ".xlsm" | ".xltm" => read_xlsx_family_bytes(data, office_metadata, warnings),
        ".xlam" => read_xlam_bytes(data, office_metadata, warnings),
        ".xls" => read_xls_bytes(data, office_metadata, warnings),
        ".xla" => read_xla_bytes(data, office_metadata, warnings),
        ".xlsb" => read_xlsb_bytes(data, office_metadata, warnings),
        ".ods" => read_ods_bytes(data, office_metadata, warnings),
        _ => Err(XbergError::parsing(format!(
            "Unsupported file extension: {}",
            file_extension
        ))),
    }
}

/// Read XLSX/XLSM/XLTM bytes: the byte-slice counterpart of [`read_xlsx_family_file`].
fn read_xlsx_family_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let cursor = Cursor::new(data);
    let workbook =
        calamine::Xlsx::new(cursor).map_err(|e| XbergError::parsing(format!("Failed to parse XLSX: {}", e)))?;
    let mut result = process_xlsx_workbook(workbook, office_metadata, &mut warnings)?;
    result.revisions = extract_xlsx_revisions_from_bytes(data);
    if let Some(comments) = extract_xlsx_comments_from_bytes(data) {
        result.metadata.insert("comments".to_owned(), comments);
    }
    Ok((result, warnings))
}

/// Read `.xlam` bytes: the byte-slice counterpart of [`read_xlam_file`].
fn read_xlam_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let cursor = Cursor::new(data);
    match calamine::Xlsx::new(cursor) {
        Ok(workbook) => {
            let result = process_xlsx_workbook(workbook, office_metadata, &mut warnings)?;
            Ok((result, warnings))
        }
        Err(e) => {
            push_warning(
                &mut warnings,
                "excel",
                format!("Workbook could not be parsed as XLSX and no sheets were extracted ({e})"),
            );
            Ok((
                ExcelWorkbook {
                    sheets: vec![],
                    metadata: office_metadata.unwrap_or_default(),
                    revisions: None,
                },
                warnings,
            ))
        }
    }
}

/// Read `.xls` bytes (no add-in fallback — see [`read_xla_bytes`] for the add-in variant).
fn read_xls_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    read_binary_workbook(calamine::Xls::new, "XLS", Cursor::new(data), office_metadata, warnings)
}

/// Read `.xla` bytes: the byte-slice counterpart of [`read_xla_file`].
fn read_xla_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    mut warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    let cursor = Cursor::new(data);
    match calamine::Xls::new(cursor) {
        Ok(workbook) => {
            let result = process_workbook(workbook, office_metadata, &mut warnings)?;
            Ok((result, warnings))
        }
        Err(e) => {
            push_warning(
                &mut warnings,
                "excel",
                format!("Workbook could not be parsed as XLS and no sheets were extracted ({e})"),
            );
            Ok((
                ExcelWorkbook {
                    sheets: vec![],
                    metadata: office_metadata.unwrap_or_default(),
                    revisions: None,
                },
                warnings,
            ))
        }
    }
}

/// Read `.xlsb` bytes: the byte-slice counterpart of [`read_xlsb_file`].
fn read_xlsb_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    read_binary_workbook(
        calamine::Xlsb::new,
        "XLSB",
        Cursor::new(data),
        office_metadata,
        warnings,
    )
}

/// Read `.ods` bytes. `read_excel_file` has no direct counterpart — an on-disk `.ods` falls
/// through to `open_workbook_auto`, which dispatches to the same `calamine::Ods` reader.
fn read_ods_bytes(
    data: &[u8],
    office_metadata: Option<HashMap<String, String>>,
    warnings: Vec<ProcessingWarning>,
) -> Result<ExcelReadResult> {
    read_binary_workbook(calamine::Ods::new, "ODS", Cursor::new(data), office_metadata, warnings)
}
