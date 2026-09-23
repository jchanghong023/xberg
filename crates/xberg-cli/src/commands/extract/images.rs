//! Writing extracted images to disk.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use xberg::ExtractedImage;

/// Write extracted images to `output_dir`, using the same `image_{index}.{format}` naming
/// convention the markdown renderer uses for its `![](image_N.ext)` references.
///
/// Images with empty data (placeholder `.bin` entries) are skipped — they have no bytes to write.
pub(super) fn write_extracted_images(images: &[ExtractedImage], output_dir: &Path) -> Result<()> {
    for img in images {
        if img.data.is_empty() {
            continue;
        }
        let filename = format!("image_{}.{}", img.image_index, img.format);
        let dest = output_dir.join(&filename);
        std::fs::write(&dest, &img.data).with_context(|| format!("Failed to write image file '{}'", dest.display()))?;
    }
    Ok(())
}

/// Whether any extracted image actually carries bytes. Placeholder entries (empty
/// `data`, e.g. relationship-only `.bin` stubs) are skipped by [`write_extracted_images`],
/// so they must not gate directory creation or reference prefixing either: a `doc_N`
/// directory holding no files, or references pointing into a directory nothing was
/// written to, are both worse than the untouched references.
pub(super) fn has_writable_images(images: &[ExtractedImage]) -> bool {
    images.iter().any(|image| !image.data.is_empty())
}

/// Directory a batch result's extracted images are written to.
///
/// Always namespaced by the result's position in `results` — the only self-proving unique key —
/// under `base` (`--output-dir` when given, otherwise `.`). Writing every document's
/// `image_N.ext` into one directory made each document overwrite the previous one's pictures
/// while their references all pointed at the survivors.
pub(super) fn batch_image_dir(base: &Path, result_index: usize) -> PathBuf {
    base.join(format!("doc_{}", result_index + 1))
}

/// Rewrite `](image_N.ext)` references so they name the directory the image files were written
/// to, using forward slashes so the result stays portable markdown.
///
/// The directory is percent-encoded for the characters that would end or unbalance a CommonMark
/// link destination: a space ends the destination, an unbalanced `)` closes it early, and `<`
/// or `>` can open the pointy-bracket form. `C:\Users\John Doe\out` otherwise produced
/// `![](C:/Users/John Doe/out/image_0.png)`, which no renderer resolves and which leaks the
/// path tail as loose text.
///
/// Lines inside a code fence are literal text — a listing that documents the very references
/// rewritten here — and are passed through verbatim.
pub(super) fn prefix_image_refs(content: &str, dir: &Path) -> String {
    let normalized = dir.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(normalized.len());
    for character in normalized.trim_end_matches('/').chars() {
        match character {
            ' ' => encoded.push_str("%20"),
            '(' => encoded.push_str("%28"),
            ')' => encoded.push_str("%29"),
            '<' => encoded.push_str("%3C"),
            '>' => encoded.push_str("%3E"),
            '"' => encoded.push_str("%22"),
            '`' => encoded.push_str("%60"),
            // A literal `%` must be encoded or a renderer percent-decodes the directory into a
            // different one (`100%25` -> `100%`). Unlike the library's `sanitize_marker_url`,
            // which rewrites document-supplied (already percent-encoded) relationship targets,
            // this is a raw filesystem path, so encoding `%` cannot double-encode anything.
            '%' => encoded.push_str("%25"),
            // A literal `#` survives CommonMark destination parsing, but a rendered URL
            // cuts the destination at it (fragment start), so the image stops loading.
            '#' => encoded.push_str("%23"),
            // `?` is the same story one query-string earlier: a rendered URL treats it
            // as the query's start, so the file lookup fails. Legal in a Unix file
            // name, so the cross-platform hand-off this function documents can
            // actually carry one.
            '?' => encoded.push_str("%3F"),
            // Control characters cannot appear in a Windows file name, but a path handed to the
            // CLI on another platform must not break the marker line either — the library drops
            // them for the same reason.
            control if control.is_control() => {}
            other => encoded.push(other),
        }
    }
    let prefix = format!("]({encoded}/image_");
    let mut out = String::with_capacity(content.len());
    let mut fence = xberg::extraction::markdown_utils::FenceTracker::default();
    for line in content.split_inclusive('\n') {
        let terminator = if line.ends_with('\n') { "\n" } else { "" };
        let line_body = line.strip_suffix('\n').unwrap_or(line);
        let had_cr = line_body.strip_suffix('\r').is_some();
        let body = line_body.strip_suffix('\r').unwrap_or(line_body);
        if fence.fenced(body) {
            out.push_str(line);
        } else {
            out.push_str(&body.replace("](image_", &prefix));
            // The replacement sees the line's own characters only; a CRLF ending keeps its `\r`.
            if had_cr {
                out.push('\r');
            }
            out.push_str(terminator);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::borrow::Cow;
    use tempfile::tempdir;

    fn make_image(index: u32, format: &'static str, data: &[u8]) -> ExtractedImage {
        ExtractedImage {
            data: Bytes::copy_from_slice(data),
            format: Cow::Borrowed(format),
            image_index: index,
            ..Default::default()
        }
    }

    #[test]
    fn write_extracted_images_creates_files_with_correct_names() {
        let dir = tempdir().unwrap();
        let images = vec![
            make_image(0, "png", b"\x89PNG\r\n"),
            make_image(1, "jpeg", b"\xff\xd8\xff"),
        ];

        write_extracted_images(&images, dir.path()).unwrap();

        assert!(dir.path().join("image_0.png").exists());
        assert!(dir.path().join("image_1.jpeg").exists());
        assert_eq!(std::fs::read(dir.path().join("image_0.png")).unwrap(), b"\x89PNG\r\n");
    }

    #[test]
    fn write_extracted_images_skips_empty_data() {
        let dir = tempdir().unwrap();
        let images = vec![make_image(0, "bin", b"")];

        write_extracted_images(&images, dir.path()).unwrap();

        assert!(
            !dir.path().join("image_0.bin").exists(),
            "empty-data image must not be written"
        );
    }

    #[test]
    fn write_extracted_images_uses_image_index_not_position() {
        let dir = tempdir().unwrap();
        let images = vec![make_image(3, "png", b"abc"), make_image(7, "png", b"def")];

        write_extracted_images(&images, dir.path()).unwrap();

        assert!(dir.path().join("image_3.png").exists());
        assert!(dir.path().join("image_7.png").exists());
        assert!(!dir.path().join("image_0.png").exists());
        assert!(!dir.path().join("image_1.png").exists());
    }

    /// Lines inside a code fence are literal text — a listing that shows the very references
    /// rewritten here — so only the reference outside the fence gains the directory prefix.
    #[test]
    fn prefix_image_refs_skips_code_fences() {
        let content = "见 ![](image_0.png)\n\n```text\n![](image_0.png)\n```\n";
        let prefixed = prefix_image_refs(content, Path::new("out dir"));

        assert!(
            prefixed.starts_with("见 ![](out%20dir/image_0.png)\n\n"),
            "the reference outside the fence must be prefixed: {prefixed:?}"
        );
        assert!(
            prefixed.contains("```text\n![](image_0.png)\n```"),
            "the fenced reference is literal text and must stay verbatim: {prefixed:?}"
        );
    }

    /// A directory name containing `#` survives CommonMark destination parsing, but the
    /// rendered URL would cut the destination at it (fragment start) and the image stops
    /// loading — so `#` is percent-encoded like the other reserved characters.
    #[test]
    fn prefix_image_refs_encodes_hash_in_directory() {
        let prefixed = prefix_image_refs("![](image_0.png)\n", Path::new("a#b"));
        assert_eq!(prefixed, "![](a%23b/image_0.png)\n");
    }
}
