#[derive(Clone, Copy)]
enum Layout {
    SingleLine,
    Multiline,
}

/// Escapes terminal controls and line breaks while keeping a value on one display line.
pub(crate) fn single_line(value: &str) -> String {
    escape_unbounded(value, Layout::SingleLine, false)
}

/// Escapes terminal controls while preserving line feeds, indentation, and ordinary Unicode.
pub(crate) fn multiline(value: &str) -> String {
    escape_unbounded(value, Layout::Multiline, false)
}

/// Preserves the patch preview's existing rendering, including visible escaping of RTL scripts.
pub(crate) fn patch_preview(value: &str, limit: usize) -> Result<String, ()> {
    let mut escaped = String::with_capacity(value.len().min(limit));
    for character in value.chars() {
        let replacement = escaped_character(character, Layout::SingleLine, true);
        let additional_bytes = replacement
            .as_ref()
            .map_or(character.len_utf8(), String::len);
        if escaped.len().saturating_add(additional_bytes) > limit {
            return Err(());
        }
        if let Some(replacement) = replacement {
            escaped.push_str(&replacement);
        } else {
            escaped.push(character);
        }
    }
    Ok(escaped)
}

fn escape_unbounded(value: &str, layout: Layout, escape_rtl_scripts: bool) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if let Some(replacement) = escaped_character(character, layout, escape_rtl_scripts) {
            escaped.push_str(&replacement);
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn escaped_character(character: char, layout: Layout, escape_rtl_scripts: bool) -> Option<String> {
    match character {
        '\n' if matches!(layout, Layout::Multiline) => None,
        '\t' if matches!(layout, Layout::Multiline) => Some("    ".to_string()),
        '\n' => Some("\\n".to_string()),
        '\r' => Some("\\r".to_string()),
        '\t' => Some("\\t".to_string()),
        '\u{1b}' => Some("\\x1b".to_string()),
        '\u{2028}' => Some("\\u{2028}".to_string()),
        '\u{2029}' => Some("\\u{2029}".to_string()),
        character if escape_rtl_scripts && matches!(character as u32, 0x200c..=0x200d) => {
            Some(format!("\\u{{{:x}}}", character as u32))
        }
        character if is_bidi_or_invisible_control(character) => {
            Some(format!("\\u{{{:x}}}", character as u32))
        }
        character if character.is_control() => Some(format!("\\u{{{:x}}}", character as u32)),
        character if escape_rtl_scripts && is_rtl_script(character) => {
            Some(format!("\\u{{{:x}}}", character as u32))
        }
        _ => None,
    }
}

fn is_bidi_or_invisible_control(character: char) -> bool {
    matches!(
        character as u32,
        0x061c
            | 0x200b
            | 0x200e..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0xfeff
    )
}

fn is_rtl_script(character: char) -> bool {
    matches!(
        character as u32,
        0x0590..=0x08ff
            | 0xfb1d..=0xfdff
            | 0xfe70..=0xfeff
            | 0x10800..=0x10fff
            | 0x1e800..=0x1edff
            | 0x1ee00..=0x1eeff
            | 0x1ef00..=0x1efff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_csi_osc_cursor_and_clear_sequences_visibly() {
        let input = "before\u{1b}[2Jmid\u{1b}[10;4H\u{1b}]0;fake title\u{7}after";
        let output = single_line(input);
        assert_eq!(
            output,
            "before\\x1b[2Jmid\\x1b[10;4H\\x1b]0;fake title\\u{7}after"
        );
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{7}'));
    }

    #[test]
    fn escapes_c0_c1_and_terminal_line_separator_controls() {
        let output = single_line("a\0\u{1b}\u{7f}\u{85}\u{2028}\u{2029}z");
        assert_eq!(output, "a\\u{0}\\x1b\\u{7f}\\u{85}\\u{2028}\\u{2029}z");
    }

    #[test]
    fn escapes_bidi_format_controls_but_preserves_rtl_text_and_emoji() {
        let input = "العربية \u{202e}txt\u{202c} 👩‍💻";
        let output = single_line(input);
        assert_eq!(output, "العربية \\u{202e}txt\\u{202c} 👩‍💻");
        assert!(output.contains("العربية"));
        assert!(output.contains("👩‍💻"));
    }

    #[test]
    fn preserves_arabic_and_emoji_codepoints_while_escaping_bidi_controls() {
        let input = "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064a}\u{0629} \u{202e}x\u{202c} \u{1f469}\u{200d}\u{1f4bb}";
        let output = single_line(input);
        assert_eq!(
            output,
            "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064a}\u{0629} \\u{202e}x\\u{202c} \u{1f469}\u{200d}\u{1f4bb}"
        );
    }

    #[test]
    fn multiline_mode_preserves_newlines_tabs_and_indentation() {
        assert_eq!(
            multiline("  one\n\t two\r\nthree"),
            "  one\n     two\\r\nthree"
        );
        assert!(!multiline("\tindent").contains('\t'));
    }

    #[test]
    fn single_line_mode_displays_embedded_line_breaks() {
        assert_eq!(
            single_line("first\nsecond\rthird\tfourth"),
            "first\\nsecond\\rthird\\tfourth"
        );
    }

    #[test]
    fn accepts_empty_and_long_untrusted_values() {
        assert_eq!(single_line(""), "");
        let input = format!("{}\u{1b}[2J", "x".repeat(100_000));
        let output = single_line(&input);
        assert_eq!(output.len(), 100_000 + "\\x1b[2J".len());
        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn patch_preview_keeps_legacy_rtl_escaping_and_enforces_its_limit() {
        assert_eq!(patch_preview("ع", 10).unwrap(), "\\u{639}");
        assert_eq!(
            patch_preview("\u{200c}\u{200d}", 20).unwrap(),
            "\\u{200c}\\u{200d}"
        );
        assert!(patch_preview("long", 3).is_err());
    }
}
