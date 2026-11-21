/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The digest trees under `var/gen_ex/`, ported from `utils/bin/gen_ex_list.php`.
//!
//! One file per board, named by its `SID` (`:148`), holding a `%menu M_EX_<sid>_<fid>` block for
//! the board's digest root and for every directory below it, plus the `S_EX_DIR` screen that
//! `M_EX_*` items open (`:100-127`). `gen_ex_dir` (`:2-88`) is the recursion: a directory's block
//! is emitted, then each sub-directory's whole tree is generated *before* the line that opens it
//! (`:30-36`), which is why a sub-menu always precedes its parent's item.
//!
//! Only the boards the daemon flagged are touched — `ex_menu_update` (`:132`) is set when a
//! digest article appears or disappears — and each file written is followed by the `UPDATE` that
//! clears the flag and stamps `ex_menu_tm` (`:151`), the timestamp `gen_section_menu` prints in
//! its own board rows.
//!
//! # Deviations from PHP
//!
//! * `dir REGEXP '^<dir>[^/]+/$'` and `IF(dir IS NULL, '', dir) = '<dir>'` had their argument
//!   interpolated by the PHP after `mysqli_real_escape_string` (`:17-19`, `:50-56`). The pattern
//!   is now built by `CONCAT` and the path is a bound parameter, so the same text reaches the
//!   server with no escaping left to audit; `SID` was already a column value.
//! * A padded field that would need a negative number of spaces — a 50-character directory name
//!   is far more than 55 cells — was a fatal `ValueError` in PHP, which stopped the whole run
//!   mid-file-list. [`batch::pad`] returns the failure and it propagates out of the recursion the
//!   same way, so the boards already written stay written and the rest are not.
//! * `ex_dir.dt` or `bbs.sub_dt` of `NULL` formats as the current date, as PHP's
//!   `new DateTimeImmutable(null)` did; a malformed one, which PHP rejected with
//!   `DateMalformedStringException`, is a decode failure of the query and drops that file.
//! * `file_put_contents()` into `var/gen_ex/` needs the directory to exist. PHP assumed the
//!   board's own copy of it does, and so this reports the failure as a fatal write error.

use chrono::NaiveDateTime;
use sqlx::FromRow;

use crate::batch::{self, Batch, Failure, EXIT_DEEP};
use crate::db::Db;
use crate::paths::VAR_GEN_EX_MENU_DIR;

/// `:100-127`, the screen every digest menu opens. The twenty blank rows are the menu area, and
/// the header's space run is what puts 日 期 over the date column — the length test below is what
/// keeps this block byte-for-byte the PHP's.
const SCREEN: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%S_EX_DIR\n",
    "  返回[\x1b[1;32m←\x1b[0;37m] 进入[\x1b[1;32m→\x1b[0;37m] 选择[\x1b[1;32m↑ PgUp\x1b[0;37m,\x1b[1;32m↓ PgDn\x1b[0;37m]\n",
    "\x1b[44;37m    \x1b[1;37m编  号  文章标题                                                日  期      \x1b[m\n",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    "%\n",
);

/// `:17-19`: the directories one level below the given path. The PHP interpolated the escaped
/// path into the `REGEXP`; `CONCAT` hands the same pattern to the server with nothing to audit.
const CHILDREN_SQL: &str = concat!(
    "SELECT FID, dir, name, dt FROM ex_dir WHERE SID = ? AND dir REGEXP ",
    "CONCAT('^', ?, '[^/]+/$') AND enable ORDER BY dir"
);

/// `:50-56`: the articles of one directory, `IF(dir IS NULL, '', dir)` being how the root — whose
/// path is the empty string — collects the files that are in no directory.
const ARTICLES_SQL: &str = concat!(
    "SELECT bbs.AID, title, sub_dt FROM bbs
				LEFT JOIN ex_file ON bbs.AID = ex_file.AID
				LEFT JOIN ex_dir ON ex_file.FID = ex_dir.FID
				WHERE bbs.SID = ? AND TID = 0 AND visible AND gen_ex
				AND IF(dir IS NULL, '', dir) = ? ORDER BY bbs.AID"
);

/// `:129-133`: the boards whose digest tree is out of date.
const SECTIONS_SQL: &str = concat!(
    "SELECT section_config.SID, section_config.title AS s_title
				FROM section_config
				INNER JOIN section_class ON	section_config.CID = section_class.CID
				WHERE section_config.enable AND section_class.enable AND ex_menu_update
				ORDER BY section_class.sort_order, section_config.sort_order"
);

/// `:151`: the flag this tool clears and the timestamp `gen_section_menu` prints.
const MARK_FRESH_SQL: &str =
    "UPDATE section_config SET ex_menu_tm = NOW(), ex_menu_update = 0 WHERE SID = ?";

/// A directory's own block, `:8-15`.
fn menu_header(sid: i16, fid: i32, name: &str) -> String {
    format!(
        "#---------------------------------------------------------------------\n\
         %menu M_EX_{sid}_{fid}\n\
         title       0, 0, \"精华区 > {name}\"\n\
         screen      2, 0, S_EX_DIR\n\
         page        4, 1, 20\n"
    )
}

/// `:41-44`: the line that opens a sub-directory. Field 5 is the raw path, which is what the
/// `S_EX_DIR` screen and `@LIST_EX_SECTION` hand back as the current directory.
fn dir_item(sid: i16, fid: i32, dir: &str, name: &str, dt: &Option<NaiveDateTime>, row: i32) -> Result<String, Failure> {
    let name_f = format!(
        "{}{}",
        batch::addslashes(name),
        batch::pad(55, batch::str_length(name) as i64)?
    );
    Ok(format!(
        "!M_EX_{sid}_{fid}   {row}, 4, 1, 0, \"{dir}\", \" [目录]  {name_f} {}\"\n",
        batch::format_datetime(dt, "Y.m.d")
    ))
}

/// `:66-74`: an article of one directory. The padding counts the title before its quotes were
/// backslashed, so an escaped title is longer than the fifty-five cells it was measured in.
fn article_item(
    aid: i32,
    title: &str,
    sub_dt: &Option<NaiveDateTime>,
    row: i32,
) -> Result<String, Failure> {
    let aid_f = aid.to_string();
    let aid_f = format!("{}{aid}", batch::pad(7, aid_f.len() as i64)?);

    let cut = batch::first_line(title, 55);
    let padded = format!(
        "{}{}",
        batch::addslashes(&cut),
        batch::pad(55, batch::str_length(&cut) as i64)?
    );

    Ok(format!(
        "@VIEW_EX_ARTICLE   {row}, 4, 1, 0, \"{aid}\", \"{aid_f}  {padded} {}\"\n",
        batch::format_datetime(sub_dt, "Y.m.d")
    ))
}

/// `:2-88`: this directory's block, then every directory below it, then its own articles. `None`
/// is the `false` the PHP returned after printing the reason.
async fn gen_ex_dir(
    db: &Db,
    sid: i16,
    fid: i32,
    dir: &str,
    name: &str,
) -> Result<Option<String>, Failure> {
    let mut output = String::new();
    let mut output_cur_dir = menu_header(sid, fid, name);

    // `:17-48`, the child directories of this one: the pattern is this path plus exactly one more
    // segment, which is how the flat `ex_dir` table is walked as a tree.
    let children: Vec<DirRow> = match sqlx::query_as::<_, DirRow>(CHILDREN_SQL)
        .bind(sid)
        .bind(dir)
        .fetch_all(db.pool())
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            print!("Query dir_list error: {error}");
            return Ok(None);
        }
    };

    let mut display_row = 4;
    for child in children {
        let child_dir = batch::text(&child.dir);
        match Box::pin(gen_ex_dir(db, sid, child.fid, child_dir, batch::text(&child.name))).await? {
            Some(subtree) => output.push_str(&subtree),
            None => {
                print!("gen_ex_dir({sid}, {child_dir}) error\n");
                return Ok(None);
            }
        }

        output_cur_dir.push_str(&dir_item(
            sid,
            child.fid,
            child_dir,
            batch::text(&child.name),
            &child.dt,
            display_row,
        )?);
        display_row = 0;
    }

    // `:50-78`, the articles filed in exactly this directory.
    let articles: Vec<ArticleRow> = match sqlx::query_as::<_, ArticleRow>(ARTICLES_SQL)
        .bind(sid)
        .bind(dir)
        .fetch_all(db.pool())
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            print!("Query article error: {error}");
            return Ok(None);
        }
    };

    for article in articles {
        output_cur_dir.push_str(&article_item(
            article.aid,
            batch::text(&article.title),
            &article.sub_dt,
            display_row,
        )?);
        display_row = 0;
    }

    // `:80-87`.
    output_cur_dir.push_str("%\n");
    output.insert_str(0, &output_cur_dir);

    Ok(Some(output))
}

/// `:129-166`: every board whose digest tree is out of date, regenerated and marked fresh.
pub async fn run(tool: Batch) -> Result<(), Failure> {
    let sections: Vec<SectionRow> = sqlx::query_as::<_, SectionRow>(SECTIONS_SQL)
        .fetch_all(tool.db.pool())
        .await
        .map_err(|error| Failure::query("Query section error: ", &error))?;

    for section in sections {
        let name = batch::text(&section.s_title);
        let Some(buffer) = gen_ex_dir(&tool.db, section.sid, 0, "", name).await? else {
            // `:161`: this board failed, the next one still runs.
            print!("gen_ex_dir({}, /) error\n", section.sid);
            continue;
        };

        batch::write_file(
            &tool.path(&output_path(section.sid)),
            &format!("{SCREEN}{buffer}"),
        )?;

        // `:151-157`: the write is what earned the timestamp, so a failure stops the run here.
        sqlx::query(MARK_FRESH_SQL)
            .bind(section.sid)
            .execute(tool.db.pool())
            .await
            .map_err(|error| Failure::new(format!("Update tm error: {error}"), EXIT_DEEP))?;
    }

    Ok(())
}

#[derive(Debug, Clone, FromRow)]
struct SectionRow {
    #[sqlx(rename = "SID")]
    sid: i16,
    s_title: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct DirRow {
    #[sqlx(rename = "FID")]
    fid: i32,
    dir: Option<String>,
    name: Option<String>,
    dt: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, FromRow)]
struct ArticleRow {
    #[sqlx(rename = "AID")]
    aid: i32,
    title: Option<String>,
    sub_dt: Option<NaiveDateTime>,
}

/// The board's own file, `:148-149`.
fn output_path(sid: i16) -> String {
    format!("{VAR_GEN_EX_MENU_DIR}{sid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(text: &str) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()
    }

    #[test]
    fn a_directory_block_names_itself_by_sid_and_fid() {
        assert_eq!(
            menu_header(18, 0, "水区"),
            "#---------------------------------------------------------------------\n\
             %menu M_EX_18_0\n\
             title       0, 0, \"精华区 > 水区\"\n\
             screen      2, 0, S_EX_DIR\n\
             page        4, 1, 20\n"
        );
    }

    #[test]
    fn a_directory_line_keeps_the_raw_path_and_pads_the_name() {
        // `gen_ex_list.php:37-44`: six cells of `老站务` leave forty-nine of padding.
        assert_eq!(
            dir_item(18, 23, "精华区/老站务/", "老站务", &dt("2026-01-02 03:04:05"), 4).unwrap(),
            format!(
                "!M_EX_18_23   4, 4, 1, 0, \"精华区/老站务/\", \" [目录]  老站务{} 2026.01.02\"\n",
                " ".repeat(49)
            )
        );
    }

    #[test]
    fn an_article_line_right_aligns_the_number_over_seven_cells() {
        // `gen_ex_list.php:66-74`: six digits take one space, four cells of title take fifty-one.
        assert_eq!(
            article_item(472407, "标题", &dt("2026-11-20 08:09:10"), 0).unwrap(),
            format!(
                "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"472407\", \" 472407  标题{} 2026.11.20\"\n",
                " ".repeat(51)
            )
        );
    }

    #[test]
    fn a_small_aid_is_padded_more_than_a_large_one() {
        // Six spaces in front of a one-digit number, none in front of a seven-digit one.
        assert!(article_item(1, "t", &None, 0).unwrap().starts_with(
            "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"1\", \"      1  t"
        ));
        assert!(article_item(1234567, "t", &None, 0).unwrap().starts_with(
            "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"1234567\", \"1234567  t"
        ));
    }

    #[test]
    fn escaping_a_quote_does_not_recount_the_field() {
        // PHP measures the fifty-five cells before the backslash appears, so the field grows.
        assert_eq!(
            article_item(9, "\"x", &dt("2026-11-20 08:09:10"), 0).unwrap(),
            format!(
                "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"9\", \"      9  \\\"x{} 2026.11.20\"\n",
                " ".repeat(53)
            )
        );
    }

    #[test]
    fn the_screen_block_is_the_php_bytes() {
        assert!(SCREEN.starts_with("#---------------------------------------------------------------------\n%S_EX_DIR\n"));
        assert!(SCREEN.contains("\x1b[44;37m    \x1b[1;37m编  号"));
        assert!(SCREEN.ends_with(&format!("{}%\n", "\n".repeat(20))));
        assert_eq!(SCREEN.len(), 318);
    }

    #[test]
    fn the_board_file_is_named_by_its_sid() {
        assert_eq!(output_path(18), "var/gen_ex/18");
    }

    /// `SELECT title FROM section_config WHERE SID = ?`: the board's name, which is the title of
    /// the root menu block `:8-15` prints.
    const BOARD_TITLE_SQL: &str = "SELECT title FROM section_config WHERE SID = ?";

    /// The tree of two real boards against the reference tool's own files. The live run is
    /// read-only, so the daemon's `ex_menu_update` flag is never set for it to find: the sandbox
    /// copy of `gen_ex_list.php` selects these two boards by `SID`, skips its `UPDATE`, and its
    /// output is what this compares against —
    /// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored gen_ex`.
    /// Each tree is also written to `/tmp/lbbs-gen_ex/` so the two files can be diffed by
    /// hand when the oracle directory is not there.
    #[tokio::test]
    #[ignore = "needs the live board database and LBBS_REF_CONF"]
    async fn a_real_digest_tree_is_the_php_bytes() {
        let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
        let text = std::fs::read_to_string(&conf).unwrap();
        let (config, _warnings) = crate::config::parse(&text);
        let db = crate::db::Db::connect(&config.db).await.unwrap();

        let out_dir = std::path::Path::new("/tmp/lbbs-gen_ex");
        std::fs::create_dir_all(out_dir).unwrap();

        for sid in [13i16, 1] {
            let title: Option<String> = sqlx::query_scalar(BOARD_TITLE_SQL)
                .bind(sid)
                .fetch_one(db.pool())
                .await
                .unwrap();
            let buffer = gen_ex_dir(&db, sid, 0, "", batch::text(&title))
                .await
                .expect("the tree generates")
                .expect("the board's queries succeeded");
            let file = format!("{SCREEN}{buffer}");
            std::fs::write(out_dir.join(sid.to_string()), &file).unwrap();

            let oracle = std::path::Path::new("/tmp/phpgen/oracle_live2/gen_ex").join(sid.to_string());
            if oracle.exists() {
                assert_eq!(
                    std::fs::read_to_string(&oracle).unwrap(),
                    file,
                    "board {sid} differs from the PHP"
                );
            }
        }
    }
}
