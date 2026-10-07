use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A wrapped composer draft and the visual location of each grapheme boundary.
/// Columns in `positions` are relative to the message text, not the prompt prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrappedDraft {
    pub lines: Vec<String>,
    pub positions: Vec<(usize, usize)>,
}

pub(crate) fn wrap_draft(prefix: &str, value: &str, width: usize) -> WrappedDraft {
    let width = width.max(1);
    let mut visible_prefix = String::new();
    let mut prefix_width = 0usize;
    for grapheme in prefix.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if prefix_width + grapheme_width > width {
            break;
        }
        visible_prefix.push_str(grapheme);
        prefix_width += grapheme_width;
    }

    let indent = prefix_width.min(width.saturating_sub(1));
    let mut capacity = width.saturating_sub(prefix_width);
    let mut lines = vec![visible_prefix];
    let mut positions = Vec::new();
    let mut row = 0usize;
    let mut used = 0usize;

    for (_, segment) in value.split_word_bound_indices() {
        let segment_width = UnicodeWidthStr::width(segment);
        let whitespace = segment.chars().all(char::is_whitespace);
        if !segment.contains('\n')
            && used > 0
            && segment_width <= capacity
            && used + segment_width > capacity
        {
            if whitespace {
                for grapheme in segment.graphemes(true) {
                    positions.push((row, used));
                    lines[row].push_str(grapheme);
                    used += UnicodeWidthStr::width(grapheme);
                }
                continue;
            }
            row += 1;
            lines.push(" ".repeat(indent));
            used = 0;
            capacity = width.saturating_sub(indent).max(1);
        }
        for grapheme in segment.graphemes(true) {
            if grapheme == "\n" || grapheme == "\r\n" {
                positions.push((row, used));
                row += 1;
                lines.push(" ".repeat(indent));
                used = 0;
                capacity = width.saturating_sub(indent).max(1);
                continue;
            }
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if used + grapheme_width > capacity && (used > 0 || row == 0) {
                row += 1;
                lines.push(" ".repeat(indent));
                used = 0;
                capacity = width.saturating_sub(indent).max(1);
            }
            positions.push((row, used));
            lines[row].push_str(grapheme);
            used += grapheme_width;
        }
    }

    if !value.is_empty() && used >= capacity {
        row += 1;
        lines.push(" ".repeat(indent));
        used = 0;
    }
    positions.push((row, used));

    WrappedDraft { lines, positions }
}

#[cfg(test)]
mod tests {
    use super::wrap_draft;

    #[test]
    fn wraps_at_grapheme_boundaries_and_tracks_cursor_positions() {
        let wrapped = wrap_draft("›  ", "abcdefg", 6);
        assert_eq!(wrapped.lines, ["›  abc", "   def", "   g"]);
        assert_eq!(wrapped.positions.len(), 8);
        assert_eq!(wrapped.positions[3], (1, 0));
        assert_eq!(wrapped.positions[6], (2, 0));
        assert_eq!(wrapped.positions[7], (2, 1));
    }

    #[test]
    fn wraps_between_words_when_the_next_word_fits_on_a_fresh_row() {
        let wrapped = wrap_draft("› ", "one two", 7);
        assert_eq!(wrapped.lines, ["› one ", "  two"]);
    }

    #[test]
    fn explicit_newlines_preserve_empty_rows_and_cursor_boundaries() {
        let wrapped = wrap_draft("› ", "a\n\nb\n", 20);
        assert_eq!(wrapped.lines, ["› a", "  ", "  b", "  "]);
        assert_eq!(
            wrapped.positions,
            [(0, 0), (0, 1), (1, 0), (2, 0), (2, 1), (3, 0)]
        );
        assert!(wrapped.lines.iter().all(|line| !line.contains('\n')));
    }

    #[test]
    fn wide_graphemes_consume_two_columns_without_splitting() {
        let wrapped = wrap_draft("› ", "ab界c", 6);
        assert_eq!(wrapped.lines, ["› ab界", "  c"]);
        assert_eq!(wrapped.positions[2], (0, 2));
        assert_eq!(wrapped.positions[3], (1, 0));
        assert_eq!(wrapped.positions[4], (1, 1));
    }
}
