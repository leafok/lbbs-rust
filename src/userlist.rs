/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The two roster pages: who is on now, and who ever registered.
//!
//! Port of `src/user_list_display.c`, except for `user_list_search` (`:476-600`), which lives
//! in [`crate::userinfo`]: the screen `user_list_draw_screen` (`:38-58`), the two row drawers
//! `user_list_draw_items` (`:60-116`) and `user_online_list_draw_items` (`:118-199`), the key
//! loop `user_list_select` (`:201-347`) and the driver `user_list_display` (`:349-474`). The
//! same function serves both menu items, `LIST_ONLINE_USER` and `LIST_USER`
//! (`menu_proc.c:382-400`).
//!
//! The queries are already ported: [`crate::roster::Roster::page`] and
//! [`crate::roster::OnlineList::page`] are `query_user_list` and `query_user_online_list`, and
//! their `page_count` accessors are `*p_page_count`.
//!
//! # Deviations from C
//!
//! * The `-2` `user_list_display` returns when a page query fails is `user_list.c`'s
//!   `user_list_rd_lock()` (`query_user_list` `:1034`, `query_user_online_list` `:1090`), and
//!   its `-3` is a row drawer failing, which neither is: both `return 0` unconditionally.
//!   [`crate::state::AppState::roster`] and [`crate::state::AppState::online`] are `Arc`s a
//!   reader cannot fail to take and their rows come back owned, so the port has no failure of
//!   either kind to report — and `menu_proc.c:382-400` drops the code into a `log_error` this
//!   module can never reach.
//! * A `page_id` outside `[0, page_count)` is C's `-3` there, which abandons the screen in the
//!   middle of its loop. The port reads it as no rows on the same page count
//!   ([`crate::roster::Roster::page`] answering `None`), because the key loop keeps `page_id`
//!   inside that range: the only values it assigns are `0`, `page_id - 1`, `page_id + 1` under
//!   `page_id + 1 < total_page`, and `total_page - 1` guarded by `total_page > 0`.
//! * C copies the page into two stack arrays of `BBS_user_limit_per_page` `USER_INFO` /
//!   `USER_ONLINE_INFO` (`:352-353`) and rewrites them in place on every page change. The port
//!   clones the same rows into a [`Vec`] and replaces the whole `Vec`, which is the same
//!   snapshot: a user who leaves while the page is shown keeps their row until the next
//!   refresh.
//! * `gmtime` returning `NULL` for a duration it cannot represent — the `log_error("Invalid
//!   time duration")` and the empty field — is chrono's `None` for a timestamp outside its
//!   range, and the same empty field. The `tm_year > 70` test is "the duration reaches 1971",
//!   which is why it prints `year - 1970`.
//! * `user_online_update(NULL)` on each accepted key is [`Session::record_activity`], which is
//!   what the rest of the port uses for the pair of `BBS_last_access_tm = time(NULL)` and the
//!   refresh.
//! * `REFRESH_LIST` and `CHANGE_PAGE` are one `case` body in C (`:426-454`); the port keeps
//!   both names because the key that answers them is worth reading, and matches them together.
//! * `%*s` of a negative width still pads, by the absolute value: an over-long nickname pushes
//!   the columns right rather than pulling them left ([`crate::browse::padding`]).

use chrono::{Datelike, Timelike};

use crate::browse::{self, padding};
use crate::paths::{
    DATA_READ_HELP, MAX_USER_IDLE_TIME, NICKNAME_MAX_LEN, USER_LIMIT_PER_PAGE, USERNAME_MAX_LEN,
};
use crate::roster::{OnlineUser, UserInfo};
use crate::screen;
use crate::session::{self, Session};
use crate::state::AppState;
use crate::term::Terminal;
use crate::term::key::{
    CR, KEY_DOWN, KEY_END, KEY_ESC, KEY_F5, KEY_HOME, KEY_LEFT, KEY_NULL, KEY_PGDN, KEY_PGUP,
    KEY_RIGHT, KEY_TIMEOUT, KEY_UP,
};

/// The rows the two drawers fill: `moveto(4 + i, 1)` for at most
/// `BBS_user_limit_per_page` of them, inside `clrline(4, 23)` (`:68`, `:102`, `:128`, `:179`).
const FIRST_ROW: usize = 4;
const LAST_ROW: usize = 23;

/// The letters of `user_list_select`'s switch, which C writes bare.
const KEY_LOWER_K: u32 = b'k' as u32;
const KEY_LOWER_J: u32 = b'j' as u32;
const KEY_UPPER_P: u32 = b'P' as u32;
const KEY_UPPER_N: u32 = b'N' as u32;
const KEY_DOLLAR: u32 = b'$' as u32;
const KEY_LOWER_S: u32 = b's' as u32;
const KEY_LOWER_H: u32 = b'h' as u32;

/// The keys `user_online_update` is called with at the top of each round (`:414-418`).
const ACTION_ONLINE_USER: &str = "USER_ONLINE";
const ACTION_USER_LIST: &str = "USER_LIST";

/// Row 2, the three C pieces joined (`:43-45`).
const KEY_HELP_LINE: &str = "返回[\x1b[1;32m←\x1b[0;37m,\x1b[1;32mESC\x1b[0;37m] 选择[\x1b[1;32m↑\x1b[0;37m,\x1b[1;32m↓\x1b[0;37m] \
查看[\x1b[1;32m→\x1b[0;37m,\x1b[1;32mENTER\x1b[0;37m] 查找[\x1b[1;32ms\x1b[0;37m] \
帮助[\x1b[1;32mh\x1b[0;37m]\x1b[m";

/// Row 3, the column titles (`:50` and `:54`). The two differ only in their last field, and
/// both are padded to the same width.
const ONLINE_TITLE: &str = "\x1b[44;37m \x1b[1;37m 编  号  用户名       昵称                 在线时长 空闲   最后活动            \x1b[m";
const REGISTERED_TITLE: &str = "\x1b[44;37m \x1b[1;37m 编  号  用户名       昵称                 上次登陆距今                        \x1b[m";

/// `select_cmd_t` (`:28-36`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListCmd {
    Exit,
    ViewUser,
    ChangePage,
    RefreshList,
    ShowHelp,
    SearchUser,
}

/// The page in hand, which is `user_list_display`'s four frame locals that outlive one round.
struct View {
    rows: Rows,
    /// `page_count`.
    total_page: i32,
    /// `page_id`.
    page_id: i32,
    /// `selected_index`.
    selected: i32,
}

/// `USER_INFO users[]` and `USER_ONLINE_INFO online_users[]` (`:352-353`): a page of rows, of
/// which the drawers know two shapes.
enum Rows {
    Registered(Vec<UserInfo>),
    Online(Vec<OnlineUser>),
}

/// `query_user_online_list` and `query_user_list` (`:362-379`), which read the published
/// tables rather than a per-child copy of shared memory.
fn query(state: &AppState, online_user: bool, page_id: i32) -> (Rows, i32) {
    if online_user {
        let list = state.online();
        (
            Rows::Online(list.page(page_id).unwrap_or_default().to_vec()),
            list.page_count(),
        )
    } else {
        let roster = state.roster();
        (
            Rows::Registered(roster.page(page_id).unwrap_or_default().to_vec()),
            roster.page_count(),
        )
    }
}

/// `user_list_draw_screen` (`:38`): clear, title, key line, column titles. The row the cursor
/// sits on belongs to `user_list_select`, not here, which is why a redraw after a profile
/// leaves the cursor to be placed again by the next key round.
fn draw_screen(term: &mut Terminal, session: &Session, online_user: bool) {
    screen::clearscr(term);
    let title = if online_user {
        "[线上使用者]"
    } else {
        "[已注册用户]"
    };
    // `show_top((online_user ? "[线上使用者]" : "[已注册用户]"), BBS_name, "")` (`:41`).
    screen::show_top(term, title, &session.config.bbs_name, "");
    term.moveto(2, 0);
    term.print(KEY_HELP_LINE);
    term.moveto(3, 0);
    term.print(if online_user {
        ONLINE_TITLE
    } else {
        REGISTERED_TITLE
    });
}

impl Rows {
    /// The page before the query fills it, which is what C's two stack arrays hold on the way
    /// in (`:352-353`).
    fn empty(online_user: bool) -> Rows {
        if online_user {
            Rows::Online(Vec::new())
        } else {
            Rows::Registered(Vec::new())
        }
    }

    /// `user_count`.
    fn count(&self) -> i32 {
        match self {
            Rows::Registered(users) => users.len() as i32,
            Rows::Online(users) => users.len() as i32,
        }
    }

    /// `&(online_users[selected_index].user_info)` / `&users[selected_index]` (`:456`), the
    /// row the profile screen is called with.
    fn user(&self, index: i32) -> Option<UserInfo> {
        let index = usize::try_from(index).ok()?;
        match self {
            Rows::Registered(users) => users.get(index).cloned(),
            Rows::Online(users) => users.get(index).map(|user| user.user.clone()),
        }
    }
}

impl View {
    /// `query_*` plus the two adjustments of `user_list_display` (`:381-384`, `:446-453`).
    fn query(&mut self, state: &AppState, online_user: bool) {
        let (rows, total_page) = query(state, online_user, self.page_id);
        self.rows = rows;
        self.total_page = total_page;
        if self.rows.count() == 0 {
            self.selected = 0;
        } else if self.selected >= self.rows.count() {
            self.selected = self.rows.count() - 1;
        }
    }

    /// `user_list_draw_items` and `user_online_list_draw_items` (`:60`, `:118`), each of which
    /// starts by wiping the rows it is about to write.
    fn draw_items(&self, term: &mut Terminal) {
        screen::clrline(term, FIRST_ROW, LAST_ROW);
        let fixed = term.fixed_width();
        let now = session::now();
        match &self.rows {
            Rows::Registered(users) => {
                for (i, user) in users.iter().enumerate() {
                    term.moveto(FIRST_ROW + i, 1);
                    term.print(&registered_line(user, fixed, now));
                }
            }
            Rows::Online(users) => {
                for (i, user) in users.iter().enumerate() {
                    term.moveto(FIRST_ROW + i, 1);
                    term.print(&online_line(user, fixed, now));
                }
            }
        }
    }

    /// `show_bottom` of the page counter (`:407-412`), with an empty list as page 1/1.
    fn show_page_info(&self, term: &mut Terminal, session: &Session) {
        let info = format!(
            "\x1b[33m[第\x1b[36m{}\x1b[33m/\x1b[36m{}\x1b[33m页]",
            self.page_id + 1,
            self.total_page.max(1)
        );
        screen::show_bottom(term, &info, &session.username, session.login_tm);
    }

    /// `user_list_select` (`:201`).
    async fn select(&mut self, term: &mut Terminal, session: &mut Session) -> ListCmd {
        let old_page_id = self.page_id;
        let mut old_selected = self.selected;
        let item_count = self.rows.count();
        let total_page = self.total_page;

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
                KEY_NULL => {
                    tracing::debug!("KEY_NULL");
                    return ListCmd::Exit;
                }
                KEY_ESC | KEY_LEFT => return ListCmd::Exit,
                KEY_TIMEOUT => {
                    if session::now() - session.last_access_tm >= MAX_USER_IDLE_TIME as i64 {
                        tracing::debug!("User input timeout");
                        return ListCmd::Exit;
                    }
                    continue;
                }
                CR | KEY_RIGHT => {
                    if item_count > 0 {
                        return ListCmd::ViewUser;
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
                            self.selected = USER_LIMIT_PER_PAGE - 1;
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
                KEY_LOWER_S => return ListCmd::SearchUser,
                KEY_F5 => return ListCmd::RefreshList,
                KEY_LOWER_H => return ListCmd::ShowHelp,
                _ => {}
            }

            if old_page_id != self.page_id {
                return ListCmd::ChangePage;
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
            }
        }

        ListCmd::Exit
    }
}

/// `user_list_display` (`:349`), reached as `list_online_user` and `list_user`
/// (`menu_proc.c:382-400`), which is the same call with the other flag. The `param` the menu
/// hands both items is discarded by C and so by the port.
pub async fn user_list_display(term: &mut Terminal, session: &mut Session, online_user: bool) {
    let action = if online_user {
        ACTION_ONLINE_USER
    } else {
        ACTION_USER_LIST
    };

    let mut view = View {
        rows: Rows::empty(online_user),
        total_page: 0,
        page_id: 0,
        selected: 0,
    };

    draw_screen(term, session, online_user);
    view.query(&session.state, online_user);

    while !session.state.is_exiting() {
        view.draw_items(term);
        view.show_page_info(term, session);
        let _ = term.flush().await;

        if let Err(e) = session.online_update(Some(action)).await {
            tracing::error!("user_online_update({action}) error: {e}");
        }

        match view.select(term, session).await {
            ListCmd::Exit => return,
            ListCmd::ChangePage | ListCmd::RefreshList => view.query(&session.state, online_user),
            ListCmd::ViewUser => {
                // C ignores the profile screen's return code (`:456`), and the rows it shows
                // are the snapshot the page query took.
                if let Some(user) = view.rows.user(view.selected) {
                    crate::userinfo::user_info_display(term, session, &user).await;
                }
                draw_screen(term, session, online_user);
            }
            ListCmd::SearchUser => {
                crate::userinfo::user_list_search(term, session).await;
                draw_screen(term, session, online_user);
            }
            ListCmd::ShowHelp => {
                session.display_file(term, DATA_READ_HELP, 1).await;
                draw_screen(term, session, online_user);
            }
        }
    }
}

/// `%s%*s` of `str_length(s, 1)`: the text, then the room the column has left.
fn field(text: &str, cells: usize, fixed: bool) -> String {
    let used = crate::text::display_width(text.as_bytes(), true, fixed) as i64;
    format!("{text}{}", " ".repeat(padding(cells as i64 - used)))
}

/// `gmtime(&duration)`: a duration read as seconds since the epoch, in UTC. `None` is the
/// `NULL` C's `gmtime` answers with.
fn broken_down(duration: i64) -> Option<chrono::NaiveDateTime> {
    chrono::DateTime::from_timestamp(duration, 0).map(|moment| moment.naive_utc())
}

/// `user_list_draw_items:73-100`, the field headed 上次登陆距今.
fn since_visit(duration: i64) -> String {
    let Some(moment) = broken_down(duration) else {
        tracing::error!("Invalid time duration");
        return String::new();
    };
    let years = moment.year() - 1970;
    if years > 0 {
        format!("{years}年{}天", moment.ordinal0())
    } else if moment.ordinal0() > 0 {
        format!("{}天", moment.ordinal0())
    } else if moment.hour() > 0 {
        format!("{}:{:02}", moment.hour(), moment.minute())
    } else {
        format!("{}'{:02}\"", moment.minute(), moment.second())
    }
}

/// `user_online_list_draw_items:134-155`, the field headed 在线时长. The years are not split
/// out here: a session of a week is `6d0h`.
fn since_login(duration: i64) -> String {
    let Some(moment) = broken_down(duration) else {
        tracing::error!("Invalid time duration");
        return String::new();
    };
    if moment.ordinal0() > 0 {
        format!("{}d{}h", moment.ordinal0(), moment.hour())
    } else if moment.hour() > 0 {
        format!("{}:{:02}", moment.hour(), moment.minute())
    } else {
        format!("{}'{:02}\"", moment.minute(), moment.second())
    }
}

/// `user_online_list_draw_items:157-173`, the field headed 空闲. Neither form pads the seconds.
fn since_activity(duration: i64) -> String {
    let Some(moment) = broken_down(duration) else {
        tracing::error!("Invalid time duration");
        return String::new();
    };
    if moment.minute() > 0 {
        format!("{}'{}\"", moment.minute(), moment.second())
    } else {
        format!("{}\"", moment.second())
    }
}

/// One registered user: the pool number, the two names in their columns and the age of the
/// last visit (`:104-112`).
fn registered_line(user: &UserInfo, fixed: bool, now: i64) -> String {
    format!(
        "  {:>6}  {} {} {}",
        user.id + 1,
        field(&user.username, USERNAME_MAX_LEN, fixed),
        field(&user.nickname, NICKNAME_MAX_LEN / 2, fixed),
        since_visit(now - user.last_login_dt),
    )
}

/// One session: the same three fields, then the two durations and what the session says it is
/// doing (`:181-195`). The number is `user_online`'s slot, not the user's.
fn online_line(user: &OnlineUser, fixed: bool, now: i64) -> String {
    // `p_action_title` (`:175-177`): the label of a known action, the raw code of one the
    // dictionary does not know. [`crate::roster::OnlineList::load`] keeps no such row, so in
    // the port the second arm is only the empty action, whose title `load` stores as `""`.
    let title = match user.current_action_title {
        Some(title) => title,
        None => user.current_action.as_str(),
    };
    let login = since_login(now - user.login_tm);
    let idle = since_activity(now - user.last_tm);
    format!(
        "  {:>6}  {} {} {} {} {title}",
        user.id + 1,
        field(&user.user.username, USERNAME_MAX_LEN, fixed),
        field(&user.user.nickname, NICKNAME_MAX_LEN / 2, fixed),
        field(&login, 8, fixed),
        field(&idle, 6, fixed),
    )
}

#[cfg(test)]
#[path = "../tests/test_userlist.rs"]
mod tests;
