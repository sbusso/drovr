//! Text selection in the doc pane: positions are (rendered line, display
//! column), so a selection survives scrolling and maps straight to the text.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::render::{line_text, RLine};

/// Kitty image placeholder; lines holding one are images, not text.
const PLACEHOLDER: char = '\u{10EEEE}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    pub fn new(anchor: (usize, usize), head: (usize, usize)) -> Self {
        Self { anchor, head }
    }

    /// First and last selected cell, in reading order.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Selected display columns of `line`; the last cell is included.
    pub fn columns_on(&self, line: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if line < start.0 || line > end.0 {
            return None;
        }
        let from = if line == start.0 { start.1 } else { 0 };
        let to = if line == end.0 {
            end.1.saturating_add(1)
        } else {
            usize::MAX
        };
        Some(from..to)
    }
}

/// Each char with the display column it starts at.
fn cells(text: &str) -> Vec<(usize, char)> {
    let mut col = 0;
    text.chars()
        .map(|c| {
            let at = col;
            col += c.width().unwrap_or(0);
            (at, c)
        })
        .collect()
}

/// The text of `line` in display columns `range`.
fn slice(text: &str, range: &Range<usize>) -> String {
    cells(text)
        .into_iter()
        .filter(|(col, _)| range.contains(col))
        .map(|(_, c)| c)
        .collect()
}

/// The selected text: one line per rendered line, trailing padding removed,
/// image rows left empty.
pub fn selected_text(lines: &[RLine], selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    (start.0..=end.0.min(lines.len().saturating_sub(1)))
        .filter_map(|index| {
            let text = line_text(lines.get(index)?);
            if text.contains(PLACEHOLDER) {
                return Some(String::new());
            }
            let range = selection.columns_on(index)?;
            Some(slice(&text, &range).trim_end().to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Display columns of the word under `col`: letters, digits and `_`, or the
/// single character there when it is not part of a word.
pub fn word_at(line: &RLine, col: usize) -> Range<usize> {
    let cells = cells(&line_text(line));
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let Some(index) = cells.iter().rposition(|(at, _)| *at <= col) else {
        return col..col;
    };
    let width = |i: usize| cells[i].1.width().unwrap_or(0).max(1);
    if !is_word(cells[index].1) {
        return cells[index].0..cells[index].0 + width(index) - 1;
    }
    let mut first = index;
    while first > 0 && is_word(cells[first - 1].1) {
        first -= 1;
    }
    let mut last = index;
    while last + 1 < cells.len() && is_word(cells[last + 1].1) {
        last += 1;
    }
    cells[first].0..cells[last].0 + width(last) - 1
}

/// Last display column of `line` that holds a character.
pub fn last_column(line: &RLine) -> usize {
    let text = line_text(line);
    let text = text.trim_end();
    cells(text)
        .last()
        .map_or(0, |(col, c)| col + c.width().unwrap_or(0).max(1) - 1)
}

/// `spans` with `style` patched onto the display columns in `range`.
pub fn highlight(
    spans: Vec<Span<'static>>,
    range: Range<usize>,
    style: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    let mut col = 0;
    for span in spans {
        let mut pieces: Vec<(bool, String)> = Vec::new();
        for c in span.content.chars() {
            let inside = range.contains(&col);
            col += c.width().unwrap_or(0);
            match pieces.last_mut() {
                Some((was, text)) if *was == inside => text.push(c),
                _ => pieces.push((inside, c.to_string())),
            }
        }
        for (inside, text) in pieces {
            let piece_style = if inside {
                span.style.patch(style)
            } else {
                span.style
            };
            out.push(Span::styled(text, piece_style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::render::RSpan;
    use super::*;
    use ratatui::style::Modifier;

    fn line(text: &str) -> RLine {
        vec![RSpan {
            text: text.into(),
            style: Style::default(),
            link: None,
        }]
    }

    #[test]
    fn selection_spans_lines_in_reading_order() {
        let lines = vec![line("first line"), line("second  "), line("third one")];
        // Dragged backwards: from "one" up to "line".
        let selection = Selection::new((2, 4), (0, 6));
        assert_eq!(selected_text(&lines, &selection), "line\nsecond\nthird");
        assert_eq!(selection.columns_on(0), Some(6..usize::MAX));
        assert_eq!(selection.columns_on(1), Some(0..usize::MAX));
        assert_eq!(selection.columns_on(2), Some(0..5));
        assert_eq!(selection.columns_on(3), None);
        // One cell.
        let one = Selection::new((0, 0), (0, 0));
        assert_eq!(selected_text(&lines, &one), "f");
    }

    #[test]
    fn image_rows_copy_as_empty_lines() {
        let lines = vec![line("a"), line("\u{10EEEE}\u{305}\u{305}"), line("b")];
        let selection = Selection::new((0, 0), (2, 0));
        assert_eq!(selected_text(&lines, &selection), "a\n\nb");
    }

    #[test]
    fn word_at_finds_words_and_single_symbols() {
        let text = line("see docs_2/plan.md now");
        assert_eq!(word_at(&text, 5), 4..9, "docs_2");
        assert_eq!(word_at(&text, 10), 10..10, "the slash alone");
        assert_eq!(word_at(&text, 21), 19..21, "now");
        assert_eq!(word_at(&line("界面 ok"), 1), 0..3, "wide chars");
        assert_eq!(last_column(&line("ab  ")), 1);
    }

    #[test]
    fn highlight_splits_spans_at_column_edges() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let spans = vec![Span::styled("abc", bold), Span::raw("def")];
        let marked = Style::default().add_modifier(Modifier::REVERSED);
        let out = highlight(spans, 2..4, marked);
        let texts: Vec<_> = out.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, ["ab", "c", "d", "ef"]);
        assert!(out[1]
            .style
            .add_modifier
            .contains(Modifier::REVERSED | Modifier::BOLD));
        assert!(!out[3].style.add_modifier.contains(Modifier::REVERSED));
    }
}
