use super::*;
use crate::paths::SCREEN_COLS;

fn render0(s: &str) -> String {
    String::from_utf8_lossy(&render(
        s.as_bytes(),
        ARTICLE_CONTENT_MAX_LEN,
        SCREEN_COLS as i64,
        false,
        true,
    ))
    .into_owned()
}

fn render1(s: &str) -> String {
    String::from_utf8_lossy(&render(
        s.as_bytes(),
        ARTICLE_CONTENT_MAX_LEN,
        SCREEN_COLS as i64,
        true,
        true,
    ))
    .into_owned()
}

#[test]
fn quote_mode_keeps_the_markup_visible() {
    // What the editor quotes back is the source with the tag names spelled out, so that
    // re-posting it round-trips.
    assert_eq!(render1("[left]A[/b][plain]x"), "[left]A[plain]x");
    assert_eq!(render1("[image http://x/]"), "http://x/");
    assert_eq!(render1("[bwf]y"), "****y");
    assert_eq!(render1("a\tb"), "a   b");
    // Quote mode keeps escape sequences, which is how a quoted article stays coloured.
    assert_eq!(render1("\x1b[33mq\x1b[m"), "\x1b[33mq\x1b[m");
    assert_eq!(render1("A[url BC DE]x[/url]"), "Ax");
    assert_eq!(render1("[b]bold[/b] plain"), "bold plain");
}

#[test]
fn tags_expand_to_ansi() {
    assert_eq!(render0("[left]ABCD[right]EFG"), "[ABCD]EFG");
    assert_eq!(
        render0("A[u]B[italic]CD[/i]E[/u]F[b]G[/bold]"),
        "A\x1b[4mB\x1b[5mCD\x1b[mE\x1b[mF\x1b[1mG\x1b[22m"
    );
    assert_eq!(render0("AB[bwf]CDEFG"), "AB\x1b[1;31m****\x1b[mCDEFG");
}

#[test]
fn named_colours_and_unknown_ones() {
    assert_eq!(render0("[color red]R[/color]"), "\x1b[1;31mR\x1b[m");
    assert_eq!(render0("[color BLACK]k[/Color]"), "\x1b[1;30;47mk\x1b[m");
    // An unknown colour name expands to nothing at all.
    assert_eq!(render0("[color sepia]x[/color]"), "x\x1b[m");
}

#[test]
fn closing_tags_carry_their_parameter() {
    assert_eq!(
        render0("A[url BC DE]测试a网址[/url]FG"),
        "A测试a网址（链接: BC DE）FG"
    );
    assert_eq!(render0("A[ image  BCD]EFG"), "A（图片: BCD）EFG");
}

#[test]
fn plain_stops_tag_processing() {
    assert_eq!(
        render0("[color red]Red[/color][plain][color blue]Blue[/color][plain]"),
        "\x1b[1;31mRed\x1b[m[color blue]Blue[/color][plain]"
    );
}

#[test]
fn unclosed_tag_restarts_one_character_in() {
    // `[a[b]` is not a tag, so the first bracket is literal and `[b]` still applies.
    assert_eq!(render0("x[a[b]y"), "x[a\x1b[1my");
    // An unknown name costs the opening bracket and nothing else: the rest is text.
    assert_eq!(render0("[lef]A[right]BCD"), "[lef]A]BCD");
}

#[test]
fn quote_nesting_cycles_three_colours() {
    // The level is 1-based, so the outermost quote is green, not yellow.
    assert_eq!(
        render0("[quote]a[/quote]b[quote]c[/quote]"),
        "\x1b[32ma\x1b[mb\x1b[32mc\x1b[m"
    );
}

#[test]
fn fb2000_quote_prefix_is_reemitted() {
    // The colour is chosen by the depth (2 = magenta) and precedes the re-emitted prefix.
    assert_eq!(
        render0(": : quoted\nplain"),
        "\x1b[35m: : quoted\x1b[m\nplain"
    );
}

#[test]
fn tabs_and_ansi_passthrough() {
    assert_eq!(render0("a\tb"), "a   b");
    assert_eq!(
        render0("\x1b[1;35;42mABC\x1b[0m"),
        "\x1b[1;35;42mABC\x1b[0m"
    );
    // A lone escape is dropped outside quote mode.
    assert_eq!(render0("a\x1b[1mb"), "a\x1b[1mb");
}

#[test]
fn long_lines_wrap_at_the_width() {
    let line = "A".repeat(200);
    let out = render0(&line);
    assert_eq!(
        out.lines().map(|l| l.chars().count()).collect::<Vec<_>>(),
        vec![80, 80, 40]
    );
}

#[test]
fn a_tag_too_wide_for_the_remainder_is_deferred_then_dropped() {
    let line = format!("{}[b]", "A".repeat(79));
    assert_eq!(render0(&line), format!("{}\n\x1b[1m", "A".repeat(79)));
}

#[test]
fn output_stops_when_the_buffer_is_full() {
    let input = "abcdefghij";
    let out = render(input.as_bytes(), 5, 80, false, true);
    assert_eq!(&out, b"abcd");
}

#[test]
fn width_zero_disables_wrapping() {
    let line = "A".repeat(300);
    assert_eq!(render(line.as_bytes(), 4096, 0, false, true).len(), 300);
}

/// Every case `tests/test_lml.c` exercises, rendered by the C code and recorded here as hex so
/// the port can be diffed against the original.
#[test]
fn matches_the_c_golden_output() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lml_golden.txt");
    let text = std::fs::read_to_string(path).expect("golden fixture");
    let mut expected: Option<(usize, usize, Vec<u8>)> = None;
    let mut checked = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("CASE ") {
            let mut it = rest.split_whitespace();
            let idx: usize = it.next().unwrap().parse().unwrap();
            it.next(); // "mode"
            let mode: usize = it.next().unwrap().parse().unwrap();
            it.next(); // "ret"
            it.next(); // the C return value, which is out.len() here
            expected = Some((idx, mode, Vec::new()));
        } else if let Some(hex) = line.strip_prefix("in=") {
            let (idx, mode, _) = expected.as_ref().unwrap();
            let got = render(&decode_hex(hex), 4096, 80, *mode == 1, false);
            expected = Some((*idx, *mode, got));
        } else if let Some(hex) = line.strip_prefix("out=") {
            let (idx, mode, got) = expected.take().unwrap();
            let want = decode_hex(hex);
            assert_eq!(got, want, "case {idx} mode {mode}");
            checked += 1;
        }
    }

    assert_eq!(checked, 52);
}

fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}
