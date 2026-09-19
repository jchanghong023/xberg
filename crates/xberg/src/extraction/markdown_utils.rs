//! Shared Markdown helpers that must compile without the `office` feature.

/// A code fence's opening marker: the fence character and its length.
///
/// CommonMark: a fence opens with three or more backticks or tildes, optionally indented by
/// up to three spaces; the line may carry an info string after the marker (for a backtick
/// fence the info string must not contain a backtick). Shared by the content rewriters that
/// must leave fenced code untouched — an `image_N` reference inside a fence is literal
/// text, not a file reference.
///
/// Known blind spot: a fence the CommonMark writer emitted inside a container carries the
/// container's prefix (`"> "` for block quotes, deeper indentation for nested list items),
/// and a prefixed opener is not recognized here. Fenced bodies in that shape take part in
/// the prose passes; accepted trade-off — recognizing arbitrary prefixes would need a
/// matching closer rule and risks reclassifying indented prose as code.
pub fn code_fence_open(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = rest.chars().take_while(|&character| character == marker).count();
    if length < 3 {
        return None;
    }
    if marker == '`' && rest[length..].contains('`') {
        return None;
    }
    Some((marker, length))
}

/// Whether `line` closes the fence that `(marker, length)` opened: the same character, at
/// least as many of them, nothing else on the line but ASCII spaces and tabs (CommonMark
/// accepts no other trailing content — a general `trim()` would let arbitrary Unicode
/// whitespace pass as a closer), at most three spaces of indentation.
pub fn code_fence_close(line: &str, marker: char, length: usize) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let run = rest.chars().take_while(|&character| character == marker).count();
    run >= length && rest[run..].bytes().all(|byte| byte == b' ' || byte == b'\t')
}

/// Code-fence state across the lines of a Markdown document.
///
/// [`FenceTracker::fenced`] reports whether a line is fence content (the opening and closing
/// lines included) after updating the state with it; content rewriters skip such lines so
/// fenced code stays verbatim.
#[derive(Debug, Default)]
pub struct FenceTracker {
    open: Option<(char, usize)>,
}

impl FenceTracker {
    /// Whether `line` belongs to a code fence, having first updated the state with it.
    pub fn fenced(&mut self, line: &str) -> bool {
        match self.open {
            Some((marker, length)) => {
                if code_fence_close(line, marker, length) {
                    self.open = None;
                }
                true
            }
            None => match code_fence_open(line) {
                Some(opened) => {
                    self.open = Some(opened);
                    true
                }
                None => false,
            },
        }
    }
}

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
///
/// Accepted trade-off: the first line alone decides, so a fence whose genuine content starts
/// with a full marker shape (an OCR'd Markdown tutorial, say) loses that line from the fence.
/// The builder's baked-in marker and such a first line are indistinguishable at this stage,
/// and the corpus contains no fence of that shape.
pub fn lift_image_markers_out_of_fences(content: &mut String) {
    if !content.contains("```text") {
        return;
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content.as_str();
    // Whether `rest` begins at a real line start. The skip paths below slice `rest` at
    // arbitrary byte offsets; a slice cut mid-line has no newline behind its first
    // match, and without this flag the line-start check below would mistake the same
    // line's second "```text" for an opener at column zero.
    let mut rest_starts_at_line_start = true;
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
        // A fence opener must sit at the start of its line (CommonMark allows up to three
        // spaces of indentation). "```text" anywhere else on the line is literal text — an
        // inline snippet in a markdown tutorial — and must survive verbatim.
        let at_line_start = match rest[..position].rfind('\n') {
            Some(index) => {
                let indent = &rest[index + 1..position];
                indent.len() <= 3 && indent.bytes().all(|byte| byte == b' ')
            }
            None => rest_starts_at_line_start && {
                let indent = &rest[..position];
                indent.len() <= 3 && indent.bytes().all(|byte| byte == b' ')
            },
        };
        let body_start = if at_line_start {
            match rest[opening_end..].strip_prefix("\r\n") {
                Some(_) => Some(opening_end + 2),
                None if rest[opening_end..].starts_with('\n') => Some(opening_end + 1),
                None => None,
            }
        } else {
            None
        };
        let body_start = match body_start {
            Some(start) => {
                start + rest[start..].len() - rest[start..].trim_start_matches(['\r', '\n']).len()
            }
            None => {
                out.push_str(&rest[..opening_end]);
                rest = &rest[opening_end..];
                rest_starts_at_line_start = false;
                continue;
            }
        };
        let body = &rest[body_start..];
        let first_line_end = body.find('\n').map_or(body.len(), |index| index + 1);
        let first_line = body[..first_line_end].trim_end_matches(['\r', '\n']).trim();
        let is_marker = first_line.starts_with("![") && first_line.contains("](") && first_line.ends_with(')');
        if !is_marker {
            out.push_str(&rest[..body_start]);
            // The fence's body is literal content: skip past its closing line
            // instead of scanning inside it, so a ```text line in tutorial-like
            // OCR text can never be mistaken for a fresh opener and rewritten.
            // The fence itself is kept verbatim (its opener was just pushed).
            let mut consumed = 0usize;
            for line in body.split_inclusive('\n') {
                consumed += line.len();
                let bare = line.trim_end_matches(['\r', '\n']);
                if code_fence_close(bare, '`', opener_backticks) {
                    break;
                }
            }
            out.push_str(&body[..consumed]);
            rest = &body[consumed..];
            rest_starts_at_line_start = true;
            continue;
        }
        out.push_str(&rest[..position]);
        out.push_str(first_line);
        out.push_str("\n\n");
        rest = &body[first_line_end..];
        // `rest` begins at a line start here; both arms of the closer match below
        // re-slice to a line boundary and set `rest_starts_at_line_start` themselves.
        // Find the closing line: only a line of nothing but the fence character —
        // at least as many as the opener, at most three spaces of indent — closes
        // the fence. A body line that merely starts with backticks (a nested
        // ```python opener, say) is content: skipping it dropped the line from the
        // output and left the re-opened fence below unclosed, which then swallowed
        // every paragraph after it on re-parse.
        let mut inter_body_end = 0usize;
        let mut closer_end = None;
        let mut consumed = 0usize;
        for line in rest.split_inclusive('\n') {
            let bare = line.trim_end_matches(['\r', '\n']);
            if code_fence_close(bare, '`', opener_backticks) {
                closer_end = Some(consumed + line.len());
                break;
            }
            consumed += line.len();
            inter_body_end = consumed;
        }
        match closer_end {
            Some(end) if rest[..inter_body_end].trim().is_empty() => {
                // The marker was the fence's only content: drop the fence with
                // its closer instead of re-opening an empty one.
                rest = &rest[end..];
                rest_starts_at_line_start = true;
            }
            // The marker was the fence's only content and the input's own
            // fence never closed (EOF came first): there is nothing left to
            // fence, and re-opening would only append a dangling empty
            // ```text block after the lifted marker.
            None if rest.trim().is_empty() => {
                rest = &rest[rest.len()..];
                rest_starts_at_line_start = true;
            }
            _ => {
                // Re-open with the opener's own run: its length was picked to
                // outgrow the body's backtick runs, and a shorter one could be
                // closed early by a body line. The body and closer are literal
                // content of the fence just re-opened: emit them verbatim and
                // resume after the closer — a "```text" line inside the body
                // would otherwise be mistaken for a fresh opener and rewritten
                // while still inside a fence the output already opened.
                let end = closer_end.unwrap_or(rest.len());
                out.push_str(&"`".repeat(opener_backticks));
                out.push_str("text\n");
                out.push_str(&rest[..end]);
                rest = &rest[end..];
                rest_starts_at_line_start = true;
            }
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

    /// A fence opener must sit at the start of its line: "```text" mid-line is literal text
    /// even when a newline follows it (CommonMark never opens a fence there). The old
    /// implementation took it for an opener, lifted the next line's marker out of a fence
    /// that never existed, and deleted the closing line — `"text before ```text\n![](i.png)\n```\n"`
    /// came back as `"text before ![](i.png)\n\n"` — so the assertion below discriminates:
    /// with the line-start check the passage must survive byte for byte.
    #[test]
    fn midline_text_fence_before_a_newline_is_not_an_opener() {
        let source = String::from("text before ```text\n![](image_1.png)\n```\n");
        let mut content = source.clone();
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, source,
            "a mid-line ```text is literal text: nothing may be lifted or deleted"
        );
    }

    /// A ```text line that sits INSIDE another fence's body is literal content
    /// (a markdown tutorial the fence quotes): the lifter must skip the whole
    /// outer fence instead of treating the inner line as a fresh opener — the
    /// old scan continued inside the body, lifted the "marker" out of a fence
    /// that never opened, and deleted the inner closing line.
    #[test]
    fn text_fence_line_inside_another_fence_body_is_not_lifted() {
        let source = String::from("````text\n```text\n![](image_1.png)\n```\n````\n");
        let mut content = source.clone();
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, source,
            "a ```text inside a fence body is literal text: nothing may be lifted or deleted"
        );
    }

    /// Two literal "```text" spellings on ONE line: the scan skips past the first one
    /// mid-line, and the slice that remains must remember it is no longer at a line
    /// start — the old line-start check saw no newline behind the second match, took it
    /// for an opener at column zero, lifted the next line's "marker" out of a fence that
    /// never opened, and deleted the closing line.
    #[test]
    fn second_midline_text_fence_on_the_same_line_is_not_an_opener() {
        let source = String::from("intro\n\n```text```text\n![](image_9.png)\n```\n\noutro\n");
        let mut content = source.clone();
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, source,
            "a second ```text on the same line is literal text: nothing may be lifted or deleted"
        );
    }

    /// A ```text line inside the body of a fence whose marker was lifted is literal
    /// content of the re-opened fence: emitting only the opener and leaving the body to
    /// the next scan iteration let that inner line pass as a fresh opener, lifted the
    /// "marker" after it, and deleted the closing line — leaving an unclosed fence that
    /// swallowed the rest of the document on re-parse.
    #[test]
    fn text_fence_inside_a_lifted_marker_fence_stays_literal() {
        let mut content =
            String::from("```text\n![](image_0.png)\n```text\n![](image_1.png)\n```\n");
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, "![](image_0.png)\n\n```text\n```text\n![](image_1.png)\n```\n",
            "only the outer marker is lifted; the inner ```text and its marker stay verbatim"
        );
    }

    /// When the lifted marker was the fence's only content apart from blank
    /// lines, the whole fence goes away — re-opening an empty ```text block
    /// left stray fence noise in the output.
    #[test]
    fn fence_left_with_only_the_marker_and_blanks_is_dropped_whole() {
        let mut content = String::from("```text\n![](image_0.png)\n\n```\nafter\n");
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(content, "![](image_0.png)\n\nafter\n");
    }

    /// An unclosed fence whose marker is its only content ends the document:
    /// the input's own fence never closed, so re-opening one after the lift
    /// appended a dangling empty ```text block at EOF.
    #[test]
    fn unclosed_marker_only_fence_at_eof_does_not_reopen() {
        let mut content = String::from("```text\n![](image_0.png)\n");
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(
            content, "![](image_0.png)\n\n",
            "no dangling empty fence after the lifted marker"
        );
        // With body text after the marker the fence is still re-opened: that
        // content must survive verbatim behind its own opener.
        let mut content = String::from("```text\n![](image_0.png)\nlet x = 1;\n");
        lift_image_markers_out_of_fences(&mut content);
        assert_eq!(content, "![](image_0.png)\n\n```text\nlet x = 1;\n");
    }

    /// The tracker recognizes backtick and tilde fences with info strings, keeps lines
    /// between an opener and its closer fenced, and requires a closing run at least as long
    /// as the opener.
    #[test]
    fn fence_tracker_pairs_openers_with_their_closers() {
        let mut tracker = FenceTracker::default();
        assert!(tracker.fenced("```rust"));
        assert!(tracker.fenced("let x = ![](image_0.png);"));
        // A two-backtick line inside a three-backtick fence is fence content, not a closer.
        assert!(tracker.fenced("`` above is code"));
        assert!(tracker.fenced("  ```"));
        assert!(!tracker.fenced("plain paragraph"));

        let mut tracker = FenceTracker::default();
        assert!(tracker.fenced("~~~text info with ~~~ tildes"));
        assert!(tracker.fenced("body"));
        assert!(tracker.fenced("~~ a shorter run does not close"));
        assert!(tracker.fenced("~~~~"));
        assert!(!tracker.fenced("plain paragraph"));
    }
}
