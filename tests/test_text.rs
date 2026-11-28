use super::*;

const FIX: bool = true;
const VAR: bool = false;

#[test]
fn seq_len_matches_lead_bytes() {
    assert_eq!(seq_len(0xC2), 2);
    assert_eq!(seq_len(0xE4), 3);
    assert_eq!(seq_len(0xF0), 4);
    assert_eq!(seq_len(0x80), 1);
    assert_eq!(seq_len(b'A'), 0);
}

#[test]
fn fixed_mode_counts_cjk_as_two() {
    let s = "中A文".as_bytes();
    assert_eq!(display_width(s, false, FIX), 5);
}

#[test]
fn variable_mode_matches_unicode_width() {
    let s = "中A文".as_bytes();
    assert_eq!(display_width(s, false, VAR), 5);
    // Combining mark contributes zero cells.
    let combining = "e\u{0301}".as_bytes();
    assert_eq!(display_width(combining, false, VAR), 1);
    // Fixed width ignores the sequence and charges the two bytes.
    assert_eq!(display_width(combining, false, FIX), 3);
}

#[test]
fn split_line_never_breaks_a_wide_char() {
    let s = "中文测试".as_bytes();
    // 5 cells available, each char is 2 -> only 2 chars fit (4 cells).
    let sp = split_line(s, 5, false, FIX);
    assert_eq!(sp.bytes, 6);
    assert_eq!(sp.display_len, 4);
    assert!(!sp.eol);
}

#[test]
fn split_line_consumes_newline_and_flags_eol() {
    let s = "ab\ncd".as_bytes();
    let sp = split_line(s, 80, false, FIX);
    assert_eq!(sp.bytes, 3);
    assert_eq!(sp.display_len, 3);
    assert!(sp.eol);
}

#[test]
fn split_line_skips_color_sequence() {
    let s = "\x1b[1;33m中\x1b[m".as_bytes();
    let sp = split_line(s, 2, true, FIX);
    assert_eq!(sp.display_len, 2);
    // The escape skip ignores the width budget, so the trailing sequence is
    // consumed even though no cells remain.
    assert_eq!(sp.bytes, 13);
}

#[test]
fn wrapped_offsets_tile_the_buffer() {
    let s = "abcdef\nghi".as_bytes();
    let w = split_data_lines(s, 4, false, FIX, 65536);
    assert_eq!(w.count(), 3);
    assert_eq!(w.line(s, 0), b"abcd");
    assert_eq!(w.line(s, 1), b"ef\n");
    assert_eq!(w.line(s, 2), b"ghi");
}

#[test]
fn wrapped_respects_line_cap() {
    let s = "a\nb\nc\nd\ne\n".as_bytes();
    let w = split_data_lines(s, 10, false, FIX, 3);
    assert_eq!(w.count(), 3);
    // A capped split leaves no width and no offset for a line that was not returned.
    assert_eq!(w.widths.len(), w.count());
    assert_eq!(w.offsets.len(), w.count() + 1);
}

#[test]
fn wrapped_of_empty_buffer_is_one_empty_line() {
    // Same as the C do/while: an empty body still yields one (zero-width) line.
    let w = split_data_lines(b"", 80, false, FIX, 65536);
    assert_eq!(w.count(), 1);
    assert_eq!(w.line(b"", 0), b"");
}

#[test]
fn truncated_multibyte_tail_does_not_panic() {
    let s = [b'A', 0xE4, 0xB8];
    let sp = split_line(&s, 80, false, FIX);
    assert_eq!(sp.bytes, 3);
}

#[test]
fn str_filter_drops_cr_bell_and_color() {
    let mut v = b"a\r\x07b\x1b[31mc\x1b[m d".to_vec();
    str_filter(&mut v, true);
    assert_eq!(v, b"abc d".to_vec());
}

#[test]
fn fit_display_pads_to_exact_cells() {
    assert_eq!(
        fit_display("中文".as_bytes(), 6, FIX),
        "中文  ".as_bytes().to_vec()
    );
    assert_eq!(fit_display("abc".as_bytes(), 3, FIX), b"abc".to_vec());
}

// The four tables below replay `tests/test_str_process.c` case for case. That file is the
// reference's own proof of this module, so it is quoted rather than paraphrased: the C
// `str_length(text, skip)` is [`display_width`] with `fixed` set the way `UTF8_fixed_width`
// is in each C function, and C's `count` — the number of slots in the caller's array — is
// `max_lines + 1` here, because `offsets` carries one entry more than the lines it describes.
#[test]
fn the_reference_str_length_table() {
    for (text, skip, want) in [
        ("", 1, 0),
        ("hello", 1, 5),
        ("hello\n", 1, 6),
        ("a\u{d}b\u{7}c", 1, 3),
        ("\u{1b}[1;31mred\u{1b}[m", 1, 3),
        ("\u{1b}[1;31mred\u{1b}[m", 0, 13),
        ("\u{1b}[2J\u{1b}[10Hhome", 1, 4),
        ("\u{1b}[", 1, 0),
        ("\u{1b}A", 1, 2),
        ("中", 1, 2),
        ("中文测试", 1, 8),
        ("A中B", 1, 4),
        ("é", 1, 2),
        ("😀", 1, 2),
    ] {
        assert_eq!(
            display_width(text.as_bytes(), skip == 1, FIX),
            want,
            "str_length({text:?}, {skip})"
        );
    }

    // Malformed input is measured, never walked past. C's own truncated case (`:212`) spells
    // out a whole 中 rather than cutting it, so the three bytes it means to show a partial
    // sequence are what is measured here.
    for (bytes, want) in [
        (&[0xE4, 0xB8][..], 2),
        (&[0x80, 0x80][..], 4),
        (&[0xC0, 0x80][..], 2),
        (&[0xFF, 0xFF][..], 2),
    ] {
        assert_eq!(display_width(bytes, true, FIX), want, "{bytes:02x?}");
    }

    // Variable width asks the locale, so only the locale-independent cases are quoted.
    assert_eq!(display_width(b"hello", true, VAR), 5);
    assert_eq!(display_width("中文".as_bytes(), true, VAR), 4);
}

#[test]
fn the_reference_split_line_table() {
    for (text, cells, skip, bytes, eol, width) in [
        ("hello", 80, 1, 5, 0, 5),
        ("hello", 5, 1, 5, 0, 5),
        ("hello", 3, 1, 3, 0, 3),
        ("", 80, 1, 0, 0, 0),
        ("hello", 0, 1, 0, 0, 0),
        ("ab\ncd", 80, 1, 3, 1, 3),
        ("\n", 80, 1, 1, 1, 1),
        ("ab\ncd", 2, 1, 2, 0, 2),
        ("a\u{d}b\u{7}c", 80, 1, 5, 0, 3),
        ("\u{1b}[1;31mred\u{1b}[m", 80, 1, 13, 0, 3),
        ("\u{1b}[1;31mred\u{1b}[m", 80, 0, 13, 0, 13),
        ("中中", 3, 1, 3, 0, 2),
        ("中中", 4, 1, 6, 0, 4),
        ("\u{1b}[1;33m中\u{1b}[mx", 4, 1, 14, 0, 3),
    ] {
        let split = split_line(text.as_bytes(), cells, skip == 1, FIX);
        assert_eq!(
            (split.bytes, split.eol, split.display_len),
            (bytes, eol == 1, width),
            "split_line({text:?}, {cells}, {skip})"
        );
    }
}

#[test]
fn the_reference_split_capacity_table() {
    // (tag, text, cells, C's count, skip, widths, lines, offsets, widths)
    let cases: [(&str, &str, usize, usize, bool, bool, usize, &[u64], &[u32]); 13] = [
        (
            "one short line",
            "hello",
            80,
            4,
            true,
            true,
            1,
            &[0, 5],
            &[5],
        ),
        ("empty buffer", "", 80, 4, true, true, 1, &[0, 0], &[0]),
        ("count 1", "hello", 80, 1, true, true, 0, &[0], &[]),
        ("count 1, empty", "", 80, 1, true, false, 0, &[0], &[]),
        (
            "three lines",
            "a\nb\nc\n",
            80,
            6,
            true,
            true,
            4,
            &[0, 2, 4, 6, 6],
            &[2, 2, 2, 0],
        ),
        (
            "three lines, capped",
            "a\nb\nc\n",
            80,
            4,
            true,
            true,
            3,
            &[0, 2, 4, 6],
            &[2, 2, 2],
        ),
        (
            "wrapped",
            "aaaaaaaaaa",
            4,
            8,
            true,
            true,
            3,
            &[0, 4, 8, 10],
            &[4, 4, 2],
        ),
        (
            "wrapped, capped",
            "aaaaaaaaaa",
            4,
            3,
            true,
            true,
            2,
            &[0, 4, 8],
            &[4, 4],
        ),
        (
            "without widths",
            "hello\nworld",
            80,
            4,
            true,
            false,
            2,
            &[0, 6, 11],
            &[],
        ),
        (
            "break at wrap column",
            "ab\ncd",
            2,
            8,
            true,
            true,
            3,
            &[0, 2, 3, 5],
            &[2, 1, 2],
        ),
        (
            "utf-8 wrap",
            "中中中",
            4,
            8,
            true,
            true,
            2,
            &[0, 6, 9],
            &[4, 2],
        ),
        (
            "utf-8 with color",
            "\u{1b}[1;31m中中\u{1b}[m",
            80,
            4,
            true,
            true,
            1,
            &[0, 16],
            &[4],
        ),
        (
            "utf-8 mixed",
            "A中\u{1b}[0mB",
            4,
            4,
            true,
            true,
            1,
            &[0, 9],
            &[4],
        ),
    ];

    for (tag, text, cells, count, skip, with_widths, lines, offsets, widths) in cases {
        let buf = text.as_bytes();
        let wrapped = split_data_lines(buf, cells, skip, FIX, count - 1);
        assert_eq!(wrapped.count(), lines, "{tag}: lines");
        assert_eq!(wrapped.offsets, offsets, "{tag}: offsets");
        if with_widths {
            assert_eq!(wrapped.widths, widths, "{tag}: widths");
        }
        // C checks this with canary slots around both arrays: `p_line_offsets[0..=lines]` and
        // `p_line_widths[0..lines]` and nothing else.
        assert_eq!(
            wrapped.offsets.len(),
            wrapped.count() + 1,
            "{tag}: offset slots"
        );
        assert_eq!(wrapped.widths.len(), wrapped.count(), "{tag}: width slots");
        assert!(
            wrapped.count() <= count - 1,
            "{tag}: never more than count-1"
        );
        if wrapped.count() < count - 1 {
            assert_eq!(
                wrapped.offsets[wrapped.count()] as usize,
                buf.len(),
                "{tag}: covers the whole text"
            );
        }
        for i in 1..=wrapped.count() {
            assert!(
                wrapped.offsets[i - 1] <= wrapped.offsets[i],
                "{tag}: offset[{i}] grows"
            );
            assert!(
                wrapped.offsets[i] as usize <= buf.len(),
                "{tag}: offset[{i}] inside the buffer"
            );
            if with_widths {
                assert_eq!(
                    wrapped.widths[i - 1] as usize,
                    display_width(wrapped.line(buf, i - 1), skip, FIX),
                    "{tag}: width[{}] of the same slice",
                    i - 1
                );
            }
        }
    }
}

#[test]
fn the_reference_str_filter_table() {
    for (text, skip, want) in [
        ("hello", true, "hello"),
        ("a\u{d}b\u{7}c", true, "abc"),
        ("\u{1b}[1;31mred\u{1b}[m", true, "red"),
        ("\u{1b}[1;31mred\u{1b}[m", false, "\u{1b}[1;31mred\u{1b}[m"),
        ("", true, ""),
        ("a\u{d}b\u{1b}[0mc\u{7}d", true, "abcd"),
    ] {
        let mut buf = text.as_bytes().to_vec();
        str_filter(&mut buf, skip);
        assert_eq!(
            buf,
            want.as_bytes().to_vec(),
            "str_filter({text:?}, {skip})"
        );
        assert_eq!(
            buf.len(),
            display_width(&buf, skip, FIX),
            "{text:?}: filtered text measures its own length"
        );
    }

    // Filtering changes nothing about what is left, so running it twice is stable.
    let mut buf = "a\u{d}b\u{1b}[0mc\u{7}d".as_bytes().to_vec();
    str_filter(&mut buf, true);
    let once = buf.clone();
    str_filter(&mut buf, true);
    assert_eq!(buf, once, "str_filter: idempotent");
}
