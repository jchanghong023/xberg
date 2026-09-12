//! Shared Markdown helpers that must compile without the `office` feature.

/// Drop filesystem-path alt text that Office authors left as local image paths.
///
/// Word/PowerPoint sometimes bake `C:\Users\...\image.png` into `@descr`/`@name`.
/// Those strings are useless as Markdown alt text and leak host paths into output.
/// Returns `None` when the value is empty or path-like; otherwise returns the
/// original string trimmed.
pub(crate) fn sanitize_image_alt_text(alt: Option<String>) -> Option<String> {
    let alt = alt?;
    let trimmed = alt.trim();
    if trimmed.is_empty() {
        return None;
    }
    if looks_like_filesystem_path(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Heuristic for absolute/relative filesystem paths that should not become alt text.
fn looks_like_filesystem_path(value: &str) -> bool {
    let has_sep = value.contains('\\') || value.contains('/');
    if !has_sep {
        return false;
    }
    // Windows drive path (`C:\...`, `C:/...`) or UNC (`\\server\share`).
    if value.len() >= 3 {
        let bytes = value.as_bytes();
        if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return true;
        }
    }
    if value.starts_with("\\\\") || value.starts_with("//") {
        return true;
    }
    // Path-like with a trailing image extension (e.g. `media/image1.png`, `../media/x.emf`).
    let lower = value.to_ascii_lowercase();
    matches!(
        lower.rsplit(|c| c == '/' || c == '\\').next(),
        Some(name)
            if name.ends_with(".png")
                || name.ends_with(".jpg")
                || name.ends_with(".jpeg")
                || name.ends_with(".gif")
                || name.ends_with(".bmp")
                || name.ends_with(".emf")
                || name.ends_with(".wmf")
                || name.ends_with(".tif")
                || name.ends_with(".tiff")
                || name.ends_with(".webp")
                || name.ends_with(".svg")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_alt_drops_windows_paths() {
        assert_eq!(
            sanitize_image_alt_text(Some(
                r"C:\Users\l00281545\AppData\Roaming\eSpace_Desktop\UserData\l00662707\imagefiles\0F6B87CD-8E5F-455F-83F1-A3D604D8CE3A.png".to_string()
            )),
            None
        );
        assert_eq!(sanitize_image_alt_text(Some("media/image1.emf".to_string())), None);
        assert_eq!(sanitize_image_alt_text(Some("海思-修".to_string())), Some("海思-修".to_string()));
        assert_eq!(sanitize_image_alt_text(Some("  BD21298_  ".to_string())), Some("BD21298_".to_string()));
        assert_eq!(sanitize_image_alt_text(None), None);
        assert_eq!(sanitize_image_alt_text(Some("   ".to_string())), None);
    }
}
