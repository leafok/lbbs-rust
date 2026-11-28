/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The full-screen text editor, ported from `src/editor.c`.
//!
//! C keeps two views of the same text: the *data lines* it edits in place and the *display
//! lines* those bytes wrap into at [`SCREEN_COLS`]. A display line is a slice of a data line,
//! so every edit re-splits the line it touched and slides the display array behind it. Rust
//! owns its buffers, so [`EditorData`] holds the data lines as byte vectors and the wrap of
//! each one beside it; the display-line numbering [`editor_display`] works in is derived from
//! the two, and an edit re-wraps only the line it changed ([`EditorData::recount_line`])
//! instead of the whole document: re-splitting every line after every character made a pasted
//! article quadratic in its size, while C only re-splits the line it touched (`:467-468`).
//! [`EditorData::recount`] is what loading does, and what an edit falls back to where the
//! display-line cap makes the lines depend on each other. Everything the screen half of C reads
//! — `display_line_lengths`, `display_line_widths`, the `split_line` that turns a column into an
//! offset — has a method here that returns the same number.
//!
//! A data line is the text up to and including its `'\n'`; only the last one may lack it, and
//! [`EditorData::load`] adds an empty line after a buffer that ends with one, which is what
//! makes the cursor land on a blank row after the last line of an existing post.
//!
//! # Deviations from C
//!
//! * The two memory pools (`editor_memory_pool_init`, `:37-75`) are gone. What they bought
//!   was a fixed 1024-byte stride per line, and C keeps that stride honest in three places the
//!   port has no counterpart for: a loaded line leaves two bytes of its block free for the
//!   line feed and terminator an insert at its end writes (`:120-124`), cleanup releases every
//!   block a logical line longer than one block spans (`:234-244`), and a load that runs out of
//!   blocks publishes the rows it filled before it cleans up (`:144-146`). All three are
//!   properties of the packing, not of the text.
//! * Since `dd5b5d1` the loader itself breaks a text line that no longer fits a block at each
//!   boundary, leaving a `'\n'` behind where it cut (`:103-114`, `:126-137`), so a line that
//!   ends on a line feed is already several data lines by the time C edits it;
//!   `break_over_long_lines` makes the same cut in [`EditorData::load`]. The bytes saved agree,
//!   and so do the rows, because a load re-splits the whole text on both sides. What differs is
//!   a line that ends without a line feed — only the unterminated tail of a loaded article can
//!   be one — which stays a single entry here where C spans several blocks, where
//!   `len_data_line` counts within the block of the cursor only (`is_data_line_end`,
//!   `:262-282`, and `:604-630`), so the split and merge guards measure that one document
//!   differently.
//! * C moves the cursor to the display line it re-wrote and returns it (`:765-770`). The port's
//!   rows are slices of a buffer it owns, so the cursor is already on the row the edit left.
//! * The cap on display lines is applied the way C applies it: [`EditorData::load`] drops what
//!   does not fit in the [`MAX_EDITOR_DATA_LINES`] entries of its array (`:81, :100`) and an edit
//!   into a document that already fills it loses the text that needs a row the array has not
//!   got (`:472-493`, `:553-564`). One case is not reproduced: a deletion that joins the last
//!   line of a full array leaves C with one row more than the document has — a tail entry it
//!   no longer maintains, which paints blank where the port paints the row that is really
//!   there. The saved bytes agree either way.
//! * `:543-550`, which moves the cursor to the next display line when an insert ran past the
//!   end of the current one, is kept; it is what makes typing at the end of a line feel like
//!   the text is pushing the cursor along.
//! * C maintains its display array as it edits, and the maintenance disagrees with the loader
//!   in one case: an Enter that ends a row already [`SCREEN_COLS`] wide glues that row's `'\n'`
//!   to the row itself (`:397-399`, `:433-435` set it to one byte longer and re-split only the
//!   text behind the cursor), while [`crate::text::split_data_lines`], what [`EditorData::recount`]
//!   runs over, gives that newline a row of its own. [`EditorData::insert`] follows the edit:
//!   [`EditorData::glue_row_line_feed`] folds the line feed back into the row the cursor stood
//!   on, so the cursor drops one row and the screen shows what C shows. The array is
//!   history-dependent on both sides — the same bytes reloaded gain that row back.

use std::time::Duration;

use crate::paths::{DATA_EDITOR_HELP, MAX_USER_IDLE_TIME, SCREEN_COLS, SCREEN_ROWS, TAB_SIZE};
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
/// its `'\n'` included. Growing past it forces a hard break (`editor.c:346`, `:364`).
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// `editor_data_load` (`:77-185`).
    ///
    /// C wraps the whole buffer and packs the display lines back into 1024-byte data lines;
    /// the partition is the same as splitting the buffer after each `'\n'`, except that a
    /// buffer ending in a newline gets one more, empty, data line — the display line the C
    /// `do/while` produces after the last `'\n'` becomes its data line during packing
    /// (`:122-152`).
    pub fn load(data: &[u8], fixed: bool) -> EditorData {
        // `:81, :100`: the wrap is computed into an array of [`MAX_EDITOR_DATA_LINES`] display
        // lines and only what fits in it is packed into data lines (`:116-180`), so the text
        // past the cap is not merely hidden — it is not in the editor, and a `'\n'` left as the
        // array's last byte is trimmed off it (`:171-178`).
        let wrapped =
            text::split_data_lines(data, SCREEN_COLS, false, fixed, MAX_EDITOR_DATA_LINES);
        let rows = wrapped.count();
        let mut end = wrapped.offsets[rows] as usize;
        // C's `:171-178` trims the `'\n'` off the array's last display line, which only bites
        // when the last row that fits in the cap is a terminated one: a buffer that ends in a
        // `'\n'` keeps its empty row after it, and that row has nothing to trim.
        if wrapped.offsets[rows] > wrapped.offsets[rows - 1] && data[..end].ends_with(b"\n") {
            end -= 1;
        }
        let data = &data[..end];

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

        // `:161-168`, tab to single space: the editor has no tab stops of its own, and a
        // typed tab already expands on the way in (`editor_display`).
        for line in lines.iter_mut() {
            for byte in line.iter_mut() {
                if *byte == b'\t' {
                    *byte = b' ';
                }
            }
        }

        // `:103-114, :126-137` (`dd5b5d1`): the loader packs display rows into data lines of
        // [`MAX_EDITOR_DATA_LINE_LENGTH`] bytes, and where a row no longer fits it leaves a line
        // feed behind in the data line it is finishing. Only up to the last row that already ends
        // on one: the tail of a buffer that has no line feed of its own keeps its boundaries
        // empty. The injected bytes are part of the document from then on, saved with it.
        let lines = break_over_long_lines(lines, SCREEN_COLS, fixed);

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

    /// `editor_data_save` (`:188-215`). `None` is C's `-2`: the destination buffer was too
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

    /// `editor_data_insert` (`:292-567`) for the bytes at the cursor.
    ///
    /// `display_line` and `offset` are the cursor in C's coordinates — a display line and a
    /// byte offset inside it — and come back on the slot C's edit leaves the text cursor at,
    /// which is what lets [`editor_display`] keep its `row_pos`/`col_pos` arithmetic.
    /// `last_updated_line` is the redraw bound the caller carries from edit to edit; C's
    /// `MAX()` over it (`:541`) is reproduced, so a line that grows repaints everything below.
    pub fn insert(
        &mut self,
        display_line: &mut i64,
        offset: &mut i64,
        bytes: &[u8],
        last_updated_line: &mut i64,
    ) -> Result<(), OverLines> {
        let (line, seg) = self.locate(*display_line);
        let segment = self.segments[line][seg];
        let old_count = self.segments[line].len();
        let pos = segment.start + (*offset).clamp(0, segment.len as i64) as usize;
        let len_data_line = self.lines[line].len();
        let old_total = self.total as i64;
        let free = MAX_EDITOR_DATA_LINES.saturating_sub(old_total as usize);
        let cr = bytes == b"\r";
        // A carriage return is not text: it ends the line, and the cursor starts the next one.
        let inserted: &[u8] = if cr { &[] } else { bytes };
        let str_len = inserted.len();

        // `editor.c:308, :340` (`ae52b6a`): whether the line ends on a line feed decides how much
        // room a break at its end still needs — one byte for the terminator when the line feed is
        // already there, two when the break has to write one.
        let lf_ending = self.lines[line].ends_with(b"\n");
        let reserve = if lf_ending && pos < len_data_line {
            1
        } else {
            2
        };

        // `:346`, `:364`: the line is full, so the text is broken at the cursor and the rest
        // of it moves to a new data line. C picks the side of the break by whether the
        // insert itself still fits in the current line.
        if len_data_line + bytes.len() + reserve > MAX_EDITOR_DATA_LINE_LENGTH || cr {
            if self.total >= MAX_EDITOR_DATA_LINES {
                tracing::debug!(
                    "Split line error, display_line_total({}) reach limit({MAX_EDITOR_DATA_LINES})",
                    self.total
                );
                return Err(OverLines);
            }

            // C chooses the side of the break by whether the insert still fits in the line it
            // is typed into (`:364`).
            let break_reserve = if lf_ending { 1 } else { 2 };
            let break_before_insert =
                pos + bytes.len() + break_reserve >= MAX_EDITOR_DATA_LINE_LENGTH || cr;
            let mut head = self.lines[line][..pos].to_vec();
            let mut tail = if break_before_insert {
                [&inserted[..], &self.lines[line][pos..]].concat()
            } else {
                head.extend_from_slice(inserted);
                self.lines[line][pos..].to_vec()
            };
            // Both halves of a break are terminated lines; the tail keeps the `'\n'` the line
            // it came from had, if it had one — except where the cursor is at the very end of a
            // line that already ends on one (`:385-393`, `:421-429`). A carriage return asked for
            // at that place does want the empty line, and C gives it the new data line's own
            // `'\n'`.
            let keeps_its_line_feed = break_before_insert
                && lf_ending
                && pos == len_data_line
                && *offset >= segment.len as i64;
            if keeps_its_line_feed {
                if cr {
                    tail = vec![b'\n'];
                }
            } else {
                head.push(b'\n');
            }

            *offset = if break_before_insert {
                str_len as i64
            } else {
                (pos - segment.start + str_len) as i64
            };

            self.lines[line] = head;
            self.lines.insert(line + 1, tail);
            if self.segments.len() >= line + 1 {
                self.segments.insert(line + 1, Vec::new());
            }
            self.recount_line(line);
            if !keeps_its_line_feed {
                self.glue_row_line_feed(line);
            }
            self.recount_line(line + 1);

            if break_before_insert {
                // `:402`: C leaves the cursor on the display line after the head's last one, which
                // is the first row of the data line the break opened.
                *display_line = self.first_display(line + 1);
            }
            // The tail gets the rows the head did not use plus the ones the array still had.
            let allowed = old_count.saturating_sub(self.segments[line].len()) + free;
            let cut = self.cut_overflow(line + 1, allowed, display_line, offset);
            // `:451`: the redraw reaches as far as the document used to.
            *last_updated_line = old_total.max(self.last_display(line + 1) + cut as i64);
        } else {
            // `:455-456`: the insert goes into the line at the cursor.
            self.lines[line].splice(pos..pos, inserted.iter().copied());
            // `:464`, before the re-split: the clamp below belongs to the offset the insert left
            // behind, not to the one it started from.
            *offset += str_len as i64;
            self.recount_line(line);
            let cut = self.cut_overflow(line, old_count + free, display_line, offset);
            // `:523`, `:541`: rows that appeared shove every row below them down, so the redraw
            // has to reach as far as the document used to end; one that only re-split inside the
            // line stops at the line's last row.
            let resplit = (*last_updated_line).max(self.last_display(line) + cut as i64);
            *last_updated_line = if (self.total as i64) > old_total {
                old_total.max(resplit)
            } else {
                resplit
            };
        }

        // `:543-550`: an insert that ran to the end of its display line puts the cursor on
        // the next one.
        let length = self.display_line_length(*display_line) as i64;
        if *offset >= length && *display_line + 1 < self.total as i64 {
            *offset -= length;
            *display_line += 1;
        }

        self.trim_last_row(display_line, offset);

        Ok(())
    }

    /// The line feed a break writes into the row the cursor stood on, `:397-399` and `:433-435`.
    ///
    /// A row that is filled to the last cell has no room left for that line feed, so the wrap of
    /// the head puts it on a row of its own — the shape the same bytes take once they are loaded
    /// again (`editor_data_load` splits them from scratch). The keystroke does not: C never
    /// re-splits the head, it only re-measures the width of the row it just extended (`:443-444`),
    /// so the head keeps the rows it had. Folding the line feed back into the row above gives the
    /// same array, which is what decides how far the cursor drops.
    fn glue_row_line_feed(&mut self, line: usize) {
        let Some(last) = self.segments[line].last().copied() else {
            return;
        };
        if self.segments[line].len() < 2
            || last.len != 1
            || self.lines[line].get(last.start) != Some(&b'\n')
        {
            return;
        }
        self.segments[line].pop();
        self.segments[line].last_mut().unwrap().len += 1;
        self.total -= 1;
    }

    /// `editor_data_delete` (`:569-772`), one character or the rest of the display line.
    ///
    /// `exclude_lf` is C's `ae52b6a` argument: `Ctrl-K` asks for the text of the row and leaves
    /// its line feed, so the row stays as an empty one (`:1041-1044`), while `Ctrl-Y` takes the
    /// line feed too and the line goes away.
    ///
    /// The return is C's: the bytes removed, `0` when the cursor was already at the end of
    /// the line and nothing could go.
    pub fn delete(
        &mut self,
        display_line: &mut i64,
        offset: &mut i64,
        last_updated_line: &mut i64,
        del_line: bool,
        exclude_lf: bool,
    ) -> i64 {
        let (line, seg) = self.locate(*display_line);
        let segment = self.segments[line][seg];
        let old_count = self.segments[line].len();
        let parallel = self.segments.len() == self.lines.len();
        let pos = segment.start + (*offset).clamp(0, segment.len as i64) as usize;
        let len_data_line = self.lines[line].len();
        let old_total = self.total as i64;
        let row_ends_lf =
            segment.len > 0 && self.lines[line][segment.start + segment.len - 1] == b'\n';

        // `:632`: at the end of the data line there is nothing to delete.
        if pos >= len_data_line {
            return 0;
        }

        // `:592-602`: a cursor at the end of a display line whose logical line goes on in the
        // next 1024-byte block is moved to the start of that block, where the character is. The
        // port keeps a logical line in one buffer, so `pos` already sits on the character and
        // there is no block to move into — see the module's deviations.
        // `:637-665`: how much the one deletion takes away.
        let str_len = if del_line {
            let rest = segment.len as i64 - (*offset).clamp(0, segment.len as i64);
            // `:641-645`: `Ctrl-K` leaves the line feed, and with it the row.
            if exclude_lf && rest > 0 && row_ends_lf {
                rest - 1
            } else {
                rest
            }
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

        // `:668-704`: the deletion reaches the end of the line, so the next line is pulled up
        // into this one instead of a plain cut.
        let reaches_end = pos + str_len as usize > len_data_line
            || (pos + str_len as usize == len_data_line
                && self.lines[line][if del_line { len_data_line - 1 } else { pos }] == b'\n');

        let mut allowed = old_count;
        if reaches_end {
            if line + 1 >= self.lines.len() {
                // Last line: if deleting the whole line, remove it; otherwise nothing to do.
                if del_line && pos == 0 {
                    self.lines.remove(line);
                    if parallel {
                        let rows = self.segments.remove(line).len();
                        self.total -= rows;
                    }
                    if line >= self.lines.len() && !self.lines.is_empty() {
                        *display_line -= 1;
                    }
                    self.recount_line(line.saturating_sub(1));
                    *last_updated_line = self.last_display(line.saturating_sub(1));
                    return str_len;
                }
                return 0;
            }
            let next_len = self.lines[line + 1].len();
            // `editor.c:692-694`: the merged line keeps the room a split of it at the end needs
            // for its terminator — and for the line feed, unless the line behind it already ends
            // on one and that byte comes along in the copy (`ae52b6a`, undoing `6dd6846`).
            let next_lf_ending = self.lines[line + 1].ends_with(b"\n");
            if pos + next_len + if next_lf_ending { 1 } else { 2 } > MAX_EDITOR_DATA_LINE_LENGTH {
                return 0;
            }
            let next = self.lines.remove(line + 1);
            if parallel {
                let rows = self.segments.remove(line + 1).len();
                self.total -= rows;
                // `:715-718`: the merged line keeps the rows both lines used to take.
                allowed += rows;
            }
            let mut head = self.lines[line][..pos].to_vec();
            head.extend_from_slice(&next);
            self.lines[line] = head;
        } else {
            self.lines[line].splice(pos..pos + str_len as usize, []);
        }

        self.recount_line(line);
        // `editor.c:714-734`: C re-splits the changed data line from the edited display row, not
        // from its head, so a row the cursor was sitting on survives as a real zero-width row when
        // the deletion leaves nothing after it. Re-wrapping from the head drops that row, the
        // cursor's display line then runs past the end of the document, and the next keystroke is
        // located back onto row 0 — which is what made typing corrupt the first line.
        if pos == self.lines[line].len()
            && pos > 0
            && !self.lines[line].ends_with(b"\n")
            && self.segments[line].last().is_some_and(|s| s.len != 0)
        {
            self.segments[line].push(Segment {
                start: pos,
                len: 0,
                width: 0,
            });
            self.total += 1;
        }
        if parallel {
            self.cut_rows(line, allowed);
        }
        // `:736-762`: the redraw reaches the last display line of the line that changed, or
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
    /// instead (`:534-537`), so the empty tail of a terminated line is dropped here — but only
    /// when the split got far enough to produce it, which the display-line budget below can cut
    /// off.
    ///
    /// C splits the whole buffer into one display array of [`MAX_EDITOR_DATA_LINES`] entries
    /// (`:81, :100`), so a document longer than that keeps only its first 65536 display lines;
    /// the budget is what carries that into the per-line re-split.
    fn recount(&mut self) {
        self.segments.clear();
        self.total = 0;
        for line in 0..self.lines.len() {
            let budget = MAX_EDITOR_DATA_LINES.saturating_sub(self.total);
            if budget == 0 {
                tracing::debug!(
                    "Line count {} reaches limit {}",
                    self.total,
                    MAX_EDITOR_DATA_LINES
                );
                self.segments.push(Vec::new());
                continue;
            }
            let mut segments = Vec::new();
            self.wrap_line(line, budget, &mut segments);
            self.total += segments.len();
            self.segments.push(segments);
        }
    }

    /// The wrap of data line `line`, at most `budget` display rows, written into `out`.
    fn wrap_line(&self, line: usize, budget: usize, out: &mut Vec<Segment>) {
        let bytes = &self.lines[line];
        let wrapped = text::split_data_lines(bytes, self.cols, false, self.fixed, budget);
        out.clear();
        out.extend((0..wrapped.count()).map(|index| Segment {
            start: wrapped.offsets[index] as usize,
            len: (wrapped.offsets[index + 1] - wrapped.offsets[index]) as usize,
            width: wrapped.widths[index] as usize,
        }));
        if bytes.last() == Some(&b'\n') && out.last().is_some_and(|s| s.len == 0) {
            out.pop();
        }
        if out.is_empty() {
            out.push(Segment::default());
        }
    }

    /// Re-wrap one data line in place, leaving the rest of the array as it is.
    ///
    /// C only ever re-splits the line it edited (`:467-468`, over at most
    /// [`MAX_EDITOR_DATA_LINE_LENGTH`] bytes), while a whole-document [`recount`] after every
    /// character made a pasted article quadratic in its size. Whether the re-wrap still fits the
    /// display array is the caller's business: [`EditorData::cut_overflow`] is what C does when it
    /// does not.
    fn recount_line(&mut self, line: usize) {
        if self.segments.len() != self.lines.len() {
            self.recount();
            return;
        }

        let mut segments = Vec::new();
        self.wrap_line(line, usize::MAX, &mut segments);
        self.total = self.total + segments.len() - self.segments[line].len();
        self.segments[line] = segments;
    }

    /// `:472-493`: the display array is [`MAX_EDITOR_DATA_LINES`] rows long, so an edit only
    /// gets the rows the array still has plus the ones its data line already occupied. What the
    /// re-split needs beyond that is lost: C terminates the last row the line can keep at
    /// [`SCREEN_COLS`] - 1 cells with a `'\n'`, drops the text after it and pulls the cursor back
    /// inside that row. `true` is the redraw bound C then uses one row past the line's last.
    fn cut_overflow(
        &mut self,
        line: usize,
        allowed: usize,
        display_line: &mut i64,
        offset: &mut i64,
    ) -> bool {
        if allowed == 0 || self.segments[line].len() <= allowed {
            return false;
        }

        let rows = self.segments[line].len();
        let row = self.segments[line][allowed - 1];
        let end = self.segments[line][allowed].start;
        let cut = text::split_line(
            &self.lines[line][row.start..end],
            self.cols - 1,
            false,
            self.fixed,
        );
        self.lines[line].truncate(row.start + cut.bytes);
        self.lines[line].push(b'\n');
        // `:485-486`: the terminating `'\n'` joins the row the cut left, it does not open
        // another one — that is how C keeps the array's row count while dropping the text.
        self.segments[line].truncate(allowed);
        let last = &mut self.segments[line][allowed - 1];
        last.len = cut.bytes + 1;
        last.width = cut.display_len;
        self.total = self.total - rows + allowed;

        // `:488-491`.
        let length = self.display_line_length(*display_line) as i64;
        if *offset >= length {
            *offset = length - 1;
        }
        true
    }

    /// `:715`: a deletion re-splits from the edited row with a budget of the rows the line
    /// already had, so a shorter line never pushes the display array past
    /// [`MAX_EDITOR_DATA_LINES`]: what does not fit is dropped from the document, without the
    /// `'\n'` glue [`EditorData::cut_overflow`] adds.
    fn cut_rows(&mut self, line: usize, allowed: usize) {
        if allowed == 0 || self.segments[line].len() <= allowed {
            return;
        }

        let last = self.segments[line][allowed - 1];
        self.lines[line].truncate(last.start + last.len);
        self.recount_line(line);
    }

    /// `:553-564`: whatever the last row of a full display array cannot show in
    /// [`SCREEN_COLS`] - 1 cells is cut off the document, its `'\n'` included, and the cursor is
    /// clamped onto what is left. C runs this after every insert that leaves the array full, so
    /// editing anywhere in such a document costs the text at the very end of it.
    fn trim_last_row(&mut self, display_line: &mut i64, offset: &mut i64) {
        if self.total != MAX_EDITOR_DATA_LINES {
            return;
        }

        let line = self
            .segments
            .iter()
            .rposition(|segments| !segments.is_empty())
            .unwrap_or(0);
        let seg = self.segments[line].len() - 1;
        let segment = self.segments[line][seg];
        let cut = text::split_line(
            &self.lines[line][segment.start..segment.start + segment.len],
            self.cols - 1,
            false,
            self.fixed,
        );
        self.lines[line].truncate(segment.start + cut.bytes);
        self.segments[line][seg] = Segment {
            start: segment.start,
            len: cut.bytes,
            width: cut.display_len,
        };

        // `:559-563`.
        if *display_line + 1 >= self.total as i64 {
            *offset = (*offset).min(cut.bytes as i64);
            *display_line = self.total as i64 - 1;
        }
    }
}

/// `:103-114, :126-137` (`dd5b5d1`): the line feeds that the loader's block packing leaves
/// behind where it cuts a text line that has a line feed of its own.
fn break_over_long_lines(lines: Vec<Vec<u8>>, cols: usize, fixed: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // A line that fits a block with the two bytes the packing keeps free never crosses a
        // boundary, and the unclosed tail of the buffer has no line feed to be copied from.
        if !line.ends_with(b"\n") || line.len() + 2 <= MAX_EDITOR_DATA_LINE_LENGTH {
            out.push(line);
            continue;
        }

        let wrapped = text::split_data_lines(&line, cols, false, fixed, usize::MAX);
        let rows = wrapped.count();
        let mut piece = 0usize;
        let mut current = 0usize;
        for row in 0..rows {
            let len = (wrapped.offsets[row + 1] - wrapped.offsets[row]) as usize;
            if current > 0 && current + len + 2 > MAX_EDITOR_DATA_LINE_LENGTH {
                let at = wrapped.offsets[row] as usize;
                let mut broken = line[piece..at].to_vec();
                broken.push(b'\n');
                out.push(broken);
                piece = at;
                current = 0;
            }
            current += len;
        }
        out.push(line[piece..].to_vec());
    }
    out
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

/// `SCREEN_ROWS - screen_begin_row` (`:717`).
const SCREEN_ROW_TOTAL: i64 = SCREEN_ROWS as i64 - SCREEN_BEGIN_ROW;

/// `EDITOR_CTX` (`include/editor.h:30-36`).
#[derive(Debug, Default, Clone)]
pub struct EditorCtx {
    /// `msg`: the right half of the status row.
    pub msg: String,
    /// `line_cursor`: the line number the status row shows.
    pub line_cursor: i64,
}

/// `editor_display_key_handler` (`:774-789`).
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
    /// The screen rows a keystroke of the current batch left waiting for a paint pass: the
    /// delete path blanks them, and only the paint loop puts the text back.
    stale_from: i64,
    stale_through: i64,
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
            stale_from: SCREEN_BEGIN_ROW,
            stale_through: SCREEN_BEGIN_ROW - 1,
        }
    }

    /// Widen the band about to be painted to the rows an earlier keystroke of the same batch
    /// left blank, keeping the `line_current` ↔ `output_current_row` mapping intact. C lets the
    /// later, narrower band win, so the rows the first keystroke cleared are never painted
    /// again: the delete looks like it did nothing and the following rows stay missing.
    fn cover_stale(&mut self) {
        if self.stale_through < self.stale_from {
            return;
        }
        let raise = self.output_current_row - self.stale_from;
        if raise > 0 && self.line_current >= raise {
            self.line_current -= raise;
            self.output_current_row = self.stale_from;
        }
        self.output_end_row = self.output_end_row.max(self.stale_through);
    }

    /// The band this keystroke changed now waits for the paint pass.
    fn note_stale(&mut self) {
        self.stale_from = self.output_current_row;
        self.stale_through = self.output_end_row;
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

    /// `:1189-1228`, `KEY_LEFT`: one character back, or the last column of the row above.
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

    /// `:1229-1247`, `KEY_UP`.
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

    /// `:1248-1292`, `KEY_RIGHT`: one character on, or the first column of the row below.
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

    /// `:1293-1314`, `KEY_DOWN`: one row down, scrolling the window when it runs off.
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

    /// The repaint C asks for after `Ctrl-Q` help or `KEY_F5` (`:1355-1361`).
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

/// The status row, `:837-854`: cursor position, line number, insert mode and the message the
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

/// `editor_display` (`:791-1426`): the full-screen edit of `data`.
///
/// The return is C's last key — `Ctrl-W`/`Ctrl-X` when the user left the way they should,
/// [`KEY_NULL`] or [`KEY_TIMEOUT`] when the connection or the idle limit ended it. C's
/// callers ignore it and ask their own question afterwards, which is what
/// [`crate::post`] does too; the value is kept because a port that can tell a saved body from
/// a dropped connection is worth the extra return.
///
/// # Deviations from C
///
/// * `show_help` (`:793`) was a process-wide `static int` guarding the help pager against
///   re-entering the editor. One task owns one editor here, so the flag has nothing to
///   guard and the refresh C falls through to is done directly.
/// * `:1392-1396` catches a negative display-line length. Rust cannot produce one.
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
    // The key that ends the edit, kept until the paint pass it interrupted is finished.
    let mut exit_key: Option<u32> = None;

    clr_rows(term, SCREEN_BEGIN_ROW, SCREEN_ROWS as i64);
    key_handler(&mut ch, &mut ctx, data.fixed);

    while !session.state.is_exiting() {
        // The text area is behind when the window has run out of rows to paint.
        if view.line_current >= data.total() as i64 || view.output_current_row > view.output_end_row
        {
            if view.output_current_row > view.output_end_row {
                view.stale_from = SCREEN_BEGIN_ROW;
                view.stale_through = SCREEN_BEGIN_ROW - 1;
            }
            ctx.line_cursor = view.line_current - view.output_current_row + view.row_pos + 1;
            let status = status_line(data, &view, &ctx, key_insert);
            term.moveto(SCREEN_ROWS, 0);
            term.print_bytes(&status);
            term.moveto_signed(view.row_pos as i32, view.col_pos as i32);
            let _ = term.flush().await;

            // An exit key ends the edit only after the rows it left unpainted are drawn, so the
            // text the last keystroke changed is on the screen when control goes back. C stops at
            // `:1117-1120` and leaves them blank.
            if let Some(ch) = exit_key {
                return ch;
            }

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
                // `igetch(100)` calls (`:882-906`).
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
                        // `:931-934`: overwrite mode deletes the character it is replacing, line
                        // feed included.
                        data.delete(
                            &mut display_line_out,
                            &mut offset_out,
                            &mut view.last_updated_line,
                            false,
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
                        view.output_end_row = (SCREEN_ROWS as i64 - 1).min(
                            view.output_current_row + view.last_updated_line - view.line_current,
                        );
                        view.line_current -= view.output_current_row - view.row_pos;
                        view.output_current_row = view.row_pos;

                        let scroll_rows = (display_line_out
                            - display_line_in
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
                        view.cover_stale();
                        view.note_stale();
                    }

                    if display_line_out != display_line_in {
                        break;
                    }

                    if ch == b' ' as u32 && tab_width > 0 {
                        tab_width -= 1;
                    } else {
                        // Keep reading while the client is still sending; paint as soon as it
                        // stops (`:1003-1007`).
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
                    let mut exclude_lf = false;

                    if ch == BACKSPACE {
                        // At the very top of the document there is nothing back to.
                        if view.cursor_line() <= 0 && view.col_pos <= 1 {
                            break;
                        }
                        let display_len =
                            data.col_to_offset(view.cursor_line(), view.col_pos).1 as i64;
                        view.col_pos = display_len;
                        // `:1037-1040`: the cursor is on the first cell, so it backs onto the
                        // last cell of the row above. `row_pos` is already decremented when C
                        // reads the width, so it reads the row the cursor just landed on.
                        if view.col_pos < 1 && view.cursor_line() >= 0 {
                            view.row_pos -= 1;
                            view.col_pos =
                                data.display_line_width(view.cursor_line()).max(1) as i64;
                        }
                    } else if ch == KEY_CTRL_K {
                        // `:1041-1044`: `Ctrl-K` kills the text of the row and keeps its line
                        // feed, so the row is left standing empty.
                        del_line = true;
                        exclude_lf = true;
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
                        exclude_lf,
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
                        // to it (`:1077-1087`).
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

                        view.cover_stale();
                        clr_rows(term, view.output_current_row, view.output_end_row);
                        view.note_stale();
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
                        exit_key = Some(ch);
                        break;
                    }
                    KEY_TIMEOUT => {
                        tracing::debug!("User input timeout");
                        exit_key = Some(ch);
                        break;
                    }
                    KEY_CTRL_W | KEY_CTRL_X => {
                        exit_key = Some(ch);
                        break;
                    }
                    KEY_CTRL_S | KEY_CTRL_LEFT => view.col_pos = 1,
                    KEY_CTRL_E | KEY_CTRL_RIGHT => {
                        if view.cursor_line() == data.total() as i64 - 1 {
                            view.col_pos = data.display_line_width(view.cursor_line()) as i64 + 1;
                        } else {
                            view.col_pos =
                                data.display_line_width(view.cursor_line()).max(1) as i64;
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
                            view.col_pos = data.display_line_width(view.cursor_line()) as i64 + 1;
                        } else {
                            view.line_current = data.total() as i64 - SCREEN_ROW_TOTAL;
                            view.output_current_row = SCREEN_BEGIN_ROW;
                            view.output_end_row = SCREEN_ROWS as i64 - 1;
                            view.row_pos = SCREEN_ROWS as i64 - 1;
                            view.col_pos = data.display_line_width(view.cursor_line()) as i64 + 1;
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

        // One display line per row, escapes shown as stars (`:1386-1420`).
        let mut len = data.display_line_length(view.line_current);
        if len >= MAX_EDITOR_DATA_LINE_LENGTH {
            tracing::error!("Buffer overflow: len={len} line={} ", view.line_current);
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
#[path = "../tests/test_editor.rs"]
mod tests;
