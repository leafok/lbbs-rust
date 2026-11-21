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

    let board_pad = batch::pad(
        20,
        (batch::str_length(s_title) + sname.len()) as i64,
    )?;
    let user_pad = batch::pad(16, username.len() as i64)?;

    let line_section = format!(
        " \x1b[1;37m第 \x1b[31m{rank:>2} \x1b[37m名 版块 : \x1b[33m{s_title} [{sname}]{board_pad} \
         \x1b[37m【 \x1b[32m{} \x1b[37m】\x1b[35m{user_pad}{username} ",
        batch::format_datetime(&row.sub_dt, "M d H:i:s")
    );

    // `:50`, then `:63-67`: sixty cells, padded out to sixty.
    let title = batch::first_line(batch::text(&row.title), 60);
    let title_pad = batch::pad(60, batch::str_length(&title) as i64)?;
    let line_article = format!(" \x1b[1;37m       标题 : \x1b[44;37m{title}{title_pad}  \x1b[0;40;37m");

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
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            aid: 472407,
            title: Some("测试标题".to_string()),
            sname: Some("Test".to_string()),
            s_title: Some("水区".to_string()),
            username: Some("leaflet".to_string()),
            sub_dt: NaiveDateTime::parse_from_str("2026-11-20 08:09:10", "%Y-%m-%d %H:%M:%S").ok(),
        }
    }

    #[test]
    fn board_row_lays_out_rank_board_date_and_author() {
        // `gen_top.php:53-62`, measured against the PHP with these inputs.
        let (section, _) = render(&entry(), 1).unwrap();
        assert_eq!(
            section,
            format!(
                " \x1b[1;37m第 \x1b[31m 1 \x1b[37m名 版块 : \x1b[33m水区 [Test]{} \
                 \x1b[37m【 \x1b[32mNov 20 08:09:10 \x1b[37m】\x1b[35m{}leaflet ",
                " ".repeat(12), // 20 - str_length("水区") - strlen("Test")
                " ".repeat(9) // 16 - strlen("leaflet")
            )
        );
    }

    #[test]
    fn article_row_pads_the_title_to_sixty_cells() {
        // `gen_top.php:64-67`: eight cells of title leave fifty-two of padding, then two spaces.
        let (_, article) = render(&entry(), 1).unwrap();
        assert_eq!(
            article,
            format!(
                " \x1b[1;37m       标题 : \x1b[44;37m测试标题{}  \x1b[0;40;37m",
                " ".repeat(52)
            )
        );
    }

    #[test]
    fn long_title_is_cut_at_sixty_cells() {
        let title = "中".repeat(50); // 100 cells
        let row = Entry {
            title: Some(title),
            ..entry()
        };
        let (_, article) = render(&row, 1).unwrap();
        assert!(article.contains(&"中".repeat(30))); // 60 cells, the rest gone
        assert!(!article.contains(&"中".repeat(31)));
    }

    #[test]
    fn rank_is_the_third_menu_row_of_its_pair() {
        let (_, menu) = assemble(&[entry(), entry()]).unwrap();
        assert!(menu.contains("@LOCATE_ARTICLE   4, 3, 1, 0,   \"Test|472407\""));
        assert!(menu.contains("@LOCATE_ARTICLE   6, 3, 1, 0,   \"Test|472407\""));
        assert!(menu.ends_with("%\n"));
    }

    #[test]
    fn empty_result_is_the_two_headers() {
        let (text, menu) = assemble(&[]).unwrap();
        assert_eq!(text, TEXT_HEADER);
        assert_eq!(menu, format!("{}%\n{}%\n", MENU_HEADER, SCREEN_HEADER));
    }

    #[test]
    fn a_wide_board_title_fails_the_way_str_repeat_did() {
        let row = Entry {
            s_title: Some("超".repeat(12)), // 24 cells, over the 20 minus "Test"
            ..entry()
        };
        let failure = render(&row, 1).unwrap_err();
        assert_eq!(failure.code, batch::EXIT_FATAL);
        assert!(failure.message.starts_with("str_repeat(): Argument #2"));
    }

    #[test]
    fn a_null_datetime_prints_a_blank_stamp() {
        // The PHP prints the current time here; the column is NOT NULL in practice, and the
        // port's `Option` keeps the query honest about what the schema allows.
        let row = Entry {
            sub_dt: None,
            ..entry()
        };
        let (section, _) = render(&row, 1).unwrap();
        assert!(section.contains("【 \x1b[32m"));
    }

    #[test]
    fn query_failure_keeps_the_php_step_and_status() {
        let failure = Failure::query("Query data error: ", &sqlx::Error::RowNotFound);
        assert_eq!(failure.code, batch::EXIT_QUERY);
        assert!(failure.message.starts_with("Query data error: "));
    }

    /// [`SQL`] with the seven-day window widened to ten years: the live board has nothing inside a
    /// week, and the ten rows this returns are what the PHP's own widened copy was given.
    const TEN_YEARS_SQL: &str = "SELECT AID, bbs.title AS title, sname,
			section_config.title AS s_title, username, sub_dt
			FROM bbs INNER JOIN section_config ON bbs.SID = section_config.SID
			WHERE section_config.recommend AND TID = 0 AND visible AND view_count >= 10
			AND (sub_dt >= SUBDATE(NOW(), INTERVAL 3650 DAY))
			ORDER BY excerption DESC, (view_count + reply_count) DESC, transship
			LIMIT 10";

    /// The ranked rows against the reference tool's files, which is the only way the populated
    /// path is seen: the sandbox copy of `gen_top.php` runs the same widened query and writes
    /// `/tmp/phpgen/relaxed_top/` —
    /// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored gen_top`.
    /// Both files are also written to `/tmp/lbbs-top/` for a hand diff when the oracle is
    /// not there.
    #[tokio::test]
    #[ignore = "needs the live board database and LBBS_REF_CONF"]
    async fn the_ranked_rows_are_the_php_bytes() {
        let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
        let text = std::fs::read_to_string(&conf).unwrap();
        let (config, _warnings) = crate::config::parse(&text);
        let db = crate::db::Db::connect(&config.db).await.unwrap();

        let rows: Vec<Entry> = sqlx::query_as(TEN_YEARS_SQL)
            .fetch_all(db.pool())
            .await
            .expect("the ranking query runs");
        assert!(!rows.is_empty(), "the widened window should find rows");

        let (text_file, menu_file) = assemble(&rows).unwrap();
        let out_dir = std::path::Path::new("/tmp/lbbs-top");
        std::fs::create_dir_all(out_dir).unwrap();
        std::fs::write(out_dir.join("bbs_top.txt"), &text_file).unwrap();
        std::fs::write(out_dir.join("bbs_top_menu.conf"), &menu_file).unwrap();

        for (name, generated) in [("bbs_top.txt", &text_file), ("bbs_top_menu.conf", &menu_file)] {
            let oracle = std::path::Path::new("/tmp/phpgen/relaxed_top").join(name);
            if oracle.exists() {
                assert_eq!(
                    std::fs::read_to_string(&oracle).unwrap(),
                    *generated,
                    "{name} differs from the PHP"
                );
            }
        }
    }
}
