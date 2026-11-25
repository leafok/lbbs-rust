//! The article cache — a port of `src/article_cache.c`.
//!
//! The web application owns the `article` table; the terminal server cannot render a body from
//! SQL rows fast enough, so each article is materialised once as `var/articles/<cid>`: an
//! LML-rendered body wrapped at 80 columns, a generated header and footer, and a byte-offset
//! index of the display lines. Reading the file back gives the pager a random-access line index
//! without any work.
//!
//! The on-disk layout is the raw C struct followed by the data:
//!
//! ```text
//! p_mmap      i64   always 0 on disk (it is a runtime pointer)
//! mmap_len    u64   the file size
//! p_data      i64   always 0 on disk (runtime pointer into the mapping)
//! data_len    u64   the byte length of the data region
//! line_total  i64   the number of display lines
//! line_offsets[line_total + 1]   i64, byte offsets into the data region
//! data        data_len bytes: header, body, footer
//! ```
//!
//! The prefix is native-endian 8-byte words because C `write(2)`s the struct image. The web
//! side does not read these files and the two servers are meant to run on the same host, so
//! native order is the compatible choice.
//!
//! # Deviations from C
//!
//! * The header and footer are cut on a UTF-8 boundary; `snprintf` cuts at an arbitrary byte.
//! * The cache buffer C keeps as a `static` (`content_f`) is a local here, which is what lets
//!   two sessions generate caches at the same time.
//! * A negative `aid` cannot index the footer colour table out of bounds: the modulo is
//!   Euclidean. C reads before the start of the array.
//! * An unreadable cache file is one error here. C splits the `-2`s of `article_cache.c:250-277`
//!   (open, `fstat`, `mmap`, close) from the `-3` of `:280-285` (the length inside the file does
//!   not describe it), and both of its callers branch on `< 0` and log one line, so the split
//!   earns no second error type.

use std::fs::OpenOptions;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::board::{Article, Section};
use crate::lml;
use crate::paths::{
    ARTICLE_TITLE_MAX_LEN, FILE_PATH_LEN, MAX_SPLIT_FILE_LINES, NICKNAME_MAX_LEN, SCREEN_COLS,
    USERNAME_MAX_LEN,
};
use crate::text;

/// `ARTICLE_HEADER_MAX_LEN`: the `snprintf` buffer of the C header.
pub const ARTICLE_HEADER_MAX_LEN: usize = 4096;
/// `ARTICLE_FOOTER_MAX_LEN`: the `snprintf` buffer of the C footer.
pub const ARTICLE_FOOTER_MAX_LEN: usize = 4096;

/// `BBS_article_footer_color[]`, selected by `aid % 7`.
const FOOTER_COLORS: [&str; 7] = [
    "\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[35m", "\x1b[36m", "\x1b[37m",
];

/// The fixed fields of `struct article_cache_t` ahead of the offset table, in words.
const PREFIX_WORDS: usize = 5;
const WORD: usize = 8;

/// `%s%d`: in C the directory is the literal `var/articles/`, so the separator comes from the
/// constant. The callers here hand over a `PathBuf` spelling instead, and on Windows that one ends
/// with no separator at all — `canonicalize()` returns a `\\?\`-prefixed path and `join` folds
/// `var/articles/` down to `var\articles`. Concatenating unconditionally then writes the cache of
/// cid 9001 beside the directory, as `var\articles9001`, so the separator is added when missing.
pub fn path(cache_dir: &str, cid: i32) -> String {
    let sep = match cache_dir.ends_with(['/', '\\']) {
        true => "",
        false => std::path::MAIN_SEPARATOR_STR,
    };
    let joined = format!("{cache_dir}{sep}{cid}");
    joined.chars().take(FILE_PATH_LEN - 1).collect()
}

fn field(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `SUB_DT_MAX_LEN`: the `str_sub_dt` buffer of the C header.
const SUB_DT_MAX_LEN: usize = 50;

/// `localtime_r` plus `strftime("%c %Z", …)` into the 51-byte `str_sub_dt`.
///
/// This goes to libc rather than to `chrono` because both the field order and the time zone
/// abbreviation come from the locale: `chrono` renders the zone of `Local` as `+08:00` where
/// `glibc` writes `CST`, and the header is the one string in a cache file that a reader can see
/// without parsing anything. [`crate::daemon::apply_locale`] pins that locale to the
/// `en_US.UTF-8` C installs at startup, because the glibc `C` locale's `%c` puts the year last
/// and every header would otherwise differ from the file the C loader wrote for the same post.
#[cfg(unix)]
fn submission_time(sub_dt: i64) -> String {
    unsafe {
        let when = sub_dt as libc::time_t;
        let mut broken: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&when, &mut broken).is_null() {
            return String::new();
        }

        let mut buf = [0u8; SUB_DT_MAX_LEN + 1];
        let len = libc::strftime(
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            c"%c %Z".as_ptr(),
            &broken,
        );
        String::from_utf8_lossy(&buf[..len.min(buf.len() - 1)]).into_owned()
    }
}

/// The Windows build has no `strftime`, so nothing here reads the process locale: `chrono` spells
/// the layout out by hand instead. `%a %b %e %H:%M:%S %Y` is the `%c` the fixtures under
/// `tests/fixtures/articles` were cut with, and the zone is appended the only way `chrono` can name
/// a `Local` one — as a UTC offset, since the abbreviation needs a zone database it does not query.
/// So a post cached on Windows reads `Wed Nov 15 06:13:20 2023 +0800` where the same post cached on
/// a Unix station reads `Wed Nov 15 06:13:20 2023 CST`. Nothing in the port parses this field, but
/// the two spellings do differ, which is why the byte-for-byte check against the C fixtures is a
/// Unix-only test.
#[cfg(not(unix))]
fn submission_time(sub_dt: i64) -> String {
    use chrono::{Local, TimeZone};

    Local
        .timestamp_opt(sub_dt, 0)
        .single()
        .map(|when| {
            when.format("%a %b %e %H:%M:%S %Y %Z")
                .to_string()
                .chars()
                .take(SUB_DT_MAX_LEN)
                .collect()
        })
        .unwrap_or_default()
}

/// The `发布者:` block, cut to the C buffer and then to the display-line limit by the caller.
fn header(article: &Article, section: &Section, bbs_name: &str, sub_dt: &str) -> Vec<u8> {
    let text = format!(
        "发布者: {} ({}), 版块: {} ({})\n标  题: {}\n发布于: {} ({})\n\n",
        field(&article.username, USERNAME_MAX_LEN),
        field(&article.nickname, NICKNAME_MAX_LEN),
        section.sname,
        section.stitle,
        field(&article.title, ARTICLE_TITLE_MAX_LEN),
        bbs_name,
        sub_dt,
    );
    // `snprintf` keeps the last byte of the buffer for the NUL.
    truncate_bytes(text.as_bytes(), ARTICLE_HEADER_MAX_LEN - 1)
}

/// The `--` signature line.
fn footer(article: &Article, bbs_name: &str, bbs_server: &str, sub_ip: &str) -> Vec<u8> {
    let color = FOOTER_COLORS[article.aid.rem_euclid(FOOTER_COLORS.len() as i32) as usize];
    let text = format!(
        "\n--\n{color}※ 来源: {} https://{} [FROM: {}]\x1b[m\n\n",
        bbs_name, bbs_server, sub_ip
    );
    truncate_bytes(text.as_bytes(), ARTICLE_FOOTER_MAX_LEN - 1)
}

fn truncate_bytes(bytes: &[u8], max: usize) -> Vec<u8> {
    if bytes.len() <= max {
        return bytes.to_vec();
    }
    let mut end = max;
    while end > 0 && bytes[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    bytes[..end].to_vec()
}

/// `article_cache_generate`. `Ok(false)` is the C early return: the cache exists and
/// `overwrite` was not asked for.
///
/// `fixed` is the `(V)变宽/(F)定宽` width mode used to measure the wrapped lines. C takes it
/// from the `UTF8_fixed_width` global of whichever session posted the article; the port asks
/// the caller to say so, because a cache file is shared by every session.
pub fn generate(
    cache_dir: &str,
    bbs_name: &str,
    bbs_server: &str,
    article: &Article,
    section: &Section,
    content: &[u8],
    sub_ip: &str,
    overwrite: bool,
    fixed: bool,
) -> io::Result<bool> {
    let data_file = path(cache_dir, article.cid);

    if !overwrite && Path::new(&data_file).exists() {
        return Ok(false);
    }

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The mode C passes to `open`, which has no Windows counterpart; the ACL a fresh file
    // inherits from its directory stands in for it there.
    #[cfg(unix)]
    options.mode(0o640);
    let mut file = options.open(&data_file)?;

    let mut header = header(article, section, bbs_name, &submission_time(article.sub_dt));
    let mut footer = footer(article, bbs_name, bbs_server, sub_ip);
    let body = lml::render(
        content,
        lml::ARTICLE_CONTENT_MAX_LEN,
        SCREEN_COLS as i64,
        false,
        fixed,
    );

    // The offset table is filled the way C fills `cache.line_offsets`: the header split owns
    // indices `0..=header_lines`, the body split is written over it from `header_lines`, and
    // the footer overwrites the last of those again. Each part re-bases its own relative
    // offsets, and a part whose final offset falls short of its length was wrapped away.
    //
    // C hands `split_data_lines` the number of *slots* in that table and stops one line before
    // it fills them (`str_process.c:203`), so the port asks for one line fewer than the table
    // has places. A post that long is rare, but it decides how many bytes the body keeps:
    // 65535 lines here, exactly as in a file the C loader wrote.
    let header_lines =
        text::split_data_lines(&header, SCREEN_COLS, true, fixed, MAX_SPLIT_FILE_LINES - 1);
    if let Some(&cut) = header_lines.offsets.get(header_lines.count()) {
        header.truncate(cut as usize);
    }
    let header_len = header.len();

    let body_lines = text::split_data_lines(
        &body,
        SCREEN_COLS + 1,
        true,
        fixed,
        (MAX_SPLIT_FILE_LINES - 1).saturating_sub(header_lines.count()),
    );
    // C compares the rendered length with the last offset and keeps the smaller of the two,
    // which is what cuts a body that does not fit in the offset table.
    let body_end = header_len + body_lines.offsets[body_lines.count()] as usize;

    let mut line_offsets: Vec<i64> =
        Vec::with_capacity(header_lines.count() + body_lines.count() + 1);
    line_offsets.extend(header_lines.offsets.iter().map(|&o| o as i64));
    line_offsets.extend(
        body_lines
            .offsets
            .iter()
            .skip(1)
            .map(|&o| (header_len + o as usize) as i64),
    );
    let line_total = header_lines.count() + body_lines.count();

    let footer_lines = text::split_data_lines(
        &footer,
        SCREEN_COLS,
        true,
        fixed,
        (MAX_SPLIT_FILE_LINES - 1).saturating_sub(line_total),
    );
    if let Some(&cut) = footer_lines.offsets.get(footer_lines.count()) {
        footer.truncate(cut as usize);
    }
    line_offsets.extend(
        footer_lines
            .offsets
            .iter()
            .skip(1)
            .map(|&o| (body_end + o as usize) as i64),
    );

    let line_total = line_total + footer_lines.count();
    let data_len = body_end + footer.len();
    // `sizeof(cache) - sizeof(long) * (MAX_SPLIT_FILE_LINES - line_total - 1) + data_len`
    let mmap_len = (PREFIX_WORDS + line_total + 1) * WORD + data_len;

    let mut prefix = Vec::with_capacity(mmap_len - data_len);
    prefix.extend_from_slice(&0u64.to_ne_bytes()); // p_mmap
    prefix.extend_from_slice(&(mmap_len as u64).to_ne_bytes());
    prefix.extend_from_slice(&0u64.to_ne_bytes()); // p_data
    prefix.extend_from_slice(&(data_len as u64).to_ne_bytes());
    prefix.extend_from_slice(&(line_total as i64).to_ne_bytes());
    for offset in &line_offsets {
        prefix.extend_from_slice(&offset.to_ne_bytes());
    }

    file.write_all(&prefix)?;
    file.write_all(&header)?;
    file.write_all(&body[..body_end - header_len])?;
    file.write_all(&footer)?;
    file.flush()?;

    Ok(true)
}

/// A cache file read into memory: `ARTICLE_CACHE` plus the mapping it points into.
pub struct Cached {
    pub mmap_len: usize,
    pub data_len: usize,
    pub line_total: usize,
    /// `line_offsets`: `line_total + 1` byte offsets into [`Cached::data`].
    pub line_offsets: Vec<i64>,
    /// `p_data`: header, body and footer.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Cached {
    /// The body of an article is not something a test failure should print whole.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cached")
            .field("mmap_len", &self.mmap_len)
            .field("data_len", &self.data_len)
            .field("line_total", &self.line_total)
            .finish_non_exhaustive()
    }
}

impl Cached {
    /// The offset index in the shape [`text::Wrapped`] wants, for `screen::display_data`.
    pub fn wrapped(&self, fixed: bool) -> text::Wrapped {
        let offsets: Vec<u64> = self.line_offsets.iter().map(|&o| o as u64).collect();
        let widths = (0..self.line_total)
            .map(|i| {
                let start = (self.line_offsets[i] as usize).min(self.data.len());
                let end = (self.line_offsets[i + 1] as usize).min(self.data.len());
                text::display_width(&self.data[start..end], true, fixed) as u32
            })
            .collect();
        text::Wrapped { offsets, widths }
    }
}

/// `article_cache_load`. `Ok(None)` means the cache file is not there yet — C's `-2` out of
/// `open(2)` (`article_cache.c:250-253`), and the caller's cue to generate it. A file that is
/// there but is not a cache is an error: C's other `-2`s, for `fstat`, for `mmap`, and for the
/// mapping `mmap(2)` refuses on a file of no bytes, join its `-3`s in the one arm every caller
/// takes (`menu_proc.c:231-234`, `section_list_display.c:774-777`).
pub fn load(cache_dir: &str, cid: i32) -> io::Result<Option<Cached>> {
    let data_file = path(cache_dir, cid);
    let bytes = match std::fs::read(&data_file) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    if bytes.len() < PREFIX_WORDS * WORD {
        return Err(invalid(&data_file, "shorter than the struct prefix"));
    }

    let word = |i: usize| i64::from_ne_bytes(bytes[i * WORD..i * WORD + WORD].try_into().unwrap());
    let mmap_len = word(1);
    let data_len = word(3);
    let line_total = word(4);

    if mmap_len != bytes.len() as i64 {
        // `article_cache.c:280-285` (`29eb758`), which is what catches a file truncated by a crash
        // mid-write. Until that commit C named the caller's struct in the message, which is the
        // value this fills a few lines later, so both `%ld`s could print the same number; the
        // length read from the mapping is the one this prints.
        return Err(invalid(
            &data_file,
            &format!("inconsistent mmap len, {mmap_len} != {}", bytes.len()),
        ));
    }
    let line_total = usize::try_from(line_total).unwrap_or(usize::MAX);
    let prefix_len = (PREFIX_WORDS + line_total + 1)
        .checked_mul(WORD)
        .filter(|len| (PREFIX_WORDS * WORD..=bytes.len()).contains(len))
        .ok_or_else(|| invalid(&data_file, "offset table does not fit"))?;
    let data_len = usize::try_from(data_len)
        .ok()
        .filter(|len| *len == bytes.len() - prefix_len)
        .ok_or_else(|| invalid(&data_file, "data length disagrees with the file size"))?;

    let line_offsets = bytes[(PREFIX_WORDS * WORD)..prefix_len]
        .chunks_exact(WORD)
        .map(|w| i64::from_ne_bytes(w.try_into().unwrap()))
        .collect();

    Ok(Some(Cached {
        mmap_len: mmap_len as usize,
        data_len,
        line_total,
        line_offsets,
        data: bytes[prefix_len..prefix_len + data_len].to_vec(),
    }))
}

fn invalid(file: &str, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{file}: {why}"))
}

/// `article_cache_unload` is a `munmap`; holding a `Vec` needs no counterpart. Deleting the
/// file is what `article_del` does, so it is here too.
pub fn remove(cache_dir: &str, cid: i32) -> io::Result<()> {
    match std::fs::remove_file(path(cache_dir, cid)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article(cid: i32, aid: i32) -> Article {
        Article {
            aid,
            tid: 12340,
            sid: 3,
            cid,
            uid: 77,
            visible: true,
            username: "tester".into(),
            nickname: "测试者".into(),
            title: "Re: 一个标题 [test]".into(),
            sub_dt: 1700000000,
            ..Default::default()
        }
    }

    fn section() -> Section {
        Section::new(3, "Test", "测试版", "")
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/articles/");
    const CONTENTS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/article_contents/"
    );

    /// The shape the daemon hands over: `root.join(VAR_ARTICLE_CACHE_DIR)` rendered as a string,
    /// which on Windows has no trailing separator. Appending one here would hide the difference
    /// from [`path`].
    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("lbbs-cache-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn paths_match_the_c_naming() {
        assert_eq!(path("var/articles/", 1234), "var/articles/1234");
    }

    /// The failure [`path`] guards: a directory with no trailing separator still has to hold the
    /// file. Writing beside it round-trips through [`load`], so only a listing can see it — and
    /// [`temp_dir`] empties the directory first, so a cid in there is one this test put there.
    #[test]
    fn a_generated_cache_lands_inside_the_directory() {
        let dir = temp_dir("inside");
        let content = std::fs::read(format!("{CONTENTS}c1.txt")).unwrap();
        generate(
            &dir,
            "MyBBS",
            "www.bbs.test",
            &article(9001, 12345),
            &section(),
            &content,
            "203.0.113.7",
            true,
            false,
        )
        .unwrap();

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["9001"]);
    }

    #[test]
    fn the_cache_file_is_inside_the_directory_however_the_directory_is_spelled() {
        for dir in ["var/articles/", "var/articles"] {
            let joined = path(dir, 9001);
            let file = Path::new(&joined);
            assert_eq!(file.file_name().and_then(|n| n.to_str()), Some("9001"));
            assert_eq!(
                file.parent(),
                Some(Path::new("var/articles")),
                "{dir} put the file elsewhere"
            );
        }
    }

    /// The four `tests/fixtures/articles` files were written by `article_cache_generate` in the
    /// C tree, so loading them exercises this reader against the real writer.
    #[test]
    fn reads_cache_files_written_by_c() {
        for cid in 9001..=9004 {
            let cache = load(FIXTURES, cid)
                .unwrap_or_else(|err| panic!("load {cid}: {err}"))
                .unwrap();
            assert_eq!(cache.data.len(), cache.data_len);
            assert_eq!(cache.line_offsets.len(), cache.line_total + 1);
            assert_eq!(cache.line_offsets[0], 0);
            assert_eq!(
                cache.line_offsets[cache.line_total] as usize,
                cache.data_len
            );
            assert!(
                cache.line_offsets.windows(2).all(|w| w[0] <= w[1]),
                "cid {cid}: offsets are not monotone"
            );
            let text = String::from_utf8_lossy(&cache.data);
            assert!(
                text.starts_with("发布者: tester (测试者), 版块: Test (测试版)\n"),
                "cid {cid}"
            );
            assert!(
                text.contains("※ 来源: MyBBS https://www.bbs.test [FROM: 203.0.113.7]"),
                "cid {cid}"
            );
        }

        let short = load(FIXTURES, 9001).unwrap().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&short.data),
            "发布者: tester (测试者), 版块: Test (测试版)\n标  题: Re: 一个标题 [test]\n\
             发布于: MyBBS (Wed Nov 15 06:13:20 2023 CST)\n\nHello world\n\n--\n\
             \x1b[35m※ 来源: MyBBS https://www.bbs.test [FROM: 203.0.113.7]\x1b[m\n\n"
        );
        // The footer colour follows `aid % 7`, and 12345 % 7 == 4.
        assert!(String::from_utf8_lossy(&short.data).contains("\x1b[35m※"));
    }

    #[test]
    fn the_page_index_of_a_c_file_slices_its_data() {
        let cache = load(FIXTURES, 9002).unwrap().unwrap();
        let lines = cache.wrapped(false);
        let body: Vec<String> = (0..lines.count())
            .map(|i| String::from_utf8_lossy(lines.line(&cache.data, i)).into_owned())
            .collect();
        assert!(
            body.contains(&"A\x1b[4mB\x1b[5mCD\x1b[mE\x1b[m\n".to_string()),
            "{body:?}"
        );
        assert!(body.iter().any(|l| l.contains("中文测试")), "{body:?}");
        assert!(
            body.iter().any(|l| l.starts_with("\x1b[32m: 引用一行")),
            "{body:?}"
        );
        assert!(
            body.iter()
                .any(|l| l.contains("（图片: https://x.example/a.png）")),
            "{body:?}"
        );
    }

    #[test]
    fn a_rejected_file_is_reported_instead_of_panicking() {
        let dir = temp_dir("reject");
        std::fs::write(path(&dir, 7), b"not a cache file at all, but long enough").unwrap();
        assert_eq!(
            load(&dir, 7).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "mmap_len does not match the size"
        );

        std::fs::write(path(&dir, 8), b"short").unwrap();
        assert_eq!(
            load(&dir, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(load(&dir, 9).unwrap().is_none());
    }

    /// The fixtures are the bytes the C generator wrote, and its `strftime("%c %Z")` spells the
    /// zone `CST` where [`submission_time`] can only write a UTC offset on Windows. Everything
    /// except that token is meant to agree on both platforms, so this stays a Unix-only check.
    /// That token is also the host's own, so the check needs the zone the fixtures were cut in:
    /// `TZ=Asia/Shanghai`, which is what the CI and release jobs set for their test steps.
    #[cfg(unix)]
    #[test]
    fn generates_the_same_bytes_the_c_generator_does() {
        let dir = temp_dir("generate");

        for (cid, content_file) in [
            (9001, "c1.txt"),
            (9002, "c2.txt"),
            (9003, "c3.txt"),
            (9004, "c4.txt"),
        ] {
            let content = std::fs::read(format!("{CONTENTS}{content_file}")).unwrap();
            assert!(
                generate(
                    &dir,
                    "MyBBS",
                    "www.bbs.test",
                    &article(cid, 12345),
                    &section(),
                    &content,
                    "203.0.113.7",
                    true,
                    false,
                )
                .unwrap(),
                "cid {cid} was not written"
            );

            let mine = std::fs::read(path(&dir, cid)).unwrap();
            let theirs = std::fs::read(format!("{FIXTURES}{cid}")).unwrap();
            assert_eq!(mine.len(), theirs.len(), "cid {cid} length differs");
            if mine != theirs {
                let at = mine
                    .iter()
                    .zip(theirs.iter())
                    .position(|(a, b)| a != b)
                    .unwrap();
                let window = at.saturating_sub(32);
                panic!(
                    "cid {cid} differs at byte {at}:\n  ours: {:?}\n  C:    {:?}",
                    &mine[window..(at + 32).min(mine.len())],
                    &theirs[window..(at + 32).min(theirs.len())]
                );
            }
        }
    }

    #[test]
    fn an_existing_cache_is_kept_unless_overwritten() {
        let dir = temp_dir("keep");
        let article = article(4242, 1);

        generate(
            &dir,
            "MyBBS",
            "www.bbs.test",
            &article,
            &section(),
            b"first",
            "10.0.0.1",
            true,
            true,
        )
        .unwrap();
        let file = path(&dir, 4242);
        let before = std::fs::read(&file).unwrap();
        assert!(
            !generate(
                &dir,
                "MyBBS",
                "www.bbs.test",
                &article,
                &section(),
                b"second",
                "10.0.0.1",
                false,
                true
            )
            .unwrap()
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(
            generate(
                &dir,
                "MyBBS",
                "www.bbs.test",
                &article,
                &section(),
                b"second",
                "10.0.0.1",
                true,
                true
            )
            .unwrap()
        );
        assert_ne!(std::fs::read(&file).unwrap(), before);

        let cache = load(&dir, 4242).unwrap().unwrap();
        let body = String::from_utf8_lossy(&cache.data);
        assert!(body.contains("second"));
        assert!(!body.contains("first"));

        remove(&dir, 4242).unwrap();
        remove(&dir, 4242).unwrap();
        assert!(load(&dir, 4242).unwrap().is_none());
    }

    #[test]
    fn the_page_index_slices_a_generated_file() {
        let dir = temp_dir("index");
        generate(
            &dir,
            "MyBBS",
            "www.bbs.test",
            &article(5150, 3),
            &section(),
            b"one\ntwo\n",
            "10.0.0.2",
            true,
            true,
        )
        .unwrap();

        let cache = load(&dir, 5150).unwrap().unwrap();
        let lines = cache.wrapped(true);
        assert_eq!(lines.count(), cache.line_total);
        let joined: Vec<String> = (0..lines.count())
            .map(|i| String::from_utf8_lossy(lines.line(&cache.data, i)).into_owned())
            .collect();
        assert!(joined.iter().any(|l| l == "one\n"), "{joined:?}");
        assert!(joined.iter().any(|l| l == "two\n"), "{joined:?}");
    }

    #[test]
    fn a_body_beyond_the_line_limit_is_cut_not_corrupted() {
        let dir = temp_dir("huge");
        let content = "x\n".repeat(MAX_SPLIT_FILE_LINES * 2);

        generate(
            &dir,
            "MyBBS",
            "www.bbs.test",
            &article(6000, 1),
            &section(),
            content.as_bytes(),
            "10.0.0.3",
            true,
            true,
        )
        .unwrap();
        let cache = load(&dir, 6000).unwrap().unwrap();
        assert_eq!(
            cache.line_total,
            MAX_SPLIT_FILE_LINES - 1,
            "C fills the offset table to one line short of its slots"
        );
        assert_eq!(
            cache.line_offsets[cache.line_total] as usize,
            cache.data_len
        );
        assert_eq!(
            cache.mmap_len,
            std::fs::metadata(path(&dir, 6000)).unwrap().len() as usize
        );
    }

    // ---- The reference suite's own reader of a terminal ----------------------------------------

    // `test_article_cache.c`, added whole at `29eb758`, never lets the writer grade its own work:
    // it builds the header, the footer and the rendered body from the article by hand, splits them
    // with a line-reader of its own, and only then looks at the file. The helpers below are that
    // model transcribed. Reusing [`crate::text`] or [`generate`] here would prove nothing, because
    // they are what the model is checking.
    //
    // Where the reference's character model and the daemon's differ is the width of a multibyte
    // character: `test_article_cache.c:322-385` charges two columns for every byte above `0x7F`,
    // while [`crate::text::multibyte_width`] asks for the width of the code point unless the
    // fixed-width mode is on. Han characters, which is all these documents use, are two columns
    // either way.

    /// `SUB_DT`, the one submission time the suite needs.
    const SUB_DT: i64 = 1700000000;
    /// `BBS_name` and `BBS_server`, which the reference fixes at the top of its file: the
    /// [[no-db-name-in-code-or-docs]] rule keeps the names here neutral.
    const SITE: &str = "MyBBS";
    const SERVER: &str = "www.bbs.test";
    const SUB_IP: &str = "1.2.3.4";

    /// `HEADER_MAX_COLS` / `BODY_MAX_COLS` / `FOOTER_MAX_COLS`: the body has one column more than
    /// the screen, because the render breaks a line a column before it runs out and the line feed
    /// it writes is then drawn in the column the rest of the screen would have held.
    const HEADER_MAX_COLS: usize = SCREEN_COLS;
    const BODY_MAX_COLS: usize = SCREEN_COLS + 1;
    const FOOTER_MAX_COLS: usize = SCREEN_COLS;

    const REGION_HEADER: usize = 0;
    const REGION_BODY: usize = 1;
    const REGION_FOOTER: usize = 2;

    /// `FILE_BUF_LEN`: the longest file the reference reads back with its own code.
    const REF_FILE_BUF_LEN: usize = 1 << 20;

    /// `footer_colors[]` (`test_article_cache.c:106`), the reference's copy of the table.
    const REF_FOOTER_COLORS: [&str; 7] = [
        "\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[34m", "\x1b[35m", "\x1b[36m", "\x1b[37m",
    ];

    /// `unit_at` (`test_article_cache.c:322-385`): the bytes and the columns of what sits at the
    /// front of a run of text as a terminal draws it. An escape sequence costs nothing, a wide
    /// character two columns, a line feed the one column it is drawn in, a carriage return or a
    /// bell nothing at all.
    fn unit_at(p: &[u8], skip_ctrl_seq: bool) -> (usize, usize) {
        let Some(&first) = p.first() else {
            return (0, 0);
        };
        if first == b'\r' || first == b'\x07' {
            return (1, 0);
        }
        if skip_ctrl_seq && first == 0x1b && p.len() > 1 && p[1] == b'[' {
            let mut i = 2;
            while i < p.len() && p[i] != b'm' {
                i += 1;
            }
            return (if i < p.len() { i + 1 } else { p.len() }, 0);
        }
        if first >= 0x80 {
            let bytes = match first {
                c if c & 0xf8 == 0xf0 => 4,
                c if c & 0xf0 == 0xe0 => 3,
                c if c & 0xe0 == 0xc0 => 2,
                _ => 1,
            };
            return (bytes.min(p.len()), 2);
        }
        (1, 1)
    }

    /// `col_width` (`test_article_cache.c:387`).
    fn col_width(p: &[u8], skip_ctrl_seq: bool) -> usize {
        let mut at = 0usize;
        let mut left = p.len();
        let mut width = 0usize;
        while left > 0 {
            let (bytes, cols) = unit_at(&p[at..], skip_ctrl_seq);
            if bytes == 0 {
                break;
            }
            width += cols;
            at += bytes;
            left -= bytes;
        }
        width
    }

    /// `spans_of` (`test_article_cache.c:411-474`): the display lines of a text as a terminal reads
    /// them. A line ends on a line feed, which stays with the line, or where the next character
    /// would pass the width, and a text ending on a line feed has an empty line after it — the one
    /// the cursor rests on. `None` is the reference's `NO_SPAN`, returned when the text needs more
    /// lines than `cap` has places for.
    fn spans_of(
        text: &[u8],
        max_cols: usize,
        skip_ctrl_seq: bool,
        cap: usize,
    ) -> Option<Vec<usize>> {
        let len = text.len();
        let mut spans: Vec<usize> = Vec::new();
        let mut start = 0usize;

        loop {
            let mut at = start;
            let mut width = 0usize;
            let mut on_line_feed = false;

            while at < len {
                let (bytes, cols) = unit_at(&text[at..], skip_ctrl_seq);
                let is_line_feed = bytes == 1 && cols == 1 && text[at] == b'\n';
                if bytes == 0 {
                    break;
                }
                if width + cols > max_cols {
                    break;
                }
                width += cols;
                at += bytes;
                if is_line_feed {
                    on_line_feed = true;
                    break;
                }
            }

            if spans.len() >= cap {
                return None;
            }
            spans.push(at - start);
            start = at;

            if start >= len {
                if on_line_feed {
                    if spans.len() >= cap {
                        return None;
                    }
                    spans.push(0);
                }
                break;
            }
        }

        Some(spans)
    }

    /// `spans_sum` (`test_article_cache.c:493`).
    fn spans_sum(spans: &[usize], count: usize) -> usize {
        spans[..count].iter().sum()
    }

    /// `file_size` (`test_article_cache.c:169`): `-1` for a file that is not there.
    fn file_size(file: &str) -> i64 {
        std::fs::metadata(file).map_or(-1, |m| m.len() as i64)
    }

    /// `make_article` (`test_article_cache.c:139`), which for these cases is the suite's author on
    /// a short subject.
    fn posted(cid: i32, aid: i32, title: &str) -> Article {
        Article {
            aid,
            cid,
            sid: 1,
            sub_dt: SUB_DT,
            username: "tester".into(),
            nickname: "Tester".into(),
            title: title.into(),
            ..Default::default()
        }
    }

    /// `build_header` (`test_article_cache.c:265`). The time comes from the module's own
    /// [`submission_time`], exactly as the reference takes it from the same `strftime` call, so
    /// another timezone or another locale moves the expectation along with the document.
    fn ref_header(article: &Article, section: &Section) -> Vec<u8> {
        format!(
            "发布者: {} ({}), 版块: {} ({})\n标  题: {}\n发布于: {} ({})\n\n",
            article.username,
            article.nickname,
            section.sname,
            section.stitle,
            article.title,
            SITE,
            submission_time(article.sub_dt),
        )
        .into_bytes()
    }

    /// `build_footer` (`test_article_cache.c:273`).
    fn ref_footer(aid: i32, sub_ip: &str) -> Vec<u8> {
        format!(
            "\n--\n{}※ 来源: {} https://{} [FROM: {}]\x1b[m\n\n",
            REF_FOOTER_COLORS[aid as usize % REF_FOOTER_COLORS.len()],
            SITE,
            SERVER,
            sub_ip
        )
        .into_bytes()
    }

    /// `build_body` (`test_article_cache.c:281`): the render of the article text, at the width of
    /// the screen and in ordinary quote mode.
    fn ref_body(content: &[u8]) -> Vec<u8> {
        lml::render(
            content,
            lml::ARTICLE_CONTENT_MAX_LEN,
            SCREEN_COLS as i64,
            false,
            false,
        )
    }

    /// `read_cache` (`test_article_cache.c:196-246`): the file as it lies, parsed here by offsets
    /// and sizes and not by [`load`]. `None` is the reference's `-1`, its own refusal to look at a
    /// file whose index does not fit inside it.
    struct Raw {
        p_mmap: i64,
        mmap_len: i64,
        p_data: i64,
        data_len: i64,
        line_total: i64,
        offsets: Vec<i64>,
        data: Vec<u8>,
        size: i64,
    }

    fn read_raw(cache_dir: &str, cid: i32) -> Option<Raw> {
        let bytes = std::fs::read(path(cache_dir, cid)).ok()?;
        let size = bytes.len() as i64;
        let index_at = PREFIX_WORDS * WORD;
        if size < index_at as usize as i64 || bytes.len() > REF_FILE_BUF_LEN {
            return None;
        }

        let word =
            |i: usize| i64::from_ne_bytes(bytes[i * WORD..i * WORD + WORD].try_into().unwrap());
        let (p_mmap, mmap_len, p_data, data_len, line_total) =
            (word(0), word(1), word(2), word(3), word(4));

        // `test_article_cache.c:232-233`: an index that does not fit in the file is not a cache.
        if line_total < 0
            || usize::try_from(line_total).ok()? >= MAX_SPLIT_FILE_LINES
            || index_at + WORD * (line_total as usize + 1) > bytes.len()
        {
            return None;
        }

        let prefix_len = index_at + WORD * (line_total as usize + 1);
        let offsets = bytes[index_at..prefix_len]
            .chunks_exact(WORD)
            .map(|w| i64::from_ne_bytes(w.try_into().unwrap()))
            .collect();

        Some(Raw {
            p_mmap,
            mmap_len,
            p_data,
            data_len,
            line_total,
            offsets,
            data: bytes[prefix_len..].to_vec(),
            size,
        })
    }

    /// The whole document as the reference expects to find it: the three parts, the lines each of
    /// them splits into, and the index those lines add up to.
    struct Expected {
        header: Vec<u8>,
        body: Vec<u8>,
        footer: Vec<u8>,
        offsets: Vec<i64>,
        region_start: [usize; 3],
        region_lines: [usize; 3],
    }

    impl Expected {
        fn lines(&self) -> usize {
            self.offsets.len() - 1
        }

        fn data_len(&self) -> usize {
            self.header.len() + self.body.len() + self.footer.len()
        }

        /// `region_of` (`test_article_cache.c:506`): which part of the document a display line of
        /// the index belongs to.
        fn region(&self, i: usize) -> usize {
            if i >= self.region_start[REGION_FOOTER] {
                REGION_FOOTER
            } else if i >= self.region_start[REGION_BODY] {
                REGION_BODY
            } else {
                REGION_HEADER
            }
        }

        /// `region_last_index` (`test_article_cache.c:521`).
        fn last_line_of_region(&self, region: usize) -> usize {
            self.region_start[region] + self.region_lines[region] - 1
        }

        fn max_cols(&self, region: usize) -> usize {
            [HEADER_MAX_COLS, BODY_MAX_COLS, FOOTER_MAX_COLS][region]
        }
    }

    /// `verify_cache` up to the point of writing the file (`test_article_cache.c:629-714`).
    /// `body_spans` states the body's display lines by hand, the way `check_with_body_lines`
    /// (`test_article_cache.c:756`) does; where they are not stated, they are what this reader of a
    /// terminal makes of the render.
    fn expected(
        what: &str,
        article: &Article,
        section: &Section,
        content: &[u8],
        sub_ip: &str,
        body_spans: Option<&[usize]>,
    ) -> Expected {
        let header = ref_header(article, section);
        let footer = ref_footer(article.aid, sub_ip);
        let body = ref_body(content);
        let texts: [&[u8]; 3] = [&header, &body, &footer];
        let max_cols = [HEADER_MAX_COLS, BODY_MAX_COLS, FOOTER_MAX_COLS];

        let mut spans: Vec<usize> = Vec::new();
        let mut region_start = [0usize; 3];
        let mut region_lines = [0usize; 3];

        for region in REGION_HEADER..=REGION_FOOTER {
            region_start[region] = spans.len();
            let cap = MAX_SPLIT_FILE_LINES - spans.len();

            let lines = if region == REGION_BODY {
                match body_spans {
                    // `spans_copy` (`test_article_cache.c:476`): the stated lines have to fit too.
                    Some(stated) => {
                        assert!(
                            stated.len() <= cap,
                            "{what}: the stated lines of the body do not fit the index"
                        );
                        stated.to_vec()
                    }
                    None => {
                        spans_of(texts[region], max_cols[region], true, cap).unwrap_or_else(|| {
                            panic!("{what}: the body needs more lines than the index holds")
                        })
                    }
                }
            } else {
                spans_of(texts[region], max_cols[region], true, cap).unwrap_or_else(|| {
                    panic!("{what}: a part of the document needs more lines than the index holds")
                })
            };

            assert!(
                !lines.is_empty(),
                "{what}: a part of the document splits into lines"
            );
            assert_eq!(
                spans_sum(&lines, lines.len()),
                texts[region].len(),
                "{what}: the lines of a part of the document do not add up to it"
            );

            region_lines[region] = lines.len();
            spans.extend(lines);
        }

        let mut offsets: Vec<i64> = vec![0];
        for span in &spans {
            offsets.push(offsets.last().copied().unwrap() + *span as i64);
        }

        Expected {
            header,
            body,
            footer,
            offsets,
            region_start,
            region_lines,
        }
    }

    /// `check_index` (`test_article_cache.c:526-625`): the numbers in the file, the index against
    /// the prediction, and then what the index says about every line it names. The last part is
    /// read from the file rather than from the expectation that just matched it, which is what stops
    /// a writer and a reader that make the same mistake from agreeing.
    fn check_index(what: &str, raw: &Raw, exp: &Expected) {
        let total = exp.lines();
        let data_len = exp.data_len();
        let (header_len, body_len, footer_len) =
            (exp.header.len(), exp.body.len(), exp.footer.len());

        assert_eq!(
            raw.line_total as usize, total,
            "{what}: the lines the index holds"
        );
        assert_eq!(
            raw.mmap_len as usize, raw.size as usize,
            "{what}: the stored length is the length of the file"
        );
        assert_eq!(
            raw.data_len as usize, data_len,
            "{what}: the stored data length is header, body and footer"
        );
        assert_eq!(
            raw.mmap_len as usize,
            (PREFIX_WORDS + total + 1) * WORD + data_len,
            "{what}: the file is the header, the index and the data"
        );

        // The two pointers of the struct in memory have no meaning inside a file.
        assert_eq!(raw.p_mmap, 0, "{what}: the stored mapping pointer is empty");
        assert_eq!(raw.p_data, 0, "{what}: the stored data pointer is empty");

        assert_eq!(
            &raw.data[..header_len],
            &exp.header[..],
            "{what}: the document starts with the header"
        );
        assert_eq!(
            &raw.data[header_len..header_len + body_len],
            &exp.body[..],
            "{what}: the body follows the header"
        );
        assert_eq!(
            &raw.data[header_len + body_len..data_len],
            &exp.footer[..footer_len],
            "{what}: the footer follows the body"
        );

        assert_eq!(
            raw.offsets.len(),
            total + 1,
            "{what}: the index holds one entry per line and one for the end"
        );
        for i in 0..=total {
            assert_eq!(
                raw.offsets[i], exp.offsets[i],
                "{what}: offset {i} of the line index, which decides every one after it"
            );
        }
        assert_eq!(
            raw.offsets[0], 0,
            "{what}: the index starts at the first byte"
        );
        assert_eq!(
            raw.offsets[total] as usize, data_len,
            "{what}: the index ends at the length of the data"
        );

        for i in 0..total {
            let at = raw.offsets[i] as usize;
            let end = raw.offsets[i + 1] as usize;
            assert!(
                at <= end && end <= data_len,
                "{what}: the index runs forward and stays inside the data"
            );

            let line = &raw.data[at..end];
            let region = exp.region(i);
            let max_cols = exp.max_cols(region);
            let width = col_width(line, true);
            let on_line_feed = end > at && line[line.len() - 1] == b'\n';

            assert!(
                width <= max_cols,
                "{what}: display line {i} is {width} columns, past the {max_cols} its region allows"
            );
            assert!(
                line.iter()
                    .position(|&b| b == b'\n')
                    .map_or(true, |cut| cut + 1 == line.len()),
                "{what}: a line feed only ends a display line, and line {i} holds one inside it"
            );

            if i != exp.last_line_of_region(region) {
                assert!(
                    end > at,
                    "{what}: a display line inside a region has bytes of its own"
                );
                if !on_line_feed {
                    // The line was cut because the character behind it would no longer fit, the
                    // only other way a display line can end before the document does.
                    let (_, next_cols) = unit_at(&raw.data[end..data_len], true);
                    assert!(
                        width + next_cols > max_cols,
                        "{what}: display line {i} is broken where it no longer fits"
                    );
                }
            }
        }
    }

    /// `verify_cache` (`test_article_cache.c:629`): predict, write the file (`:716`), read it
    /// back with the raw reader (`:724`) and compare (`:731`).
    fn verify(
        what: &str,
        cache_dir: &str,
        article: &Article,
        content: &[u8],
        sub_ip: &str,
        body_spans: Option<&[usize]>,
    ) -> (Raw, Expected) {
        let section = section();
        let exp = expected(what, article, &section, content, sub_ip, body_spans);

        assert!(
            generate(
                cache_dir, SITE, SERVER, article, &section, content, sub_ip, true, false
            )
            .unwrap(),
            "{what}: article_cache_generate() wrote nothing"
        );
        let raw = read_raw(cache_dir, article.cid)
            .unwrap_or_else(|| panic!("{what}: the generated cache file cannot be read back"));

        check_index(what, &raw, &exp);
        (raw, exp)
    }

    /// #1 of the reference (`test_article_cache.c:766`): the calls that must refuse, and refuse
    /// without leaving a file behind. C's `NULL` arguments are Rust types, and its `-1` for an
    /// unmapped struct has no counterpart in a `Vec`, so what survives of the section is the
    /// directory that is not there and the load of a cache that is not either.
    #[test]
    fn a_refused_call_leaves_no_cache_file() {
        let missing = std::env::temp_dir().join("lbbs-cache-nowhere");
        let _ = std::fs::remove_dir_all(&missing);
        let dir = missing.to_string_lossy().into_owned();
        let article = posted(100, 1, "Hello");

        let error = generate(
            &dir,
            SITE,
            SERVER,
            &article,
            &section(),
            b"body",
            SUB_IP,
            true,
            false,
        )
        .expect_err("article_cache_generate() into a directory that is not there");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            file_size(&path(&dir, 100)),
            -1,
            "a refused call leaves no cache file"
        );

        // C's `article_cache_load()` of a file that is not there is its `-2` out of `open(2)`,
        // which the port answers with `Ok(None)`.
        assert!(
            load(&dir, 100).unwrap().is_none(),
            "article_cache_load() of a cache that is not there"
        );
    }

    /// #2 of the reference (`test_article_cache.c:810`): the documents whose display lines the
    /// suite states by hand, so a render that expands, drops or adds a byte shows up as a length
    /// that is not the one written down. `a\tb\n` is the sharpest of them: four bytes in, six out,
    /// because the tab is filled to the next of the reference's four-column stops.
    #[test]
    fn the_reference_states_the_lines_of_every_simple_document() {
        let dir = temp_dir("ref-simple");

        // Two lines that end on their line feeds take a third, empty line behind them: the last
        // line feed of a document always leaves one the cursor rests on.
        verify(
            "two lines",
            &dir,
            &posted(101, 1, "Hello"),
            b"First line\nSecond line\n",
            SUB_IP,
            Some(&[11usize, 12, 0]),
        );
        // A document that does not end on a line feed has one line, and no empty one behind it.
        verify(
            "no trailing line feed",
            &dir,
            &posted(102, 2, "Hello"),
            b"abc",
            SUB_IP,
            Some(&[3usize]),
        );
        // An empty body is still one line, of no bytes at all.
        verify(
            "empty body",
            &dir,
            &posted(103, 3, "Hello"),
            b"",
            SUB_IP,
            Some(&[0usize]),
        );
        verify(
            "one line feed",
            &dir,
            &posted(104, 4, "Hello"),
            b"\n",
            SUB_IP,
            Some(&[1usize, 0]),
        );
        verify(
            "spaces",
            &dir,
            &posted(105, 5, "Hello"),
            b"   \n",
            SUB_IP,
            Some(&[4usize, 0]),
        );
        verify(
            "blank lines",
            &dir,
            &posted(106, 6, "Hello"),
            b"\n\n\n",
            SUB_IP,
            Some(&[1usize, 1, 1, 0]),
        );
        // Eight bytes: four letters, three spaces and the line feed that ends them.
        verify(
            "trailing spaces",
            &dir,
            &posted(107, 7, "Hello"),
            b"text   \n\n",
            SUB_IP,
            Some(&[8usize, 1, 0]),
        );
        // Nineteen: six characters of three bytes and the line feed.
        verify(
            "cjk",
            &dir,
            &posted(108, 8, "Hello"),
            "中文测试一行\n".as_bytes(),
            SUB_IP,
            Some(&[19usize, 0]),
        );
        // Four and four: the render drops the carriage returns.
        verify(
            "crlf",
            &dir,
            &posted(109, 9, "Hello"),
            b"one\r\ntwo\r\n",
            SUB_IP,
            Some(&[4usize, 4, 0]),
        );
        verify(
            "tab",
            &dir,
            &posted(110, 10, "Hello"),
            b"a\tb\n",
            SUB_IP,
            Some(&[6usize, 0]),
        );
        // Left to this reader of a terminal: what the render makes of control characters is not
        // something the index should be trusted to hide.
        verify(
            "bell and backspace",
            &dir,
            &posted(111, 11, "Hello"),
            b"a\x08b\x07c\n",
            SUB_IP,
            None,
        );
        verify(
            "long subject",
            &dir,
            &posted(112, 12, "Hello"),
            "012345678901234567890123456789, 这是一段中文的行\n".as_bytes(),
            SUB_IP,
            None,
        );
    }

    /// #3 of the reference (`test_article_cache.c:873`): the lines of a document wider than the
    /// screen, stated down to the byte.
    #[test]
    fn a_wide_document_breaks_where_the_reference_breaks_it() {
        let dir = temp_dir("ref-wrapped");

        // A line of exactly the width of the screen is one display line, and the render has
        // nothing left to break.
        let screen_full = "x".repeat(SCREEN_COLS);
        verify(
            "a line of the screen width",
            &dir,
            &posted(120, 1, "Hello"),
            screen_full.as_bytes(),
            SUB_IP,
            Some(&[80usize]),
        );
        // One column past it and the render breaks the line, which gives the line feed the column
        // of the whole display line it would otherwise have needed.
        let past = "x".repeat(SCREEN_COLS + 1);
        verify(
            "a line past the screen width",
            &dir,
            &posted(121, 2, "Hello"),
            past.as_bytes(),
            SUB_IP,
            Some(&[81usize, 1]),
        );
        verify(
            "a line of two screens",
            &dir,
            &posted(122, 3, "Hello"),
            "y".repeat(200).as_bytes(),
            SUB_IP,
            Some(&[81usize, 81, 40]),
        );
        // Forty-one wide characters are eighty-two columns, which the render breaks into whole
        // lines first and one for the rest.
        verify(
            "wide characters past the screen",
            &dir,
            &posted(123, 4, "Hello"),
            "中".repeat(41).as_bytes(),
            SUB_IP,
            Some(&[121usize, 3]),
        );
        // A subject long enough to wrap spreads the header over several display lines, which the
        // index has to count as part of the document.
        let subject = "0123456789".repeat(13);
        verify(
            "a wrapping subject",
            &dir,
            &posted(124, 5, &subject[..124]),
            b"body\n",
            SUB_IP,
            None,
        );
    }

    /// #4 of the reference (`test_article_cache.c:934`): markup the render rewrites, where the
    /// escape sequences it emits cost the index no columns at all.
    #[test]
    fn a_rendered_document_indexes_what_the_render_made() {
        let dir = temp_dir("ref-rendered");

        for (n, content) in [
            "[color red]Red[/color] body\n",
            ": quoted line\nplain line\n",
            ": : deep quote\n",
            "[url http://www.bbs.test]click[/url]\n",
            "[notatag]text[/notatag]",
            "[nolml][color red]red[/color]",
            "\tx\t\ty\n",
            "--\nnot a footer\n",
        ]
        .iter()
        .enumerate()
        {
            verify(
                &format!("rendered document {content:?}"),
                &dir,
                &posted(130 + n as i32, n as i32 + 1, "Hello"),
                content.as_bytes(),
                SUB_IP,
                None,
            );
        }
    }

    /// #5 of the reference (`test_article_cache.c:952`): the file is named for the content id, and
    /// an existing cache is rewritten only when the caller asks for it.
    #[test]
    fn a_cache_is_named_by_its_content_id_and_rewritten_only_on_request() {
        let dir = temp_dir("ref-naming");
        let article = posted(5001, 77, "Named");

        verify(
            "an article to name",
            &dir,
            &article,
            b"body\n",
            SUB_IP,
            None,
        );
        assert!(
            file_size(&path(&dir, 5001)) > 0,
            "the cache of an article is named by its content id"
        );
        assert_eq!(
            file_size(&path(&dir, 77)),
            -1,
            "no cache is named by the article number"
        );

        let renamed = posted(5002, 77, "Named");
        verify(
            "the same article under another content id",
            &dir,
            &renamed,
            b"body\n",
            SUB_IP,
            None,
        );
        assert!(
            file_size(&path(&dir, 5002)) > 0,
            "another content id is another file"
        );

        let file = path(&dir, 5002);
        let before = file_size(&file);
        let longer = "a longer body, with more lines than the one before it held\n";
        assert!(
            !generate(
                &dir,
                SITE,
                SERVER,
                &renamed,
                &section(),
                longer.as_bytes(),
                SUB_IP,
                false,
                false
            )
            .unwrap(),
            "article_cache_generate() over a cache that is already there"
        );
        assert_eq!(
            file_size(&file),
            before,
            "a cache that is already there is left as it is"
        );
        assert!(
            generate(
                &dir,
                SITE,
                SERVER,
                &renamed,
                &section(),
                longer.as_bytes(),
                SUB_IP,
                true,
                false
            )
            .unwrap(),
            "article_cache_generate() over a cache the caller wants rewritten"
        );
        assert_ne!(
            file_size(&file),
            before,
            "a cache is rewritten when the caller asks for it"
        );

        verify(
            "the rewritten cache",
            &dir,
            &renamed,
            longer.as_bytes(),
            SUB_IP,
            None,
        );
    }

    /// #6 of the reference (`test_article_cache.c:1004`): the footer cycles through the seven
    /// colours with the article number, twice around.
    #[test]
    fn the_footer_colour_cycles_with_the_article_number() {
        let dir = temp_dir("ref-colours");

        for aid in 0..2 * REF_FOOTER_COLORS.len() as i32 {
            let article = posted(600 + aid, aid, "Colour");
            let (raw, exp) = verify(
                &format!("the footer of article {aid}"),
                &dir,
                &article,
                b"footer\n",
                SUB_IP,
                None,
            );

            // Behind the two lines that open the footer, which is where the cycle shows.
            let at = raw.data.len() - exp.footer.len() + 4;
            assert_eq!(
                &raw.data[at..at + 5],
                REF_FOOTER_COLORS[aid as usize % REF_FOOTER_COLORS.len()].as_bytes(),
                "the colour of the footer of article {aid}"
            );
        }
    }

    /// #7 of the reference (`test_article_cache.c:1034`): a source address long enough to bring the
    /// footer to the edge of the screen, where it makes a difference that the colour codes are
    /// drawn in no room at all.
    #[test]
    fn a_footer_at_the_edge_of_the_screen_is_indexed_as_it_is_drawn() {
        let dir = temp_dir("ref-footer");
        let article = posted(7501, 9, "Edge");

        let mut chosen: Option<String> = None;
        for len in 1..40usize {
            let sub_ip = "9".repeat(len);
            let footer = ref_footer(article.aid, &sub_ip);
            let kept = spans_of(&footer, FOOTER_MAX_COLS, true, MAX_SPLIT_FILE_LINES).unwrap();
            let counted = spans_of(&footer, FOOTER_MAX_COLS, false, MAX_SPLIT_FILE_LINES).unwrap();
            if !kept.is_empty() && counted.len() > kept.len() {
                chosen = Some(sub_ip);
                break;
            }
        }

        let sub_ip =
            chosen.expect("a source address that brings the footer to the edge of the screen");
        verify(
            "a footer at the edge of the screen",
            &dir,
            &article,
            b"short body\n",
            &sub_ip,
            None,
        );
    }

    /// #8 of the reference (`test_article_cache.c:1090`): the loader is checked against the bytes,
    /// not against the writer that just produced them.
    #[test]
    fn the_loader_agrees_with_the_bytes_on_disk() {
        let dir = temp_dir("ref-loader");
        let article = posted(7001, 4, "Loaded");
        let (raw, _exp) = verify(
            "a document to load",
            &dir,
            &article,
            b"First line\nSecond line\n",
            SUB_IP,
            None,
        );

        let cached = load(&dir, 7001)
            .expect("article_cache_load()")
            .expect("a cache file that is there");
        assert_eq!(
            cached.mmap_len as i64, raw.mmap_len,
            "the loaded length is the length of the file"
        );
        assert_eq!(
            cached.data_len as i64, raw.data_len,
            "the loaded data length is the length in the file"
        );
        assert_eq!(
            cached.line_total as i64, raw.line_total,
            "the loaded number of lines"
        );
        assert_eq!(
            cached.line_offsets, raw.offsets,
            "an offset of the loaded index"
        );
        assert_eq!(
            cached.data, raw.data,
            "the loaded data is the data of the file"
        );
        assert_eq!(
            raw.size as usize - cached.data.len(),
            (PREFIX_WORDS + cached.line_total + 1) * WORD,
            "the data of the mapping follows its index"
        );
    }

    /// #9 of the reference (`test_article_cache.c:1165`): a file that is not a cache is refused,
    /// and nothing about it is handed back. C splits these into `-2` for a file that cannot be
    /// mapped at all and `-3` for one whose own length disagrees with it; the port answers all of
    /// them with one `InvalidData`, which is what every caller already branches on.
    #[test]
    fn a_file_that_is_not_a_cache_is_refused() {
        let dir = temp_dir("ref-broken");
        let article = posted(8001, 6, "Broken");

        // A file the size of nothing cannot be mapped at all (`test_article_cache.c:1182-1183`).
        std::fs::write(path(&dir, 8002), b"").unwrap();
        // Shorter than its own header: the length it claims is the nothing that follows it.
        std::fs::write(path(&dir, 8003), b"0123456789").unwrap();
        std::fs::write(path(&dir, 8004), b"This is not an article cache.").unwrap();

        for cid in [8002, 8003, 8004] {
            let error = load(&dir, cid)
                .expect_err(&format!("article_cache_load() of the cache file {cid}"));
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidData,
                "cid {cid} was accepted"
            );
            assert!(
                read_raw(&dir, cid).is_none(),
                "cid {cid} is not a cache either"
            );
        }

        // A cache cut short keeps the header of the whole document, which no longer describes it.
        verify(
            "a document to truncate",
            &dir,
            &article,
            b"First line\nSecond line\n",
            SUB_IP,
            None,
        );
        let file = path(&dir, 8001);
        let whole = std::fs::read(&file).unwrap();
        std::fs::write(&file, &whole[..whole.len() / 2]).unwrap();
        assert_eq!(
            load(&dir, 8001).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "article_cache_load() of a cache file cut in half"
        );
        assert!(
            read_raw(&dir, 8001).map_or(false, |raw| raw.mmap_len != raw.size),
            "the stored length of the truncated file describes the whole document"
        );
    }

    /// #10 of the reference (`test_article_cache.c:1230`): a document longer than the index has
    /// places for. No entry is left for the footer, so the footer is cut short and never written,
    /// and the last lines of the body go with it.
    #[test]
    fn a_document_past_the_index_ends_on_its_body() {
        let dir = temp_dir("ref-past-index");
        let article = posted(9001, 8, "Long");
        let content = "a\n".repeat(MAX_SPLIT_FILE_LINES - 4);

        assert!(
            generate(
                &dir,
                SITE,
                SERVER,
                &article,
                &section(),
                content.as_bytes(),
                SUB_IP,
                true,
                false
            )
            .unwrap(),
            "article_cache_generate() a document longer than the index"
        );
        let raw = read_raw(&dir, 9001).expect("the long cache file can be read back");

        let header = ref_header(&article, &section());
        let hdr = spans_of(&header, HEADER_MAX_COLS, true, MAX_SPLIT_FILE_LINES).unwrap();
        assert_eq!(hdr.len(), 5, "the lines of the header of a long document");

        let body_lines = MAX_SPLIT_FILE_LINES - 1 - hdr.len();
        let body_len = 2 * body_lines;
        let data_len = header.len() + body_len;

        assert_eq!(
            raw.line_total as usize,
            MAX_SPLIT_FILE_LINES - 1,
            "the lines of a document past the index"
        );
        assert_eq!(
            raw.data_len as usize, data_len,
            "the data of a document past the index"
        );
        assert_eq!(
            file_size(&path(&dir, 9001)) as usize,
            (PREFIX_WORDS + raw.line_total as usize + 1) * WORD + data_len,
            "the file of a document past the index"
        );
        assert_eq!(
            content.len() - body_len,
            4,
            "the bytes of the body no line of the index is left for"
        );

        for i in 0..=raw.line_total as usize {
            let expect = if i < hdr.len() {
                spans_sum(&hdr, i)
            } else if i == hdr.len() {
                header.len()
            } else {
                header.len() + 2 * (i - hdr.len())
            };
            assert_eq!(
                raw.offsets[i] as usize, expect,
                "offset {i} of a document past the index"
            );
        }

        assert_eq!(
            &raw.data[data_len - 2..],
            b"a\n",
            "a document past the index ends on its body"
        );
        assert!(
            !String::from_utf8_lossy(&raw.data).contains("--"),
            "the footer of a document past the index is never written"
        );

        // The port's own reader has to agree with the file it is reading, or the pager would be
        // handed an index that runs past the data.
        let cached = load(&dir, 9001)
            .expect("article_cache_load() of a document past the index")
            .expect("a cache file that is there");
        assert_eq!(
            cached.line_offsets.len(),
            cached.line_total + 1,
            "the index of a document past the index"
        );
        assert_eq!(
            cached.data.len(),
            cached.data_len,
            "the data of a document past the index"
        );
    }
}
