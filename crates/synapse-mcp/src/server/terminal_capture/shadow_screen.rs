//! VT shadow screen (#902, #914).
//!
//! A server-side projection of the current terminal contents, fed the same raw
//! VT byte stream the asciicast recorder sees. Its job is the **dump** half of
//! the dump-then-delta attach (#914): when a viewer attaches, it gets a
//! snapshot of what is on screen right now, then live output deltas.
//!
//! This is a deliberately compact VT interpreter covering the sequences a CLI
//! agent's TUI actually emits: printable text (UTF-8), the C0 controls
//! (CR/LF/BS/HT), and the common CSI sequences — cursor movement (CUU/CUD/CUF/
//! CUB/CUP/HVP), erase-in-display (ED) and erase-in-line (EL). SGR (color)
//! sequences are parsed and skipped: they do not change the text snapshot. The
//! supported subset is asserted by the unit tests; anything unrecognized is
//! consumed without corrupting the grid rather than rendered as garbage.

/// A fixed-size character grid with a cursor, advanced by raw terminal bytes.
#[derive(Clone, Debug)]
pub(crate) struct ShadowScreen {
    cols: usize,
    rows: usize,
    cells: Vec<char>,
    cursor_row: usize,
    cursor_col: usize,
    parser: VtParser,
}

impl ShadowScreen {
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let cols = (cols as usize).max(1);
        let rows = (rows as usize).max(1);
        Self {
            cols,
            rows,
            cells: vec![' '; cols * rows],
            cursor_row: 0,
            cursor_col: 0,
            parser: VtParser::default(),
        }
    }

    pub(crate) fn cols(&self) -> u16 {
        self.cols as u16
    }

    pub(crate) fn rows(&self) -> u16 {
        self.rows as u16
    }

    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let new_cols = (cols as usize).max(1);
        let new_rows = (rows as usize).max(1);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }
        let mut new_cells = vec![' '; new_cols * new_rows];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for row in 0..copy_rows {
            let old_start = row * self.cols;
            let new_start = row * new_cols;
            new_cells[new_start..new_start + copy_cols]
                .copy_from_slice(&self.cells[old_start..old_start + copy_cols]);
        }
        self.cols = new_cols;
        self.rows = new_rows;
        self.cells = new_cells;
        self.cursor_row = self.cursor_row.min(self.rows - 1);
        self.cursor_col = self.cursor_col.min(self.cols - 1);
    }

    /// Feeds raw terminal bytes, updating the grid and cursor.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        // Drive the byte-level VT state machine, which emits decoded actions.
        let actions = self.parser.feed(bytes);
        for action in actions {
            self.apply(action);
        }
    }

    /// Renders the current screen as text: one line per row, trailing blanks on
    /// each row trimmed, trailing empty rows dropped.
    pub(crate) fn render_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows)
            .map(|row| {
                let start = row * self.cols;
                let line: String = self.cells[start..start + self.cols].iter().collect();
                line.trim_end().to_owned()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn apply(&mut self, action: VtAction) {
        match action {
            VtAction::Print(ch) => self.print(ch),
            VtAction::CarriageReturn => self.cursor_col = 0,
            VtAction::LineFeed => self.line_feed(),
            VtAction::Backspace => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            VtAction::Tab => {
                // Advance to the next 8-column tab stop, bounded to the row.
                let next = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next.min(self.cols - 1);
            }
            VtAction::CursorUp(n) => {
                self.cursor_row = self.cursor_row.saturating_sub(n.max(1));
            }
            VtAction::CursorDown(n) => {
                self.cursor_row = (self.cursor_row + n.max(1)).min(self.rows - 1);
            }
            VtAction::CursorForward(n) => {
                self.cursor_col = (self.cursor_col + n.max(1)).min(self.cols - 1);
            }
            VtAction::CursorBack(n) => {
                self.cursor_col = self.cursor_col.saturating_sub(n.max(1));
            }
            VtAction::CursorPosition(row, col) => {
                // 1-based in the protocol; clamp into the grid.
                self.cursor_row = row.saturating_sub(1).min(self.rows - 1);
                self.cursor_col = col.saturating_sub(1).min(self.cols - 1);
            }
            VtAction::EraseInLine(mode) => self.erase_in_line(mode),
            VtAction::EraseInDisplay(mode) => self.erase_in_display(mode),
        }
    }

    fn print(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            // Auto-wrap to the next line.
            self.cursor_col = 0;
            self.line_feed();
        }
        let index = self.cursor_row * self.cols + self.cursor_col;
        if let Some(cell) = self.cells.get_mut(index) {
            *cell = ch;
        }
        self.cursor_col += 1;
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self) {
        // Drop the top row, append a blank bottom row.
        self.cells.drain(0..self.cols);
        self.cells.extend(std::iter::repeat_n(' ', self.cols));
    }

    fn erase_in_line(&mut self, mode: usize) {
        let row_start = self.cursor_row * self.cols;
        let (from, to) = match mode {
            1 => (row_start, row_start + self.cursor_col + 1), // cursor to start
            2 => (row_start, row_start + self.cols),           // whole line
            _ => (row_start + self.cursor_col, row_start + self.cols), // cursor to end
        };
        let end = to.min(self.cells.len());
        for cell in &mut self.cells[from..end] {
            *cell = ' ';
        }
    }

    fn erase_in_display(&mut self, mode: usize) {
        let cursor_index = self.cursor_row * self.cols + self.cursor_col;
        let (from, to) = match mode {
            1 => (0, cursor_index + 1),            // start to cursor
            2 | 3 => (0, self.cells.len()),        // entire display
            _ => (cursor_index, self.cells.len()), // cursor to end
        };
        let end = to.min(self.cells.len());
        for cell in &mut self.cells[from..end] {
            *cell = ' ';
        }
    }
}

/// Decoded terminal actions emitted by [`VtParser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VtAction {
    Print(char),
    CarriageReturn,
    LineFeed,
    Backspace,
    Tab,
    CursorUp(usize),
    CursorDown(usize),
    CursorForward(usize),
    CursorBack(usize),
    CursorPosition(usize, usize),
    EraseInLine(usize),
    EraseInDisplay(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum VtState {
    #[default]
    Ground,
    /// Saw ESC.
    Escape,
    /// Inside a CSI (`ESC [`) sequence, collecting parameters.
    Csi,
    /// Inside an OSC (`ESC ]`) sequence, skipped for text-grid rendering.
    Osc,
    /// Saw ESC inside OSC; `ESC \` terminates the OSC.
    OscEscape,
}

/// Byte-level VT state machine. Decodes UTF-8 printables and the supported C0 /
/// CSI control sequences, holding partial UTF-8 and partial CSI sequences
/// across `feed` calls so chunk boundaries never corrupt decoding.
#[derive(Clone, Debug, Default)]
struct VtParser {
    state: VtState,
    /// Numeric CSI parameters collected so far (text form, split on ';').
    csi_params: String,
    /// Partial trailing UTF-8 bytes carried to the next feed.
    pending_utf8: Vec<u8>,
}

impl VtParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<VtAction> {
        let mut actions = Vec::new();
        let mut buffer = std::mem::take(&mut self.pending_utf8);
        buffer.extend_from_slice(bytes);

        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            match self.state {
                VtState::Ground => {
                    if byte == 0x1B {
                        self.state = VtState::Escape;
                        i += 1;
                    } else if byte < 0x20 || byte == 0x7F {
                        if let Some(action) = control_action(byte) {
                            actions.push(action);
                        }
                        i += 1;
                    } else {
                        // Printable: decode one UTF-8 scalar, buffering an
                        // incomplete trailing sequence for the next feed.
                        match next_utf8_char(&buffer[i..]) {
                            Utf8Decode::Char(ch, len) => {
                                actions.push(VtAction::Print(ch));
                                i += len;
                            }
                            Utf8Decode::Incomplete => {
                                self.pending_utf8 = buffer[i..].to_vec();
                                return actions;
                            }
                            Utf8Decode::Invalid => {
                                actions.push(VtAction::Print('\u{FFFD}'));
                                i += 1;
                            }
                        }
                    }
                }
                VtState::Escape => {
                    if byte == b'[' {
                        self.state = VtState::Csi;
                        self.csi_params.clear();
                    } else if byte == b']' {
                        self.state = VtState::Osc;
                    } else {
                        // Non-CSI escape (e.g. ESC c reset) — not modeled; drop.
                        self.state = VtState::Ground;
                    }
                    i += 1;
                }
                VtState::Csi => {
                    if (0x30..=0x3F).contains(&byte) {
                        // Parameter / intermediate bytes (digits, ';', '?', etc.).
                        self.csi_params.push(byte as char);
                        i += 1;
                    } else if (0x40..=0x7E).contains(&byte) {
                        // Final byte: dispatch.
                        if let Some(action) = csi_action(byte, &self.csi_params) {
                            actions.push(action);
                        }
                        self.state = VtState::Ground;
                        i += 1;
                    } else {
                        // Unexpected byte inside CSI — abort the sequence.
                        self.state = VtState::Ground;
                        i += 1;
                    }
                }
                VtState::Osc => {
                    if byte == 0x07 {
                        self.state = VtState::Ground;
                    } else if byte == 0x1B {
                        self.state = VtState::OscEscape;
                    }
                    i += 1;
                }
                VtState::OscEscape => {
                    self.state = if byte == b'\\' {
                        VtState::Ground
                    } else {
                        VtState::Osc
                    };
                    i += 1;
                }
            }
        }
        actions
    }
}

fn control_action(byte: u8) -> Option<VtAction> {
    match byte {
        b'\r' => Some(VtAction::CarriageReturn),
        b'\n' => Some(VtAction::LineFeed),
        0x08 => Some(VtAction::Backspace),
        b'\t' => Some(VtAction::Tab),
        _ => None,
    }
}

/// Parses the first `;`-separated CSI parameter as a number (default `def`).
fn csi_param(params: &str, index: usize, def: usize) -> usize {
    params
        .trim_start_matches('?')
        .split(';')
        .nth(index)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(def)
}

fn csi_action(final_byte: u8, params: &str) -> Option<VtAction> {
    match final_byte {
        b'A' => Some(VtAction::CursorUp(csi_param(params, 0, 1))),
        b'B' => Some(VtAction::CursorDown(csi_param(params, 0, 1))),
        b'C' => Some(VtAction::CursorForward(csi_param(params, 0, 1))),
        b'D' => Some(VtAction::CursorBack(csi_param(params, 0, 1))),
        b'H' | b'f' => Some(VtAction::CursorPosition(
            csi_param(params, 0, 1),
            csi_param(params, 1, 1),
        )),
        b'J' => Some(VtAction::EraseInDisplay(csi_param(params, 0, 0))),
        b'K' => Some(VtAction::EraseInLine(csi_param(params, 0, 0))),
        // SGR (m) and every other CSI: parsed and skipped (no text effect).
        _ => None,
    }
}

enum Utf8Decode {
    Char(char, usize),
    Incomplete,
    Invalid,
}

/// Decodes the first UTF-8 scalar from `bytes`, distinguishing an incomplete
/// trailing sequence (needs more bytes) from genuinely invalid bytes.
fn next_utf8_char(bytes: &[u8]) -> Utf8Decode {
    let len = utf8_len(bytes[0]);
    match len {
        0 => Utf8Decode::Invalid,
        n if n > bytes.len() => {
            // Could still be invalid even if short; only treat as incomplete
            // when the available bytes are a valid prefix.
            match std::str::from_utf8(bytes) {
                Ok(_) => Utf8Decode::Incomplete,
                Err(error) if error.valid_up_to() == 0 && error.error_len().is_none() => {
                    Utf8Decode::Incomplete
                }
                Err(_) => Utf8Decode::Invalid,
            }
        }
        n => match std::str::from_utf8(&bytes[..n]) {
            Ok(text) => text
                .chars()
                .next()
                .map_or(Utf8Decode::Invalid, |ch| Utf8Decode::Char(ch, n)),
            Err(_) => Utf8Decode::Invalid,
        },
    }
}

const fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_plain_text_into_grid() {
        let mut screen = ShadowScreen::new(20, 3);
        screen.feed(b"hello world");
        assert_eq!(screen.render_text(), "hello world");
    }

    #[test]
    fn newline_and_carriage_return_move_cursor() {
        let mut screen = ShadowScreen::new(20, 4);
        screen.feed(b"line1\r\nline2");
        assert_eq!(screen.render_text(), "line1\nline2");
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        let mut screen = ShadowScreen::new(20, 2);
        // Classic progress-bar pattern: write, CR, overwrite.
        screen.feed(b"50%\r100%");
        assert_eq!(screen.render_text(), "100%");
    }

    #[test]
    fn cursor_position_and_print_lands_at_coordinates() {
        let mut screen = ShadowScreen::new(10, 5);
        // CUP to row 3, col 4 (1-based), then print.
        screen.feed(b"\x1b[3;4HXY");
        let rendered = screen.render_text();
        let lines: Vec<&str> = rendered.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "   XY");
    }

    #[test]
    fn erase_in_line_to_end_clears_tail() {
        let mut screen = ShadowScreen::new(10, 1);
        screen.feed(b"abcdef");
        // Move cursor back to col 3 (1-based) and erase to end of line.
        screen.feed(b"\x1b[1;4H\x1b[0K");
        assert_eq!(screen.render_text(), "abc");
    }

    #[test]
    fn sgr_color_sequences_do_not_corrupt_text() {
        let mut screen = ShadowScreen::new(20, 1);
        // Bold red "OK" then reset — SGR must be skipped, leaving just "OK".
        screen.feed(b"\x1b[1;31mOK\x1b[0m");
        assert_eq!(screen.render_text(), "OK");
    }

    #[test]
    fn osc_title_sequences_do_not_render_as_text() {
        let mut screen = ShadowScreen::new(40, 2);
        screen.feed(b"\x1b]0;window title\x07ready");
        assert_eq!(screen.render_text(), "ready");
    }

    #[test]
    fn split_osc_st_sequence_is_consumed() {
        let mut screen = ShadowScreen::new(40, 2);
        screen.feed(b"before\x1b]2;window");
        screen.feed(b" title\x1b\\after");
        assert_eq!(screen.render_text(), "beforeafter");
    }

    #[test]
    fn resize_preserves_overlapping_cells() {
        let mut screen = ShadowScreen::new(6, 2);
        screen.feed(b"abcdef\r\nXYZ");
        screen.resize(4, 3);
        assert_eq!(screen.render_text(), "abcd\nXYZ");
    }

    #[test]
    fn scroll_up_when_output_exceeds_rows() {
        let mut screen = ShadowScreen::new(5, 2);
        screen.feed(b"aaa\r\nbbb\r\nccc");
        // Row 1 scrolled off; only the last two rows remain.
        assert_eq!(screen.render_text(), "bbb\nccc");
    }

    #[test]
    fn split_csi_sequence_across_feeds_is_handled() {
        let mut screen = ShadowScreen::new(10, 5);
        // Split the CUP sequence across two feeds at an arbitrary point.
        screen.feed(b"\x1b[3;");
        screen.feed(b"4HZ");
        let rendered = screen.render_text();
        let lines: Vec<&str> = rendered.split('\n').collect();
        assert_eq!(lines[2], "   Z");
    }

    #[test]
    fn split_utf8_across_feeds_renders_one_char() {
        let mut screen = ShadowScreen::new(10, 1);
        // '✓' = E2 9C 93, split across feeds.
        screen.feed(&[b'a', 0xE2, 0x9C]);
        screen.feed(&[0x93, b'b']);
        assert_eq!(screen.render_text(), "a✓b");
    }
}
