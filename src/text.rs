//! Display-width aware text splitting, ported from `src/str_process.c`.
//!
//! The C original reads the global `UTF8_fixed_width`; because that value is chosen
//! per session (the `(V)变宽/(F)定宽` prompt) and the C server is fork-per-connection,
//! it is threaded explicitly through this module instead.

/// Cells consumed by a split, mirroring `split_line`'s out-parameters.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Split {
    /// Bytes consumed from the input, including the `\n` when `eol` is set.
    pub bytes: usize,
    /// Display cells consumed.
    pub display_len: usize,
    /// Split stopped on a newline rather than on the width limit.
    pub eol: bool,
}

/// Length of the UTF-8 sequence implied by a lead byte, reproducing the bit-shift loop
/// in `str_process.c:65-79`. Bytes >= 0x80 always start a sequence, so a stray
/// continuation byte yields 1, matching the C behaviour on malformed input.
pub fn seq_len(lead: u8) -> usize {
    let mut c = lead & 0xf0;
    let mut n = 0;
    while c & 0x80 != 0 {
        n += 1;
        c = (c & 0x7f) << 1;
    }
    n
}

/// Width in cells of the multi-byte sequence `buf[..n]`, or the fixed-mode fallback.
pub fn multibyte_width(buf: &[u8], n: usize, fixed: bool) -> usize {
    if fixed {
        return 2;
    }
    match std::str::from_utf8(&buf[..n]) {
        Ok(s) => match s.chars().next() {
            // `wcwidth` reports -1 for control characters; the C pager treated those as
            // invisible, so clamp to 0 instead of letting the column count go negative.
            Some(c) => unicode_width::UnicodeWidthChar::width(c)
                .unwrap_or(0)
                .max(0) as usize,
            None => 0,
        },
        // `mbstowcs` failure fallback in the C code is width 1 in variable mode.
        Err(_) => 1,
    }
}

/// `str_length` from `str_process.c:24`.
pub fn display_width(buf: &[u8], skip_ctrl_seq: bool, fixed: bool) -> usize {
    let mut total = 0usize;
    let mut i = 0usize;
    while i < buf.len() {
        let c = buf[i];
        if c == b'\r' || c == b'\x07' {
            i += 1;
            continue;
        }

        if skip_ctrl_seq && c == 0x1b && buf.get(i + 1) == Some(&b'[') {
            // Parameter bytes, then a terminator. Only `m` is treated as valid, other
            // letters are silently consumed, anything else is re-scanned (C: `i--`).
            let mut j = i + 2;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';' || buf[j] == b'?') {
                j += 1;
            }
            i = match buf.get(j) {
                Some(b'm') => j + 1,
                Some(b) if b.is_ascii_alphabetic() => j + 1,
                // Unrecognised terminator: the C code re-processes that byte as text.
                _ => j,
            };
            continue;
        }

        if c & 0x80 != 0 {
            let want = seq_len(c);
            let n = want.min(buf.len() - i).max(1);
            total += multibyte_width(&buf[i..i + n], n, fixed);
            i += n;
        } else {
            total += 1;
            i += 1;
        }
    }
    total
}

/// `split_line` from `str_process.c:104`. Returns the largest prefix whose display
/// width fits `max_display_len`, plus one extra byte when the prefix ends in `\n`.
pub fn split_line(buf: &[u8], max_display_len: usize, skip_ctrl_seq: bool, fixed: bool) -> Split {
    let mut i = 0usize;
    let mut display_len = 0usize;

    while i < buf.len() {
        let c = buf[i];
        if c == b'\r' || c == b'\x07' {
            i += 1;
            continue;
        }

        if skip_ctrl_seq && c == 0x1b && buf.get(i + 1) == Some(&b'[') {
            // Unlike `display_width`, this variant scans forward to the next `m` and
            // consumes it. With no `m` at all the C original runs to the end of the
            // string and swallows everything after the escape; the port clamps instead
            // so the returned byte count can never exceed the buffer.
            i += 2;
            while i < buf.len() && buf[i] != b'm' {
                i += 1;
            }
            i = (i + 1).min(buf.len());
            continue;
        }

        if c & 0x80 != 0 {
            let want = seq_len(c);
            let n = want.min(buf.len() - i).max(1);
            let w = multibyte_width(&buf[i..i + n], n, fixed);
            if display_len + w > max_display_len {
                break;
            }
            display_len += w;
            i += n;
        } else {
            if display_len + 1 > max_display_len {
                break;
            }
            display_len += 1;
            i += 1;
            // A newline is one cell wide here, unlike the web rendering.
            if c == b'\n' {
                return Split {
                    bytes: i,
                    display_len,
                    eol: true,
                };
            }
        }
    }

    Split {
        bytes: i,
        display_len,
        eol: false,
    }
}

/// Soft-wrap result: byte offset of every display line plus its width in cells.
#[derive(Debug, Clone, Default)]
pub struct Wrapped {
    /// `line_offsets[i]`..`line_offsets[i+1]` is display line `i`. Has `count + 1` entries.
    pub offsets: Vec<u64>,
    /// Display cells per line, `count` entries.
    pub widths: Vec<u32>,
}

impl Wrapped {
    pub fn count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn line<'a>(&self, buf: &'a [u8], index: usize) -> &'a [u8] {
        let start = self.offsets[index] as usize;
        let end = self.offsets[index + 1] as usize;
        &buf[start..end]
    }
}

/// `split_data_lines` from `str_process.c:190`, capped at `max_lines` display lines.
pub fn split_data_lines(
    buf: &[u8],
    max_display_len: usize,
    skip_ctrl_seq: bool,
    fixed: bool,
    max_lines: usize,
) -> Wrapped {
    let mut offsets: Vec<u64> = vec![0];
    let mut widths: Vec<u32> = Vec::new();
    let mut pos = 0usize;

    loop {
        let rest = &buf[pos..];
        let split = split_line(rest, max_display_len, skip_ctrl_seq, fixed);
        widths.push(split.display_len as u32);

        // `max_lines` counts display lines, `offsets` one more entry than that; the saturating
        // add is what lets a caller ask for "no cap" with `usize::MAX`.
        //
        // The width of the line we are about to reject was already pushed, so the truncate is the
        // port of `336b670`: C wrote `p_line_widths[line_cnt]` before this check and left a width
        // describing a line the returned count excludes.
        if offsets.len() >= max_lines.saturating_add(1) {
            widths.truncate(offsets.len() - 1);
            break;
        }

        pos += split.bytes;
        offsets.push(pos as u64);

        let exhausted = pos >= buf.len();
        if exhausted && !split.eol {
            break;
        }
    }

    Wrapped { offsets, widths }
}

/// `str_filter` from `str_process.c:224`, in place over a byte buffer.
pub fn str_filter(buf: &mut Vec<u8>, skip_ctrl_seq: bool) {
    let src = std::mem::take(buf);
    buf.reserve(src.len());
    let mut i = 0usize;
    while i < src.len() {
        let c = src[i];
        if c == b'\r' || c == b'\x07' {
            i += 1;
            continue;
        }
        if skip_ctrl_seq && c == 0x1b && src.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < src.len() && src[i] != b'm' {
                i += 1;
            }
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
}

/// Truncate to at most `max_display_len` cells without splitting a wide character.
pub fn truncate_display(buf: &[u8], max_display_len: usize, fixed: bool) -> &[u8] {
    let split = split_line(buf, max_display_len, false, fixed);
    &buf[..split.bytes]
}

/// Pad or truncate `text` to exactly `cells` display columns.
pub fn fit_display(text: &[u8], cells: usize, fixed: bool) -> Vec<u8> {
    let mut out = truncate_display(text, cells, fixed).to_vec();
    let used = display_width(&out, false, fixed);
    for _ in used..cells {
        out.push(b' ');
    }
    out
}

#[cfg(test)]
#[path = "../tests/test_text.rs"]
mod tests;
