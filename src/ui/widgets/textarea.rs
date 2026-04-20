//! A tiny multi-line text-input widget.
//!
//! Deliberately minimal — we own < 200 lines rather than pull in
//! `tui-textarea`. Covers what the Compose screen actually needs:
//! printable-char insertion, backspace, Enter (split line), Arrow
//! movement, and correct cursor placement for IME-committed text.
//!
//! Grapheme awareness: cursor column tracks by **`char` index** within
//! a line (not byte index or grapheme cluster index). That is good
//! enough for Latin, CJK (full-width chars are single `char`s), and
//! Cyrillic; emoji ZWJ sequences and combining marks will behave
//! slightly oddly — punt to a future polish.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

/// A minimal multi-line text area.
#[derive(Debug, Clone)]
pub struct TextArea {
    lines: Vec<String>,
    /// Cursor position as (row, col) where col counts **characters**,
    /// not bytes or display columns.
    row: usize,
    col: usize,
    /// How many rows were scrolled off the top during last render. We
    /// recompute this each frame from cursor-visibility rules.
    scroll: u16,
}

impl Default for TextArea {
    fn default() -> Self {
        Self::new()
    }
}

impl TextArea {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            scroll: 0,
        }
    }

    /// Construct pre-filled. Cursor lands at the end.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut ta = Self::new();
        ta.insert_str(text);
        ta
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    #[must_use]
    pub fn char_count(&self) -> usize {
        self.lines.iter().map(|l| l.chars().count()).sum::<usize>()
            + self.lines.len().saturating_sub(1) // count newlines
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.row];
        let byte_idx = char_to_byte(line, self.col);
        line.insert(byte_idx, c);
        self.col += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for (i, chunk) in s.split('\n').enumerate() {
            if i > 0 {
                self.enter();
            }
            for c in chunk.chars() {
                self.insert_char(c);
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let byte_end = char_to_byte(line, self.col);
            let byte_start = char_to_byte(line, self.col - 1);
            line.replace_range(byte_start..byte_end, "");
            self.col -= 1;
        } else if self.row > 0 {
            // Join with previous line.
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            let prev = &mut self.lines[self.row];
            self.col = prev.chars().count();
            prev.push_str(&removed);
        }
    }

    pub fn delete(&mut self) {
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            let line = &mut self.lines[self.row];
            let byte_start = char_to_byte(line, self.col);
            let byte_end = char_to_byte(line, self.col + 1);
            line.replace_range(byte_start..byte_end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn enter(&mut self) {
        let line = &mut self.lines[self.row];
        let byte_idx = char_to_byte(line, self.col);
        let rest = line.split_off(byte_idx);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row == 0 {
            self.col = 0;
            return;
        }
        self.row -= 1;
        let line_len = self.lines[self.row].chars().count();
        self.col = self.col.min(line_len);
    }

    pub fn move_down(&mut self) {
        if self.row + 1 >= self.lines.len() {
            self.col = self.lines[self.row].chars().count();
            return;
        }
        self.row += 1;
        let line_len = self.lines[self.row].chars().count();
        self.col = self.col.min(line_len);
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// Feed a key event. Returns true if the key was consumed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_char(c);
                true
            }
            KeyCode::Enter if !ctrl && !alt => {
                self.enter();
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Left => {
                self.move_left();
                true
            }
            KeyCode::Right => {
                self.move_right();
                true
            }
            KeyCode::Up => {
                self.move_up();
                true
            }
            KeyCode::Down => {
                self.move_down();
                true
            }
            KeyCode::Home => {
                self.move_home();
                true
            }
            KeyCode::End => {
                self.move_end();
                true
            }
            _ => false,
        }
    }

    /// Render the lines. Call `frame.set_cursor_position` separately
    /// when this widget should own the cursor.
    ///
    /// Long lines are visually wrapped at `area.width` (character-level
    /// wrap — break wherever the next char would overflow). Cursor
    /// position is computed in **visual** row/col space so it stays
    /// pinned to the right spot on the wrapped line.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, style: Style, focused: bool) {
        let wrap_w = area.width;
        let mut visual_lines: Vec<Line<'static>> = Vec::new();
        let mut cursor_visual_row: u16 = 0;
        let mut cursor_visual_col: u16 = 0;

        for (i, logical) in self.lines.iter().enumerate() {
            if i == self.row {
                let (dr, dc) = cursor_in_wrapped(logical, self.col, wrap_w);
                cursor_visual_row = (visual_lines.len() as u16).saturating_add(dr);
                cursor_visual_col = dc;
            }
            let chunks = visual_wrap(logical, wrap_w);
            for chunk in chunks {
                visual_lines.push(Line::from(Span::styled(chunk, style)));
            }
        }

        // Adjust scroll so the cursor row (in visual-row units) is visible.
        let h = area.height.max(1);
        if cursor_visual_row < self.scroll {
            self.scroll = cursor_visual_row;
        } else if cursor_visual_row >= self.scroll + h {
            self.scroll = cursor_visual_row - h + 1;
        }

        let p = Paragraph::new(visual_lines)
            .style(style)
            .scroll((self.scroll, 0));
        frame.render_widget(p, area);

        if focused {
            let x = area.x + cursor_visual_col.min(area.width.saturating_sub(1));
            let y = area.y + cursor_visual_row.saturating_sub(self.scroll);
            frame.set_cursor_position(Position { x, y });
        }
    }
}

/// Break a single logical line into visual chunks, each at most
/// `width` display columns. Purely greedy / char-level — we don't
/// try to respect word boundaries, since compose text is live-typed
/// and users expect predictable edge-wrap (matching what a plain
/// terminal does).
fn visual_wrap(line: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let w = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for c in line.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > w && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(c);
        cur_w += cw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Where does char `cursor_col` land on line `logical` after a
/// visual-wrap at `width` columns? Returned as a (row_offset,
/// col) pair relative to the first visual row of that logical
/// line. Matches [`visual_wrap`]'s exact break points.
fn cursor_in_wrapped(logical: &str, cursor_col: usize, width: u16) -> (u16, u16) {
    if width == 0 {
        return (0, display_col(logical, cursor_col) as u16);
    }
    let w = width as usize;
    let mut vrow: u16 = 0;
    let mut vcol: usize = 0;
    for (char_idx, c) in logical.chars().enumerate() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        // Check overflow *before* recording the cursor position so
        // `cursor_col` sitting exactly on a wrap boundary lands at
        // col 0 of the next visual row, not past the end of the
        // current one.
        if vcol + cw > w && vcol > 0 {
            vrow = vrow.saturating_add(1);
            vcol = 0;
        }
        if char_idx == cursor_col {
            return (vrow, vcol as u16);
        }
        vcol += cw;
    }
    // Cursor is at (or past) the end of the line. Exact-fit lines
    // wrap the trailing cursor onto a new visual row so it doesn't
    // overlap the last rendered char.
    if vcol >= w && cursor_col > 0 {
        (vrow.saturating_add(1), 0)
    } else {
        (vrow, vcol as u16)
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or_else(|| s.len(), |(b, _)| b)
}

fn display_col(line: &str, char_idx: usize) -> usize {
    line.chars()
        .take(char_idx)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn insert_and_text() {
        let mut ta = TextArea::new();
        for c in "hello".chars() {
            ta.insert_char(c);
        }
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.col, 5);
    }

    #[test]
    fn enter_splits_line() {
        let mut ta = TextArea::from_text("abc");
        ta.col = 1;
        ta.enter();
        assert_eq!(ta.text(), "a\nbc");
        assert_eq!(ta.row, 1);
        assert_eq!(ta.col, 0);
    }

    #[test]
    fn backspace_joins_lines() {
        let mut ta = TextArea::from_text("a\nb");
        ta.row = 1;
        ta.col = 0;
        ta.backspace();
        assert_eq!(ta.text(), "ab");
        assert_eq!(ta.row, 0);
        assert_eq!(ta.col, 1);
    }

    #[test]
    fn cjk_char_count_is_by_character() {
        let mut ta = TextArea::from_text("你好世界");
        assert_eq!(ta.char_count(), 4);
        ta.move_left();
        assert_eq!(ta.col, 3);
    }

    #[test]
    fn handle_key_inserts_printable() {
        let mut ta = TextArea::new();
        assert!(ta.handle_key(key(KeyCode::Char('a'))));
        assert!(ta.handle_key(key(KeyCode::Enter)));
        assert!(ta.handle_key(key(KeyCode::Char('b'))));
        assert_eq!(ta.text(), "a\nb");
    }

    #[test]
    fn ctrl_keys_not_consumed_as_insert() {
        let mut ta = TextArea::new();
        let ev = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(!ta.handle_key(ev));
        assert_eq!(ta.text(), "");
    }

    #[test]
    fn char_count_includes_newlines() {
        let ta = TextArea::from_text("ab\ncd");
        // 2 + newline + 2 = 5
        assert_eq!(ta.char_count(), 5);
    }

    #[test]
    fn visual_wrap_breaks_at_width() {
        assert_eq!(visual_wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Exact width — no extra split.
        assert_eq!(visual_wrap("abcd", 4), vec!["abcd"]);
        // CJK chars are width 2 each → 3 per line at width 6.
        assert_eq!(visual_wrap("今天天气真好", 6), vec!["今天天", "气真好"]);
    }

    #[test]
    fn visual_wrap_handles_empty_and_zero() {
        assert_eq!(visual_wrap("", 10), vec![String::new()]);
        assert_eq!(visual_wrap("abc", 0), vec!["abc".to_string()]);
    }

    #[test]
    fn cursor_in_wrapped_tracks_break_points() {
        // "abcdefghij" at width 4 → "abcd" / "efgh" / "ij"
        // cursor at col 0  → (0, 0)
        assert_eq!(cursor_in_wrapped("abcdefghij", 0, 4), (0, 0));
        // cursor at col 4  → (1, 0) — start of second visual row
        assert_eq!(cursor_in_wrapped("abcdefghij", 4, 4), (1, 0));
        // cursor at col 9 (before 'j') → (2, 1)
        assert_eq!(cursor_in_wrapped("abcdefghij", 9, 4), (2, 1));
        // cursor at end of 4-char exact-fit → should park on new row
        assert_eq!(cursor_in_wrapped("abcd", 4, 4), (1, 0));
    }

    #[test]
    fn cursor_in_wrapped_cjk() {
        // 6 chars × width 2 = 12 total, wrap at 6 → 3 per row.
        // cursor at col 3 → start of second row
        assert_eq!(cursor_in_wrapped("今天天气真好", 3, 6), (1, 0));
    }
}
