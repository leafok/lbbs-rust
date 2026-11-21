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

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::board::{Article, Section};
use crate::lml;
use crate::paths::{
    ARTICLE_TITLE_MAX_LEN, FILE_PATH_LEN, MAX_SPLIT_FILE_LINES, NICKNAME_MAX_LEN, SCREEN_COLS, USERNAME_MAX_LEN,
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

/// `%s%d`: the cache directory already ends with a separator (`var/articles/`).
pub fn path(cache_dir: &str, cid: i32) -> String {
    let joined = format!("{cache_dir}{cid}");
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

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&data_file)?;

    let mut header = header(article, section, bbs_name, &submission_time(article.sub_dt));
    let mut footer = footer(article, bbs_name, bbs_server, sub_ip);
    let body = lml::render(content, lml::ARTICLE_CONTENT_MAX_LEN, SCREEN_COLS as i64, false, fixed);

    // The offset table is filled the way C fills `cache.line_offsets`: the header split owns
    // indices `0..=header_lines`, the body split is written over it from `header_lines`, and
    // the footer overwrites the last of those again. Each part re-bases its own relative
    // offsets, and a part whose final offset falls short of its length was wrapped away.
    //
    // C hands `split_data_lines` the number of *slots* in that table and stops one line before
    // it fills them (`str_process.c:203`), so the port asks for one line fewer than the table
    // has places. A post that long is rare, but it decides how many bytes the body keeps:
    // 65535 lines here, exactly as in a file the C loader wrote.
    let header_lines = text::split_data_lines(&header, SCREEN_COLS, true, fixed, MAX_SPLIT_FILE_LINES - 1);
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

    let mut line_offsets: Vec<i64> = Vec::with_capacity(header_lines.count() + body_lines.count() + 1);
    line_offsets.extend(header_lines.offsets.iter().map(|&o| o as i64));
    line_offsets.extend(body_lines.offsets.iter().skip(1).map(|&o| (header_len + o as usize) as i64));
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
    line_offsets.extend(footer_lines.offsets.iter().skip(1).map(|&o| (body_end + o as usize) as i64));

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
/// `open(2)`, and the caller's cue to generate it.
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
        // The C check, which is what catches a file truncated by a crash mid-write.
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
    const CONTENTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/article_contents/");

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("lbbs-cache-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        format!("{}/", dir.to_str().unwrap())
    }

    #[test]
    fn paths_match_the_c_naming() {
        assert_eq!(path("var/articles/", 1234), "var/articles/1234");
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
            assert_eq!(cache.line_offsets[cache.line_total] as usize, cache.data_len);
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
                text.contains("※ 来源: 枫林在线 https://www.bbs.test [FROM: 203.0.113.7]"),
                "cid {cid}"
            );
        }

        let short = load(FIXTURES, 9001).unwrap().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&short.data),
            "发布者: tester (测试者), 版块: Test (测试版)\n标  题: Re: 一个标题 [test]\n\
             发布于: 枫林在线 (Wed Nov 15 06:13:20 2023 CST)\n\nHello world\n\n--\n\
             \x1b[35m※ 来源: 枫林在线 https://www.bbs.test [FROM: 203.0.113.7]\x1b[m\n\n"
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
        assert!(body.contains(&"A\x1b[4mB\x1b[5mCD\x1b[mE\x1b[m\n".to_string()), "{body:?}");
        assert!(body.iter().any(|l| l.contains("中文测试")), "{body:?}");
        assert!(body.iter().any(|l| l.starts_with("\x1b[32m: 引用一行")), "{body:?}");
        assert!(body.iter().any(|l| l.contains("（图片: https://x.example/a.png）")), "{body:?}");
    }

    #[test]
    fn a_rejected_file_is_reported_instead_of_panicking() {
        let dir = temp_dir("reject");
        std::fs::write(format!("{dir}7"), b"not a cache file at all, but long enough").unwrap();
        assert_eq!(
            load(&dir, 7).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "mmap_len does not match the size"
        );

        std::fs::write(format!("{dir}8"), b"short").unwrap();
        assert_eq!(load(&dir, 8).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(load(&dir, 9).unwrap().is_none());
    }

    #[test]
    fn generates_the_same_bytes_the_c_generator_does() {
        let dir = temp_dir("generate");

        for (cid, content_file) in [(9001, "c1.txt"), (9002, "c2.txt"), (9003, "c3.txt"), (9004, "c4.txt")] {
            let content = std::fs::read(format!("{CONTENTS}{content_file}")).unwrap();
            assert!(
                generate(
                    &dir,
                    "枫林在线",
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

            let mine = std::fs::read(format!("{dir}{cid}")).unwrap();
            let theirs = std::fs::read(format!("{FIXTURES}{cid}")).unwrap();
            assert_eq!(mine.len(), theirs.len(), "cid {cid} length differs");
            if mine != theirs {
                let at = mine.iter().zip(theirs.iter()).position(|(a, b)| a != b).unwrap();
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

        generate(&dir, "枫林在线", "www.bbs.test", &article, &section(), b"first", "10.0.0.1", true, true).unwrap();
        let before = std::fs::read(format!("{dir}4242")).unwrap();
        assert!(!generate(&dir, "枫林在线", "www.bbs.test", &article, &section(), b"second", "10.0.0.1", false, true).unwrap());
        assert_eq!(std::fs::read(format!("{dir}4242")).unwrap(), before);
        assert!(generate(&dir, "枫林在线", "www.bbs.test", &article, &section(), b"second", "10.0.0.1", true, true).unwrap());
        assert_ne!(std::fs::read(format!("{dir}4242")).unwrap(), before);

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
            "枫林在线",
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
            "枫林在线",
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
        assert_eq!(cache.line_offsets[cache.line_total] as usize, cache.data_len);
        assert_eq!(cache.mmap_len, std::fs::metadata(format!("{dir}6000")).unwrap().len() as usize);
    }
}
