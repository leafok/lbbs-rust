/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The two top-ten files, ported from `utils/bin/gen_top.php`.
//!
//! `var/bbs_top.txt` (`:11`) is the text the `T`op menu item pages through and `data_files_startup`
//! preloads; `var/bbs_top_menu.conf` (`:12`) is the menu block the daemon appends to its menu set,
//! whose `@LOCATE_ARTICLE` lines carry the same rendered rows so the screen and the menu cannot
//! drift apart. Both come from one query (`:32-38`): the ten most-read articles of the recommended
//! boards posted in the last week, ordered by digest first, then by reads plus replies, then by
//! how often the piece was forwarded.
//!
//! The layout is `:14-30` for the headers, `:47-81` for one row and `:86-90` for the tail, and the
//! whole thing is `buffer_text` and `buffer_menu` written out at the end.
//!
//! # Deviations from PHP
//!
//! * `split_line($row["title"], "", 60, 1, "")` with a one-line limit is the truncation
//!   [`batch::first_line`] performs, ANSI sequences and all — this generator has no
//!   `skip_ctrl_seq` argument to pass, so a colour sequence inside a title costs the title its
//!   cells.
//! * A `NULL` `title`, `sname`, `s_title` or `username` is printed as the empty string. PHP 8.4
//!   coerces it for `strlen()` and `str_length()` — with a deprecation notice — but raises a
//!   `TypeError` when `split_line()` is handed one, which stops the script before it writes
//!   either file.
//! * `new DateTimeImmutable($row["sub_dt"])` on a `NULL` column is the current time plus a
//!   notice; [`batch::format_datetime`] keeps the time.
//! * The padding is [`batch::pad`], so a board title of more than twenty cells — `varchar(20)`
//!   holds twenty characters, which is forty cells — makes this tool fail the way PHP's
//!   `str_repeat()` did instead of writing a ragged line.

use chrono::NaiveDateTime;
use sqlx::FromRow;

use crate::batch::{self, Batch, Failure};
use crate::paths::{CONF_TOP10_MENU, VAR_BBS_TOP};

/// The query of `:32-38`. `section_config.recommend` narrows it to the boards the staff flagged,
/// and `SUBDATE(NOW(), INTERVAL 7 DAY)` is evaluated by the server, in the session time zone
/// `db_open` set.
const SQL: &str = "SELECT AID, bbs.title AS title, sname,
			section_config.title AS s_title, username, sub_dt
			FROM bbs INNER JOIN section_config ON bbs.SID = section_config.SID
			WHERE section_config.recommend AND TID = 0 AND visible AND view_count >= 10
			AND (sub_dt >= SUBDATE(NOW(), INTERVAL 7 DAY))
			ORDER BY excerption DESC, (view_count + reply_count) DESC, transship
			LIMIT 10";

/// The banner, `:15`'s string with its trailing blank line.
const TEXT_HEADER: &str = concat!(
    "                     \x1b[1;34m-----\x1b[37m=====\x1b[41;37m 本站十大热门话题 \x1b[40m=====\x1b[34m-----\x1b[m\n",
    "\n",
);

/// `S_TOP10`, the screen block the menu file ends with (`:17-22`). The heredoc's blank line before
/// its terminator is not part of the string, which is why these blocks end after the last text
/// line.
const SCREEN_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%S_TOP10\n",
    "                     \x1b[1;34m-----\x1b[37m=====\x1b[41;37m 本站十大热门话题 \x1b[40m=====\x1b[34m-----\x1b[m\n",
);

/// `M_TOP10`, `:24-30`.
const MENU_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%menu M_TOP10\n",
    "title       0, 0, \"十大热门话题\"\n",
    "screen      2, 0, S_TOP10\n",
);

/// One row of [`SQL`].
#[derive(Debug, Clone, FromRow)]
pub struct Entry {
    #[sqlx(rename = "AID")]
    pub aid: i32,
    pub title: Option<String>,
    pub sname: Option<String>,
    pub s_title: Option<String>,
    pub username: Option<String>,
    pub sub_dt: Option<NaiveDateTime>,
}

/// `:14-90`: the text file and the menu file the ten rows produce.
pub fn assemble(rows: &[Entry]) -> Result<(String, String), Failure> {
    let mut text = String::from(TEXT_HEADER);
    let mut screen = String::from(SCREEN_HEADER);
    let mut menu = String::from(MENU_HEADER);

    for (index, row) in rows.iter().enumerate() {
        let rank = index + 1;
        let (line_section, line_article) = render(row, rank)?;

        // `:69-76`.
        text.push_str(&line_section);
        text.push_str("\n  ");
        text.push_str(&line_article);
        text.push('\n');

        screen.push_str(&line_section);
        screen.push_str("\n\n");

        // `:73`: the row the article lands on is two lines per rank plus the two header lines.
        menu.push_str(&format!(
            "@LOCATE_ARTICLE   {}, 3, 1, 0,   \"{}|{}\",    \"{}\"\n",
            rank * 2 + 2,
            batch::text(&row.sname),
            row.aid,
            line_article
        ));
    }

    // `:86-87`: the screen closes, and the menu file ends with it.
    screen.push_str("%\n");
    menu.push_str("%\n");
    menu.push_str(&screen);

    Ok((text, menu))
}

/// `:50-67`, the two lines one ranking contributes: the board's row and the title's row.
fn render(row: &Entry, rank: usize) -> Result<(String, String), Failure> {
    let s_title = batch::text(&row.s_title);
    let sname = batch::text(&row.sname);
    let username = batch::text(&row.username);

    let board_pad = batch::pad(20, (batch::str_length(s_title) + sname.len()) as i64)?;
    let user_pad = batch::pad(16, username.len() as i64)?;

    let line_section = format!(
        " \x1b[1;37m第 \x1b[31m{rank:>2} \x1b[37m名 版块 : \x1b[33m{s_title} [{sname}]{board_pad} \
         \x1b[37m【 \x1b[32m{} \x1b[37m】\x1b[35m{user_pad}{username} ",
        batch::format_datetime(&row.sub_dt, "M d H:i:s")
    );

    // `:50`, then `:63-67`: sixty cells, padded out to sixty.
    let title = batch::first_line(batch::text(&row.title), 60);
    let title_pad = batch::pad(60, batch::str_length(&title) as i64)?;
    let line_article =
        format!(" \x1b[1;37m       标题 : \x1b[44;37m{title}{title_pad}  \x1b[0;40;37m");

    Ok((line_section, line_article))
}

/// The script's body: query, render, write both files.
pub async fn run(tool: Batch) -> Result<(), Failure> {
    let rows: Vec<Entry> = sqlx::query_as(SQL)
        .fetch_all(tool.db.pool())
        .await
        .map_err(|error| Failure::query("Query data error: ", &error))?;

    let (text, menu) = assemble(&rows)?;

    batch::write_file(&tool.path(VAR_BBS_TOP), &text)?;
    batch::write_file(&tool.path(CONF_TOP10_MENU), &menu)
}

#[cfg(test)]
#[path = "../tests/test_gen_top.rs"]
mod tests;
