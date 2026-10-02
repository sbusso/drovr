//! drovr fork: the inbox's multi-line note editor (docs/design/inbox-pane.md,
//! section 5). Replies and notes open it inside the item; it grows to
//! [`MAX_ROWS`] lines and scrolls past that.
//!
//! Keys: text inserts, `enter` adds a line, `ctrl+s` or `alt+enter` sends,
//! `ctrl+e` opens `$EDITOR`, `esc` cancels (the inbox keeps the draft).
//! Arrows, Home/End, Backspace and Delete edit; `ctrl+u` clears the line
//! before the cursor.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use super::render::display_width;

/// Lines shown before the editor scrolls.
pub(super) const MAX_ROWS: usize = 10;

/// What a key did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditorKey {
    Edited,
    Send,
    Cancel,
    External,
    Ignored,
}

/// Lines of text and a cursor (line, char index).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NoteEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Default for NoteEditor {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }
}

fn char_len(line: &str) -> usize {
    line.chars().count()
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

impl NoteEditor {
    /// An editor holding `text`, the cursor at its end.
    pub(super) fn new(text: &str) -> Self {
        let mut editor = Self::default();
        editor.insert(text);
        editor
    }

    pub(super) fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Replaces the text (a reload from `$EDITOR`), the cursor at its end.
    pub(super) fn set_text(&mut self, text: &str) {
        *self = Self::new(text);
    }

    /// Inserts text at the cursor; newlines split the line, other control
    /// characters (tabs aside) are dropped.
    pub(super) fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.newline();
            }
            let part: String = part
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .collect();
            let line = &mut self.lines[self.row];
            let at = byte_at(line, self.col);
            line.insert_str(at, &part);
            self.col += char_len(&part);
        }
    }

    fn newline(&mut self) {
        let line = &mut self.lines[self.row];
        let rest = line.split_off(byte_at(line, self.col));
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let at = byte_at(line, self.col - 1);
            line.remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = char_len(&self.lines[self.row]);
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < char_len(&self.lines[self.row]) {
            let line = &mut self.lines[self.row];
            let at = byte_at(line, self.col);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_row(&mut self, down: bool) {
        let row = if down {
            (self.row + 1).min(self.lines.len() - 1)
        } else {
            self.row.saturating_sub(1)
        };
        self.row = row;
        self.col = self.col.min(char_len(&self.lines[row]));
    }

    pub(super) fn handle_key(&mut self, key: &crate::input::TerminalKey) -> EditorKey {
        if key.kind == KeyEventKind::Release {
            return EditorKey::Ignored;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let ctrl = modifiers == KeyModifiers::CONTROL;
        let alt = modifiers == KeyModifiers::ALT;
        let plain = modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match code {
            KeyCode::Char('s') if ctrl => return EditorKey::Send,
            KeyCode::Enter if alt => return EditorKey::Send,
            KeyCode::Char('e') if ctrl => return EditorKey::External,
            KeyCode::Esc => return EditorKey::Cancel,
            KeyCode::Enter if plain => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char('h') if ctrl => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if self.col > 0 => self.col -= 1,
            KeyCode::Left if self.row > 0 => {
                self.row -= 1;
                self.col = char_len(&self.lines[self.row]);
            }
            KeyCode::Right if self.col < char_len(&self.lines[self.row]) => self.col += 1,
            KeyCode::Right if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = 0;
            }
            KeyCode::Left | KeyCode::Right => {}
            KeyCode::Up => self.move_row(false),
            KeyCode::Down => self.move_row(true),
            KeyCode::Home => self.col = 0,
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::End => self.col = char_len(&self.lines[self.row]),
            KeyCode::Char('u') if ctrl => {
                let line = &mut self.lines[self.row];
                line.replace_range(..byte_at(line, self.col), "");
                self.col = 0;
            }
            _ => {
                let text = key
                    .generated_text
                    .clone()
                    .filter(|text| !text.is_empty())
                    .or_else(|| {
                        plain
                            .then(|| crate::input::keybind_help_text_char(key))
                            .flatten()
                            .map(String::from)
                    });
                match text {
                    Some(text) => self.insert(&text),
                    None => return EditorKey::Ignored,
                }
            }
        }
        EditorKey::Edited
    }

    /// Rows the editor takes: one per line, at most [`MAX_ROWS`].
    pub(super) fn rows(&self) -> usize {
        self.lines.len().min(MAX_ROWS)
    }

    /// Draws visible row `row` (0 to [`Self::rows`]) at `(x, y)`, `width`
    /// columns wide, the cursor cell reversed. The rows follow the cursor
    /// line; a long line shows the part around the cursor.
    pub(super) fn render_row(
        &self,
        buffer: &mut Buffer,
        (x, y): (u16, u16),
        width: u16,
        row: usize,
        style: Style,
    ) {
        let width = usize::from(width);
        let first = (self.row + 1).saturating_sub(MAX_ROWS);
        let index = first + row;
        let Some(line) = self.lines.get(index).filter(|_| width > 0) else {
            return;
        };
        let chars: Vec<char> = line.chars().collect();
        let cursor = (index == self.row).then_some(self.col);
        let start = cursor.map_or(0, |col| (col + 1).saturating_sub(width));
        let mut column = 0;
        for (at, ch) in chars.iter().enumerate().skip(start) {
            let symbol = ch.to_string();
            let cell_width = usize::from(display_width(&symbol)).max(1);
            if column + cell_width > width {
                break;
            }
            let cell_style = if cursor == Some(at) {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };
            if let Some(cell) = buffer.cell_mut((x + column as u16, y)) {
                cell.set_symbol(&symbol).set_style(cell_style);
            }
            column += cell_width;
        }
        if cursor.is_some_and(|col| col >= chars.len()) && column < width {
            if let Some(cell) = buffer.cell_mut((x + column as u16, y)) {
                cell.set_symbol(" ")
                    .set_style(style.add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TerminalKey;

    fn press(editor: &mut NoteEditor, code: KeyCode, modifiers: KeyModifiers) -> EditorKey {
        editor.handle_key(&TerminalKey::new(code, modifiers))
    }

    fn typed(editor: &mut NoteEditor, text: &str) {
        for ch in text.chars() {
            press(editor, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    #[test]
    fn enter_adds_lines_and_ctrl_s_or_alt_enter_sends() {
        let mut editor = NoteEditor::default();
        typed(&mut editor, "keep the plan");
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE),
            EditorKey::Edited
        );
        typed(&mut editor, "but add tests");
        assert_eq!(editor.text(), "keep the plan\nbut add tests");
        assert_eq!(editor.rows(), 2);
        assert_eq!(
            press(&mut editor, KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorKey::Send
        );
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::ALT),
            EditorKey::Send
        );
        assert_eq!(
            press(&mut editor, KeyCode::Char('e'), KeyModifiers::CONTROL),
            EditorKey::External
        );
        assert_eq!(
            press(&mut editor, KeyCode::Esc, KeyModifiers::NONE),
            EditorKey::Cancel
        );
        assert_eq!(editor.text(), "keep the plan\nbut add tests");
    }

    #[test]
    fn editing_across_lines() {
        let mut editor = NoteEditor::new("ab\ncd");
        // Backspace at a line start joins it to the line above.
        press(&mut editor, KeyCode::Home, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), "abcd");
        // Enter in the middle splits the line.
        press(&mut editor, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(editor.text(), "ab\ncd");
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        press(&mut editor, KeyCode::End, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.text(), "abcd");
        // Multi-byte text and pasted newlines.
        editor.set_text("é");
        editor.insert("\r\nü\tx");
        assert_eq!(editor.text(), "é\nü x");
        press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), "é\nüx");
        // Twelve lines: the editor shows ten.
        editor.set_text(&"line\n".repeat(11));
        assert_eq!(editor.rows(), MAX_ROWS);
    }

    #[test]
    fn render_scrolls_to_the_cursor() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 2));
        let editor = NoteEditor::new("abcdefghij");
        editor.render_row(&mut buffer, (0, 0), 6, 0, Style::default());
        let row: String = (0..6).map(|x| buffer[(x, 0)].symbol().to_owned()).collect();
        assert_eq!(row, "fghij ");
        assert!(buffer[(5, 0)].modifier.contains(Modifier::REVERSED));
    }
}
