/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The class/section hierarchy the batch generators walk, ported from
//! `utils/lib/section_list.inc.php` — `load_section_list` (`:3-69`), whose SQL is `:7-13` and
//! whose grouping is `:29-63`.
//!
//! The PHP function takes two callables: a `$filter` that decides whether a section is in the
//! output at all, and a `$udf_value_gen` that fetches whatever extra numbers the caller wants
//! beside each section. Both talk to the database, so both become one async callback here:
//! [`load`] hands it every row in the query's order and keeps the section only when the
//! callback returns `Some`.
//!
//! # Deviations from PHP
//!
//! * The PHP streamed rows with `mysqli_fetch_array` and called the two callables from inside
//!   that loop; here the rows are collected first and then walked. The statement, its order
//!   and the per-section queries are unchanged, so the generated files and the number of
//!   queries are identical — only the peak memory differs.
//! * A `NULL` column arrives as PHP `null`, which every caller here immediately prints as the
//!   empty string. `Option<String>` plus `unwrap_or_default` says the same thing, in the open.
//! * `$last_cid = -1` starts the grouping in PHP, where `CID` is a signed `smallint` that a
//!   board class can genuinely have. A class with `CID = -1` therefore loses its first flush;
//!   the port keeps that start value rather than inventing a "no previous class" marker,
//!   because the difference would show up in the generated menu.

use std::future::Future;

use sqlx::FromRow;

use crate::db::Db;

/// One row of `section_config INNER JOIN section_class`, in the query's own order.
#[derive(Debug, Clone, FromRow)]
pub struct Row {
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "CID")]
    pub cid: i16,
    pub sname: Option<String>,
    pub s_title: Option<String>,
    pub comment: Option<String>,
    pub cname: Option<String>,
    pub c_title: Option<String>,
    pub recommend: bool,
    pub read_user_level: i16,
    pub write_user_level: i16,
}

/// The `"name"`, `"title"` and friends PHP builds for a section (`:49-57`), minus its
/// `udf_values`, which [`load`] keeps beside the section instead.
#[derive(Debug, Clone)]
pub struct Section {
    pub sid: i16,
    pub name: String,
    pub title: String,
    pub comment: String,
    pub read_user_level: i16,
    pub write_user_level: i16,
}

/// One entry of the hierarchy: `"cid"`, `"name"`, `"title"`, `"sections"` (`:34-40`).
#[derive(Debug, Clone)]
pub struct Group<T> {
    pub cid: i16,
    pub name: String,
    pub title: String,
    pub sections: Vec<(Section, T)>,
}

impl From<&Row> for Section {
    fn from(row: &Row) -> Self {
        Section {
            sid: row.sid,
            name: text(&row.sname),
            title: text(&row.s_title),
            comment: text(&row.comment),
            read_user_level: row.read_user_level,
            write_user_level: row.write_user_level,
        }
    }
}

/// `load_section_list`: the two enabled-only queries' worth of rows, grouped into classes and
/// reduced by the caller's filter, with the caller's per-section values beside each section.
pub async fn load<F, Fut, T>(db: &Db, mut inspect: F) -> Result<Vec<Group<T>>, sqlx::Error>
where
    F: FnMut(Row) -> Fut,
    Fut: Future<Output = Option<T>>,
{
    // `section_list.inc.php:7-13`, verbatim. Both sides of the join must be enabled, and the
    // order is the class's `sort_order` first: the menu the generators emit follows it.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT SID, section_config.CID, section_config.sname, section_config.title AS s_title,
                section_config.comment, section_class.cname, section_class.title AS c_title,
                section_config.recommend, section_config.read_user_level, section_config.write_user_level
                FROM section_config INNER JOIN section_class ON section_config.CID = section_class.CID
                WHERE section_class.enable AND section_config.enable
                ORDER BY section_class.sort_order, section_config.sort_order",
    )
    .fetch_all(db.pool())
    .await?;

    let mut groups: Vec<Group<T>> = Vec::new();
    let mut last_cid: i16 = -1;
    let mut last_name = String::new();
    let mut last_title = String::new();
    let mut sections: Vec<(Section, T)> = Vec::new();

    for row in rows {
        // `:31-45`: the class closes when the id changes, and a class whose sections were all
        // filtered out is never opened — the guard is on the accumulated list, not on the id.
        if row.cid != last_cid {
            if !sections.is_empty() {
                groups.push(Group {
                    cid: last_cid,
                    name: std::mem::take(&mut last_name),
                    title: std::mem::take(&mut last_title),
                    sections: std::mem::take(&mut sections),
                });
                sections = Vec::new();
            }

            last_cid = row.cid;
            last_name = text(&row.cname);
            last_title = text(&row.c_title);
        }

        // `:47-58`: filter first, then the per-section values; a filtered row costs no query.
        if let Some(value) = inspect(row.clone()).await {
            sections.push((Section::from(&row), value));
        }
    }

    // `:60-68`: the class still open when the result set ended.
    if !sections.is_empty() {
        groups.push(Group {
            cid: last_cid,
            name: last_name,
            title: last_title,
            sections,
        });
    }

    Ok(groups)
}

/// PHP's coercion of a `null` column to `""` in the strings this hierarchy prints.
fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}
