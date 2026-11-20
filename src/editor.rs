/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The full-screen text editor, ported from `src/editor.c`.
//!
//! C keeps two views of the same text: the *data lines* it edits in place and the *display
//! lines* those bytes wrap into at [`SCREEN_COLS`]. A display line is a slice of a data line,
//! so every edit re-splits the line it touched and slides the display array behind it. Rust
//! owns its buffers, so [`EditorData`] holds the data lines as byte vectors and the wrap of
//! each one beside it; the display-line numbering [`editor_display`] works in is derived from
//! the two. Everything the screen half of C reads — `display_line_lengths`,
//! `display_line_widths`, the `split_line` that turns a column into an offset — has a method
//! here that returns the same number.
//!
//! A data line is the text up to and including its `'\n'`; only the last one may lack it, and
//! [`EditorData::load`] adds an empty line after a buffer that ends with one, which is what
//! makes the cursor land on a blank row after the last line of an existing post.
//!
//! # Deviations from C
//!
//! * The two memory pools (`editor_memory_pool_init`, `:37-75`) are gone. What they bought
//!   was a fixed 1024-byte stride per line, and that stride is the source of the two bugs
//!   below.
//! * A text line loaded longer than `MAX_EDITOR_DATA_LINE_LENGTH` stays one line. C packs it
//!   into several fixed buffers but still sums their lengths into `len_data_line`, so the
//!   first edit indexes past the end of the chunk it came from (`:349` `memmove`). The wrap
//!   and the saved bytes are unchanged; only the overflow is gone.
//! * Where C truncates the text once the display-line array is full — `:366-387` stops the
//!   re-split and terminates the last line, `:447-458` cuts the final display line short —
//!   the port refuses the edit instead. [`EditorData::load`] does keep C's cap and drops the
//!   display lines past [`MAX_EDITOR_DATA_LINES`], because a body of short lines reaches it
//!   long before the 4 MB content limit does.
//! * `:437-444`, which moves the cursor to the next display line when an insert ran past the
//!   end of the current one, is kept; it is what makes typing at the end of a line feel like
//!   the text is pushing the cursor along.
//! * C maintains its display array as it edits, and the maintenance disagrees with the loader
//!   in one case: an Enter that ends a row already [`SCREEN_COLS`] wide leaves that row's
//!   `'\n'` glued to it — `:308-310` sets the entry to `offset + 1` bytes and re-wraps only the
//!   text after it — while [`crate::text::split_data_lines`], what [`EditorData::recount`] runs
//!   over, gives the newline a row of its own. So the same text has one display line more in
//!   the port than in C right after the edit, and C's `display_line + 1` would point at that
//!   row; [`EditorData::insert`] returns the first display line of the new data line instead,
//!   which is the text position C's cursor is at. The screen still differs: the port shows the
//!   blank row where C only shows it once the article is loaded again.

use std::time::Duration;

use crate::paths::{
    DATA_EDITOR_HELP, MAX_USER_IDLE_TIME, SCREEN_COLS, SCREEN_ROWS, TAB_SIZE,
};
use crate::screen;
use crate::session::{self, Session};
use crate::term::Terminal;
use crate::term::key::{
    BACKSPACE, CR, KEY_CSI, KEY_CTRL_DOWN, KEY_CTRL_LEFT, KEY_CTRL_RIGHT, KEY_CTRL_UP, KEY_DEL,
    KEY_DOWN, KEY_END, KEY_ESC, KEY_F1, KEY_F5, KEY_HOME, KEY_INS, KEY_LEFT, KEY_NULL, KEY_PGDN,
    KEY_PGUP, KEY_RIGHT, KEY_TAB, KEY_TIMEOUT, KEY_UP,
};
use crate::text;

/// `MAX_EDITOR_DATA_LINES` (`include/editor.h:24`): the cap on *display* lines, not on lines
/// of text.
pub const MAX_EDITOR_DATA_LINES: usize = 65536;

/// `MAX_EDITOR_DATA_LINE_LENGTH` (`include/editor.h:23`): the bytes one text line may hold,
/// its `'\n'` included. Growing past it forces a hard break (`editor.c:272`, `:290`).
pub const MAX_EDITOR_DATA_LINE_LENGTH: usize = 1024;

/// `Ctrl(C)` (`include/io.h:89`), the control keys of the C switch.
const fn ctrl(ch: u8) -> u32 {
    (ch & 0x1f) as u32
}

const KEY_CTRL_W: u32 = ctrl(b'W');
const KEY_CTRL_X: u32 = ctrl(b'X');
const KEY_CTRL_S: u32 = ctrl(b'S');
const KEY_CTRL_E: u32 = ctrl(b'E');
const KEY_CTRL_T: u32 = ctrl(b'T');
const KEY_CTRL_B: u32 = ctrl(b'B');
const KEY_CTRL_K: u32 = ctrl(b'K');
const KEY_CTRL_Y: u32 = ctrl(b'Y');
const KEY_CTRL_Q: u32 = ctrl(b'Q');

/// `EDITOR_ESC_DISPLAY_STR` (`:32`): an escape byte in the text is shown as a green `*`, and
/// stays an escape byte in the saved body.
const ESC_DISPLAY_STR: &[u8] = b"\x1b[32m*\x1b[m";

/// One display line of one data line: `split_line`'s byte count and cell count.
#[derive(Debug, Clone, Copy, Default)]
struct Segment {
    /// Byte offset inside the data line.
    start: usize,
    /// Bytes in this display line, its `'\n'` included.
    len: usize,
    /// Cells in this display line, the `'\n'` charged one of them.
    width: usize,
}

/// `editor_data_insert`'s and `editor_data_delete`'s negative return: the document holds
/// [`MAX_EDITOR_DATA_LINES`] display lines and cannot grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverLines;

/// `EDITOR_DATA`.
#[derive(Debug, Clone)]
pub struct EditorData {
    /// The `SCREEN_COLS` the display lines are wrapped at.
    cols: usize,
    /// `UTF8_fixed_width`, decided by the session's `(V)变宽/(F)定宽` answer.
    fixed: bool,
    /// The data lines, `'\n'` included at the end of every line but the last.
    lines: Vec<Vec<u8>>,
    /// The wrap of each data line, index-parallel with [`EditorData::lines`].
    segments: Vec<Vec<Segment>>,
    /// `display_line_total`.
    total: usize,
}

impl EditorData {
    /// `editor_data_load` (`:77-155`).
    ///
    /// C wraps the whole buffer and packs the display lines back into 1024-byte data lines;
    /// the partition is the same as splitting the buffer after each `'\n'`, except that a
    /// buffer ending in a newline gets one more, empty, data line — the display line the C
    /// `do/while` produces after the last `'\n'` becomes its data line during packing
    /// (`:106-122`).
    pub fn load(data: &[u8], fixed: bool) -> EditorData {
        let mut lines: Vec<Vec<u8>> = data
            .split_inclusive(|b| *b == b'\n')
            .map(|line| line.to_vec())
            .collect();
        if data.last() == Some(&b'\n') {
            lines.push(Vec::new());
        }
        if lines.is_empty() {
            lines.push(Vec::new());
        }

        // `:131-138`, tab to single space: the editor has no tab stops of its own, and a
        // typed tab already expands on the way in (`editor_display`).
        for line in lines.iter_mut() {
            for byte in line.iter_mut() {
                if *byte == b'\t' {
                    *byte = b' ';
                }
            }
        }

        let mut this = EditorData {
            cols: SCREEN_COLS,
            fixed,
            lines,
            segments: Vec::new(),
            total: 0,
        };
        this.recount();
        this
    }

    /// `editor_data_save` (`:158-185`). `None` is C's `-2`: the destination buffer was too
    /// small, and like C the port logs what it found and lets the caller give up.
    pub fn save(&self, buf_len: usize) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        for line in &self.lines {
            if out.len() + line.len() + 1 > buf_len {
                tracing::error!(
                    "Data buffer not longer enough {} > {}",
                    out.len() + line.len() + 1,
                    buf_len
                );
                return None;
            }
            out.extend_from_slice(line);
        }
        Some(out)
    }

    /// `display_line_total`.
    pub fn total(&self) -> usize {
        self.total
    }

    /// `p_display_lines[i]` with `display_line_lengths[i]` bytes of it, `'\n'` included when
    /// the display line ends a data line. An index off either end reads the nearest one,
    /// where C would have indexed out of its array.
    pub fn display_line(&self, index: i64) -> &[u8] {
        let (line, seg) = self.locate(index);
        let seg = self.segments[line][seg];
        &self.lines[line][seg.start..seg.start + seg.len]
    }

    /// `display_line_lengths[i]`.
    pub fn display_line_length(&self, index: i64) -> usize {
        let (line, seg) = self.locate(index);
        self.segments[line][seg].len
    }

    /// `display_line_widths[i]`.
    pub fn display_line_width(&self, index: i64) -> usize {
        let (line, seg) = self.locate(index);
        self.segments[line][seg].width
    }

    /// `split_line(p_display_lines[display_line], col_pos - 1, &eol, &display_len, 0)`: the
    /// byte offset of the character that starts at column `col`, and the cells up to it.
    ///
    /// The C callers use the offset for the edit and `display_len` for the column, so both
    /// come back.
    pub fn col_to_offset(&self, index: i64, col: i64) -> (usize, usize) {
        let bytes = self.display_line(index);
        let max = (col - 1).max(0) as usize;
        let split = text::split_line(bytes, max, false, self.fixed);
        (split.bytes, split.display_len)
    }

    /// `editor_data_insert` (`:220-461`) for the bytes at the cursor.
    ///
    /// `display_line` and `offset` are the cursor in C's coordinates — a display line and a
    /// byte offset inside it — and come back on the slot C's edit leaves the text cursor at,
    /// which is what lets [`editor_display`] keep its `row_pos`/`col_pos` arithmetic.
    /// `last_updated_line` is the redraw bound the caller carries from edit to edit; C's
    /// `MAX()` over it (`:434`) is reproduced, so a line that grows repaints everything below.
    pub fn insert(
        &mut self,
        display_line: &mut i64,
        offset: &mut i64,
        bytes: &[u8],
        last_updated_line: &mut i64,
    ) -> Result<(), OverLines> {
        let (line, seg) = self.locate(*display_line);
        let segment = self.segments[line][seg];
        let pos = segment.start + (*offset).clamp(0, segment.len as i64) as usize;
        let len_data_line = self.lines[line].len();
        let old_total = self.total as i64;
        let cr = bytes == b"\r";
        // A carriage return is not text: it ends the line, and the cursor starts the next one.
        let inserted: &[u8] = if cr { &[] } else { bytes };
        let str_len = inserted.len();

        // `:271`, `:289`: the line is full, so the text is broken at the cursor and the rest
        // of it moves to a new data line. C picks the side of the break by whether the
        // insert itself still fits in the current line.
        if len_data_line + bytes.len() + 2 > MAX_EDITOR_DATA_LINE_LENGTH || cr {
            if self.total >= MAX_EDITOR_DATA_LINES {
                tracing::debug!(
                    "Split line error, display_line_total({}) reach limit({MAX_EDITOR_DATA_LINES})",
                    self.total
                );
                return Err(OverLines);
            }

            // C chooses the side of the break by whether the insert still fits in the line it
            // is typed into (`:289`).
            let break_before_insert =
                pos + bytes.len() + 2 >= MAX_EDITOR_DATA_LINE_LENGTH || cr;
            let mut head = self.lines[line][..pos].to_vec();
            let tail = if break_before_insert {
                [&inserted[..], &self.lines[line][pos..]].concat()
            } else {
                head.extend_from_slice(inserted);
                self.lines[line][pos..].to_vec()
            };
            // Both halves of a break are terminated lines; the tail keeps the `'\n'` the line
            // it came from had, if it had one.
            head.push(b'\n');

            *offset = if break_before_insert {
                str_len as i64
            } else {
                (pos - segment.start + str_len) as i64
            };

            self.lines[line] = head;
            self.lines.insert(line + 1, tail);
            self.recount();

            if break_before_insert {
                // `:312`: C's cursor goes to the display line after the head's last one, which
                // is where it glues the terminating `'\n'` to a row that is already full. The
                // port's wrap gives that newline a row of its own, so the same slot number
                // would point at it and the next keystroke would edit the head line again.
                *display_line = self.first_display(line + 1);
            }
            // `:344`: the redraw reaches as far as the document used to.
            *last_updated_line = old_total.max(self.last_display(line + 1));
        } else {
            // `:348-349`: the insert goes into the line at the cursor.
            self.lines[line].splice(pos..pos, inserted.iter().copied());
            self.recount();
            *offset += str_len as i64;
            *last_updated_line = (*last_updated_line).max(self.last_display(line));
        }

        // `:436-443`: an insert that ran to the end of its display line puts the cursor on
        // the next one.
        let length = self.display_line_length(*display_line) as i64;
        if *offset >= length && *display_line + 1 < self.total as i64 {
            *offset -= length;
            *display_line += 1;
        }

        Ok(())
    }

    /// `editor_data_delete` (`:463-643`), one character or the rest of the display line.
    ///
    /// The return is C's: the bytes removed, `0` when the cursor was already at the end of
    /// the line and nothing could go.
    pub fn delete(
        &mut self,
        display_line: &mut i64,
        offset: &mut i64,
        last_updated_line: &mut i64,
        del_line: bool,
    ) -> i64 {
        let (line, seg) = self.locate(*display_line);
        let segment = self.segments[line][seg];
        let pos = segment.start + (*offset).clamp(0, segment.len as i64) as usize;
        let len_data_line = self.lines[line].len();
        let old_total = self.total as i64;

        // `:512`: at the end of the data line there is nothing to delete.
        if pos >= len_data_line {
            return 0;
        }

        // `:518-541`: how much the one deletion takes away.
        let str_len = if del_line {
            segment.len as i64 - (*offset).clamp(0, segment.len as i64)
        } else {
            let byte = self.lines[line][pos];
            if byte > 0 && byte < 127 {
                1
            } else if byte & 0x80 != 0 {
                text::seq_len(byte) as i64
            } else {
                tracing::error!(
                    "Some strange character at display_line {display_line}, offset {offset}: {byte} {next}",
                    offset = *offset,
                    next = self.lines[line].get(pos + 1).copied().unwrap_or(0)
                );
                1
            }
        };

        // `:544-578`: the deletion reaches the end of the line, so the next line is pulled up
        // into this one instead of a plain cut.
        let reaches_end = pos + str_len as usize > len_data_line
            || (pos + str_len as usize == len_data_line
                && self.lines[line][if del_line { len_data_line - 1 } else { pos }] == b'\n');

        if reaches_end {
            if line + 1 >= self.lines.len() {
                return 0;
            }
            let next_len = self.lines[line + 1].len();
            if pos + next_len + 1 > MAX_EDITOR_DATA_LINE_LENGTH {
                return 0;
            }
            let next = self.lines.remove(line + 1);
            let mut head = self.lines[line][..pos].to_vec();
            head.extend_from_slice(&next);
            self.lines[line] = head;
        } else {
            self.lines[line].splice(pos..pos + str_len as usize, []);
        }

        self.recount();
        // `:609-636`: the redraw reaches the last display line of the line that changed, or
        // the end of what used to be there when the document got shorter.
        let last = self.last_display(line);
        *last_updated_line = if (self.total as i64) < old_total {
            (old_total - 1).max(last)
        } else {
            last
        };

        str_len
    }

    /// The `mbstowcs` + `wcwidth` C applies to the character it just inserted or moved onto:
    /// two cells in fixed-width mode, the Unicode width otherwise, one if either fails.
    fn cells_of(&self, bytes: &[u8]) -> usize {
        let lead = bytes.first().copied().unwrap_or(0);
        let n = text::seq_len(lead).min(bytes.len()).max(1);
        text::multibyte_width(bytes, n, self.fixed)
    }

    /// The data line and display-line-within-it that `index` falls on.
    fn locate(&self, index: i64) -> (usize, usize) {
        let mut rest = index.clamp(0, self.total.saturating_sub(1) as i64) as usize;
        for (line, segments) in self.segments.iter().enumerate() {
            if rest < segments.len() {
                return (line, rest);
            }
            rest -= segments.len();
        }
        let last = self.segments.len() - 1;
        (last, self.segments[last].len() - 1)
    }

    /// The display index of the first line of data line `line`.
    fn first_display(&self, line: usize) -> i64 {
        self.segments
            .iter()
            .take(line)
            .map(|segments| segments.len())
            .sum::<usize>() as i64
    }

    /// The display index of the last line of data line `line`.
    fn last_display(&self, line: usize) -> i64 {
        self.first_display(line) + self.segments.get(line).map(|s| s.len()).unwrap_or(1) as i64 - 1
    }

    /// Re-wrap every data line and update `display_line_total`. `split_data_lines` adds a
    /// display line after the one that ends on a `'\n'`; C's re-split stops at that newline
    /// instead (`:427-431`), so the empty tail of a terminated line is dropped here — but only
    /// when the split got far enough to produce it, which the display-line budget below can cut
    /// off.
    ///
    /// C splits the whole buffer into one display array of [`MAX_EDITOR_DATA_LINES`] entries
    /// (`:81, :99`), so a document longer than that keeps only its first 65536 display lines;
    /// the budget is what carries that into the per-line re-split.
    fn recount(&mut self) {
        self.segments.clear();
        self.total = 0;
        for line in &self.lines {
            if self.total >= MAX_EDITOR_DATA_LINES {
                tracing::debug!(
                    "Line count {} reaches limit {}",
                    self.total,
                    MAX_EDITOR_DATA_LINES
                );
                break;
            }
            let budget = MAX_EDITOR_DATA_LINES - self.total;
            let wrapped = text::split_data_lines(line, self.cols, false, self.fixed, budget);
            let mut segments: Vec<Segment> = (0..wrapped.count())
                .map(|index| Segment {
                    start: wrapped.offsets[index] as usize,
                    len: (wrapped.offsets[index + 1] - wrapped.offsets[index]) as usize,
                    width: wrapped.widths[index] as usize,
                })
                .collect();
            if line.last() == Some(&b'\n') && segments.last().is_some_and(|s| s.len == 0) {
                segments.pop();
            }
            if segments.is_empty() {
                segments.push(Segment::default());
            }
            self.total += segments.len();
            self.segments.push(segments);
        }
    }
}

/// `EDITOR_ESC_DISPLAY_STR`'s replacement, exposed for [`editor_display`] and its tests.
fn escape_display(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for byte in bytes {
        if *byte == 0x1b {
            out.extend_from_slice(ESC_DISPLAY_STR);
        } else {
            out.push(*byte);
        }
    }
    out
}

/// `screen.c:1007`'s window: the rows the text is painted on, the status row excluded.
const SCREEN_BEGIN_ROW: i64 = 1;

/// `SCREEN_ROWS - screen_begin_row` (`:674`).
const SCREEN_ROW_TOTAL: i64 = SCREEN_ROWS as i64 - SCREEN_BEGIN_ROW;

/// `EDITOR_CTX` (`include/editor.h:30-36`).
#[derive(Debug, Default, Clone)]
pub struct EditorCtx {
    /// `msg`: the right half of the status row.
    pub msg: String,
    /// `line_cursor`: the line number the status row shows.
    pub line_cursor: i64,
}

/// `editor_display_key_handler` (`:645-660`).
///
/// The C function always returns 0, which makes the `!= 0` branches of its two callers dead
/// code; the port has no return for them to check.
fn key_handler(ch: &mut u32, ctx: &mut EditorCtx, fixed: bool) {
    match *ch {
        // `igetch` reporting "nothing decoded" is the refresh of the status row.
        0 => {
            ctx.msg = format!(
                "| 退出[\x1b[32mCtrl-W\x1b[33m] | [\x1b[32m{}\x1b[33m]",
                if fixed { "定宽" } else { "变宽" }
            );
        }
        KEY_CSI => *ch = KEY_ESC,
        _ => {}
    }
}

/// The part of `editor_display` that is not the key loop: which display line is on which
/// screen row, and where the cursor is.
#[derive(Debug, Clone)]
struct View {
    /// The display line painted on `output_current_row`.
    line_current: i64,
    /// The screen row `line_current` goes on.
    output_current_row: i64,
    /// The last screen row worth painting.
    output_end_row: i64,
    /// The cursor's screen row and column.
    row_pos: i64,
    col_pos: i64,
    /// `last_updated_line`, which C carries from one edit to the next.
    last_updated_line: i64,
}

impl View {
    fn new() -> View {
        View {
            line_current: 0,
            output_current_row: SCREEN_BEGIN_ROW,
            output_end_row: SCREEN_ROWS as i64 - 1,
            row_pos: 1,
            col_pos: 1,
            last_updated_line: 0,
        }
    }

    /// `line_current - output_current_row + row_pos`: the display line under the cursor.
    fn cursor_line(&self) -> i64 {
        self.line_current - self.output_current_row + self.row_pos
    }

    /// `MIN(col_pos, MAX(1, display_line_widths[cursor_line]))`, the clamp every vertical
    /// movement applies so the cursor does not land past a short line.
    fn clamp_col(&mut self, data: &EditorData) {
        let width = data.display_line_width(self.cursor_line()) as i64;
        self.col_pos = self.col_pos.min(width.max(1));
    }

    /// `:1057-1096`, `KEY_LEFT`: one character back, or the last column of the row above.
    /// `false` is the fall-through into `KEY_UP`.
    fn left(&mut self, data: &EditorData) -> bool {
        let line = self.cursor_line();
        let (offset, display_len) = data.col_to_offset(line, self.col_pos);
        self.col_pos = display_len as i64;
        if offset > 0 {
            let bytes = data.display_line(line);
            let mut start = offset - 1;
            let mut len = 1;
            if bytes[start] & 0x80 != 0 {
                while start > 0 && bytes[start] & 0xc0 != 0xc0 {
                    len += 1;
                    start -= 1;
                }
                if len > 4 {
                    tracing::error!("Invalid UTF-8 data detected: str_len > 4");
                }
                // C walks back to the head byte and charges the character's width; the
                // cursor ends up one cell further back for a double-width one.
                if data.cells_of(&bytes[start..(start + len).min(bytes.len())]) == 2 {
                    self.col_pos -= 1;
                }
            }
        }
        if self.col_pos >= 1 {
            return true;
        }
        self.col_pos = SCREEN_COLS as i64;
        false
    }

    /// `:1097-1115`, `KEY_UP`.
    fn up(&mut self, data: &EditorData) {
        if self.row_pos > SCREEN_BEGIN_ROW {
            self.row_pos -= 1;
            self.clamp_col(data);
            return;
        }
        if self.line_current - self.output_current_row < 0 {
            self.col_pos = 1;
            return;
        }
        self.line_current -= self.output_current_row;
        self.output_current_row = SCREEN_BEGIN_ROW;
        // The commented-out `\033[T` of C's: the whole window is repainted instead.
        self.output_end_row = SCREEN_ROWS as i64 - 1;
        self.clamp_col(data);
    }

    /// `:1116-1160`, `KEY_RIGHT`: one character on, or the first column of the row below.
    /// `false` is the fall-through into `KEY_DOWN`.
    fn right(&mut self, data: &EditorData) -> bool {
        let line = self.cursor_line();
        let (offset, display_len) = data.col_to_offset(line, self.col_pos);
        // The cell after the character the cursor is on: two past its own start, plus one
        // more when it is double-width.
        self.col_pos = display_len as i64 + 2;
        let bytes = data.display_line(line);
        if offset < data.display_line_length(line) && bytes[offset] & 0x80 != 0 {
            let len = text::seq_len(bytes[offset]);
            if len > 4 {
                tracing::error!("Invalid UTF-8 data detected: str_len > 4");
            }
            if data.cells_of(&bytes[offset..(offset + len).min(bytes.len())]) == 2 {
                self.col_pos += 1;
            }
        }
        if self.col_pos <= data.display_line_width(line) as i64 {
            return true;
        }
        self.col_pos = 1;
        false
    }

    /// `:1161-1182`, `KEY_DOWN`: one row down, scrolling the window when it runs off.
    fn down(&mut self, data: &EditorData, term: &mut Terminal) {
        if self.row_pos < SCREEN_ROW_TOTAL.min(data.total() as i64) {
            self.row_pos += 1;
            self.clamp_col(data);
            return;
        }
        if self.cursor_line() == data.total() as i64 - 1 {
            // The last display line has no `'\n'`, so its width is the last column.
            self.col_pos = data.display_line_width(self.cursor_line()) as i64 + 1;
            return;
        }
        self.line_current += SCREEN_ROW_TOTAL - (self.output_current_row - SCREEN_BEGIN_ROW);
        self.output_current_row = SCREEN_ROW_TOTAL;
        self.output_end_row = SCREEN_ROWS as i64 - 1;
        self.clamp_col(data);
        // C scrolls with a bare `"\n"` because the legacy terminals it was written for only
        // scrolled on that; `\033[S` is commented out beside it.
        term.moveto(SCREEN_ROWS, 0);
        term.clrtoeol();
        term.print("\n");
    }

    /// The repaint C asks for after `Ctrl-Q` help or `KEY_F5` (`:1223-1229`).
    fn refresh(&mut self, term: &mut Terminal) {
        self.line_current -= self.output_current_row - SCREEN_BEGIN_ROW;
        self.output_current_row = SCREEN_BEGIN_ROW;
        self.output_end_row = SCREEN_ROWS as i64 - 1;
        clr_rows(term, self.output_current_row, SCREEN_ROWS as i64);
    }
}

/// `clrline`, over the signed row numbers the editor keeps.
fn clr_rows(term: &mut Terminal, begin: i64, end: i64) {
    if end < begin {
        return;
    }
    screen::clrline(term, begin.max(1) as usize, end.max(1) as usize);
}

/// The status row, `:706-724`: cursor position, line number, insert mode and the message the
/// extended key handler left behind, padded to the full width and reset.
fn status_line(data: &EditorData, view: &View, ctx: &EditorCtx, key_insert: bool) -> Vec<u8> {
    let buffer = format!(
        "\x1b[1;44;33m[\x1b[32m{}\x1b[33m;\x1b[32m{}\x1b[33m] \
         第\x1b[32m{}\x1b[33m/\x1b[32m{}\x1b[33m行 [\x1b[32m{}\x1b[33m] {}",
        view.row_pos,
        view.col_pos,
        ctx.line_cursor,
        data.total(),
        if key_insert { "插入" } else { "替换" },
        ctx.msg,
    );

    let bytes = buffer.as_bytes();
    let split = text::split_line(bytes, SCREEN_COLS, true, data.fixed);
    let mut out = bytes[..split.bytes].to_vec();
    for _ in split.display_len..SCREEN_COLS {
        out.push(b' ');
    }
    out.extend_from_slice(b"\x1b[m");
    out
}

/// `editor_display` (`:662-1294`): the full-screen edit of `data`.
///
/// The return is C's last key — `Ctrl-W`/`Ctrl-X` when the user left the way they should,
/// [`KEY_NULL`] or [`KEY_TIMEOUT`] when the connection or the idle limit ended it. C's
/// callers ignore it and ask their own question afterwards, which is what
/// [`crate::post`] does too; the value is kept because a port that can tell a saved body from
/// a dropped connection is worth the extra return.
///
/// # Deviations from C
///
/// * `show_help` (`:663`) was a process-wide `static int` guarding the help pager against
///   re-entering the editor. One task owns one editor here, so the flag has nothing to
///   guard and the refresh C falls through to is done directly.
/// * `:1260-1264` catches a negative display-line length. Rust cannot produce one.
/// * `MIN`/`MAX` of C's `long`s are the same over [`i64`]; the array indexes they produce are
///   clamped to the document, where C would have read past `p_display_lines`.
pub async fn editor_display(
    term: &mut Terminal,
    session: &mut Session,
    data: &mut EditorData,
) -> u32 {
    let mut view = View::new();
    let mut ctx = EditorCtx::default();
    let mut key_insert = true;
    let mut ch: u32 = 0;
    // `input_str`, the bytes of the character about to be inserted.
    let mut input: Vec<u8> = Vec::new();
    // `tab_width`: the spaces a typed tab still owes; reset with every key read.
    let mut tab_width: i64;

    clr_rows(term, SCREEN_BEGIN_ROW, SCREEN_ROWS as i64);
    key_handler(&mut ch, &mut ctx, data.fixed);

    while !session.state.is_exiting() {
        // The text area is behind when the window has run out of rows to paint.
        if view.line_current >= data.total() as i64 || view.output_current_row > view.output_end_row
        {
            ctx.line_cursor = view.line_current - view.output_current_row + view.row_pos + 1;
            let status = status_line(data, &view, &ctx, key_insert);
            term.moveto(SCREEN_ROWS, 0);
            term.print_bytes(&status);
            term.moveto_signed(view.row_pos as i32, view.col_pos as i32);
            let _ = term.flush().await;

            tab_width = 0;
            input.clear();
            ch = term.read_key_wait(MAX_USER_IDLE_TIME).await;

            // One keystroke, then the row that was typed on is repainted — which is what the
            // `break`s out of this loop are for.
            while !session.state.is_exiting() {
                if ch != KEY_NULL && ch != KEY_TIMEOUT {
                    session.last_access_tm = session::now();
                }

                key_handler(&mut ch, &mut ctx, data.fixed);

                // A tab is a space on the way in, and the editor fills the rest of the tab
                // stop before it reads another key.
                if ch == KEY_TAB {
                    ch = b' ' as u32;
                    tab_width = TAB_SIZE as i64 - (view.col_pos - 1) % TAB_SIZE as i64 - 1;
                }

                // The lead byte of a multi-byte character; the rest arrive on their own
                // `igetch(100)` calls (`:751-776`).
                if ch < 256 && (ch as u8) & 0x80 != 0 {
                    input.clear();
                    let mut c: u8 = (ch as u8) & 0xf0;
                    while c & 0x80 != 0 {
                        input.push(ch as u8);
                        c = (c & 0x7f) << 1;
                        if c & 0x80 == 0 {
                            break;
                        }
                        ch = term.read_key(Duration::from_millis(100)).await;
                        if ch == KEY_NULL || ch == KEY_TIMEOUT {
                            tracing::debug!(
                                "Ignore {} bytes of incomplete UTF8 character",
                                input.len()
                            );
                            input.clear();
                            break;
                        }
                    }
                }

                // Printable text, `CR` and `ESC` all go into the document — ESC included,
                // which is why an escape is shown as a star rather than as colour.
                if (32..127).contains(&ch) || input.len() >= 2 || ch == CR || ch == KEY_ESC {
                    if let Err(e) = session.online_update(None).await {
                        tracing::error!("user_online_update(NULL) error: {e}");
                    }

                    if input.is_empty() {
                        input.push(ch as u8);
                    }

                    let display_line_in = view.cursor_line();
                    let offset_in = data.col_to_offset(display_line_in, view.col_pos).0 as i64;
                    let mut display_line_out = display_line_in;
                    let mut offset_out = offset_in;
                    view.last_updated_line = display_line_in;

                    if !key_insert {
                        data.delete(
                            &mut display_line_out,
                            &mut offset_out,
                            &mut view.last_updated_line,
                            false,
                        );
                    }

                    if data
                        .insert(
                            &mut display_line_out,
                            &mut offset_out,
                            &input,
                            &mut view.last_updated_line,
                        )
                        .is_err()
                    {
                        tracing::error!("editor_data_insert(str_len={}) error", input.len());
                    } else {
                        view.output_end_row = (SCREEN_ROWS as i64 - 1)
                            .min(view.output_current_row + view.last_updated_line - view.line_current);
                        view.line_current -= view.output_current_row - view.row_pos;
                        view.output_current_row = view.row_pos;

                        let scroll_rows = (display_line_out - display_line_in
                            - (view.output_end_row - view.output_current_row))
                            .max(0);
                        if scroll_rows > 0 {
                            term.moveto(SCREEN_ROWS, 0);
                            term.clrtoeol();
                            for _ in 0..scroll_rows {
                                term.print("\n");
                            }
                            view.output_current_row -= scroll_rows;
                            if view.output_current_row < SCREEN_BEGIN_ROW {
                                view.line_current += SCREEN_BEGIN_ROW - view.output_current_row;
                                view.output_current_row = SCREEN_BEGIN_ROW;
                            }
                            view.row_pos = view.output_end_row;
                        } else {
                            view.row_pos += display_line_out - display_line_in;
                        }

                        if offset_out != offset_in {
                            if display_line_out != display_line_in {
                                view.col_pos = 1;
                            }
                            if offset_out > 0 {
                                view.col_pos += if input.len() == 1 {
                                    1
                                } else {
                                    data.cells_of(&input) as i64
                                };
                            }
                        }
                    }

                    if display_line_out != display_line_in {
                        break;
                    }

                    if ch == b' ' as u32 && tab_width > 0 {
                        tab_width -= 1;
                    } else {
                        // Keep reading while the client is still sending; paint as soon as it
                        // stops (`:873-877`).
                        ch = term.read_key(Duration::ZERO).await;
                        if ch == KEY_NULL || ch == KEY_TIMEOUT {
                            break;
                        }
                    }

                    input.clear();
                    continue;
                }

                if ch == KEY_DEL || ch == BACKSPACE || ch == KEY_CTRL_K || ch == KEY_CTRL_Y {
                    if let Err(e) = session.online_update(None).await {
                        tracing::error!("user_online_update(NULL) error: {e}");
                    }

                    let mut del_line = false;

                    if ch == BACKSPACE {
                        // At the very top of the document there is nothing back to.
                        if view.cursor_line() <= 0 && view.col_pos <= 1 {
                            break;
                        }
                        let display_len =
                            data.col_to_offset(view.cursor_line(), view.col_pos).1 as i64;
                        view.col_pos = display_len;
                        // `:906-909`: the cursor is on the first cell, so it backs onto the
                        // last cell of the row above. `row_pos` is already decremented when C
                        // reads the width, so it reads the row the cursor just landed on.
                        if view.col_pos < 1 && view.cursor_line() >= 0 {
                            view.row_pos -= 1;
                            view.col_pos =
                                data.display_line_width(view.cursor_line()).max(1) as i64;
                        }
                    } else if ch == KEY_CTRL_K {
                        del_line = true;
                    } else if ch == KEY_CTRL_Y {
                        view.col_pos = 1;
                        del_line = true;
                    }

                    let display_line_in = view.cursor_line();
                    let (offset_in, display_len) =
                        data.col_to_offset(display_line_in, view.col_pos);
                    let offset_in = offset_in as i64;
                    let mut display_line_out = display_line_in;
                    let mut offset_out = offset_in;

                    let str_len = data.delete(
                        &mut display_line_out,
                        &mut offset_out,
                        &mut view.last_updated_line,
                        del_line,
                    );
                    if str_len < 0 {
                        tracing::error!("editor_data_delete() error: {str_len}");
                    } else {
                        view.col_pos = display_len as i64 + 1;

                        view.output_end_row = (SCREEN_ROWS as i64 - 1).min(
                            view.output_current_row + view.last_updated_line - view.line_current,
                        );
                        view.line_current -= view.output_current_row - view.row_pos;
                        view.output_current_row = view.row_pos;

                        if view.output_current_row < SCREEN_BEGIN_ROW {
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.row_pos = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                        }

                        // The cursor ran past the end of the document: pull the window back
                        // to it (`:945-955`).
                        if view.line_current + (SCREEN_ROW_TOTAL - view.output_current_row)
                            >= data.total() as i64
                            && data.total() > SCREEN_ROW_TOTAL as usize
                        {
                            let scroll_rows = (view.line_current
                                - (view.output_current_row - SCREEN_BEGIN_ROW))
                                - (data.total() as i64 - SCREEN_ROW_TOTAL);
                            view.line_current = data.total() as i64 - SCREEN_ROW_TOTAL;
                            view.row_pos += scroll_rows;
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                        }

                        clr_rows(term, view.output_current_row, view.output_end_row);
                    }

                    if display_line_out != display_line_in {
                        break;
                    }

                    ch = term.read_key(Duration::ZERO).await;
                    if ch == KEY_NULL || ch == KEY_TIMEOUT {
                        break;
                    }

                    input.clear();
                    continue;
                }

                let mut input_ok = true;
                match ch {
                    KEY_NULL => {
                        tracing::debug!("KEY_NULL");
                        return ch;
                    }
                    KEY_TIMEOUT => {
                        tracing::debug!("User input timeout");
                        return ch;
                    }
                    KEY_CTRL_W | KEY_CTRL_X => return ch,
                    KEY_CTRL_S | KEY_CTRL_LEFT => view.col_pos = 1,
                    KEY_CTRL_E | KEY_CTRL_RIGHT => {
                        if view.cursor_line() == data.total() as i64 - 1 {
                            view.col_pos = data.display_line_width(view.cursor_line()) as i64 + 1;
                        } else {
                            view.col_pos = data.display_line_width(view.cursor_line()).max(1) as i64;
                        }
                    }
                    KEY_CTRL_T | KEY_CTRL_UP => {
                        view.row_pos = SCREEN_BEGIN_ROW;
                        view.clamp_col(data);
                    }
                    KEY_CTRL_B | KEY_CTRL_DOWN => {
                        if data.total() < SCREEN_ROW_TOTAL as usize {
                            view.row_pos = data.total() as i64;
                        } else {
                            view.row_pos = SCREEN_ROWS as i64 - 1;
                        }
                        let width = data.display_line_width(view.cursor_line()) as i64;
                        if view.line_current
                            + (SCREEN_ROW_TOTAL - (view.output_current_row - SCREEN_BEGIN_ROW))
                            >= data.total() as i64
                        {
                            view.col_pos = view.col_pos.min(width + 1);
                        } else {
                            view.col_pos = view.col_pos.min(width.max(1));
                        }
                    }
                    KEY_INS => key_insert = !key_insert,
                    KEY_HOME => {
                        view.row_pos = 1;
                        view.col_pos = 1;
                        if view.line_current - view.output_current_row >= 0 {
                            view.line_current = 0;
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                            clr_rows(term, view.output_current_row, SCREEN_ROWS as i64);
                        }
                    }
                    KEY_END => {
                        if data.total() < SCREEN_ROW_TOTAL as usize {
                            view.row_pos = data.total() as i64;
                            view.col_pos =
                                data.display_line_width(view.cursor_line()) as i64 + 1;
                        } else {
                            view.line_current = data.total() as i64 - SCREEN_ROW_TOTAL;
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                            view.row_pos = SCREEN_ROWS as i64 - 1;
                            view.col_pos =
                                data.display_line_width(view.cursor_line()) as i64 + 1;
                            clr_rows(term, view.output_current_row, SCREEN_ROWS as i64);
                        }
                    }
                    KEY_LEFT => {
                        if !view.left(data) {
                            view.up(data);
                        }
                    }
                    KEY_UP => view.up(data),
                    KEY_RIGHT => {
                        if !view.right(data) {
                            view.down(data, term);
                        }
                    }
                    KEY_DOWN => view.down(data, term),
                    KEY_PGUP => {
                        if view.line_current - view.output_current_row >= 0 {
                            view.line_current -= (SCREEN_ROW_TOTAL - 1)
                                + (view.output_current_row - SCREEN_BEGIN_ROW);
                            view.line_current = view.line_current.max(0);
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                            view.clamp_col(data);
                            clr_rows(term, view.output_current_row, SCREEN_ROWS as i64);
                        }
                    }
                    KEY_PGDN => {
                        if view.line_current + SCREEN_ROW_TOTAL
                            - (view.output_current_row - SCREEN_BEGIN_ROW)
                            < data.total() as i64
                        {
                            view.line_current += (SCREEN_ROW_TOTAL - 1)
                                - (view.output_current_row - SCREEN_BEGIN_ROW);
                            if view.line_current + SCREEN_ROW_TOTAL > data.total() as i64 {
                                view.line_current = data.total() as i64 - SCREEN_ROW_TOTAL;
                            }
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                            view.clamp_col(data);
                            clr_rows(term, view.output_current_row, SCREEN_ROWS as i64);
                        }
                    }
                    // The help file, then the same refresh `F5` asks for — C reaches it by
                    // falling out of the `F1` case without a `break`.
                    KEY_CTRL_Q | KEY_F1 => {
                        session.display_file(term, DATA_EDITOR_HELP, 1).await;
                        view.refresh(term);
                    }
                    KEY_F5 => view.refresh(term),
                    0 => {}
                    _ => input_ok = false,
                }

                if let Err(e) = session.online_update(None).await {
                    tracing::error!("user_online_update(NULL) error: {e}");
                }

                if input_ok {
                    break;
                }

                ch = term.read_key_wait(MAX_USER_IDLE_TIME).await;
            }

            continue;
        }

        // One display line per row, escapes shown as stars (`:1254-1288`).
        let mut len = data.display_line_length(view.line_current);
        if len >= MAX_EDITOR_DATA_LINE_LENGTH {
            tracing::error!(
                "Buffer overflow: len={len} line={} ",
                view.line_current
            );
            len = MAX_EDITOR_DATA_LINE_LENGTH - 1;
        }
        let painted = escape_display(&data.display_line(view.line_current)[..len]);

        term.moveto_signed(view.output_current_row as i32, 0);
        term.clrtoeol();
        term.print_bytes(&painted);
        view.line_current += 1;
        view.output_current_row += 1;
    }

    ch
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{ByteIo, Pipe, Terminal};

    /// The callers save into the article body limit; the tests only need one big enough for
    /// what they assert.
    const BUF: usize = 1 << 16;

    fn doc(text: &str) -> EditorData {
        EditorData::load(text.as_bytes(), false)
    }

    fn saved(data: &EditorData) -> Vec<u8> {
        data.save(BUF).unwrap()
    }

    /// The editor over a byte pipe: the client end comes back so a test can feed keys and
    /// read the painted screen.
    fn editor_pipe() -> (Terminal, Session, Pipe) {
        let (client, server) = Pipe::new();
        let mut session = crate::session::offline_session();
        // The insert and delete paths call `online_update(None)`; a fresh action time keeps it
        // inside the refresh interval and therefore off the database.
        session.current_action_tm = session::now();
        (Terminal::new(ByteIo::Pipe(server)), session, client)
    }

    fn painted(client: &mut Pipe) -> String {
        String::from_utf8_lossy(&client.take_available()).into_owned()
    }

    /// What a test feeds the editor, with the exit key the flows behind [`editor_display`]
    /// always press.
    fn keys(client: &Pipe, input: &[u8]) {
        client.push(input);
        client.push(&[ctrl(b'W') as u8]);
    }

    /// The editor with a wall-clock guard: a key sequence that does not end it would otherwise
    /// wait out [`MAX_USER_IDLE_TIME`].
    async fn edit(term: &mut Terminal, session: &mut Session, data: &mut EditorData) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), editor_display(term, session, data))
            .await
            .expect("the editor stopped reading keys")
    }

    // ---- the document model ----

    #[test]
    fn a_carriage_return_costs_no_cells() {
        let data = doc("hello\r\nworld");
        assert_eq!(data.total(), 2);
        assert_eq!(data.display_line(0), b"hello\r\n");
        assert_eq!(data.display_line_length(0), 7);
        assert_eq!(data.display_line_width(0), 6);
        assert_eq!(data.display_line(1), b"world");
        assert_eq!(data.display_line_width(1), 5);
        // `split_line` counts the `'\r'` into the byte offset it returns without charging it a
        // cell, so the third column of the row is the `'\n'` two bytes further in: the row
        // reads `ab` in two cells and the newline is the third.
        assert_eq!(doc("ab\r\ncd").col_to_offset(0, 3), (3, 2));
    }

    #[test]
    fn a_buffer_ending_in_a_newline_opens_a_row() {
        assert_eq!(doc("one\ntwo\n").total(), 3);
        assert_eq!(doc("one\ntwo").total(), 2);
        assert_eq!(doc("").total(), 1);
    }

    #[test]
    fn tabs_load_as_spaces() {
        assert_eq!(doc("a\tb").display_line(0), b"a b");
    }

    #[test]
    fn a_long_line_wraps_at_the_screen_width() {
        let data = doc(&"x".repeat(SCREEN_COLS + 5));
        assert_eq!(data.total(), 2);
        assert_eq!(data.display_line_length(0), SCREEN_COLS);
        assert_eq!(data.display_line_length(1), 5);
        assert_eq!(saved(&data), "x".repeat(SCREEN_COLS + 5).into_bytes());
    }

    #[test]
    fn save_round_trips_and_leaves_a_byte_for_the_terminator() {
        assert_eq!(saved(&doc("one\ntwo\n")), b"one\ntwo\n");
        assert_eq!(doc("abc").save(3), None);
        assert_eq!(doc("abc").save(4), Some(b"abc".to_vec()));
    }

    #[test]
    fn an_enter_breaks_the_line_at_the_cursor() {
        let mut data = doc("hello");
        let (mut line, mut offset) = (0i64, 2i64);
        let mut last_updated = 0i64;
        data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
            .unwrap();
        assert_eq!(data.display_line(0), b"he\n");
        assert_eq!(data.display_line(1), b"llo");
        // The cursor starts the next display line, which is what ends the inner key loop.
        assert_eq!((line, offset), (1, 0));
        assert_eq!(saved(&data), b"he\nllo");
    }

    #[test]
    fn typing_past_the_end_of_a_row_carries_the_cursor_onto_the_next() {
        // `:436-443`: an insert that fills a display line leaves the cursor one cell into the
        // row after it.
        let mut data = doc(&"x".repeat(SCREEN_COLS));
        assert_eq!(data.total(), 1);
        let (mut line, mut offset) = (0i64, SCREEN_COLS as i64);
        let mut last_updated = 0i64;
        data.insert(&mut line, &mut offset, b"y", &mut last_updated)
            .unwrap();
        assert_eq!(data.total(), 2);
        assert_eq!((line, offset), (1, 1));
    }

    #[test]
    fn a_deletion_at_the_end_of_a_line_joins_the_next_one() {
        let mut data = doc("ab\ncd");
        let (mut line, mut offset) = (0i64, 2i64);
        let mut last_updated = 0i64;
        assert_eq!(
            data.delete(&mut line, &mut offset, &mut last_updated, false),
            1
        );
        assert_eq!(data.total(), 1);
        assert_eq!(saved(&data), b"abcd");
        // And once the cursor is past the merged line's last byte there is nothing left.
        offset = 4;
        assert_eq!(
            data.delete(&mut line, &mut offset, &mut last_updated, false),
            0
        );
    }

    #[test]
    fn a_deletion_cuts_a_multibyte_character_whole() {
        let mut data = doc("中x");
        let (mut line, mut offset) = (0i64, 0i64);
        let mut last_updated = 0i64;
        assert_eq!(
            data.delete(&mut line, &mut offset, &mut last_updated, false),
            3
        );
        assert_eq!(saved(&data), b"x");
    }

    #[test]
    fn the_rest_of_a_row_deletes_as_one_block() {
        let mut data = doc("abcdef");
        let (mut line, mut offset) = (0i64, 2i64);
        let mut last_updated = 0i64;
        assert_eq!(
            data.delete(&mut line, &mut offset, &mut last_updated, true),
            4
        );
        assert_eq!(saved(&data), b"ab");
    }

    #[test]
    fn an_escape_stays_text_and_shows_as_a_star() {
        assert_eq!(
            escape_display(b"a\x1bb"),
            b"a\x1b[32m*\x1b[mb".to_vec(),
            "the ESC byte is replaced, its surroundings are untouched"
        );
    }

    #[test]
    fn the_document_refuses_to_grow_past_the_display_line_limit() {
        let mut text = "a\n".repeat(MAX_EDITOR_DATA_LINES - 1);
        text.push('a');
        let mut data = doc(&text);
        assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
        let (mut line, mut offset) = (0i64, 0i64);
        let mut last_updated = 0i64;
        assert_eq!(
            data.insert(&mut line, &mut offset, b"\r", &mut last_updated),
            Err(OverLines)
        );
        assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    }

    /// The same cap as `editor.c:99`, where the display array is the size of the limit: a
    /// body of short lines fills it and the rest of the document is not editable.
    #[test]
    fn loading_stops_counting_display_lines_at_the_limit() {
        let over = MAX_EDITOR_DATA_LINES + 4_000;
        let text = "a\n".repeat(over);
        let data = doc(&text);
        assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
        assert_eq!(data.display_line(data.total() as i64 - 1), b"a\n");
        assert_eq!(
            data.locate(data.total() as i64 - 1).0 + 1,
            MAX_EDITOR_DATA_LINES,
            "the last display line is the last data line C had room for"
        );
    }

    // ---- the screen half ----

    #[tokio::test]
    async fn typed_text_reaches_the_document() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, b"hi");

        let mut data = doc("");
        assert_eq!(edit(&mut term, &mut session, &mut data).await, ctrl(b'W'));
        assert_eq!(saved(&data), b"hi");
    }

    #[tokio::test]
    async fn enter_after_a_row_that_is_exactly_full_starts_the_next_line() {
        let (mut term, mut session, mut client) = editor_pipe();
        // Rows that fill the screen to the last cell: their `'\n'` has no cell left to share the
        // row with, which is the one case where C's incremental wrap disagrees with the wrap it
        // loads with. Text typed after such an Enter used to land inside the row above it, and a
        // pasted article came out of the editor with its lines merged.
        let paste: String = (0..3)
            .map(|i| format!("{}{}\n", "z".repeat(SCREEN_COLS - 1), i))
            .collect();
        keys(&client, paste.as_bytes());

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), paste.as_bytes());
        // Each filled row keeps a display line of its own for the newline, and the last Enter
        // opens the blank line the cursor is on.
        assert_eq!(data.total(), 7);
    }

    #[tokio::test]
    async fn an_enter_breaks_the_row_and_repaints_it() {
        let (mut term, mut session, mut client) = editor_pipe();
        // The `"\r\n"` pair is one Enter, as on the wire.
        keys(&client, b"hi\rthere\r");

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"hi\nthere\n");
        // An Enter ends the inner key loop, so the rows are painted again up to the status row,
        // which is the only place the port flushes. The text typed after the last Enter is not
        // on the screen yet — like C, `Ctrl-W` returns without repainting.
        let screen = painted(&mut client);
        assert!(screen.contains("hi"), "{screen}");
        assert!(screen.contains("there"), "{screen}");
    }

    #[tokio::test]
    async fn a_tab_fills_the_rest_of_its_stop() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, b"\tX");

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        let mut expected = vec![b' '; TAB_SIZE];
        expected.push(b'X');
        assert_eq!(saved(&data), expected);
    }

    #[tokio::test]
    async fn the_status_row_describes_the_cursor_and_the_document() {
        let (mut term, mut session, mut client) = editor_pipe();
        keys(&client, b"\x1b[4~");

        let mut data = doc("one\ntwo\n");
        edit(&mut term, &mut session, &mut data).await;
        let screen = painted(&mut client);
        // `[row;col] 第line/total行 [mode]` — the last display line of a newline-terminated
        // document is the empty row the cursor ends on.
        assert!(
            screen.contains("\x1b[1;44;33m[\x1b[32m3\x1b[33m;\x1b[32m1\x1b[33m] \
                             第\x1b[32m3\x1b[33m/\x1b[32m3\x1b[33m行 [\x1b[32m插入\x1b[33m]"),
            "{screen}"
        );
    }

    #[tokio::test]
    async fn backspace_at_the_start_of_a_row_joins_it_to_the_one_above() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, b"\x1b[B\x08");

        let mut data = doc("ab\ncd");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"abcd");
    }

    #[tokio::test]
    async fn backspace_at_the_very_top_does_nothing() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, b"a\x08\x08");

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"");
    }

    #[tokio::test]
    async fn ctrl_k_takes_the_rest_of_the_row() {
        let (mut term, mut session, client) = editor_pipe();
        // `abc`, one column back onto the `c`, then the block delete.
        keys(&client, b"abc\x1b[D\x0b");

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"ab");
    }

    #[tokio::test]
    async fn insert_switch_toggles_overwrite() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, b"\x1b[2~ac");

        let mut data = doc("xyz");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"acz");
    }

    #[tokio::test]
    async fn a_wide_character_takes_its_two_columns() {
        let (mut term, mut session, client) = editor_pipe();
        keys(&client, "中".as_bytes());

        let mut data = doc("");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), "中".as_bytes());
    }

    #[tokio::test]
    async fn a_wide_character_deletes_whole() {
        let (mut term, mut session, client) = editor_pipe();
        // Right onto the last column of `中`, then one backspace: the character goes as one.
        keys(&client, b"\x1b[C\x08");

        let mut data = doc("中x");
        edit(&mut term, &mut session, &mut data).await;
        assert_eq!(saved(&data), b"x");
    }

    #[tokio::test]
    async fn a_dropped_connection_ends_the_edit() {
        let (mut term, mut session, client) = editor_pipe();
        client.push(b"x");
        // Closing the client end is the end of stream the terminal reads.
        drop(client);

        let mut data = doc("");
        assert_eq!(edit(&mut term, &mut session, &mut data).await, KEY_NULL);
        // Whatever was typed before the hang-up stays in the document, as in C.
        assert_eq!(saved(&data), b"x");
    }
}
