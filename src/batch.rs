/* SPDX-License-Identifier: GPL-3.0-or-later */
//! What the three batch generators have in common, ported from `utils/lib/db_open.inc.php`
//! (`:4-32` `db_open`, `:34-42` the connect-and-die step) and from the lines the scripts repeat.
//!
//! The originals were `php utils/bin/gen_top.php &c.` runs from a shell or a cron table: no
//! arguments, the board's database, and one or more files under `var/` written at the end.
//! [`run`] gives a generator exactly that sequence and [`Batch`] is what it hands over.
//!
//! # Deviations from PHP
//!
//! * The credentials come from `conf/bbsd.conf` — the daemon's own configuration — instead of
//!   from `utils/conf/db_conn.conf.php`. One file then holds the address of the database for the
//!   server and for its generators: `db_host`, `db_username`, `db_password`, `db_database` and
//!   `db_timezone` stand for `$DB_hostname`, `$DB_username`, `$DB_password`, `$DB_database` and
//!   `$DB_session_timezone`, and sqlx sends the charset and the session time zone on connect the
//!   way `db_open` did with `set_charset("utf8")` and `SET time_zone` (`:18-29`).
//! * `!isset($_SERVER["argc"])` kept the scripts from running under a web server. A CLI binary
//!   always has an argument count, so the same guard becomes "no arguments at all": anything
//!   else prints `Invalid usage` and leaves with the script's `-1`.
//! * The two `chdir()`s that find the server root happen before the runtime is built, for the
//!   reason `main.rs` gives: `chdir()` is process-wide and would race with a worker thread that
//!   had already started. A generator is therefore expected to sit in `<root>/bin/`, next to the
//!   daemon, exactly as `bbsd` does.
//! * `file_put_contents()` failure in PHP is a warning, and the script still exits `0`. Here a
//!   failed write is reported and changes the status, because on a cron run that warning would be
//!   the only sign that a menu file had gone stale.
//! * `new DateTimeImmutable(null)` — which is what a `NULL` `sub_dt`, `dt` or `ex_menu_tm`
//!   becomes — is deprecated in PHP 8.4: it yields the current time *and* prints a notice. The
//!   port keeps the time and drops the notice.
//! * A date reaches PHP as the text of its column and is reformatted with no time zone
//!   arithmetic at all, so `NaiveDateTime` — not `DateTime<Utc>` or the session offset — is the
//!   faithful decode here. `db.rs` avoids chrono for the server's own queries because those ask
//!   MySQL for `UNIX_TIMESTAMP()`; these generators only print the wall clock the row already
//!   says.
//! * Every template the generators write is a PHP heredoc, and a heredoc does not keep the
//!   newline that ends the line its closing marker sits on: `<<<MENU`, an item line, a blank
//!   line and `MENU;` contribute `item\n`, one newline rather than the two the blank line
//!   suggests. The blocks in [`crate::gen_top`], [`crate::gen_section_menu`] and
//!   [`crate::gen_ex_list`] are therefore the bytes PHP emitted, one line per item, with no blank
//!   line between a header and its first item.

use std::future::Future;
use std::path::PathBuf;

use chrono::{Local, NaiveDateTime};

use crate::config::{self, Config};
use crate::daemon;
use crate::db::Db;
use crate::paths::CONF_BBSD;
use crate::text;

/// `db_open.inc.php:14-15` plus `:40` — `echo ("Mysqli connection error: " . …)` and
/// `exit(-1)`, and the same `-1` every script used for `Invalid usage` and for an uncaught error.
pub const EXIT_FATAL: i8 = -1;
/// `exit(-2)`: `gen_top.php:44`'s query failure, `gen_ex_list.php:139`'s, and the menu file
/// `gen_section_menu.php:19` could not read.
pub const EXIT_QUERY: i8 = -2;
/// `exit(-3)`, the failures one step further out: `gen_section_menu.php:85`'s section list,
/// `gen_section_menu.php:230`'s excerptional one, `gen_ex_list.php:156`'s `UPDATE`.
pub const EXIT_DEEP: i8 = -3;

/// The server root, the configuration and the database a generator runs against.
pub struct Batch {
    pub root: PathBuf,
    pub config: Config,
    pub db: Db,
}

impl Batch {
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

/// Why a generator is leaving: the text PHP echoed to stdout and the status it exited with.
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub code: i8,
}

impl Failure {
    pub fn new(message: impl Into<String>, code: i8) -> Self {
        Failure {
            message: message.into(),
            code,
        }
    }

    /// `echo("Query data error: " . mysqli_error($db_conn)); exit(-2)`: the PHP prefixed most of
    /// its query failures with the step that failed, colon and space included.
    pub fn query(action: &str, error: &sqlx::Error) -> Self {
        Failure::new(format!("{action}{error}"), EXIT_QUERY)
    }
}

/// The whole life of one generator: arguments, server root, configuration, runtime, database,
/// then `generate`. The status is the script's and so is the stream its errors went to — a cron
/// run reads the mail this produces, so nothing here is diverted to stderr or to the log files.
///
/// The generator takes the [`Batch`] by value rather than by reference: a borrow would make its
/// future capture a lifetime that `FnOnce(&Batch) -> Fut` cannot name.
pub fn run<F, Fut>(generate: F) -> std::process::ExitCode
where
    F: FnOnce(Batch) -> Fut,
    Fut: Future<Output = Result<(), Failure>>,
{
    let argv: Vec<String> = std::env::args().collect();
    let program = argv.first().cloned().unwrap_or_default();

    if argv.len() > 1 {
        print!("Invalid usage");
        return leave(EXIT_FATAL);
    }

    let root = match daemon::enter_server_root(&program) {
        Ok(root) => root,
        Err(error) => {
            println!("{error}");
            return leave(EXIT_FATAL);
        }
    };

    let config = match config::load(CONF_BBSD) {
        Ok(config) => config,
        Err(error) => {
            println!("{error}");
            return leave(EXIT_QUERY);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("{error}");
            return leave(EXIT_FATAL);
        }
    };

    runtime.block_on(async move {
        let db = match Db::connect(&config.db).await {
            Ok(db) => db,
            Err(error) => {
                print!("Mysqli connection error: {error}");
                return leave(EXIT_FATAL);
            }
        };
        let batch = Batch { root, config, db };
        match generate(batch).await {
            Ok(()) => leave(0),
            Err(failure) => {
                print!("{}", failure.message);
                leave(failure.code)
            }
        }
    })
}

/// PHP's negative exit statuses are the same bytes the shell sees: `-1` is `255`.
fn leave(code: i8) -> std::process::ExitCode {
    std::process::ExitCode::from(code as u8)
}

/// PHP's `str_length` (`utils/lib/str_process.inc.php:2-64`). Its mask is `0x70` rather than C's
/// `0xf0` and it charges two cells for any lead byte, which is what [`text::display_width`] does
/// in fixed mode: the two agree on every well-formed sequence, and differ only on a lead byte of
/// `0xF8` or above, which UTF-8 does not have.
pub fn str_length(s: &str) -> usize {
    text::display_width(s.as_bytes(), false, true)
}

/// `split_line($str, "", $width, 1, "")` (`str_process.inc.php:66-163`): the longest prefix of
/// the first display line that fits in `$width` cells. With a one-line limit the wrap test
/// (`:122-133`, `:141-152`) breaks out of the scan instead of starting a second line, and the
/// newline branch (`:91-96`) breaks before appending it, so the result is the plain truncation
/// with the newline left out. `\r` and `\7` are dropped on the way (`:86-89`).
pub fn first_line(s: &str, width: usize) -> String {
    let split = text::split_line(s.as_bytes(), width, false, true);
    let mut end = split.bytes;
    if split.eol {
        end -= 1; // the '\n' the C-side splitter counts as a cell of its own.
    }
    s[..end]
        .chars()
        .filter(|c| *c != '\r' && *c != '\u{7}')
        .collect()
}

/// PHP's coercion of a `NULL` column to `""`, which is what every caller here does with one:
/// `strlen(null)`, `"[$title]"` interpolation and `str_repeat` all see the empty string.
pub fn text(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or_default()
}

/// `str_repeat(" ", $width - $used)`. PHP 8 raises `ValueError` when the repeat count goes
/// negative — a board title wider than the column it is padded into does exactly that — and the
/// script dies before writing anything, so a negative width is an error here too rather than a
/// silent clamp. The message is PHP's own; its uncaught-error banner, which adds the file, the
/// line and a stack trace, is not reproduced.
pub fn pad(width: i64, used: i64) -> Result<String, Failure> {
    let times = width - used;
    if times < 0 {
        return Err(Failure::new(
            "str_repeat(): Argument #2 ($times) must be greater than or equal to 0\n",
            EXIT_FATAL,
        ));
    }
    Ok(" ".repeat(times as usize))
}

/// `addslashes` (`gen_section_menu.php:105`): the item text is written into a `"`-quoted field of
/// the menu file, and these are the four bytes PHP backslash-escapes.
pub fn addslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push_str("\\0"),
            '\'' | '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// `(new DateTimeImmutable($row[$column]))->format($format)`: a `NULL` column becomes the current
/// time, and the value of a non-null one is read back as the text the server sent. `$format` is
/// the two PHP patterns the generators use, `"M d H:i:s"` and `"Y.m.d"`, translated to strftime.
pub fn format_datetime(value: &Option<NaiveDateTime>, format: &str) -> String {
    let strftime = match format {
        "M d H:i:s" => "%b %d %H:%M:%S",
        "Y.m.d" => "%Y.%m.%d",
        other => other,
    };
    match value {
        Some(value) => value.format(strftime).to_string(),
        None => Local::now().naive_local().format(strftime).to_string(),
    }
}

/// `file_put_contents($path, $buffer)`: the write the generators end with. PHP warned and carried
/// on; here the failure is the reason for leaving.
pub fn write_file(path: &std::path::Path, buffer: &str) -> Result<(), Failure> {
    std::fs::write(path, buffer).map_err(|error| {
        Failure::new(
            format!(
                "file_put_contents({}): Failed to open stream: {}\n",
                path.display(),
                error
            ),
            EXIT_FATAL,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(text: &str) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()
    }

    #[test]
    fn str_length_charges_two_cells_per_wide_character() {
        // The values the PHP gave for the strings the generators pad.
        assert_eq!(str_length("ab"), 2);
        assert_eq!(str_length("a b"), 3);
        assert_eq!(str_length("老站务"), 6);
        assert_eq!(str_length("水区"), 4);
        assert_eq!(str_length("测试标题"), 8);
        assert_eq!(str_length("精华区/老站务/"), 14);
        assert_eq!(str_length(""), 0);
        assert_eq!(str_length("a\tb"), 3); // a tab is one cell, as in PHP
    }

    #[test]
    fn first_line_stops_at_the_newline_and_drops_control_characters() {
        // `split_line($s, "", $width, 1, "")` with a one-line limit.
        assert_eq!(first_line("abc\ndef", 55), "abc");
        assert_eq!(first_line("a\rb\u{7}c", 55), "abc");
        assert_eq!(first_line("", 55), "");
        assert_eq!(first_line("a\tb", 55), "a\tb");
    }

    #[test]
    fn first_line_never_splits_a_wide_character() {
        // Five cells take two characters and leave the third out, six take all three.
        assert_eq!(first_line("测试测试测试", 5), "测试");
        assert_eq!(first_line("测试测试测试", 6), "测试测");
        assert_eq!(
            str_length(&first_line(
                "第一行很长很长很长的标题内容需要截断掉一部分才行呢",
                12
            )),
            12
        );
    }

    #[test]
    fn addslashes_backslashes_the_four_php_escapes() {
        // The byte-for-byte result `addslashes("a'b\"c\\d\0e")` gives.
        assert_eq!(addslashes("a'b\"c\\d\0e"), "a\\'b\\\"c\\\\d\\0e");
        assert_eq!(addslashes("水区"), "水区");
    }

    #[test]
    fn a_negative_pad_is_the_php_value_error() {
        assert_eq!(pad(55, 9).unwrap(), " ".repeat(46));
        assert_eq!(pad(55, 55).unwrap(), "");
        let error = pad(20, 24).unwrap_err();
        assert_eq!(
            error.message,
            "str_repeat(): Argument #2 ($times) must be greater than or equal to 0\n"
        );
        assert_eq!(error.code, EXIT_FATAL);
    }

    #[test]
    fn a_null_column_is_the_empty_string_php_saw() {
        assert_eq!(text(&None), "");
        assert_eq!(text(&Some(String::new())), "");
        assert_eq!(text(&Some("水区".to_string())), "水区");
    }

    #[test]
    fn the_two_php_date_formats_render_as_php_did() {
        // `Nov` is PHP's English month abbreviation, whatever the locale of the cron run.
        assert_eq!(
            format_datetime(&dt("2026-11-20 08:09:10"), "M d H:i:s"),
            "Nov 20 08:09:10"
        );
        assert_eq!(
            format_datetime(&dt("2026-11-20 08:09:10"), "Y.m.d"),
            "2026.11.20"
        );
    }

    #[test]
    fn a_null_datetime_becomes_the_current_time() {
        // PHP's deprecated `new DateTimeImmutable(null)`: the notice is dropped, the time kept.
        assert_eq!(
            format_datetime(&None, "Y.m.d"),
            Local::now().naive_local().format("%Y.%m.%d").to_string()
        );
    }

    #[test]
    fn a_query_failure_carries_the_php_prefix_and_status() {
        let failure = Failure::query("Query data error: ", &sqlx::Error::RowNotFound);
        assert!(failure.message.starts_with("Query data error: "));
        assert_eq!(failure.code, EXIT_QUERY);
        assert_eq!(EXIT_FATAL as i32, -1);
        assert_eq!(-1i8 as u8, 255);
        assert_eq!(-2i8 as u8, 254);
        assert_eq!(-3i8 as u8, 253);
    }

    #[test]
    fn a_failed_write_is_the_reason_for_leaving() {
        let dir = std::env::temp_dir().join(format!("lbbs-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("menu");
        write_file(&path, "%menu M_TEST\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "%menu M_TEST\n");

        // The directory the C daemon assumes already exists: `var/gen_ex/`.
        let missing = dir.join("no-such-dir").join("18");
        let error = write_file(&missing, "").unwrap_err();
        // `write_file` prints the path as the platform spells it — backslashes on Windows — so the
        // prefix is read off the same `Path` instead of hand-joining it with a `/`.
        assert!(error.message.starts_with(&format!(
            "file_put_contents({}): Failed to open stream: ",
            missing.display()
        )));
        assert_eq!(error.code, EXIT_FATAL);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
