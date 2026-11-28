use super::*;

fn key(bytes: &[u8]) -> u32 {
    match decode(bytes) {
        Decoded::Key(k, _) => k,
        other => panic!("not a key: {other:?}"),
    }
}

fn consumed(bytes: &[u8]) -> usize {
    match decode(bytes) {
        Decoded::Key(_, n) => n,
        other => panic!("not a key: {other:?}"),
    }
}

#[test]
fn crlf_is_one_cr() {
    assert_eq!(decode(b"\r\n"), Decoded::Key(CR, 2));
    assert_eq!(decode(b"\r"), Decoded::Key(CR, 1));
    assert_eq!(decode(b"\n"), Decoded::Key(CR, 1));
}

#[test]
fn arrows_in_both_encodings() {
    assert_eq!(key(b"\x1b[A"), KEY_UP);
    assert_eq!(key(b"\x1bOB"), KEY_DOWN);
    assert_eq!(key(b"\x1b[C"), KEY_RIGHT);
    assert_eq!(key(b"\x1b[D"), KEY_LEFT);
}

#[test]
fn editing_keys() {
    assert_eq!(key(b"\x1b[1~"), KEY_HOME);
    assert_eq!(key(b"\x1b[2~"), KEY_INS);
    assert_eq!(key(b"\x1b[3~"), KEY_DEL);
    assert_eq!(key(b"\x1b[4~"), KEY_END);
    assert_eq!(key(b"\x1b[5~"), KEY_PGUP);
    assert_eq!(key(b"\x1b[6~"), KEY_PGDN);
    assert_eq!(key(b"\x7f"), BACKSPACE);
    assert_eq!(key(b"\x1b\x1b"), KEY_CSI);
}

/// `io.c` maps these only in the `<n>~` encodings, so a terminal sending the two-byte
/// forms got a bare escape and the editor inserted a star instead of moving the cursor.
#[test]
fn home_and_end_in_the_two_byte_encodings() {
    assert_eq!(key(b"\x1b[H"), KEY_HOME);
    assert_eq!(key(b"\x1b[F"), KEY_END);
    assert_eq!(key(b"\x1bOH"), KEY_HOME);
    assert_eq!(key(b"\x1bOF"), KEY_END);
    // A cursor-position report is not a key, and stays unknown.
    assert_eq!(decode(b"\x1b[10;20H"), Decoded::Partial);
}

#[test]
fn f_keys_in_both_encodings() {
    assert_eq!(key(b"\x1bOP"), KEY_F1);
    assert_eq!(key(b"\x1bOS"), KEY_F4);
    assert_eq!(key(b"\x1b[11~"), KEY_F1);
    assert_eq!(key(b"\x1b[14~"), KEY_F4);
    assert_eq!(key(b"\x1b[15~"), KEY_F5);
    assert_eq!(key(b"\x1b[17~"), KEY_F6);
    assert_eq!(key(b"\x1b[19~"), KEY_F8);
    assert_eq!(key(b"\x1b[20~"), KEY_F9);
    assert_eq!(key(b"\x1b[23~"), KEY_F11);
    assert_eq!(key(b"\x1b[24~"), KEY_F12);
}

#[test]
fn modified_keys() {
    assert_eq!(key(b"\x1b[1;5A"), KEY_CTRL_UP);
    assert_eq!(key(b"\x1b[1;5D"), KEY_CTRL_LEFT);
    assert_eq!(key(b"\x1b[1;5H"), KEY_CTRL_HOME);
    assert_eq!(key(b"\x1b[1;2P"), KEY_SHIFT_F1);
    assert_eq!(key(b"\x1b[15;5~"), KEY_CTRL_F5);
    assert_eq!(key(b"\x1b[24;2~"), KEY_SHIFT_F12);
    assert_eq!(key(b"\x1b[20;5~"), KEY_CTRL_F9);
}

#[test]
fn wide_control_codes_pass_through() {
    assert_eq!(key(b"\xff\x01\x01"), KEY_UP);
    assert_eq!(key(b"\xff\xfe\xff"), 0xfeff);
}

#[test]
fn plain_bytes_are_returned_verbatim() {
    assert_eq!(key(b"a"), b'a' as u32);
    assert_eq!(key(b" "), KEY_SPACE);
    assert_eq!(key(b"\x01"), 1);
    // A UTF-8 lead byte is only the first byte; callers reassemble the rest.
    let lead = "中".as_bytes()[0];
    assert_eq!(key(&[lead]), lead as u32);
    assert!(is_utf8_lead(lead as u32));
}

#[test]
fn sequences_are_not_confused_with_following_input() {
    assert_eq!(consumed(b"\x1b[AX"), 3);
    assert_eq!(decode(b"\x1b[AX"), Decoded::Key(KEY_UP, 3));
    assert_eq!(decode(b"ab"), Decoded::Key(b'a' as u32, 1));
}

#[test]
fn truncated_escape_is_partial() {
    assert_eq!(decode(b"\x1b"), Decoded::Partial);
    assert_eq!(decode(b"\x1b["), Decoded::Partial);
    assert_eq!(decode(b"\x1b[1;"), Decoded::Partial);
    assert_eq!(decode(b"\xff\x01"), Decoded::Key(KEY_CONTROL, 1));
}

#[test]
fn unknown_sequence_terminated_by_m_is_discarded() {
    assert_eq!(decode(b"\x1b[999mQ"), Decoded::Key(b'Q' as u32, 7));
    assert_eq!(decode(b"\x1b[999m"), Decoded::Retry);
    // The colour codes written by the pager can be echoed back by a dumb client.
    assert_eq!(decode(b"\x1b[1;44;33m\r"), Decoded::Key(CR, 11));
}

#[test]
fn unknown_esc_sequence_keeps_scanning_then_is_partial() {
    assert_eq!(decode(b"\x1b[?25l"), Decoded::Partial);
}

#[test]
fn empty_buffer_is_retry() {
    assert_eq!(decode(b""), Decoded::Retry);
}

#[test]
fn ctrl_helper_matches_c_macro() {
    assert_eq!(ctrl(b'P'), 16);
    assert_eq!(ctrl(b'A'), 1);
    assert_eq!(ctrl(b'Z'), 26);
}
