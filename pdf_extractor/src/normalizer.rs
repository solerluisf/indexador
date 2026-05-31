use unicode_normalization::UnicodeNormalization;
const MAX_LINE_LENGTH: usize = 10_000;

pub fn normalize_text(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let collapsed: String = nfkc
        .chars()
        .fold((String::with_capacity(nfkc.len()), false), |(mut acc, prev_was_space), c| {
            if c == '\n' {
                acc.push('\n');
                (acc, false)
            } else if c.is_whitespace() || c.is_control() {
                if !prev_was_space {
                    acc.push(' ');
                }
                (acc, true)
            } else {
                acc.push(c);
                (acc, false)
            }
        })
        .0;

    collapsed
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.len() > MAX_LINE_LENGTH {
                let mut truncated = String::with_capacity(MAX_LINE_LENGTH + 3);
                truncated.push_str(&trimmed[..MAX_LINE_LENGTH]);
                truncated.push_str("...");
                truncated
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Baseline / happy path ---

    #[test]
    fn test_collapse_spaces() {
        let result = normalize_text("hello    world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_collapse_tabs_and_whitespace() {
        let result = normalize_text("a\tb\nc   d");
        assert_eq!(result, "a b\nc d");
    }

    #[test]
    fn test_preserve_newlines() {
        let result = normalize_text("line1\n\n\nline2");
        assert_eq!(result, "line1\n\n\nline2");
    }

    #[test]
    fn test_trim_whitespace() {
        let result = normalize_text("  hello world  ");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_empty_string() {
        let result = normalize_text("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_only_whitespace() {
        let result = normalize_text("   \t   \n   ");
        assert_eq!(result, "");
    }

    // --- Unicode / NFKC ---

    #[test]
    fn test_nfkc_normalization() {
        let result = normalize_text("\u{FF30}\u{FF24}\u{FF26}"); // fullwidth PDF
        assert_eq!(result, "PDF");
    }

    #[test]
    fn test_nfkc_combining_characters() {
        let result = normalize_text("\u{0061}\u{0301}"); // a + combining acute
        assert_eq!(result, "\u{00E1}"); // á (NFKC composed)
    }

    #[test]
    fn test_halfwidth_to_fullwidth() {
        let result = normalize_text("\u{FF21}\u{FF24}\u{FF26}"); // fullwidth A D F
        assert_eq!(result, "ADF");
    }

    #[test]
    fn test_non_bmp_emoji_preserved() {
        let input = "hello \u{1F600} world"; // grinning face
        let result = normalize_text(input);
        assert_eq!(result, input);
    }

    // --- Unicode whitespace variants ---

    #[test]
    fn test_no_break_space_collapsed() {
        let result = normalize_text("a\u{00A0}b"); // no-break space
        assert_eq!(result, "a b");
    }

    #[test]
    fn test_thin_space_collapsed() {
        let result = normalize_text("a\u{2009}b"); // thin space
        assert_eq!(result, "a b");
    }

    #[test]
    fn test_mixed_unicode_whitespace() {
        let result = normalize_text("a\u{00A0}\u{2009}\u{2003}b"); // nbsp + thin + em
        assert_eq!(result, "a b");
    }

    // --- Boundary / edge ---

    #[test]
    fn test_exactly_max_line_length() {
        let input = "a".repeat(MAX_LINE_LENGTH);
        let result = normalize_text(&input);
        assert_eq!(result.len(), MAX_LINE_LENGTH);
        assert!(!result.ends_with("..."));
    }

    #[test]
    fn test_one_byte_over_max_line_length() {
        let input = "a".repeat(MAX_LINE_LENGTH + 1);
        let result = normalize_text(&input);
        assert_eq!(result.len(), MAX_LINE_LENGTH + 3);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_long_line_truncation() {
        let long = "a".repeat(15_000);
        let result = normalize_text(&long);
        assert_eq!(result.len(), MAX_LINE_LENGTH + 3);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_mixed_short_and_long_lines() {
        let input = format!("short\n{}\nend", "x".repeat(12_000));
        let result = normalize_text(&input);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "short");
        assert!(lines[1].ends_with("..."));
        assert_eq!(lines[2], "end");
    }

    // --- Control characters ---

    #[test]
    fn test_control_characters_replaced_or_removed() {
        let result = normalize_text("a\u{0000}b\u{0001}c"); // null + SOH
        assert_eq!(result, "a b c");
    }

    #[test]
    fn test_form_feed_as_whitespace() {
        let result = normalize_text("a\u{000C}b"); // form feed
        assert_eq!(result, "a b");
    }

    // --- Other edge cases ---

    #[test]
    fn test_only_newlines() {
        let result = normalize_text("\n\n\n\n");
        assert_eq!(result, "");
    }

    #[test]
    fn test_newline_surrounded_by_spaces() {
        let result = normalize_text("  \n  ");
        assert_eq!(result, "");
    }

    #[test]
    fn test_leading_trailing_newlines_trimmed() {
        let result = normalize_text("\n\nhello\n\n");
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_mixed_whitespace_types_preserving_newlines() {
        let input = "\t  line1  \n  line2  \t";
        let result = normalize_text(input);
        assert_eq!(result, "line1\nline2");
    }

    #[test]
    fn test_single_character() {
        assert_eq!(normalize_text("x"), "x");
    }

    #[test]
    fn test_single_space() {
        assert_eq!(normalize_text(" "), "");
    }
}
