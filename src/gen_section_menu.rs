/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `var/menu_merged.conf`, ported from `utils/bin/gen_section_menu.php`.
//!
//! The file the daemon actually reads is the hand-written `conf/menu.conf` (`:12`) with six
//! generated blocks appended: the class list `M_EGROUP` (`:89-153`), one `M__<cname>` board menu
//! per class, the digest announce list `M_ANNOUNCE` and its `M_EX_<cname>` menus (`:234-303`),
//! the flat `M_BOARD` list of every board (`:306-342`) and `M_FAVOR_SECTION` (`:345-382`), which
//! repeats those rows with the `SID` in field 3 so a favourite can jump to a board that is not on
//! the page.
//!
//! The numbers come from [`crate::section_list`] twice: once with the article count and the
//! current board master (`:25-80`), once with only the boards that have a digest article, plus the
//! count of those and the moment their own list was last generated (`:156-231`).
//!
//! # Deviations from PHP
//!
//! * `conf/menu.conf` is read as text. The PHP appended whatever bytes it had read, so a file with
//!   an undecodable sequence stops this tool with the same `Unable to load menu config file` line
//!   the missing file gets.
//! * The filter and the value-fetcher of `load_section_list` are one callback here, so the
//!   `SELECT AID … LIMIT 1` that decides whether a board has digests runs before the two queries
//!   that measure it, which is the order PHP ran the two callables in.
//! * A failed per-section query keeps the PHP's behaviour: print the driver's text to stdout and
//!   carry on with the default, which leaves a board counted as empty and a board without a master
//!   (`:43-47`, `:62-66`, `:190-194`, `:207-211`). Where that default was the poison value `0`
//!   handed to `DateTimeImmutable` — which raises `DateMalformedStringException`, so the run died
//!   with no file written — the port never reaches a date: that default is always paired with a
//!   digest count of `0`, and a `0` count drops the row (`:280-283`).
//! * `intval(log10($count))` stays the float it was for PHP rather than becoming a digit count, so
//!   `999` and `1000` both pad by `5 - 2` at the boundary where `log10` rounds.

use chrono::NaiveDateTime;
use sqlx::FromRow;

use crate::batch::{self, Batch, EXIT_DEEP, EXIT_QUERY, Failure};
use crate::db::Db;
use crate::paths::{CONF_MENU, CONF_MENU_SRC};
use crate::section_list::{self, Group};

/// `:89-95`.
const EGROUP_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%menu M_EGROUP\n",
    "title       0, 0, \"[栏目列表]\"\n",
    "screen      2, 0, S_EGROUP\n",
);

/// `:234-240`.
const ANNOUNCE_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%menu M_ANNOUNCE\n",
    "title       0, 0, \"精华公布栏\"\n",
    "screen      2, 0, S_EGROUP\n",
);

/// `:306-313`.
const BOARD_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%menu M_BOARD\n",
    "title       0, 0, \"[版块列表]\"\n",
    "screen      2, 0, S_BOARD\n",
    "page        4, 1, 20\n",
);

/// `:345-353`.
const FAVOR_HEADER: &str = concat!(
    "#---------------------------------------------------------------------\n",
    "%menu M_FAVOR_SECTION\n",
    "title       0, 0, \"[版块收藏]\"\n",
    "screen      2, 0, S_BOARD\n",
    "use_filter\n",
    "page        4, 1, 20\n",
);

/// Every generated menu block ends with this (`:113-116` and the four after it).
const MENU_END: &str = "%\n";

/// `:25-80`'s two values per board.
#[derive(Debug, Clone)]
pub struct Counts {
    pub article_count: i64,
    pub section_master: String,
}

/// `:156-231`'s two values per digest board.
#[derive(Debug, Clone)]
pub struct ExCounts {
    pub ex_menu_tm: Option<NaiveDateTime>,
    pub ex_article_count: i64,
}

/// `:39-40`, the visible articles of one board.
const ARTICLE_COUNT_SQL: &str =
    "SELECT COUNT(*) AS article_count FROM bbs WHERE SID = ? AND visible";

/// `:56-59`: the major master if the board has one, otherwise any master in term.
const SECTION_MASTER_SQL: &str = concat!(
    "SELECT user_list.UID, user_list.username, section_master.major FROM section_master
                    INNER JOIN user_list ON section_master.UID = user_list.UID WHERE SID = ?",
    " AND section_master.enable AND (NOW() BETWEEN begin_dt AND end_dt)
                    ORDER BY major DESC LIMIT 1"
);

/// `:164-165`: whether this board has anything in its digest tree at all.
const HAS_EX_SQL: &str =
    "SELECT AID AS ex_article_count FROM bbs WHERE SID = ? AND visible AND gen_ex LIMIT 1";

/// `:188` and `:203-204`.
const EX_MENU_TM_SQL: &str = "SELECT ex_menu_tm FROM section_config WHERE SID = ?";
const EX_COUNT_SQL: &str =
    "SELECT COUNT(*) AS ex_article_count FROM bbs WHERE SID = ? AND visible AND gen_ex";

#[derive(Debug, FromRow)]
struct CountRow {
    article_count: i64,
}

#[derive(Debug, FromRow)]
struct MasterRow {
    username: Option<String>,
}

#[derive(Debug, FromRow)]
struct ExTmRow {
    ex_menu_tm: Option<NaiveDateTime>,
}

/// `EX_COUNT_SQL`'s own alias, which is what the PHP read as `$row["ex_article_count"]`.
#[derive(Debug, FromRow)]
struct ExCountRow {
    ex_article_count: i64,
}

/// `:15-20`: the hand-written menus the generated blocks are appended to.
fn read_menu_source(tool: &Batch) -> Result<String, Failure> {
    let path = tool.path(CONF_MENU_SRC);
    std::fs::read_to_string(&path).map_err(|_| {
        Failure::new(
            format!("Unable to load menu config file {}\n", path.display()),
            EXIT_QUERY,
        )
    })
}

/// `:97-111` and `:242-256`, the two class lists. Only the menu an item opens differs, and the
/// first class of the list is the one drawn on the menu's fourth row.
fn class_items<T>(prefix: &str, groups: &[Group<T>]) -> Result<String, Failure> {
    let mut out = String::new();
    for (index, group) in groups.iter().enumerate() {
        let display_row = if index == 0 { 4 } else { 0 };
        out.push_str(&format!(
            "!{prefix}{}   {display_row}, 4, 1, 0,   \"{}\",    \"{}\"\n",
            group.name,
            group.name,
            class_title(group.sections.len(), &group.name, &group.title)?
        ));
    }
    Ok(out)
}

/// `:103-105`: the class's section count, its name padded to 32 bytes, its title in brackets.
fn class_title(count: usize, name: &str, title: &str) -> Result<String, Failure> {
    Ok(format!(
        "{}{count} ＋  {name}{}[{}]",
        count_pad(count as i64)?,
        batch::pad(32, name.len() as i64)?,
        batch::addslashes(title)
    ))
}

/// `:129`, `:275`, `:319`, `:359`: the class field a board row carries, bracketed and padded to
/// ten cells.
fn class_field(title: &str) -> Result<String, Failure> {
    Ok(format!(
        "[{}]{}",
        batch::addslashes(title),
        batch::pad(10, batch::str_length(title) as i64)?
    ))
}

/// `:137-140` and `:325-328`: count, board name padded to 20 bytes, class, board title padded to
/// 22 cells, and the master or the "board master wanted" text.
fn section_title(
    count: i64,
    name: &str,
    class_field: &str,
    title: &str,
    tail: &str,
) -> Result<String, Failure> {
    Ok(format!(
        "{}{count} ＋  {name}{}{class_field}{}{}{}",
        count_pad(count)?,
        batch::pad(20, name.len() as i64)?,
        batch::addslashes(title),
        batch::pad(22, batch::str_length(title) as i64)?,
        if tail.is_empty() {
            "诚征版主中"
        } else {
            tail
        }
    ))
}

/// `str_repeat(" ", 5 - intval(log10($count == 0 ? 1 : $count)))` (`:103`, `:137`, `:285`, `:325`).
fn count_pad(count: i64) -> Result<String, Failure> {
    let count = if count == 0 { 1 } else { count };
    batch::pad(5, (count as f64).log10() as i64)
}

/// `:118-153`: one `M__<cname>` menu per class, its boards in their own order.
fn class_menus(groups: &[Group<Counts>]) -> Result<String, Failure> {
    let mut out = String::new();
    for group in groups {
        out.push_str(&format!(
            "#---------------------------------------------------------------------\n\
             %menu M__{}\n\
             title       0, 0, \"[{}]\"\n\
             screen      2, 0, S_BOARD\n",
            group.name,
            batch::addslashes(&group.title)
        ));

        let class_field = class_field(&group.title)?;
        for (index, (section, counts)) in group.sections.iter().enumerate() {
            let display_row = if index == 0 { 4 } else { 0 };
            out.push_str(&format!(
                "@LIST_SECTION   {display_row}, 4, 1, {},   \"{}\",    \"{}\"\n",
                section.read_user_level,
                section.name,
                section_title(
                    counts.article_count,
                    &section.name,
                    &class_field,
                    &section.title,
                    &counts.section_master
                )?
            ));
        }

        out.push_str(MENU_END);
    }
    Ok(out)
}

/// `:263-303`: the digest equivalent, with the empty boards dropped and only the first row that
/// survives the drop landing on row 4.
fn ex_class_menus(groups: &[Group<ExCounts>]) -> Result<String, Failure> {
    let mut out = String::new();
    for group in groups {
        out.push_str(&format!(
            "#---------------------------------------------------------------------\n\
             %menu M_EX_{}\n\
             title       0, 0, \"精华区 > {}\"\n\
             screen      2, 0, S_EX_DIR\n",
            group.name,
            batch::addslashes(&group.title)
        ));

        let class_field = class_field(&group.title)?;
        let mut display_row = 4;
        for (section, counts) in &group.sections {
            if counts.ex_article_count == 0 {
                continue;
            }

            out.push_str(&format!(
                "@LIST_EX_SECTION   {display_row}, 4, 1, 0,   \"{}\",    \"{}\"\n",
                section.name,
                section_title(
                    counts.ex_article_count,
                    &section.name,
                    &class_field,
                    &section.title,
                    &batch::format_datetime(&counts.ex_menu_tm, "Y.m.d")
                )?
            ));
            display_row = 0;
        }

        out.push_str(MENU_END);
    }
    Ok(out)
}

/// `:317-342` and `:357-382`: every board of every class in one list, the first row of the whole
/// list on row 4. `M_FAVOR_SECTION` is the same text with the `SID` in field 3.
fn flat_menus(groups: &[Group<Counts>], with_sid: bool) -> Result<String, Failure> {
    let mut out = String::new();
    let mut display_row = 4;

    for group in groups {
        let class_field = class_field(&group.title)?;
        for (section, counts) in &group.sections {
            let title = section_title(
                counts.article_count,
                &section.name,
                &class_field,
                &section.title,
                &counts.section_master,
            )?;

            if with_sid {
                out.push_str(&format!(
                    "@LIST_SECTION   {display_row}, 4, {}, {},   \"{}\",    \"{title}\"\n",
                    section.sid, section.read_user_level, section.name
                ));
            } else {
                out.push_str(&format!(
                    "@LIST_SECTION   {display_row}, 4, 1, {},   \"{}\",    \"{title}\"\n",
                    section.read_user_level, section.name
                ));
            }

            display_row = 0;
        }
    }

    out.push_str(MENU_END);
    Ok(out)
}

/// `:89-382`: everything the script appends to the hand-written menus.
pub fn assemble(
    groups: &[Group<Counts>],
    ex_groups: &[Group<ExCounts>],
) -> Result<String, Failure> {
    let mut buffer = String::new();
    buffer.push_str(EGROUP_HEADER);
    buffer.push_str(&class_items("M__", groups)?);
    buffer.push_str(MENU_END);
    buffer.push_str(&class_menus(groups)?);

    buffer.push_str(ANNOUNCE_HEADER);
    buffer.push_str(&class_items("M_EX_", ex_groups)?);
    buffer.push_str(MENU_END);
    buffer.push_str(&ex_class_menus(ex_groups)?);

    buffer.push_str(BOARD_HEADER);
    buffer.push_str(&flat_menus(groups, false)?);

    buffer.push_str(FAVOR_HEADER);
    buffer.push_str(&flat_menus(groups, true)?);

    Ok(buffer)
}

/// The script: read the hand-written menus, ask the database twice, write one file.
pub async fn run(tool: Batch) -> Result<(), Failure> {
    let mut buffer = read_menu_source(&tool)?;

    // `load` walks the rows through the pool of its first argument; the callback needs a handle of
    // its own for the two queries per row, which is the cheap pool clone `db.rs` hands everywhere.
    let rows_db = tool.db.clone();
    let inspect_db = tool.db.clone();
    let groups = section_list::load(&rows_db, move |row| {
        let db = inspect_db.clone();
        async move {
            let sid = row.sid;
            let article_count = count(&db, sid).await;
            let section_master = master(&db, sid).await;
            Some(Counts {
                article_count,
                section_master,
            })
        }
    })
    .await
    .map_err(|error| list_error("Query section list error", error))?;

    let rows_db = tool.db.clone();
    let inspect_db = tool.db.clone();
    let ex_groups = section_list::load(&rows_db, move |row| {
        let db = inspect_db.clone();
        async move {
            let sid = row.sid;
            // `:158-178`: a board with no digest article is not in this list at all.
            if !has_ex_article(&db, sid).await {
                return None;
            }

            let mut ex_menu_tm = None;
            let mut ex_article_count = 0;

            // `:187-200` then `:202-217`. Either failure leaves the count at zero, and a zero
            // count drops the row later, so no date is ever formatted from a bad read.
            match sqlx::query_as::<_, ExTmRow>(EX_MENU_TM_SQL)
                .bind(sid)
                .fetch_optional(db.pool())
                .await
            {
                Ok(Some(row)) => ex_menu_tm = row.ex_menu_tm,
                Ok(None) => {}
                Err(error) => {
                    print!("{error}");
                    return Some(ExCounts {
                        ex_menu_tm,
                        ex_article_count,
                    });
                }
            }

            match sqlx::query_as::<_, ExCountRow>(EX_COUNT_SQL)
                .bind(sid)
                .fetch_one(db.pool())
                .await
            {
                Ok(row) => ex_article_count = row.ex_article_count,
                Err(error) => print!("{error}"),
            }

            Some(ExCounts {
                ex_menu_tm,
                ex_article_count,
            })
        }
    })
    .await
    .map_err(|error| list_error("Query excerptional section list error", error))?;

    buffer.push_str(&assemble(&groups, &ex_groups)?);

    batch::write_file(&tool.path(CONF_MENU), &buffer)
}

/// One of the two `load_section_list` calls, each with its own step name and the same `-3`.
fn list_error(step: &str, error: sqlx::Error) -> Failure {
    Failure::new(format!("{step}: {error}"), EXIT_DEEP)
}

/// `:38-53`: the visible article count, zero when the query failed — the board still appears.
async fn count(db: &Db, sid: i16) -> i64 {
    match sqlx::query_as::<_, CountRow>(ARTICLE_COUNT_SQL)
        .bind(sid)
        .fetch_one(db.pool())
        .await
    {
        Ok(row) => row.article_count,
        Err(error) => {
            print!("{error}");
            0
        }
    }
}

/// `:55-74`: the master's name, empty when there is none or the query failed.
async fn master(db: &Db, sid: i16) -> String {
    match sqlx::query_as::<_, MasterRow>(SECTION_MASTER_SQL)
        .bind(sid)
        .fetch_optional(db.pool())
        .await
    {
        Ok(row) => row
            .map(|row| batch::text(&row.username).to_string())
            .unwrap_or_default(),
        Err(error) => {
            print!("{error}");
            String::new()
        }
    }
}

/// `:161-177`: the filter of the digest pass, whose failure also drops the board.
async fn has_ex_article(db: &Db, sid: i16) -> bool {
    match sqlx::query(HAS_EX_SQL)
        .bind(sid)
        .fetch_optional(db.pool())
        .await
    {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            print!("{error}");
            false
        }
    }
}

#[cfg(test)]
#[path = "../tests/test_gen_section_menu.rs"]
mod tests;
