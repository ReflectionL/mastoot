//! A tiny multi-line text-input widget.
//!
//! Deliberately minimal — we own it rather than pull in
//! `tui-textarea`. Covers what the Compose screen actually needs:
//! printable-char insertion, backspace, Enter (split line), Arrow
//! movement, and correct cursor placement for IME-committed text.
//!
//! Word and line editing follow macOS text fields, with the readline
//! keys as fallbacks. Terminals encode the Mac keys differently, so
//! each action accepts every common spelling:
//!
//! | Action | Mac key | Arrives as |
//! |---|---|---|
//! | delete word back | `⌥⌫` | `Alt+Backspace` (ESC DEL), `Ctrl+W` (iTerm2 "Natural Text Editing"), `Ctrl+Backspace` |
//! | delete to line start | `⌘⌫` | `Super+Backspace` (kitty protocol), `Ctrl+U` (Ghostty, iTerm2, Terminal.app) |
//! | delete word forward | `⌥⌦` | `Alt+Delete`, `Alt+D`, `Ctrl+Delete` |
//! | delete to line end | `⌘⌦` | `Super+Delete`, `Ctrl+K` |
//! | word left / right | `⌥←` / `⌥→` | `Alt+←/→`, `Alt+B/F` (ESC b / ESC f), `Ctrl+←/→` |
//! | line start / end | `⌘←` / `⌘→` | `Super+←/→`, `Ctrl+A/E`, `Home` / `End` |
//! | text start / end | `⌘↑` / `⌘↓` | `Super+↑/↓` |
//!
//! A "word" is a run of letters/digits or a run of CJK characters, so
//! `⌥⌫` after `今天写Rust` takes `Rust` first, then `今天写`.
//! Whitespace and punctuation in front of the cursor go with the word.
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

    pub fn move_top(&mut self) {
        self.row = 0;
        self.col = 0;
    }

    pub fn move_bottom(&mut self) {
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    /// `⌥←`: to the start of the word before the cursor. At the start of
    /// a line, steps onto the end of the previous one.
    pub fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
        } else {
            self.col = word_start_before(&self.lines[self.row], self.col);
        }
    }

    /// `⌥→`: to the end of the word after the cursor. At the end of a
    /// line, steps onto the start of the next one.
    pub fn move_word_right(&mut self) {
        if self.col == self.lines[self.row].chars().count() {
            self.move_right();
        } else {
            self.col = word_end_after(&self.lines[self.row], self.col);
        }
    }

    /// `⌥⌫`: delete the word before the cursor. At the start of a line,
    /// joins it onto the previous one (like a plain backspace).
    pub fn delete_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            let start = word_start_before(&self.lines[self.row], self.col);
            self.remove_chars(start, self.col);
            self.col = start;
        }
    }

    /// `⌥⌦`: delete the word after the cursor. At the end of a line,
    /// pulls the next line up.
    pub fn delete_word_forward(&mut self) {
        if self.col == self.lines[self.row].chars().count() {
            self.delete();
        } else {
            let end = word_end_after(&self.lines[self.row], self.col);
            self.remove_chars(self.col, end);
        }
    }

    /// `⌘⌫`: delete from the start of the line to the cursor. At the
    /// start of a line, joins it onto the previous one.
    pub fn delete_to_line_start(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            self.remove_chars(0, self.col);
            self.col = 0;
        }
    }

    /// `⌘⌦` / `Ctrl+K`: delete from the cursor to the end of the line.
    /// At the end of a line, pulls the next line up.
    pub fn delete_to_line_end(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col == len {
            self.delete();
        } else {
            self.remove_chars(self.col, len);
        }
    }

    /// Remove chars `start..end` (char indices) of the cursor's line.
    fn remove_chars(&mut self, start: usize, end: usize) {
        let line = &mut self.lines[self.row];
        let range = char_to_byte(line, start)..char_to_byte(line, end);
        line.replace_range(range, "");
    }

    /// Feed a key event. Returns true if the key was consumed. See the
    /// module docs for which encodings map to which edit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let sup = key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META);
        match key.code {
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Char('u') if ctrl => self.delete_to_line_start(),
            KeyCode::Char('k') if ctrl => self.delete_to_line_end(),
            KeyCode::Char('a') if ctrl => self.move_home(),
            KeyCode::Char('e') if ctrl => self.move_end(),
            KeyCode::Char('h') if ctrl => self.backspace(),
            KeyCode::Char('b') if alt => self.move_word_left(),
            KeyCode::Char('f') if alt => self.move_word_right(),
            KeyCode::Char('d') if alt => self.delete_word_forward(),
            KeyCode::Char(c) if !ctrl && !alt && !sup => self.insert_char(c),
            KeyCode::Enter if !ctrl && !alt => self.enter(),
            KeyCode::Backspace if sup => self.delete_to_line_start(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_back(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete if sup => self.delete_to_line_end(),
            KeyCode::Delete if alt || ctrl => self.delete_word_forward(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if sup => self.move_home(),
            KeyCode::Left if alt || ctrl => self.move_word_left(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right if sup => self.move_end(),
            KeyCode::Right if alt || ctrl => self.move_word_right(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up if sup => self.move_top(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down if sup => self.move_bottom(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            _ => return false,
        }
        true
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

/// Edit a one-line prompt whose cursor always sits at the end (the
/// search and attach prompts). Same deletion keys as [`TextArea`]:
/// `⌥⌫` / `Ctrl+W` drop a word, `⌘⌫` / `Ctrl+U` clear the line.
/// Returns true if the key was consumed.
pub fn edit_prompt(text: &mut String, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let sup = key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::META);
    let end = text.chars().count();
    match key.code {
        KeyCode::Char('u') if ctrl => text.clear(),
        KeyCode::Backspace if sup => text.clear(),
        KeyCode::Char('w') if ctrl => {
            text.truncate(char_to_byte(text, word_start_before(text, end)));
        }
        KeyCode::Backspace if alt || ctrl => {
            text.truncate(char_to_byte(text, word_start_before(text, end)));
        }
        KeyCode::Char('h') if ctrl => {
            text.pop();
        }
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(c) if !ctrl && !alt && !sup => text.push(c),
        _ => return false,
    }
    true
}

/// What a char counts as when moving or deleting by word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    /// Whitespace and punctuation: skipped over on the way to a word.
    Gap,
    /// Letters, digits, `_`.
    Word,
    /// Han, kana, hangul: a run of them is one word, kept apart from
    /// Latin so `今天写Rust` splits in two.
    Cjk,
}

fn char_class(c: char) -> CharClass {
    if matches!(c as u32,
        0x3040..=0x30FF       // hiragana, katakana
        | 0x3400..=0x4DBF     // CJK extension A
        | 0x4E00..=0x9FFF     // CJK unified ideographs
        | 0xAC00..=0xD7AF     // hangul syllables
        | 0xF900..=0xFAFF     // CJK compatibility ideographs
        | 0x20000..=0x3FFFF)
    // CJK extensions B+
    {
        CharClass::Cjk
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Gap
    }
}

/// Char index where the word ending at or before `col` starts: skip
/// gaps backwards, then the run of one class.
fn word_start_before(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().take(col).collect();
    let mut i = chars.len();
    while i > 0 && char_class(chars[i - 1]) == CharClass::Gap {
        i -= 1;
    }
    if let Some(&c) = i.checked_sub(1).and_then(|j| chars.get(j)) {
        let class = char_class(c);
        while i > 0 && char_class(chars[i - 1]) == class {
            i -= 1;
        }
    }
    i
}

/// Char index where the word starting at or after `col` ends: skip
/// gaps forwards, then the run of one class.
fn word_end_after(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = col.min(chars.len());
    while i < chars.len() && char_class(chars[i]) == CharClass::Gap {
        i += 1;
    }
    if let Some(&c) = chars.get(i) {
        let class = char_class(c);
        while i < chars.len() && char_class(chars[i]) == class {
            i += 1;
        }
    }
    i
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

    fn mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn option_backspace_deletes_a_word_in_every_encoding() {
        for ev in [
            mods(KeyCode::Backspace, KeyModifiers::ALT), // ESC DEL / kitty
            mods(KeyCode::Char('w'), KeyModifiers::CONTROL), // iTerm2 natural editing
            mods(KeyCode::Backspace, KeyModifiers::CONTROL),
        ] {
            let mut ta = TextArea::from_text("hello big world");
            assert!(ta.handle_key(ev));
            assert_eq!(ta.text(), "hello big ", "{ev:?}");
            ta.handle_key(ev);
            assert_eq!(ta.text(), "hello ", "{ev:?}");
        }
    }

    #[test]
    fn cmd_backspace_deletes_to_line_start_in_every_encoding() {
        for ev in [
            mods(KeyCode::Backspace, KeyModifiers::SUPER), // kitty protocol
            mods(KeyCode::Char('u'), KeyModifiers::CONTROL), // Ghostty / iTerm2 / Terminal.app
        ] {
            let mut ta = TextArea::from_text(
                "first line
second line",
            );
            ta.col = 6; // after "second"
            assert!(ta.handle_key(ev));
            assert_eq!(
                ta.text(),
                "first line
 line",
                "{ev:?}"
            );
            assert_eq!((ta.row, ta.col), (1, 0));
            // At column 0 it eats the newline, like macOS.
            ta.handle_key(ev);
            assert_eq!(ta.text(), "first line line", "{ev:?}");
        }
    }

    #[test]
    fn word_delete_skips_punctuation_and_splits_cjk_from_latin() {
        let mut ta = TextArea::from_text("今天写Rust, ");
        ta.delete_word_back();
        assert_eq!(ta.text(), "今天写");
        ta.delete_word_back();
        assert_eq!(ta.text(), "");
        // CJK punctuation is a gap, not part of the word.
        let mut ta = TextArea::from_text("你好，世界。");
        ta.delete_word_back();
        assert_eq!(ta.text(), "你好，");
    }

    #[test]
    fn word_delete_at_line_start_joins_lines() {
        let mut ta = TextArea::from_text("ab\ncd");
        ta.col = 0;
        ta.delete_word_back();
        assert_eq!(ta.text(), "abcd");
        assert_eq!((ta.row, ta.col), (0, 2));
    }

    #[test]
    fn word_moves_in_every_encoding() {
        let left = [
            mods(KeyCode::Left, KeyModifiers::ALT),
            mods(KeyCode::Char('b'), KeyModifiers::ALT), // ESC b
            mods(KeyCode::Left, KeyModifiers::CONTROL),
        ];
        let right = [
            mods(KeyCode::Right, KeyModifiers::ALT),
            mods(KeyCode::Char('f'), KeyModifiers::ALT), // ESC f
            mods(KeyCode::Right, KeyModifiers::CONTROL),
        ];
        for (l, r) in left.into_iter().zip(right) {
            let mut ta = TextArea::from_text("one two.three");
            ta.handle_key(l);
            assert_eq!(ta.col, 8, "{l:?}");
            ta.handle_key(l);
            assert_eq!(ta.col, 4, "{l:?}");
            ta.handle_key(r);
            assert_eq!(ta.col, 7, "{r:?}");
            ta.handle_key(r);
            assert_eq!(ta.col, 13, "{r:?}");
            assert_eq!(ta.text(), "one two.three", "moves must not edit");
        }
    }

    #[test]
    fn line_and_text_jumps() {
        let mut ta = TextArea::from_text("ab\ncd");
        ta.handle_key(mods(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!((ta.row, ta.col), (1, 0));
        ta.handle_key(mods(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!((ta.row, ta.col), (1, 2));
        ta.handle_key(mods(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(ta.col, 0);
        ta.handle_key(mods(KeyCode::Up, KeyModifiers::SUPER));
        assert_eq!((ta.row, ta.col), (0, 0));
        ta.handle_key(mods(KeyCode::Down, KeyModifiers::SUPER));
        assert_eq!((ta.row, ta.col), (1, 2));
    }

    #[test]
    fn forward_deletes() {
        let mut ta = TextArea::from_text("one two three");
        ta.col = 3;
        ta.handle_key(mods(KeyCode::Delete, KeyModifiers::ALT));
        assert_eq!(ta.text(), "one three");
        ta.handle_key(mods(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(ta.text(), "one");
        let mut ta = TextArea::from_text("abc\ndef");
        ta.row = 0;
        ta.col = 1;
        ta.handle_key(mods(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(ta.text(), "a\ndef");
        ta.handle_key(mods(KeyCode::Delete, KeyModifiers::SUPER));
        assert_eq!(ta.text(), "adef");
    }

    #[test]
    fn prompt_editing_matches_the_textarea() {
        let mut s = String::from("~/Pictures/cat photo");
        assert!(edit_prompt(
            &mut s,
            mods(KeyCode::Backspace, KeyModifiers::ALT)
        ));
        assert_eq!(s, "~/Pictures/cat ");
        edit_prompt(&mut s, mods(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(s, "~/Pictures/");
        edit_prompt(&mut s, mods(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s, "~/Pictures");
        edit_prompt(&mut s, mods(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(s, "");
        edit_prompt(&mut s, mods(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(s, "x");
        assert!(!edit_prompt(
            &mut s,
            mods(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
    }
}
