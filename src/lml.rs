//! The LML markup renderer — a port of `src/lml.c`.
//!
//! LML is the BBCode-ish dialect the web side stores article bodies in: `[b]`, `[color red]`,
//! `[quote]`, `[url ...]...[/url]`, `[image ...]`, `[bwf]` and friends. Rendering turns that
//! into the ANSI byte stream a terminal page-turner shows, and it is also the line wrapper:
//! the renderer emits hard breaks so that no display line exceeds `width` cells, which is what
//! lets `cache::generate` record a line-offset index over the result.
//!
//! Two C file statics (`lml_tag_disabled`, `lml_tag_quote_level`) are per-render state here;
//! they are reset at every entry in the original, and keeping them local is what makes the
//! function safe to call from many sessions at once.

use crate::paths::TAB_SIZE;
use crate::text;

/// `LML_TAG_NAME_BUF_LEN` — tag names longer than 20 bytes are truncated and then unknown.
pub const TAG_NAME_BUF_LEN: usize = 21;
/// `LML_TAG_PARAM_BUF_LEN` — the tag parameter is cut to this length.
pub const TAG_PARAM_BUF_LEN: usize = 256;
/// `LML_TAG_OUTPUT_BUF_LEN` — the width of one tag's expansion.
pub const TAG_OUTPUT_BUF_LEN: usize = 1024;
/// `LML_TAG_QUOTE_MAX_LEVEL` — deeper nesting stops incrementing the colour index.
pub const TAG_QUOTE_MAX_LEVEL: i64 = 10;
/// `ARTICLE_CONTENT_MAX_LEN` (`article_cache.h:18`), the body buffer `cache::generate` renders into.
pub const ARTICLE_CONTENT_MAX_LEN: usize = 1024 * 1024 * 4;

const QUOTE_COLORS: [&[u8]; 3] = [b"\x1b[33m", b"\x1b[32m", b"\x1b[35m"];
const ANSI_RESET: &[u8] = b"\x1b[m";

/// The tag callbacks of `lml.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// `lml_tag_disable_filter` — stop interpreting tags for the rest of the render.
    Disable,
    /// `lml_tag_color_filter` — named colours to SGR.
    Color,
    /// `lml_tag_quote_filter` — `[quote]`/`[/quote]` nesting depth to SGR.
    Quote,
}

/// `LML_TAG_DEF`. The `Option`s mirror the `NULL` entries of the C table: a `Some("")` tag
/// expands to nothing but is still "defined", which is what decides whether the default
/// parameter applies.
#[derive(Debug, Clone, Copy)]
pub struct TagDef {
    pub name: &'static str,
    pub output: Option<&'static str>,
    pub default_param: Option<&'static str>,
    pub quote_output: Option<&'static str>,
    pub filter: Option<Filter>,
}

const fn tag(
    name: &'static str,
    output: Option<&'static str>,
    default_param: Option<&'static str>,
    quote_output: Option<&'static str>,
    filter: Option<Filter>,
) -> TagDef {
    TagDef {
        name,
        output,
        default_param,
        quote_output,
        filter,
    }
}

/// `lml_tag_def[]`, in the C order and with the C `NULL`/`""` distinction preserved. `%s` in an
/// output is the tag parameter. The C version keys a trie on the lower-cased name, so a tag
/// matches case-insensitively and nothing else does.
pub const TAGS: [TagDef; 38] = [
    tag("plain", None, None, None, Some(Filter::Disable)),
    tag("nolml", Some(""), None, Some(""), None), // deprecated
    tag("lml", Some(""), None, Some(""), None),   // deprecated
    tag("align", Some(""), Some(""), Some(""), None), // N/A
    tag("/align", Some(""), Some(""), Some(""), None), // N/A
    tag("size", Some(""), Some(""), Some(""), None), // N/A
    tag("/size", Some(""), Some(""), Some(""), None), // N/A
    tag("left", Some("["), Some(""), Some("[left]"), None),
    tag("right", Some("]"), Some(""), Some("[right]"), None),
    tag("bold", Some("\x1b[1m"), Some(""), Some(""), None), // does not work in Fterm
    tag("/bold", Some("\x1b[22m"), None, Some(""), None),
    tag("b", Some("\x1b[1m"), Some(""), Some(""), None),
    tag("/b", Some("\x1b[22m"), None, Some(""), None),
    tag("italic", Some("\x1b[5m"), Some(""), Some(""), None), // use blink instead
    tag("/italic", Some("\x1b[m"), None, Some(""), None),     // \x1b[25m does not work in Fterm
    tag("i", Some("\x1b[5m"), Some(""), Some(""), None),
    tag("/i", Some("\x1b[m"), None, Some(""), None),
    tag("underline", Some("\x1b[4m"), Some(""), Some(""), None),
    tag("/underline", Some("\x1b[m"), None, Some(""), None), // \x1b[24m does not work in Fterm
    tag("u", Some("\x1b[4m"), Some(""), Some(""), None),
    tag("/u", Some("\x1b[m"), None, Some(""), None),
    tag("color", None, None, Some(""), Some(Filter::Color)),
    tag("/color", Some("\x1b[m"), None, Some(""), None),
    tag("quote", None, None, Some(""), Some(Filter::Quote)),
    tag("/quote", None, None, Some(""), Some(Filter::Quote)),
    tag("url", Some(""), Some(""), Some(""), None),
    tag("/url", Some("（链接: %s）"), None, Some(""), None),
    tag("link", Some(""), Some(""), Some(""), None),
    tag("/link", Some("（链接: %s）"), None, Some(""), None),
    tag("email", Some(""), Some(""), Some(""), None),
    tag("/email", Some("（Email: %s）"), None, Some(""), None),
    tag("user", Some(""), Some(""), Some(""), None),
    tag("/user", Some("（用户: %s）"), None, Some(""), None),
    tag("article", Some(""), Some(""), Some(""), None),
    tag("/article", Some("（文章: %s）"), None, Some(""), None),
    tag("image", Some("（图片: %s）"), Some(""), Some("%s"), None),
    tag("flash", Some("（Flash: %s）"), Some(""), Some(""), None),
    tag(
        "bwf",
        Some("\x1b[1;31m****\x1b[m"),
        Some(""),
        Some("****"),
        None,
    ),
];

fn find_tag(name: &[u8]) -> Option<usize> {
    TAGS.iter()
        .position(|def| def.name.as_bytes().eq_ignore_ascii_case(name))
}

/// `lml_tag_color_filter`. An unrecognised colour name expands to nothing, so `[color foo]`
/// disappears without even the square brackets.
fn color_filter(param: &[u8]) -> Vec<u8> {
    const NAMED: [(&str, &str); 7] = [
        ("red", "\x1b[1;31m"),
        ("green", "\x1b[1;32m"),
        ("yellow", "\x1b[1;33m"),
        ("blue", "\x1b[1;34m"),
        ("magenta", "\x1b[1;35m"),
        ("cyan", "\x1b[1;36m"),
        ("black", "\x1b[1;30;47m"),
    ];

    NAMED
        .iter()
        .find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(param))
        .map(|(_, sgr)| sgr.as_bytes().to_vec())
        .unwrap_or_default()
}

/// `lml_tag_quote_filter` — the level is this module's per-render state, not a C file static.
fn quote_filter(name: &[u8], level: &mut i64) -> Vec<u8> {
    if name == b"quote" {
        if *level <= TAG_QUOTE_MAX_LEVEL {
            *level += 1;
        }
        QUOTE_COLORS[level.rem_euclid(QUOTE_COLORS.len() as i64) as usize].to_vec()
    } else {
        if *level > 0 {
            *level -= 1;
        }
        if *level > 0 {
            QUOTE_COLORS[level.rem_euclid(QUOTE_COLORS.len() as i64) as usize].to_vec()
        } else {
            ANSI_RESET.to_vec()
        }
    }
}

/// The expansion of one recognised tag: the format string of the current mode, else the
/// callback of the table row.
fn expand_tag(
    def: &TagDef,
    param: &[u8],
    level: &mut i64,
    disabled: &mut bool,
    quote_mode: bool,
) -> Vec<u8> {
    let fmt = if quote_mode {
        def.quote_output
    } else {
        def.output
    };
    match (fmt, def.filter) {
        (Some(fmt), _) => expand(fmt, param),
        (None, Some(Filter::Color)) => color_filter(param),
        (None, Some(Filter::Quote)) => quote_filter(def.name.as_bytes(), level),
        (None, Some(Filter::Disable)) => {
            *disabled = true;
            if quote_mode {
                b"[plain]".to_vec()
            } else {
                Vec::new()
            }
        }
        (None, None) => Vec::new(),
    }
}

/// `snprintf(buf, sizeof buf, fmt, param)` for the constant formats of [`TAGS`]: only `%s`
/// means anything, and the result is cut to the C buffer.
fn expand(fmt: &str, param: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(fmt.len() + param.len());
    let fmt = fmt.as_bytes();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] == b'%' && i + 1 < fmt.len() {
            if fmt[i + 1] == b's' {
                out.extend_from_slice(param);
                i += 2;
                continue;
            }
            if fmt[i + 1] == b'%' {
                out.push(b'%');
                i += 2;
                continue;
            }
        }
        out.push(fmt[i]);
        i += 1;
    }
    out.truncate(TAG_OUTPUT_BUF_LEN - 1);
    out
}

/// `lml_render`. Returns the rendered bytes; `out.len()` is C's return value.
///
/// `width <= 0` means "do not wrap" (`INT_MAX` in C). `buf_len` is the size of the C output
/// buffer, and a render that would reach it stops where it is, as `CHECK_AND_APPEND_OUTPUT`
/// does. C's only negative return is the `!lml_ready` test (`lml.c:267-271`) that de5fa0f and
/// a2f6c10 gave callers a branch for, which a `Vec` return cannot reproduce. `fixed` is the
/// `(V)变宽/(F)定宽` choice (`UTF8_fixed_width`) used to measure the display width of what gets
/// emitted.
// C's append macro bumps the running line width, and the wrap sites then reset it to 0,
// which the port keeps verbatim.
#[allow(unused_assignments)]
pub fn render(input: &[u8], buf_len: usize, width: i64, quote_mode: bool, fixed: bool) -> Vec<u8> {
    let width = if width <= 0 { i64::MAX } else { width };
    let mut out: Vec<u8> = Vec::new();
    let mut disabled = false;
    let mut quote_level: i64 = 0;
    let mut line_width: i64 = 0;
    let mut new_line = true;
    let mut fb_quote_level: i64 = 0;
    let mut tag_start_pos: i64 = -1;
    let mut tag_name_pos: i64 = -1;
    let mut last_i: i64 = -1;
    // `tag_param_buf` outlives a single tag in C, so `[/url]` with no parameter of its own
    // closes with whatever was parsed last. The port keeps that shared buffer.
    let mut param_buf: Vec<u8> = Vec::new();

    macro_rules! emit {
        ($src:expr) => {{
            let src: &[u8] = $src;
            if !src.is_empty() {
                if out.len() + src.len() >= buf_len {
                    // C logs and stops with what it has; the trailing NUL slot is why this is `>=`.
                    return out;
                }
                let at = out.len();
                out.extend_from_slice(src);
                line_width += text::display_width(&out[at..], true, fixed) as i64;
            }
        }};
    }

    let n = input.len() as i64;
    let byte = |i: i64| -> u8 {
        if i < 0 || i >= n {
            0
        } else {
            input[i as usize]
        }
    };

    let mut i: i64 = -1;
    loop {
        i += 1; // the C `for` increment, which even a `continue` passes through
        if i >= n || input[i as usize] == 0 {
            break;
        }

        if !disabled && new_line {
            while byte(i) == b':' && byte(i + 1) == b' ' {
                // FB2000 quote leading str
                fb_quote_level += 1;
                quote_level += 1;
                i += 2;
            }

            if !quote_mode && quote_level > 0 {
                let color =
                    QUOTE_COLORS[(quote_level.rem_euclid(QUOTE_COLORS.len() as i64)) as usize];
                emit!(color);
            }

            let prefix = b": ".repeat(fb_quote_level as usize);
            emit!(&prefix);

            new_line = false;
            i -= 1; // redo at current i
            continue;
        }

        if disabled && new_line {
            new_line = false;
        }

        if !quote_mode && !disabled && byte(i) == b'\x1b' && byte(i + 1) == b'[' {
            // Escape sequence
            let mut k = i + 2;
            while matches!(byte(k), b'0'..=b'9' | b';' | b'?') {
                k += 1;
            }

            let c = byte(k);
            if c == b'm' {
                // valid -- copy directly
                emit!(&input[i as usize..(k + 1) as usize]);
            } else if c.is_ascii_alphabetic() {
                // unsupported ANSI CSI command
            } else {
                k -= 1;
            }

            i = k;
            continue;
        }

        if byte(i) == b'\n' {
            if !disabled && tag_start_pos != -1 {
                // tag is not closed
                if line_width + 1 > width {
                    emit!(b"\n");
                    new_line = true;
                    line_width = 0;
                    i -= 1; // redo at current i
                    continue;
                }

                emit!(b"[");
                i = tag_start_pos; // restart from tag_start_pos + 1
                tag_start_pos = -1;
                tag_name_pos = -1;
                continue;
            }

            if !disabled && fb_quote_level > 0 {
                quote_level -= fb_quote_level;
                emit!(if quote_mode { b"" } else { ANSI_RESET });
                fb_quote_level = 0;
            }

            if new_line {
                continue;
            }

            tag_start_pos = -1;
            tag_name_pos = -1;
            new_line = true;
            line_width = -1;
        } else if byte(i) == b'\r' || byte(i) == b'\x07' {
            continue; // Skip special characters
        } else if byte(i) == b'\t' {
            let tab_width = TAB_SIZE as i64 - (line_width % TAB_SIZE as i64);
            if line_width + tab_width > width {
                emit!(b"\n");
                new_line = true;
                line_width = 0;
                // skip current Tab
                continue;
            }

            emit!(&vec![b' '; tab_width as usize]);
            continue;
        } else if !quote_mode && byte(i) == b'\x1b' {
            continue; // Skip control characters while not in quote mode
        }

        if !disabled && byte(i) == b'[' {
            if tag_start_pos != -1 {
                // tag is not closed
                if line_width + 1 > width {
                    emit!(b"\n");
                    new_line = true;
                    line_width = 0;
                    i -= 1; // redo at current i
                    continue;
                }

                emit!(b"[");
                i = tag_start_pos; // restart from tag_start_pos + 1
                tag_start_pos = -1;
                tag_name_pos = -1;
                continue;
            }

            tag_start_pos = i;
            tag_name_pos = i + 1;
        } else if !disabled && byte(i) == b']' && tag_name_pos >= 0 {
            let tag_end_pos = i;

            // Skip space characters
            let mut name_pos = tag_name_pos;
            while byte(name_pos) == b' ' {
                name_pos += 1;
            }

            let mut name: Vec<u8> = Vec::with_capacity(TAG_NAME_BUF_LEN);
            while name.len() < TAG_NAME_BUF_LEN - 1 && name_pos + (name.len() as i64) < tag_end_pos
            {
                let c = byte(name_pos + name.len() as i64);
                if c == b' ' || c == b']' {
                    break;
                }
                name.push(c.to_ascii_lowercase());
            }

            let found = if name.is_empty() {
                None
            } else {
                find_tag(&name)
            };

            if let Some(k) = found {
                let def = &TAGS[k];
                // C's `case ' ':` computes the parameter and falls through to `case ']':`,
                // which is the only place an expansion happens.
                let after_name = byte(name_pos + def.name.len() as i64);
                let expanded: Vec<u8> = match after_name {
                    b' ' => {
                        let mut p = name_pos + def.name.len() as i64 + 1;
                        while byte(p) == b' ' {
                            p += 1;
                        }
                        // The copy is clamped to the buffer minus the NUL that follows it, which is
                        // what c996502 made C do: at the old length a full parameter wrote one byte
                        // past `tag_param_buf`.
                        let end = tag_end_pos.min(p + TAG_PARAM_BUF_LEN as i64 - 1);
                        let start = p.min(n);
                        param_buf = if end <= start {
                            Vec::new()
                        } else {
                            input[start as usize..end as usize].to_vec()
                        };
                        expand_tag(def, &param_buf, &mut quote_level, &mut disabled, quote_mode)
                    }
                    b']' => {
                        // `lml.c:488`: a tag with both an output format and a default parameter
                        // gets that parameter; otherwise the stale buffer is reused.
                        if def.output.is_some() {
                            if let Some(default) = def.default_param {
                                param_buf = default.as_bytes().to_vec();
                            }
                        }
                        expand_tag(def, &param_buf, &mut quote_level, &mut disabled, quote_mode)
                    }
                    // C logs "Bug: tag name not match" and expands nothing.
                    _ => Vec::new(),
                };

                if matches!(after_name, b' ' | b']') {
                    if line_width + expanded.len() as i64 > width {
                        if i > last_i {
                            last_i = i;
                            emit!(b"\n");
                            new_line = true;
                            line_width = 0;
                            i -= 1; // redo at current i
                            continue;
                        } else {
                            // Output current tag in plain text if line width exceeded
                            continue;
                        }
                    }

                    emit!(&expanded);
                }
            } else {
                // tag_name not found
                if line_width + 1 > width {
                    emit!(b"\n");
                    new_line = true;
                    line_width = 0;
                    i -= 1; // redo at current i
                    continue;
                }

                emit!(b"[");
                i = tag_start_pos; // restart from tag_start_pos + 1
                tag_start_pos = -1;
                tag_name_pos = -1;
                continue;
            }

            tag_start_pos = -1;
            tag_name_pos = -1;
        } else if disabled || tag_name_pos == -1 {
            // not in LML tag
            let lead = byte(i);
            if line_width + if lead & 0x80 != 0 { 2 } else { 1 } > width {
                emit!(b"\n");
                new_line = true;
                line_width = 0;
                i -= 1; // redo at current i
                continue;
            }

            // The lead byte's continuation count comes from the shift rule, which is how the
            // original measures both GBK and UTF-8 without knowing which one it holds.
            let mut char_len: i64 = 1;
            if lead & 0x80 != 0 {
                let mut c = ((lead & 0x70) as i32) << 1;
                while (c as u8) & 0x80 != 0 {
                    if byte(i + char_len) == 0 {
                        break;
                    }
                    char_len += 1;
                    c = (c & 0x7f) << 1;
                }
            }

            let end = (i + char_len).min(n + 1) as usize;
            emit!(&input[i as usize..end.min(n as usize)]);
            i += char_len - 1;
        }
        // else: inside an LML tag, characters are dropped
    }

    if !disabled && tag_start_pos != -1 {
        // tag is not closed
        let tag_end_pos = i - 1;
        let start = tag_start_pos.max(0) as usize;
        let end = (tag_end_pos + 1).clamp(0, n) as usize;
        if end > start {
            emit!(&input[start..end]);
        }
    }

    if !quote_mode && !disabled && quote_level > 0 {
        emit!(ANSI_RESET);
    }

    out
}

#[cfg(test)]
mod tests {
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

    /// Every case `src/test_lml.c` exercises, rendered by the C code and recorded here as hex so
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
}
