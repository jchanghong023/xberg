//! Shared Markdown helpers that must compile without the `office` feature.

/// Move a markdown image marker out of the `text` fence it was baked into.
///
/// The PPTX content builder writes a picture's placeholder into the same code block as the
/// listing around it, so the picture comes out as `![](image_7.png)` *inside* a fence. Markdown
/// never fetches an image there — a reader sees a path where the drawing should be — and the
/// reference is not clickable either. The marker becomes its own paragraph and the fence keeps
/// only the code, which is the half a fence is for. A fence whose body holds nothing but the
/// marker loses the fence with it (leaving the closing line behind would open a fence that
/// never ends). Fences whose first line is any other text are left untouched. An opener longer
/// than three backticks — what a renderer writes when the fenced body itself holds backticks —
/// is matched from its first backtick and re-opened with that same run.
pub fn lift_image_markers_out_of_fences(content: &mut String) {
    if !content.contains("```text") {
        return;
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content.as_str();
    while let Some(found) = rest.find("```text") {
        // `find` lands on the last three backticks of a longer opener (like "````text"), so walk
        // back to the run's first one: anything left in the prefix leaks into the paragraph the
        // marker is lifted into as a stray backtick.
        let mut position = found;
        while position > 0 && rest.as_bytes()[position - 1] == b'`' {
            position -= 1;
        }
        let mut opener_backticks = 0usize;
        while rest.as_bytes().get(position + opener_backticks) == Some(&b'`') {
            opener_backticks += 1;
        }
        let opening_end = position + opener_backticks + "text".len();
        let body_start = match rest[opening_end..].strip_prefix("\r\n") {
            Some(_) => opening_end + 2,
            None if rest[opening_end..].starts_with('\n') => opening_end + 1,
            None => {
                out.push_str(&rest[..opening_end]);
                rest = &rest[opening_end..];
                continue;
            }
        };
        let body_start = body_start
            + rest[body_start..].len()
            - rest[body_start..].trim_start_matches(['\r', '\n']).len();
        let body = &rest[body_start..];
        let first_line_end = body.find('\n').map_or(body.len(), |index| index + 1);
        let first_line = body[..first_line_end].trim_end_matches(['\r', '\n']).trim();
        let is_marker = first_line.starts_with("![") && first_line.contains("](") && first_line.ends_with(')');
        if !is_marker {
            out.push_str(&rest[..body_start]);
            rest = body;
            continue;
        }
        out.push_str(&rest[..position]);
        out.push_str(first_line);
        out.push_str("\n\n");
        rest = &body[first_line_end..];
        // Only a line that is nothing but backticks — at least as many as the opener — closes
        // the fence. A body line that merely starts with backticks (a nested ```python opener,
        // say) is content: skipping it dropped the line from the output and left the re-opened
        // fence below unclosed, which then swallows every paragraph after it on re-parse.
        let next_line = rest.split('\n').next().unwrap_or(rest).trim_end();
        let closes_fence = next_line.len() >= opener_backticks && next_line.bytes().all(|byte| byte == b'`');
        if closes_fence {
            let closing_end = rest.find('\n').map_or(rest.len(), |index| index + 1);
            rest = &rest[closing_end..];
        } else {
            // Re-open with the opener's own run: its length was picked to outgrow the body's
            // backtick runs, and a shorter one could be closed early by a body line.
            out.push_str(&"`".repeat(opener_backticks));
            out.push_str("text\n");
        }
    }
    out.push_str(rest);
    *content = out;
}

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

    /// A fence longer than three backticks is a fence too: the marker must be lifted from it
    /// whole (the search lands on the opener's last three backticks, so the earlier ones must
    /// not be left behind as loose text) and the listing re-opened with the opener's own run.
    #[test]
    fn longer_fence_opener_is_handled_whole() {
        let mut content = String::from("````text\n![](image_3.png)\nlet x = 1;\n````\n");
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, "![](image_3.png)\n\n````text\nlet x = 1;\n````\n",
            "the 4-backtick opener must not leak a backtick and must be re-opened at its own length"
        );
    }
}
