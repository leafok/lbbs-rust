//! Terminal key codes, values identical to `enum io_key_t` in `include/io.h`.
//!
//! Kept as integer constants rather than a Rust enum so ported call sites read like the
//! original `switch (ch)`.

#![allow(non_snake_case)]

pub const CR: u32 = '\r' as u32;
pub const LF: u32 = '\n' as u32;
pub const BACKSPACE: u32 = 8;
pub const BELL: u32 = 7;
pub const KEY_TAB: u32 = '\t' as u32;
pub const KEY_ESC: u32 = 0x1b;
pub const KEY_SPACE: u32 = 0x20;

pub const KEY_NULL: u32 = 0xffff;
pub const KEY_TIMEOUT: u32 = 0xfffe;
pub const KEY_CONTROL: u32 = 0xff;
pub const KEY_BACKSPACE: u32 = 0x7f;
pub const KEY_UP: u32 = 0x0101;
pub const KEY_DOWN: u32 = 0x0102;
pub const KEY_RIGHT: u32 = 0x0103;
pub const KEY_LEFT: u32 = 0x0104;
pub const KEY_CSI: u32 = 0x011b;
pub const KEY_HOME: u32 = 0x0201;
pub const KEY_INS: u32 = 0x0202;
pub const KEY_DEL: u32 = 0x0203;
pub const KEY_END: u32 = 0x0204;
pub const KEY_PGUP: u32 = 0x0205;
pub const KEY_PGDN: u32 = 0x0206;

pub const KEY_F1: u32 = 0x0207;
pub const KEY_F2: u32 = 0x0208;
pub const KEY_F3: u32 = 0x0209;
pub const KEY_F4: u32 = 0x020a;
pub const KEY_F5: u32 = 0x020b;
pub const KEY_F6: u32 = 0x020c;
pub const KEY_F7: u32 = 0x020d;
pub const KEY_F8: u32 = 0x020e;
pub const KEY_F9: u32 = 0x020f;
pub const KEY_F10: u32 = 0x0210;
pub const KEY_F11: u32 = 0x0211;
pub const KEY_F12: u32 = 0x0212;

pub const KEY_SHIFT_F1: u32 = 0x0213;
pub const KEY_SHIFT_F2: u32 = 0x0214;
pub const KEY_SHIFT_F3: u32 = 0x0215;
pub const KEY_SHIFT_F4: u32 = 0x0216;
pub const KEY_SHIFT_F5: u32 = 0x0217;
pub const KEY_SHIFT_F6: u32 = 0x0218;
pub const KEY_SHIFT_F7: u32 = 0x0219;
pub const KEY_SHIFT_F8: u32 = 0x021a;
pub const KEY_SHIFT_F9: u32 = 0x021b;
pub const KEY_SHIFT_F10: u32 = 0x021c;
pub const KEY_SHIFT_F11: u32 = 0x021d;
pub const KEY_SHIFT_F12: u32 = 0x021e;

pub const KEY_CTRL_F1: u32 = 0x021f;
pub const KEY_CTRL_F2: u32 = 0x0220;
pub const KEY_CTRL_F3: u32 = 0x0221;
pub const KEY_CTRL_F4: u32 = 0x0222;
pub const KEY_CTRL_F5: u32 = 0x0223;
pub const KEY_CTRL_F6: u32 = 0x0224;
pub const KEY_CTRL_F7: u32 = 0x0225;
pub const KEY_CTRL_F8: u32 = 0x0226;
pub const KEY_CTRL_F9: u32 = 0x0227;
pub const KEY_CTRL_F10: u32 = 0x0228;
pub const KEY_CTRL_F11: u32 = 0x0229;
pub const KEY_CTRL_F12: u32 = 0x022a;

pub const KEY_CTRL_UP: u32 = 0x0230;
pub const KEY_CTRL_DOWN: u32 = 0x0231;
pub const KEY_CTRL_RIGHT: u32 = 0x0232;
pub const KEY_CTRL_LEFT: u32 = 0x0233;
pub const KEY_CTRL_HOME: u32 = 0x0234;
pub const KEY_CTRL_END: u32 = 0x0235;

/// `Ctrl(C)` from `include/io.h:89`.
pub const fn ctrl(c: u8) -> u32 {
    (c as u32).wrapping_sub('A' as u32 - 1)
}

/// `igetch` returning literal `0`: no key yet, the caller retries.
pub const KEY_RETRY: u32 = 0;

/// Bytes that the line editor treats as text rather than as a command.
pub fn is_printable_ascii(key: u32) -> bool {
    (32..=126).contains(&key)
}

/// `isalnum` over the byte-valued keys.
pub fn is_alnum(key: u32) -> bool {
    key < 256 && (key as u8).is_ascii_alphanumeric()
}

/// `toupper` over the byte-valued keys, used for menu hotkeys.
pub fn to_upper(key: u32) -> u32 {
    if key < 256 {
        (key as u8).to_ascii_uppercase() as u32
    } else {
        key
    }
}

/// A UTF-8 lead byte: the line editor reassembles the rest byte by byte.
pub fn is_utf8_lead(key: u32) -> bool {
    key < 256 && (key as u8) & 0x80 != 0
}

/// Bytes in the sequence a lead byte announces, mirroring the `& 0xf0` bit loop.
pub fn utf8_seq_len(lead: u8) -> usize {
    crate::text::seq_len(lead)
}

/// Outcome of feeding the buffered bytes to the decoder.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A key plus the number of bytes it consumed.
    Key(u32, usize),
    /// The buffer ends inside an escape sequence, so the key still needs the bytes behind it.
    /// [`crate::term::Terminal::read_key`] waits a moment for them; when nothing more arrives,
    /// the escape answers as [`KEY_ESC`] while it is still alone and as a plain retry once a
    /// second byte has joined it, which is what `igetch` hands back for a sequence that
    /// matches no known key.
    Partial,
    /// Every byte was spent on discarded sequences: `igetch` would return `KEY_RETRY`.
    Retry,
}

/// A CSI/SS3 body scanned so far: `Some` only when it is a complete, known sequence.
/// Anything else keeps the scan going, because `ESC O` is a prefix of `ESC O P`.
fn esc_sequence(seq: &[u8]) -> Option<u32> {
    if seq.is_empty() {
        return None;
    }

    // SS3: ESC O <letter>
    if seq[0] == b'O' {
        return if seq.len() == 2 {
            match seq[1] {
                b'A' => Some(KEY_UP),
                b'B' => Some(KEY_DOWN),
                b'C' => Some(KEY_RIGHT),
                b'D' => Some(KEY_LEFT),
                b'H' => Some(KEY_HOME),
                b'F' => Some(KEY_END),
                b'P' => Some(KEY_F1),
                b'Q' => Some(KEY_F2),
                b'R' => Some(KEY_F3),
                b'S' => Some(KEY_F4),
                _ => None,
            }
        } else {
            None
        };
    }

    if seq[0] != b'[' {
        return None;
    }
    let body = &seq[1..];

    // Two-byte CSI: ESC [ <letter>
    if body.len() == 1 {
        return match body[0] {
            b'A' => Some(KEY_UP),
            b'B' => Some(KEY_DOWN),
            b'C' => Some(KEY_RIGHT),
            b'D' => Some(KEY_LEFT),
            // `io.c:802-822` maps no `H`/`F` here, so a terminal in the xterm-style encoding
            // loses its Home and End; the port reads them as the two editing keys they are.
            b'H' => Some(KEY_HOME),
            b'F' => Some(KEY_END),
            b'V' => Some(KEY_SHIFT_F1),
            b'Z' => Some(KEY_SHIFT_F2),
            b'a' => Some(KEY_SHIFT_F3),
            b'b' => Some(KEY_SHIFT_F4),
            b'c' => Some(KEY_SHIFT_F5),
            b'd' => Some(KEY_SHIFT_F6),
            b'e' => Some(KEY_SHIFT_F7),
            b'f' => Some(KEY_SHIFT_F8),
            b'g' => Some(KEY_SHIFT_F9),
            b'h' => Some(KEY_SHIFT_F10),
            b'k' => Some(KEY_CTRL_F1),
            b'l' => Some(KEY_CTRL_F2),
            b'm' => Some(KEY_CTRL_F3),
            b'p' => Some(KEY_CTRL_F6),
            b'q' => Some(KEY_CTRL_F7),
            b'r' => Some(KEY_CTRL_F8),
            b's' => Some(KEY_CTRL_F9),
            b't' => Some(KEY_CTRL_F10),
            _ => None,
        };
    }
    if body.is_empty() {
        return None;
    }

    // ESC [ <params> ; <modifier> <letter>
    let (final_byte, params) = body.split_last().expect("body is not empty");
    if final_byte.is_ascii_alphabetic()
        && let Some((head, modifier)) = split_modifier(params)
    {
        let keys = match *final_byte {
            b'A' => (KEY_UP, KEY_CTRL_UP, 0),
            b'B' => (KEY_DOWN, KEY_CTRL_DOWN, 0),
            b'C' => (KEY_RIGHT, KEY_CTRL_RIGHT, 0),
            b'D' => (KEY_LEFT, KEY_CTRL_LEFT, 0),
            b'H' => (KEY_HOME, KEY_CTRL_HOME, 0),
            b'F' => (KEY_END, KEY_CTRL_END, 0),
            b'P' => (KEY_F1, KEY_CTRL_F1, KEY_SHIFT_F1),
            b'Q' => (KEY_F2, KEY_CTRL_F2, KEY_SHIFT_F2),
            b'R' => (KEY_F3, KEY_CTRL_F3, KEY_SHIFT_F3),
            b'S' => (KEY_F4, KEY_CTRL_F4, KEY_SHIFT_F4),
            _ => return None,
        };
        if head.is_empty() || head == b"1" {
            let key = match modifier {
                b'2' => keys.2,
                b'5' => keys.1,
                _ => keys.0,
            };
            return Some(if key == 0 { keys.0 } else { key });
        }
        return None;
    }

    // ESC [ <digits> [ ; <modifier> ] ~
    for leading in b'1'..=b'6' {
        if let Some(rest) = body.strip_prefix_digit(leading)
            && let Some(key) = tilde_key(rest, leading)
        {
            return Some(key);
        }
    }

    None
}

trait StripDigit {
    fn strip_prefix_digit(&self, d: u8) -> Option<&[u8]>;
}

impl StripDigit for [u8] {
    fn strip_prefix_digit(&self, d: u8) -> Option<&[u8]> {
        if self.first() == Some(&d) {
            Some(&self[1..])
        } else {
            None
        }
    }
}

/// Splits `<head> ; <modifier>`; the modifier is a single digit.
fn split_modifier(params: &[u8]) -> Option<(&[u8], u8)> {
    let idx = params.iter().position(|&b| b == b';')?;
    let (head, rest) = params.split_at(idx);
    if rest.len() != 2 {
        return None;
    }
    Some((head, rest[1]))
}

/// Digits after the leading parameter of a tilde sequence.
fn tilde_key(rest: &[u8], leading: u8) -> Option<u32> {
    // ESC [ <n> ~
    if rest == [b'~'] {
        return match leading {
            b'1' => Some(KEY_HOME),
            b'2' => Some(KEY_INS),
            b'3' => Some(KEY_DEL),
            b'4' => Some(KEY_END),
            b'5' => Some(KEY_PGUP),
            b'6' => Some(KEY_PGDN),
            _ => None,
        };
    }

    // ESC [ <n><d> ~  (the xterm F1-F12 encoding)
    if rest.len() == 2 && rest[1] == b'~' {
        let key = match (leading, rest[0]) {
            (b'1', b'1') => KEY_F1,
            (b'1', b'2') => KEY_F2,
            (b'1', b'3') => KEY_F3,
            (b'1', b'4') => KEY_F4,
            (b'1', b'5') => KEY_F5,
            (b'1', b'7') => KEY_F6,
            (b'1', b'8') => KEY_F7,
            (b'1', b'9') => KEY_F8,
            (b'2', b'0') => KEY_F9,
            (b'2', b'1') => KEY_F10,
            (b'2', b'3') => KEY_F11,
            (b'2', b'4') => KEY_F12,
            _ => return None,
        };
        return Some(key);
    }

    // ESC [ <n><d> ; <mod> ~
    if rest.len() == 4 && rest[1] == b';' && rest[3] == b'~' {
        let (d, modifier) = (rest[0], rest[2]);
        let (plain, shift, ctl) = match (leading, d) {
            (b'1', b'5') => (KEY_F5, KEY_SHIFT_F5, KEY_CTRL_F5),
            (b'1', b'7') => (KEY_F6, KEY_SHIFT_F6, KEY_CTRL_F6),
            (b'1', b'8') => (KEY_F7, KEY_SHIFT_F7, KEY_CTRL_F7),
            (b'1', b'9') => (KEY_F8, KEY_SHIFT_F8, KEY_CTRL_F8),
            (b'2', b'0') => (KEY_F9, KEY_SHIFT_F9, KEY_CTRL_F9),
            (b'2', b'1') => (KEY_F10, KEY_SHIFT_F10, KEY_CTRL_F10),
            (b'2', b'3') => (KEY_F11, KEY_SHIFT_F11, KEY_CTRL_F11),
            (b'2', b'4') => (KEY_F12, KEY_SHIFT_F12, KEY_CTRL_F12),
            _ => return None,
        };
        return Some(match modifier {
            b'2' => shift,
            b'5' => ctl,
            _ => plain,
        });
    }

    None
}

/// Decodes one key from the head of `buf`, following `igetch`'s ordering.
pub fn decode(buf: &[u8]) -> Decoded {
    let mut base = 0usize;

    loop {
        let rest = match buf.get(base..) {
            Some(rest) if !rest.is_empty() => rest,
            _ => return Decoded::Retry,
        };

        let c = rest[0];

        if c == b'\r' {
            let n = if rest.get(1) == Some(&b'\n') { 2 } else { 1 };
            return Decoded::Key(CR, base + n);
        }
        if c == b'\n' {
            // A lone line feed is delivered as carriage return.
            return Decoded::Key(CR, base + 1);
        }
        if c == 0x7f {
            return Decoded::Key(BACKSPACE, base + 1);
        }
        if c == 0xff {
            // Two following bytes carry an internal wide code, e.g. 0xff 0x01 0x01.
            if rest.len() >= 3 {
                let wide = (rest[1] as u32) * 256 + rest[2] as u32;
                return Decoded::Key(wide, base + 3);
            }
            return Decoded::Key(KEY_CONTROL, base + 1);
        }
        if c != KEY_ESC as u8 {
            return Decoded::Key(c as u32, base + 1);
        }

        if rest.len() == 1 {
            return Decoded::Partial;
        }
        if rest[1] == KEY_ESC as u8 {
            return Decoded::Key(KEY_CSI, base + 2);
        }

        // Collect bytes after ESC, testing for a complete sequence after each one, and
        // abandon the whole sequence at a CSI `m` terminator.
        let mut discarded_through_m = None;
        for take in 1..rest.len() {
            let seq = &rest[1..1 + take];
            if let Some(pos) = seq.iter().position(|&b| b == b'm') {
                discarded_through_m = Some(1 + pos + 1);
                break;
            }
            if let Some(key) = esc_sequence(seq) {
                return Decoded::Key(key, base + 1 + take);
            }
        }

        match discarded_through_m {
            Some(len) => base += len,
            None => return Decoded::Partial,
        }
    }
}

#[cfg(test)]
#[path = "../../tests/test_term_key.rs"]
mod tests;
