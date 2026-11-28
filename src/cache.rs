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
#[path = "../tests/test_cache.rs"]
mod tests;
