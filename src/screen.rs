//! Screen primitives ported from `src/screen.c`: cursor control, the pager, the header
//! and footer rows, and the two line editors.
//!
//! Output is byte-faithful where the original has quirks callers depend on: zero-based
//! `moveto` columns, `%*s` widths that can go negative, the pager's bare `\n` instead of
//! a scroll sequence, and the double column charge when deleting a wide character.

use std::time::Duration;

use chrono::{Datelike, Timelike};

use crate::paths::{
    DATA_READ_HELP, LINE_BUFFER_LEN, MAX_SPLIT_FILE_LINES, MAX_USER_IDLE_TIME, SCREEN_COLS,
    SCREEN_ROWS,
};
use crate::term::key::{
    self, BACKSPACE, CR, KEY_DEL, KEY_DOWN, KEY_END, KEY_ESC, KEY_HOME, KEY_LEFT, KEY_NULL,
    KEY_PGDN, KEY_PGUP, KEY_RIGHT, KEY_SPACE, KEY_TIMEOUT, KEY_UP,
};
use crate::term::{Terminal, truncate_cells};
use crate::text;

pub const CTRL_SEQ_CLR_LINE: &str = "\x1b[K";

/// `MSG_EXT_MAX_LEN` from `include/screen.h`.
pub const MSG_EXT_MAX_LEN: usize = 400;

/// The pager's help key.
const KEY_HELP: u32 = b'h' as u32;

/// `show_top` charges half of each field buffer as its width budget.
const TOP_LEFT_CELLS: usize = 40;
const TOP_MIDDLE_CELLS: usize = 20;
const TOP_RIGHT_CELLS: usize = 40;

const SCREEN_BEGIN_ROW: i32 = 1;

/// `%*s` with a negative width prints nothing.
fn spaces(width: i64) -> String {
    if width > 0 {
        " ".repeat(width as usize)
    } else {
        String::new()
    }
}

/// `clrline`.
pub fn clrline(term: &mut Terminal, line_begin: usize, line_end: usize) {
    for i in line_begin..=line_end {
        term.moveto(i, 0);
        term.print(CTRL_SEQ_CLR_LINE);
    }
}

/// `clrtobot`.
pub fn clrtobot(term: &mut Terminal, line_begin: usize) {
    term.moveto(line_begin, 0);
    term.print("\x1b[J");
    term.moveto(line_begin, 0);
}

/// `clearscr`.
pub fn clearscr(term: &mut Terminal) {
    term.print("\x1b[2J");
    term.moveto(1, 1);
}

/// `press_any_key`.
pub async fn press_any_key(term: &mut Terminal) -> u32 {
    press_any_key_ex(
        term,
        "                           \x1b[1;33m按任意键继续...\x1b[m",
        60,
    )
    .await
}

/// `press_any_key_ex`.
pub async fn press_any_key_ex(term: &mut Terminal, msg: &str, sec: u64) -> u32 {
    term.moveto(SCREEN_ROWS, 0);
    term.clrtoeol();
    term.print(msg);
    press_any_key_no_prompt(term, sec).await
}

/// `press_any_key_no_prompt`: repeats while `igetch` reports "no key yet".
pub async fn press_any_key_no_prompt(term: &mut Terminal, sec: u64) -> u32 {
    let begin = std::time::Instant::now();
    term.reset_input().await;

    let mut ch = term.read_key_wait(sec).await;
    while ch == key::KEY_RETRY && begin.elapsed().as_secs() < 60 {
        let elapsed = begin.elapsed().as_secs();
        ch = term.read_key_wait(sec.saturating_sub(elapsed)).await;
    }
    ch
}

/// `show_top`: the blue status row with left, middle and right fields.
pub fn show_top(term: &mut Terminal, left: &str, middle: &str, right: &str) {
    let fixed = term.fixed_width();

    // The middle and right widths are measured on the whole string while the printed
    // copy is truncated, which is how the original can mis-pad an over-long title.
    let left_printed = truncate_cells(left, TOP_LEFT_CELLS, fixed);
    let left_len = text::display_width(left_printed.as_bytes(), true, fixed);
    let middle_printed = truncate_cells(middle, TOP_MIDDLE_CELLS, fixed);
    let middle_len = text::display_width(middle.as_bytes(), true, fixed);
    let right_printed = truncate_cells(right, TOP_RIGHT_CELLS, fixed);
    let right_len = text::display_width(right.as_bytes(), true, fixed);

    term.moveto(1, 0);
    term.clrtoeol();
    term.print(&format!(
        "\x1b[1;44;33m{left}\x1b[37m{gap1}{middle}\x1b[33m{gap2}{right}\x1b[m",
        left = left_printed,
        gap1 = spaces(44 - left_len as i64 - middle_len as i64),
        middle = middle_printed,
        gap2 = spaces(36 - right_len as i64),
        right = right_printed,
    ));
}

/// `show_bottom`: clock, message, user name and online time.
pub fn show_bottom(term: &mut Terminal, msg: &str, username: &str, login_tm: i64) {
    let fixed = term.fixed_width();

    let now = chrono::Local::now();
    let str_time = now.format("%m/%d %H:%M %Z").to_string();
    let len_str_time = text::display_width(str_time.as_bytes(), true, fixed);

    let msg_printed = truncate_cells(msg, 23, fixed);
    let len_msg = text::display_width(msg.as_bytes(), true, fixed);

    let len_username = text::display_width(username.as_bytes(), true, fixed);

    let uptime = chrono::DateTime::from_timestamp(now.timestamp() - login_tm, 0)
        .map(|dt| dt.naive_utc())
        .unwrap_or_default();
    let days = uptime.ordinal0();
    let str_online = if days > 0 {
        format!(
            "\x1b[36m{days}\x1b[33md \x1b[36m{}\x1b[33m:\x1b[36m{:02}",
            uptime.hour(),
            uptime.minute()
        )
    } else {
        format!(
            "\x1b[36m{}\x1b[33m:\x1b[36m{:02}",
            uptime.hour(),
            uptime.minute()
        )
    };
    let len_online = text::display_width(str_online.as_bytes(), true, fixed);

    term.moveto(SCREEN_ROWS, 0);
    term.clrtoeol();
    term.print(&format!(
        "\x1b[1;44;33m时间[\x1b[36m{time}\x1b[33m]{msg}{gap} \x1b[33m用户[\x1b[36m{user}\x1b[33m][{online}\x1b[33m]\x1b[m",
        time = str_time,
        msg = msg_printed,
        gap = spaces(
            65 - len_str_time as i64
                - len_msg as i64
                - len_username as i64
                - len_online as i64
        ),
        user = username,
        online = str_online,
    ));
}

/// `DISPLAY_CTX`: what the pager reports to its key handler.
#[derive(Debug, Clone, Default)]
pub struct DisplayCtx {
    pub reach_begin: bool,
    pub reach_end: bool,
    pub line_top: i64,
    pub line_bottom: i64,
    pub msg: String,
}

/// Session services the pager needs, implemented by `session::Session`.
// Generic over the hook type, so `Send` inference on these futures still works.
#[allow(async_fn_in_trait)]
pub trait PagerHooks {
    /// `user_online_update(NULL)` plus the last-access timestamp.
    async fn on_activity(&mut self);

    /// `display_data_key_handler`: may rewrite `ch` or fill `ctx.msg`; non-zero stops.
    async fn key_handler(&mut self, ch: &mut u32, ctx: &mut DisplayCtx) -> i32;

    /// The `show_help` flag, shared across nested pagers the way the static was.
    fn help_enabled(&mut self) -> bool;

    fn set_help_enabled(&mut self, enabled: bool);

    /// `display_file(help_filename, 1)`. The pager hands its own terminal over, because the
    /// hooks live on the session, which cannot also own it.
    async fn show_help(&mut self, term: &mut Terminal, help_filename: &str) -> u32;
}

/// `display_data`: page through pre-wrapped lines, returning the key that left.
pub async fn display_data<H: PagerHooks + ?Sized>(
    term: &mut Terminal,
    data: &[u8],
    lines: &text::Wrapped,
    eof_exit: i32,
    hooks: &mut H,
    help_filename: &str,
    idle_limit: u64,
) -> u32 {
    let screen_row_total = (SCREEN_ROWS as i32) - SCREEN_BEGIN_ROW;
    let display_line_total = lines.count() as i64;

    let mut ctx = DisplayCtx::default();
    let mut ch: u32 = 0;
    let mut line_current: i64 = 0;
    let mut output_current_row: i32 = SCREEN_BEGIN_ROW;
    let mut output_end_row: i32 = SCREEN_ROWS as i32 - 1;

    clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);

    if hooks.key_handler(&mut ch, &mut ctx).await != 0 {
        return ch;
    }

    let mut loop_on = true;
    while loop_on {
        if eof_exit > 0 && line_current >= display_line_total {
            ch = if eof_exit == 1 {
                press_any_key(term).await
            } else {
                let _ = term.flush().await;
                ch
            };
            break;
        }

        let offset_row = (output_current_row - SCREEN_BEGIN_ROW) as i64;

        if line_current >= display_line_total || output_current_row > output_end_row {
            ctx.reach_begin = line_current < output_current_row as i64;

            let upcoming = line_current - offset_row + screen_row_total as i64;
            let percentile = if upcoming < display_line_total {
                ctx.reach_end = false;
                upcoming * 100 / display_line_total.max(1)
            } else {
                ctx.reach_end = true;
                100
            };

            ctx.line_top = line_current - offset_row + 1;
            ctx.line_bottom = upcoming.min(display_line_total);

            // Built in a LINE_BUFFER_LEN buffer, so the reset sequence is lost when the
            // message already fills it, exactly as in the original.
            let raw = format!(
                "\x1b[1;44;33m第\x1b[32m{}\x1b[33m-\x1b[32m{}\x1b[33m行 (\x1b[32m{}%\x1b[33m) {}",
                ctx.line_top, ctx.line_bottom, percentile, ctx.msg
            );
            let mut buffer: Vec<u8> = raw.as_bytes().to_vec();
            buffer.truncate(LINE_BUFFER_LEN - 1);
            let sp = text::split_line(&buffer, SCREEN_COLS, true, term.fixed_width());
            let end = sp.bytes.min(buffer.len());
            let mut status = buffer[..end].to_vec();
            for _ in sp.display_len..SCREEN_COLS {
                status.push(b' ');
            }
            status.extend_from_slice(b"\x1b[m");

            term.moveto(SCREEN_ROWS, 0);
            term.print_bytes(&status);

            let mut input_ok = false;
            while !input_ok {
                ch = term.read_key_wait(idle_limit).await;
                input_ok = true;

                if ch != KEY_NULL && ch != KEY_TIMEOUT {
                    hooks.on_activity().await;
                }

                if hooks.key_handler(&mut ch, &mut ctx).await != 0 {
                    return ch;
                }

                match ch {
                    KEY_NULL | KEY_TIMEOUT => return ch,
                    KEY_HOME => {
                        if line_current - (output_current_row as i64) < 0 {
                            break;
                        }
                        line_current = 0;
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    KEY_END => {
                        if display_line_total < screen_row_total as i64 {
                            break;
                        }
                        line_current = display_line_total - screen_row_total as i64;
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    KEY_UP => {
                        if line_current - (output_current_row as i64) < 0 {
                            break;
                        }
                        line_current -= output_current_row as i64;
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                    }
                    CR | KEY_DOWN => {
                        if line_current + (screen_row_total as i64 - offset_row)
                            >= display_line_total
                        {
                            break;
                        }
                        line_current += screen_row_total as i64 - offset_row;
                        output_current_row = screen_row_total;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        term.moveto(SCREEN_ROWS, 0);
                        term.clrtoeol();
                        // Legacy cterm only scrolls with a bare newline.
                        term.print("\n");
                    }
                    KEY_PGUP => {
                        if line_current - (output_current_row as i64) < 0 {
                            break;
                        }
                        line_current -= (screen_row_total as i64 - 1) + offset_row;
                        line_current = line_current.max(0);
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    KEY_SPACE | KEY_PGDN => {
                        if line_current + screen_row_total as i64 - offset_row >= display_line_total
                        {
                            break;
                        }
                        line_current += (screen_row_total as i64 - 1) - offset_row;
                        if line_current + screen_row_total as i64 > display_line_total {
                            line_current = display_line_total - screen_row_total as i64;
                        }
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    KEY_ESC | KEY_LEFT => loop_on = false,
                    KEY_HELP => {
                        if !hooks.help_enabled() {
                            break;
                        }
                        hooks.set_help_enabled(false);
                        let _ = hooks.show_help(term, help_filename).await;
                        hooks.set_help_enabled(true);
                        // Falls through to the refresh, as the original's switch does.
                        line_current -= offset_row;
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    key::KEY_F5 => {
                        line_current -= offset_row;
                        output_current_row = SCREEN_BEGIN_ROW;
                        output_end_row = SCREEN_ROWS as i32 - 1;
                        clrline(term, SCREEN_BEGIN_ROW as usize, SCREEN_ROWS);
                    }
                    key::KEY_RETRY => {}
                    _ => input_ok = false,
                }
            }

            continue;
        }

        let chunk = lines.line(data, line_current as usize);
        let len = chunk.len().min(LINE_BUFFER_LEN - 1);
        term.moveto(output_current_row as usize, 0);
        term.clrtoeol();
        term.print_bytes(&chunk[..len]);
        line_current += 1;
        output_current_row += 1;
    }

    ch
}

/// `display_file_key_handler`: the help line shown by file pagers.
pub fn file_key_handler(ch: u32, ctx: &mut DisplayCtx) {
    if ch == 0 {
        ctx.msg = "| 返回[\x1b[32m←\x1b[33m,\x1b[32mESC\x1b[33m] | \
                    移动[\x1b[32m↑\x1b[33m/\x1b[32m↓\x1b[33m/\x1b[32mPgUp\x1b[33m/\x1b[32mPgDn\x1b[33m] | \
                    帮助[\x1b[32mh\x1b[33m] |"
            .to_string();
    }
}

/// `display_file`: page an already-read text file with the file key handler
/// (`screen.c:889-911`).
///
/// C took the pre-wrapped copy out of the shared-memory file cache; the port wraps the
/// bytes in this session's width mode, which for the 80-column terminal the original
/// hard-codes is the same output. `eof_exit` keeps its meaning: `1` ends with a
/// press-any-key, `2` only flushes.
///
/// The two wrap arguments are the loader's (`file_loader.c:87`): `skip_ctrl_seq` keeps the
/// colour sequences of an ANSI logo out of the width budget — without it every decorated
/// row of `data/welcome.txt` is measured by its bytes and breaks in the middle — and the
/// count is one short of `MAX_SPLIT_FILE_LINES` because C bounds the offsets *array* and so
/// stores one line fewer than the port's `max_lines`, which bounds the lines themselves.
pub async fn display_file<H: PagerHooks + ?Sized>(
    term: &mut Terminal,
    hooks: &mut H,
    data: &[u8],
    eof_exit: i32,
) -> u32 {
    let lines = term.wrap(data, SCREEN_COLS, true, MAX_SPLIT_FILE_LINES - 1);
    display_data(
        term,
        data,
        &lines,
        eof_exit,
        hooks,
        DATA_READ_HELP,
        MAX_USER_IDLE_TIME,
    )
    .await
}

/// Echo mode of the line editor, from `enum io_echo_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    On,
    Off,
}
/// `iscntrl` over the byte-valued keys.
fn iscntrl(ch: u32) -> bool {
    ch < 32 || ch == 127
}

/// Cells charged for a multi-byte sequence: 2 in fixed mode, `wcwidth` otherwise.
fn multibyte_cells(seq: &[u8], fixed: bool) -> usize {
    if fixed {
        return 2;
    }
    std::str::from_utf8(seq)
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| {
            unicode_width::UnicodeWidthChar::width(c)
                .unwrap_or(0)
                .max(0) as usize
        })
        .unwrap_or(1)
}

/// Read a multi-byte character continuing from a lead byte, as `igetch(100)` does.
async fn read_multibyte(term: &mut Terminal, lead: u32) -> Option<Vec<u8>> {
    let mut seq = vec![lead as u8];
    let want = key::utf8_seq_len(lead as u8);
    while seq.len() < want {
        let next = term.read_key(Duration::from_millis(100)).await;
        if next == KEY_NULL || next == KEY_TIMEOUT {
            return None;
        }
        seq.push(next as u8);
    }
    Some(seq)
}

/// The byte index of the character containing `index`, walking back over continuations.
fn char_start(bytes: &[u8], index: usize) -> usize {
    let mut i = index;
    while i > 0 && bytes[i] & 0xc0 != 0xc0 {
        i -= 1;
    }
    i
}

/// `set_input_echo` (`screen.c:127-139`): tell the telnet client who is going to echo what the
/// user typed. `false` sends `IAC WILL ECHO, IAC WILL SGA`, after which a `NOECHO` prompt shows
/// only this side's `*` mask; without it the client keeps its own echo and the password walks
/// across the screen in clear text.
///
/// The bytes ride the same buffer as C's `prints`, so they are transcoded like any other output —
/// harmless only because the one caller (`bbs_main.c:355`) speaks before `bbs_charset_select` has
/// picked anything other than the default UTF-8.
pub async fn set_input_echo(term: &mut Terminal, echo: bool) {
    if echo {
        // C's `outc('\x83')`: byte 131, which names no telnet option. Nothing calls this arm.
        term.print_bytes(&[0x83]);
    } else {
        term.print_bytes(&[0xff, 0xfb, 0x01, 0xff, 0xfb, 0x03]);
    }
    let _ = term.flush().await;
}

/// `_str_input`: append-only line input at the current cursor position.
///
/// `None` means the client timed out or went away (C's `-1`); `Some(vec![])` is the
/// cancelled line an `ESC` produces.
pub async fn str_input(
    term: &mut Terminal,
    buf_size: usize,
    echo: Echo,
    idle_limit: u64,
) -> Option<Vec<u8>> {
    str_input_ex(term, b"", buf_size, echo, idle_limit).await
}

/// `str_input` with a pre-filled buffer, which is what the original accepts.
pub async fn str_input_ex(
    term: &mut Terminal,
    initial: &[u8],
    buf_size: usize,
    echo: Echo,
    idle_limit: u64,
) -> Option<Vec<u8>> {
    let fixed = term.fixed_width();
    // `str_input` passes the byte size as the display budget, so cells and bytes share
    // one limit here just as they do in the original.
    let max_display_len = buf_size;
    let limit = buf_size.saturating_sub(1);

    let sp = text::split_line(initial, max_display_len, false, fixed);
    let mut buffer = initial[..sp.bytes.min(initial.len())].to_vec();
    let mut offset = buffer.len();
    let mut display_len = sp.display_len;

    term.reset_input().await;

    let mut timed_out = false;
    loop {
        // `str_input`/`get_data` cap the idle wait at 60 seconds; the pager does not.
        let ch = term.read_key_wait(idle_limit.min(60)).await;

        match ch {
            CR => break,
            KEY_TIMEOUT | KEY_NULL => {
                timed_out = true;
                break;
            }
            key::LF | key::KEY_RETRY => continue,
            KEY_ESC => {
                buffer.clear();
                break;
            }
            BACKSPACE | KEY_DEL if offset > 0 => {
                offset -= 1;
                if buffer[offset] >= 0x80 {
                    offset = char_start(&buffer, offset);
                    display_len -= 1;
                    term.print("\x1b[D \x1b[D");
                }
                buffer.truncate(offset);
                display_len = display_len.saturating_sub(1);
                term.print("\x1b[D \x1b[D");
                let _ = term.flush().await;
            }
            ch if ch > 255 || iscntrl(ch) => {}
            ch if key::is_utf8_lead(ch) => {
                let Some(seq) = read_multibyte(term, ch).await else {
                    continue;
                };
                let wc_len = multibyte_cells(&seq, fixed);
                if offset + seq.len() > limit || display_len + wc_len > max_display_len {
                    term.bell();
                    let _ = term.flush().await;
                    continue;
                }
                buffer.extend_from_slice(&seq);
                offset += seq.len();
                // The original always charges two cells for a multi-byte character here.
                display_len += 2;
                match echo {
                    Echo::On => term.print_bytes(&seq),
                    Echo::Off => term.print("**"),
                }
                let _ = term.flush().await;
            }
            ch if (32..127).contains(&ch) => {
                if offset + 1 > limit || display_len + 1 > max_display_len {
                    term.bell();
                    let _ = term.flush().await;
                    continue;
                }
                buffer.push(ch as u8);
                offset += 1;
                display_len += 1;
                match echo {
                    Echo::On => term.print_bytes(&[ch as u8]),
                    Echo::Off => term.print("*"),
                }
                let _ = term.flush().await;
            }
            _ => {}
        }
    }

    // The `str_input` wrapper emits the closing newline even when `_str_input` gave up.
    term.print("\r\n");
    let _ = term.flush().await;
    if timed_out { None } else { Some(buffer) }
}

/// `get_data`: read a line at a fixed screen position with full cursor editing.
///
/// Returns the typed bytes, or `None` on timeout or disconnect. A cancel with `ESC`
/// yields an empty buffer, which is how callers tell the two apart from the C return
/// code (`0` versus `-1`).
pub async fn get_data(
    term: &mut Terminal,
    row: usize,
    col: usize,
    prompt: &str,
    initial: &[u8],
    buf_size: usize,
    max_display_len: usize,
    idle_limit: u64,
) -> Option<Vec<u8>> {
    let fixed = term.fixed_width();
    let limit = buf_size.saturating_sub(1);

    let sp = text::split_line(initial, max_display_len, false, fixed);
    let mut bytes = initial[..sp.bytes.min(initial.len())].to_vec();
    bytes.truncate(limit);
    let mut cursor = sp.bytes.min(bytes.len());
    let mut len = bytes.len();
    let mut display_len = text::display_width(&bytes, false, fixed);
    let prompt_cells = text::display_width(prompt.as_bytes(), true, fixed);
    let mut col_cur = col + prompt_cells + display_len;

    term.moveto(row, col);
    term.print(prompt);
    term.print_bytes(&bytes);
    term.print(&spaces(max_display_len as i64 - display_len as i64));
    term.moveto(row, col_cur);

    term.reset_input().await;

    loop {
        let ch = term.read_key_wait(idle_limit.min(60)).await;

        match ch {
            CR => break,
            KEY_TIMEOUT | KEY_NULL => return None,
            key::LF | key::KEY_RETRY => continue,
            KEY_ESC => {
                bytes.clear();
                break;
            }
            BACKSPACE if cursor > 0 => {
                let mut str_len = 1usize;
                cursor -= 1;
                if bytes[cursor] >= 0x80 {
                    while cursor > 0 && bytes[cursor] & 0xc0 != 0xc0 {
                        str_len += 1;
                        cursor -= 1;
                    }
                    if multibyte_cells(&bytes[cursor..cursor + str_len], fixed) == 2 {
                        display_len -= 1;
                        col_cur -= 1;
                    }
                }
                bytes.drain(cursor..cursor + str_len);
                len = bytes.len();
                display_len = display_len.saturating_sub(1);
                col_cur -= 1;
                redraw(
                    term,
                    row,
                    col_cur,
                    &bytes,
                    cursor,
                    display_len,
                    max_display_len,
                );
            }
            KEY_DEL if cursor < len => {
                let mut str_len = 1usize;
                if bytes[cursor] & 0x80 != 0 {
                    str_len = key::utf8_seq_len(bytes[cursor]).min(len - cursor);
                    display_len -= 1;
                }
                bytes.drain(cursor..cursor + str_len);
                len = bytes.len();
                display_len = display_len.saturating_sub(1);
                redraw(
                    term,
                    row,
                    col_cur,
                    &bytes,
                    cursor,
                    display_len,
                    max_display_len,
                );
            }
            KEY_LEFT if cursor > 0 => {
                cursor -= 1;
                if bytes[cursor] >= 0x80 {
                    let start = char_start(&bytes, cursor);
                    if multibyte_cells(&bytes[start..cursor + 1], fixed) == 2 {
                        col_cur -= 1;
                    }
                    cursor = start;
                }
                col_cur -= 1;
                term.moveto(row, col_cur);
            }
            KEY_RIGHT if cursor < len => {
                let mut str_len = 1usize;
                if bytes[cursor] & 0x80 != 0 {
                    str_len = key::utf8_seq_len(bytes[cursor]).min(len - cursor);
                    if multibyte_cells(&bytes[cursor..cursor + str_len], fixed) == 2 {
                        col_cur += 1;
                    }
                }
                col_cur += 1;
                cursor += str_len;
                term.moveto(row, col_cur);
            }
            KEY_HOME | KEY_UP if cursor > 0 => {
                cursor = 0;
                col_cur = col + prompt_cells;
                term.moveto(row, col_cur);
            }
            KEY_END | KEY_DOWN if cursor < len => {
                cursor = len;
                col_cur = col + prompt_cells + display_len;
                term.moveto(row, col_cur);
            }
            ch if ch > 255 || iscntrl(ch) => {}
            ch if key::is_utf8_lead(ch) => {
                let Some(seq) = read_multibyte(term, ch).await else {
                    continue;
                };
                let wc_len = multibyte_cells(&seq, fixed);
                if len + seq.len() > limit || display_len + wc_len > max_display_len {
                    term.bell();
                    continue;
                }
                bytes.splice(cursor..cursor, seq.clone());
                len = bytes.len();
                display_len += wc_len;

                term.moveto(row, col_cur);
                term.print_bytes(&bytes[cursor..]);
                term.print(&spaces(max_display_len as i64 - display_len as i64));
                col_cur += wc_len;
                term.moveto(row, col_cur);
                cursor += seq.len();
            }
            ch if (32..127).contains(&ch) => {
                if len + 1 > limit || display_len + 1 > max_display_len {
                    term.bell();
                    continue;
                }
                bytes.insert(cursor, ch as u8);
                len = bytes.len();
                display_len += 1;

                term.moveto(row, col_cur);
                term.print_bytes(&bytes[cursor..]);
                term.print(&spaces(max_display_len as i64 - display_len as i64));
                col_cur += 1;
                term.moveto(row, col_cur);
                cursor += 1;
            }
            _ => {}
        }
    }

    Some(bytes)
}

fn redraw(
    term: &mut Terminal,
    row: usize,
    col_cur: usize,
    bytes: &[u8],
    cursor: usize,
    display_len: usize,
    max_display_len: usize,
) {
    term.moveto(row, col_cur);
    term.print_bytes(&bytes[cursor..]);
    term.print(&spaces(max_display_len as i64 - display_len as i64));
    term.moveto(row, col_cur);
}

#[cfg(test)]
#[path = "../tests/test_screen.rs"]
mod tests;
