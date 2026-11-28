/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The 我收藏的主题 page: the threads this reader starred, one row each.
//!
//! Port of `src/article_favor_display.c` — the screen `article_favor_draw_screen` (`:37-57`),
//! the rows `article_favor_draw_items` (`:59-142`), the key loop `article_favor_select`
//! (`:144-294`) and the driver `article_favor_display` (`:296-401`) — reached from the top
//! menu's `FAVOR_TOPIC` item (`menu_proc.c:372-380`), which is the name the C table gives it
//! while the file it lives in says "favor articles".
//!
//! The star table is [`crate::favorlog`]; this module only reads it, plus the one write the
//! `-` key makes. The page query is `query_favor_articles` (`article_favor.c:450-518`) with its
//! flush-and-merge still at the front, which is what makes a removal permanent before the list
//! is drawn again.
//!
//! # Deviations from C
//!
//! * `p_articles[]` holds pointers into shared memory, so a row's fields are the live pool's and
//!   `article_favor_draw_items` can print a title the loader changed mid-screen. The page is a
//!   clone here, like [`crate::roster`]'s rows: a change lands at the next refresh.
//! * `static int display_sname` is [`Session::display_sname`]. C's static was per-fork, so it
//!   already lasted across openings of the screen and across the boards it opened from; only its
//!   lifetime is the port's, not its scope.
//! * The `NULL` argument guards of `query_favor_articles` (`:456-461`) and the `-4` of a failed
//!   `get_section_info` (`:509-512`, which is a shared-memory read lock) have no counterpart: the
//!   page is a `Vec` and the board name is a field. Every *lookup* failure is reproduced — the
//!   `-3` of an aid no longer in the pool, and the `-1` of a page id the list no longer has.
//! * C abandons the screen on either of those (`article_favor_display` `:317-322`, `:363-368`),
//!   and so does the port, including when the abandoned page is the one the removal just made
//!   unreachable: unstar the last row of the last page and the `page_id` the key loop kept is out
//!   of range for the shorter list, so the list exits instead of clamping to the new tail.
//! * After `LOCATE_ARTICLE` C redraws only rows 4-23 and the bottom row — no
//!   `article_favor_draw_screen()`, unlike the help and `n` arms — so the board the reader came
//!   back from leaves its own header and key line on the screen. Kept.
//! * The doubled `\x1b[0;37m` after the `-` of the key line (`:43`) is C's, kept.
//! * `user_online_update(NULL)` on each accepted key is [`Session::record_activity`], as
//!   elsewhere in the port.

use crate::board::Article;
use crate::browse;
use crate::favorlog::Marked;
use crate::paths::{
    ARTICLE_LIMIT_PER_PAGE, DATA_READ_HELP, MAX_USER_IDLE_TIME, SECTION_NAME_MAX_LEN,
    USERNAME_MAX_LEN,
};
use crate::screen;
use crate::session::{self, Session};
use crate::term::Terminal;
use crate::term::key::{
    CR, KEY_DOWN, KEY_END, KEY_ESC, KEY_HOME, KEY_LEFT, KEY_NULL, KEY_PGDN, KEY_PGUP, KEY_RIGHT,
    KEY_TIMEOUT, KEY_UP,
};
use crate::text;
use crate::viewlog::ViewLog;

/// The rows the drawer fills: `moveto(4 + i, 1)` inside `clrline(4, 23)` (`:75`, `:127`).
const FIRST_ROW: usize = 4;
const LAST_ROW: usize = 23;

/// The letters of `article_favor_select`'s switch, which C writes bare.
const KEY_LOWER_K: u32 = b'k' as u32;
const KEY_LOWER_J: u32 = b'j' as u32;
const KEY_UPPER_P: u32 = b'P' as u32;
const KEY_UPPER_N: u32 = b'N' as u32;
const KEY_LOWER_N: u32 = b'n' as u32;
const KEY_DOLLAR: u32 = b'$' as u32;
const KEY_MINUS: u32 = b'-' as u32;
const KEY_LOWER_H: u32 = b'h' as u32;

/// The action `user_online_update` is called with at the top of each round (`:345`).
const ACTION_FAVOR: &str = "ARTICLE_FAVOR";

/// Row 2, the key line, whose `%s` names the column `n` will switch *to* (`:42-45`).
const KEY_LINE: &str = "返回[\x1b[1;32m←\x1b[0;37m,\x1b[1;32mESC\x1b[0;37m] 选择[\x1b[1;32m↑\x1b[0;37m,\x1b[1;32m↓\x1b[0;37m] \
	定位[\x1b[1;32m→\x1b[0;37m,\x1b[1;32mENTER\x1b[0;37m] 移除[\x1b[1;32m-\x1b[0;37m\x1b[0;37m] \
	{column}[\x1b[1;32mn\x1b[0;37m] 帮助[\x1b[1;32mh\x1b[0;37m]\x1b[m";

/// Row 3, the two column titles (`:49` and `:53`), both padded to the same width.
const SNAME_TITLE: &str = "\x1b[44;37m  \x1b[1;37m 编  号   版块名称             日  期  文章标题                               \x1b[m";
const USERNAME_TITLE: &str = "\x1b[44;37m  \x1b[1;37m 编  号   发 布 者     日  期  文章标题                                       \x1b[m";

/// `select_cmd_t` (`:27-35`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FavorCmd {
    ExitList,
    LocateArticle,
    ChangePage,
    ShowHelp,
    SwitchName,
    UnsetFavor,
}

/// One row: `p_articles[i]` with the board name `p_snames[i]` copied beside it.
struct Row {
    article: Article,
    sname: String,
}

/// The page in hand, which is `article_favor_display`'s locals that outlive one round.
struct View {
    rows: Vec<Row>,
    /// `page_count`.
    total_page: i32,
    /// `page_id`.
    page_id: i32,
    /// `selected_index`.
    selected: i32,
}

impl View {
    fn new() -> View {
        View {
            rows: Vec::new(),
            total_page: 0,
            page_id: 0,
            selected: 0,
        }
    }

    /// `article_count`.
    fn count(&self) -> i32 {
        self.rows.len() as i32
    }

    /// `p_articles[selected_index]->aid` and the board name that goes with it, the two the
    /// `-` and `Enter` arms read.
    fn selected_row(&self) -> Option<(&str, i32)> {
        let index = usize::try_from(self.selected).ok()?;
        self.rows
            .get(index)
            .map(|row| (row.sname.as_str(), row.article.aid))
    }

    /// `query_favor_articles` (`article_favor.c:450`): flush what the `-` key queued, fold it
    /// into the saved set, and read the page out of the pool. `false` is any of C's negative
    /// returns, each of which ends the screen.
    async fn query(&mut self, session: &mut Session) -> bool {
        if let Err(e) = session.favor.save_inc(&session.state.db).await {
            tracing::error!("article_favor_save_inc() error: {e}");
            return false;
        }
        session.favor.merge_inc();
        let aids = session.favor.favor_aids();

        self.total_page = (aids.len() as i32 + ARTICLE_LIMIT_PER_PAGE - 1) / ARTICLE_LIMIT_PER_PAGE;
        self.rows.clear();

        if aids.is_empty() {
            // C returns the empty list here, with `*p_page_count` left at 0.
            return true;
        }
        if self.page_id < 0 || self.page_id >= self.total_page {
            tracing::error!(
                "Invalid page_id = {}, not in range [0, {})",
                self.page_id,
                self.total_page
            );
            return false;
        }

        let board = session.state.board();
        for i in 0..ARTICLE_LIMIT_PER_PAGE {
            let at = self.page_id * ARTICLE_LIMIT_PER_PAGE + i;
            if at as usize >= aids.len() {
                break;
            }
            let aid = aids[at as usize];
            let Some(article) = board.article_by_aid(aid) else {
                tracing::error!(
                    "article_block_find_by_aid(aid={aid}) error: page_id={}, i={i}",
                    self.page_id
                );
                return false;
            };
            let Some(section) = board.section(article.sid) else {
                tracing::error!("section_list_find_by_sid({}) error", article.sid);
                return false;
            };
            self.rows.push(Row {
                article: article.clone(),
                sname: section.sname.clone(),
            });
        }

        // `article_favor_display:370-377`: an empty page has nothing to select, and a removal
        // can leave the cursor past the end of the page it was on.
        if self.count() == 0 {
            self.selected = 0;
        } else if self.selected >= self.count() {
            self.selected = self.count() - 1;
        }

        true
    }

    /// `article_favor_draw_items` (`:59`), which starts by wiping the rows it is about to write.
    fn draw_items(&self, term: &mut Terminal, session: &Session) {
        screen::clrline(term, FIRST_ROW, LAST_ROW);

        let fixed = term.fixed_width();
        let now = session::now();
        let display_sname = session.display_sname;

        for (i, row) in self.rows.iter().enumerate() {
            term.moveto(FIRST_ROW + i, 1);
            term.print(&row_line(
                row,
                display_sname,
                &session.view_log,
                session.privs.uid,
                fixed,
                now,
            ));
        }
    }

    /// `show_bottom` of the page counter (`:338-342`), with an empty list as page 1/1.
    fn show_page_info(&self, term: &mut Terminal, session: &Session) {
        let info = format!(
            "\x1b[33m[第\x1b[36m{}\x1b[33m/\x1b[36m{}\x1b[33m页]",
            self.page_id + 1,
            self.total_page.max(1)
        );
        screen::show_bottom(term, &info, &session.username, session.login_tm);
    }

    /// `article_favor_select` (`:144`). The three fallthroughs of C's switch are the
    /// `matches!` tests below them: `Home` sets the page *and* the index before moving up, and
    /// `$`/`End` do the same on the way down.
    async fn select(&mut self, term: &mut Terminal, session: &mut Session) -> FavorCmd {
        let item_count = self.count();
        let total_page = self.total_page;
        let mut old_page_id = self.page_id;
        let mut old_selected = self.selected;

        if item_count > 0 && self.selected >= 0 {
            term.moveto(FIRST_ROW + self.selected as usize, 1);
            term.print(">");
            let _ = term.flush().await;
        }

        while !session.state.is_exiting() {
            let ch = term.read_key(browse::SELECT_KEY_TIMEOUT).await;

            if ch != KEY_NULL && ch != KEY_TIMEOUT {
                session.record_activity().await;
            }

            match ch {
                // C logs `KEY_NULL` and falls through to the same return as `Esc`.
                KEY_NULL => {
                    tracing::debug!("KEY_NULL");
                    return FavorCmd::ExitList;
                }
                KEY_ESC | KEY_LEFT => return FavorCmd::ExitList,
                KEY_TIMEOUT => {
                    if session::now() - session.last_access_tm >= MAX_USER_IDLE_TIME as i64 {
                        tracing::debug!("User input timeout");
                        return FavorCmd::ExitList;
                    }
                    continue;
                }
                KEY_LOWER_N => return FavorCmd::SwitchName,
                KEY_MINUS => {
                    if item_count > 0 {
                        return FavorCmd::UnsetFavor;
                    }
                }
                CR | KEY_RIGHT => {
                    if item_count > 0 {
                        return FavorCmd::LocateArticle;
                    }
                }
                KEY_HOME | KEY_UPPER_P | KEY_PGUP | KEY_LOWER_K | KEY_UP => {
                    if ch == KEY_HOME {
                        self.page_id = 0;
                    }
                    if matches!(ch, KEY_HOME | KEY_UPPER_P | KEY_PGUP) {
                        self.selected = 0;
                    }
                    if self.selected <= 0 {
                        if self.page_id > 0 {
                            self.page_id -= 1;
                            self.selected = ARTICLE_LIMIT_PER_PAGE - 1;
                        } else if matches!(ch, KEY_UP | KEY_LOWER_K) {
                            // Rotate to the tail of the list.
                            if total_page > 0 {
                                self.page_id = total_page - 1;
                            }
                            if item_count > 0 {
                                self.selected = item_count - 1;
                            }
                        }
                    } else {
                        self.selected -= 1;
                    }
                }
                KEY_DOLLAR | KEY_END | KEY_UPPER_N | KEY_PGDN | KEY_LOWER_J | KEY_DOWN => {
                    if matches!(ch, KEY_DOLLAR | KEY_END) && total_page > 0 {
                        self.page_id = total_page - 1;
                    }
                    if matches!(ch, KEY_DOLLAR | KEY_END | KEY_UPPER_N | KEY_PGDN) && item_count > 0
                    {
                        self.selected = item_count - 1;
                    }
                    if self.selected + 1 >= item_count {
                        if self.page_id + 1 < total_page {
                            self.page_id += 1;
                            self.selected = 0;
                        } else if matches!(ch, KEY_DOWN | KEY_LOWER_J) {
                            // Rotate to the head of the list.
                            self.page_id = 0;
                            self.selected = 0;
                        }
                    } else {
                        self.selected += 1;
                    }
                }
                KEY_LOWER_H => return FavorCmd::ShowHelp,
                _ => {}
            }

            if old_page_id != self.page_id {
                return FavorCmd::ChangePage;
            }

            if item_count > 0 && old_selected != self.selected {
                if old_selected >= 0 {
                    term.moveto(FIRST_ROW + old_selected as usize, 1);
                    term.print(" ");
                }
                if self.selected >= 0 {
                    term.moveto(FIRST_ROW + self.selected as usize, 1);
                    term.print(">");
                }
                let _ = term.flush().await;

                old_selected = self.selected;
                old_page_id = self.page_id;
            }
        }

        FavorCmd::ExitList
    }
}

/// `article_favor_draw_screen` (`:37`): clear, title, key line, column titles. The row the
/// cursor sits on belongs to `article_favor_select`, which is why a redraw after the help page
/// leaves the cursor to be placed again by the next key round.
fn draw_screen(term: &mut Terminal, session: &Session) {
    screen::clearscr(term);
    screen::show_top(term, "[主题收藏]", &session.config.bbs_name, "");
    term.moveto(2, 0);
    term.print(&KEY_LINE.replace(
        "{column}",
        // The key line names the *other* column, which is what `n` is for.
        if session.display_sname {
            "用户名"
        } else {
            "版块名称"
        },
    ));
    term.moveto(3, 0);
    term.print(if session.display_sname {
        SNAME_TITLE
    } else {
        USERNAME_TITLE
    });
}

/// The title with its topic and transship markers (`:116-119`). Unlike the board list this one
/// keeps the `Re: ` prefixes of a reply's title as they are.
fn row_title(article: &Article) -> String {
    let mut out = String::new();
    if article.is_topic_head() {
        out.push_str("● ");
    }
    if article.transship {
        out.push_str("[转载]");
    }
    out.push_str(&article.title);
    out
}

/// `  %7d %c %s%*s %s %s` of `article_favor_draw_items:129-138`: the article number, the read
/// flag, the name column the `n` key chooses, the date and the title cut to what is left of the
/// 59 cells.
fn row_line(
    row: &Row,
    display_sname: bool,
    view_log: &ViewLog,
    uid: i32,
    fixed: bool,
    now: i64,
) -> String {
    let article = &row.article;
    let field = if display_sname {
        SECTION_NAME_MAX_LEN as i64
    } else {
        USERNAME_MAX_LEN as i64
    };
    let name = if display_sname {
        row.sname.as_str()
    } else {
        article.username.as_str()
    };
    let pad = browse::padding(field - text::display_width(name.as_bytes(), true, fixed) as i64);

    let title = row_title(article);
    let cells = (59 - field).max(0) as usize;
    let split = text::split_line(title.as_bytes(), cells, true, fixed);
    let cut = split.bytes.min(title.len());
    let title = String::from_utf8_lossy(&title.as_bytes()[..cut]).into_owned();

    let flag = browse::article_flag(article, uid, view_log);
    let time = browse::row_time(article.sub_dt, now);

    format!(
        "  {:>7} {flag} {name}{} {time} {title}",
        article.aid,
        " ".repeat(pad)
    )
}

/// `article_favor_display` (`:296`), reached as `favor_topic` (`menu_proc.c:372-380`), which
/// drops the return code into a `log_error` and asks for a redraw either way. The `param` the
/// menu hands the item is discarded by C and so by the port.
pub async fn article_favor_display(term: &mut Terminal, session: &mut Session) {
    let mut view = View::new();

    draw_screen(term, session);

    if !view.query(session).await {
        tracing::error!("query_favor_articles(page_id=0) error");
        return;
    }

    while !session.state.is_exiting() {
        view.draw_items(term, session);
        view.show_page_info(term, session);
        let _ = term.flush().await;

        if let Err(e) = session.online_update(Some(ACTION_FAVOR)).await {
            tracing::error!("user_online_update(ARTICLE_FAVOR) error: {e}");
        }

        match view.select(term, session).await {
            FavorCmd::ExitList => return,
            FavorCmd::UnsetFavor => {
                // C indexes `p_articles[selected_index]` blindly; the key loop only returns this
                // command with a row on the page, so the guard is for a page that changed under it.
                let Some((_, aid)) = view.selected_row() else {
                    continue;
                };
                match session.favor.mark(&session.state.db, aid, false).await {
                    // `article_favor_set` returning 1 is the only answer that falls through to
                    // the refresh, which is where the removal reaches the database.
                    Ok(Marked::Pending) => {}
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::error!("article_favor_set(aid={aid}, 0) error: {e}");
                        continue;
                    }
                }
                if !view.query(session).await {
                    tracing::error!("query_favor_articles(page_id={}) error", view.page_id);
                    return;
                }
            }
            FavorCmd::ChangePage => {
                if !view.query(session).await {
                    tracing::error!("query_favor_articles(page_id={}) error", view.page_id);
                    return;
                }
            }
            FavorCmd::LocateArticle => {
                let Some((sname, aid)) = view.selected_row() else {
                    continue;
                };
                // C ignores the board's return code and redraws nothing but the rows, so the
                // header the board used stays on the screen until the reader leaves.
                if let Err(e) = browse::Listing::run(term, session, sname, aid).await {
                    tracing::error!("section_list_display(sname={sname}, aid={aid}) error: {e:?}");
                }
            }
            FavorCmd::SwitchName => {
                session.display_sname = !session.display_sname;
                draw_screen(term, session);
            }
            FavorCmd::ShowHelp => {
                session.display_file(term, DATA_READ_HELP, 1).await;
                draw_screen(term, session);
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/test_favorlist.rs"]
mod tests;
