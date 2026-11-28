/* SPDX-License-Identifier: GPL-3.0-or-later */
//! One user's profile screen, and the search that reaches it.
//!
//! Port of `src/user_info_display.c` (189 lines) and of `user_list_search`
//! (`src/user_list_display.c:476-600`), plus the attribute tables of `src/bbs.c` that the
//! profile is the only reader of: `astro_dates` `:61-63` and `astro_names` `:64-66`,
//! `BBS_user_level_points` `:67-84` and `BBS_user_level_names` `:86-103`, with the lookups
//! `get_astro_name` `:107-119`, `get_user_level` `:121-154` and `get_user_level_name`
//! `:156-159`.
//!
//! Both screens are reached from the board list: `Ctrl-A` shows the author of the row under
//! the cursor (`section_list_display.c:1058-1080`) and `u` opens the search
//! (`section_list_display.c:1336-1343`), which ends in the same profile screen. Both are
//! reached from the two roster pages too, where the keys are `Enter` and `s`
//! ([`crate::userlist`]).
//!
//! The rest of `user_list_display.c` — 环顾四周 and the roster pages — is in
//! [`crate::userlist`]; their queries are in [`crate::roster`].
//!
//! # Deviations from C
//!
//! * C hands its callers a return code per failure (`-1` for a `NULL` row, `-2` for a lost
//!   user, `-3` for a failed profile screen) and both callers drop it: `user_list_search`'s
//!   value is discarded at `section_list_display.c:1337` and `user_info_display`'s at
//!   `:1070`, where only the `log_error` survives. The port returns nothing and logs in
//!   place. The `NULL` row is a `&UserInfo` here, so that branch has no input.
//! * `query_user_info_by_uid` copies the introduction into the caller's
//!   `char user_intro[BBS_user_intro_max_len + 1]` and points `USER_INFO::intro` at it, so
//!   `user_list_search` runs a second query after the prefix search to get one
//!   (`user_list_display.c:585`). [`crate::roster::UserInfo`] carries `intro` with the row,
//!   so the second query — and the `-2` for a user who vanished between the two — has no
//!   counterpart. Likewise `query_user_online_info_by_uid`'s `-2`/`-1`, which are the shared
//!   memory lock: [`crate::state::AppState::online`] is an `Arc` a reader cannot fail to take.
//! * `user_list_search` ignores `get_data`'s `-1`, which is what a 60-second pause and a lost
//!   client both return with the prefix left in the buffer
//!   (`user_list_display.c:499-504`, `screen.c:319-345`), so C searches the stale prefix once
//!   more before the next read reports the loss again. The port leaves the screen on either,
//!   which is what an empty first buffer already does.
//! * `user_info_display.c:33` names the offset count after the array that holds it —
//!   `USER_INFO_MAX_LINE = BBS_user_intro_max_line + 9`, the nineteen slots of `:62` and the
//!   `MIN(SCREEN_ROWS - 1, USER_INFO_MAX_LINE)` of `:183` are one constant since `9e1eff6`.
//!   [`crate::text::Wrapped`] grows, so the port keeps only the eighteen display lines that
//!   count asks for.
//! * `get_user_level`'s binary search and backward scan together give the largest index
//!   whose threshold the point has reached, because `BBS_user_level_points[0]` is `INT_MIN`.
//!   That is what the port computes, and it is why `get_user_level_name`'s unchecked array
//!   index (`bbs.c:158`) is in range here without a test.
//! * `lml_render` can fail with `!lml_ready` (`lml.c:267-271`); the port's render cannot, and
//!   C's `intro_len > 0` test is `!intro.is_empty()`.
//! * Timestamps follow the rest of the port: `localtime_r` is `chrono::Local`, so the
//!   `user_info.user_tz` a user sets is recorded but not applied. A timestamp outside the
//!   zone's range is read as the epoch; C leaves the `struct tm` uninitialised.
//! * `user_info_f` is one `snprintf` into `char[USER_INFO_MAX_LEN]`, so the port builds the
//!   same bytes and cuts them at 8191 — the bound is out of reach (a 4096-byte introduction
//!   plus the header and ten action brackets does not fill it), and the buffer is a `Vec<u8>`
//!   rather than a `String` because a cut there would land inside a colour sequence or a
//!   UTF-8 character.

use chrono::{Datelike, TimeZone};

use crate::browse;
use crate::ipmask;
use crate::lml;
use crate::paths::{
    DATA_READ_HELP, LINE_BUFFER_LEN, MAX_USER_IDLE_TIME, SCREEN_COLS, SCREEN_ROWS,
    USER_INTRO_MAX_LEN, USER_INTRO_MAX_LINE, USERNAME_MAX_LEN,
};
use crate::privilege::{P_ADMIN_M, P_ADMIN_S};
use crate::roster::UserInfo;
use crate::screen::{self, DisplayCtx, PagerHooks};
use crate::session::Session;
use crate::term::Terminal;
use crate::term::key::{self, CR, KEY_ESC, KEY_NULL, KEY_TIMEOUT};

/// `BBS_max_sessions_per_user` and `USER_INFO_MAX_LEN` (`user_info_display.c:27-34`). The
/// two members that only sized a C buffer — `LAST_LOGIN_DT_MAX_LEN` and
/// `ASTRO_STR_MAX_LEN` — format into a `String` here.
const MAX_SESSIONS_PER_USER: usize = 10;
const USER_INFO_MAX_LEN: usize = 8192;

/// `USER_INFO_MAX_LINE` (`:33`): the introduction plus the nine header lines above it, which since
/// `9e1eff6` is both the size of the C offset array (`:62`) and the count it asks for (`:183`).
const USER_INFO_MAX_LINE: usize = USER_INTRO_MAX_LINE + 9;

/// `user_list_search`'s grid (`user_list_display.c:478-480`), including the one extra slot
/// that is how a search of more than a hundred matches says so.
const USERS_PER_LINE: usize = 5;
const MAX_USER_LINES: usize = 20;
const MAX_USER_CNT: usize = USERS_PER_LINE * MAX_USER_LINES + 1;

/// `astro_dates` and `astro_names` (`bbs.c:61-65`): the day the sign changes in month
/// `tm_mon`, and the twelve names with the first one repeated at the end so a December
/// birthday past its date wraps to 摩羯.
const ASTRO_DATES: [i32; 12] = [21, 20, 21, 21, 22, 22, 23, 24, 24, 24, 23, 22];
const ASTRO_NAMES: [&str; 13] = [
    "摩羯", "水瓶", "双鱼", "白羊", "金牛", "双子", "巨蟹", "狮子", "处女", "天秤", "天蝎", "射手",
    "摩羯",
];

/// `BBS_user_level_points` (`bbs.c:67-84`): the point each rank starts at.
const USER_LEVEL_POINTS: [i32; 16] = [
    i32::MIN,
    50,
    200,
    500,
    1000,
    2000,
    5000,
    10000,
    20000,
    30000,
    50000,
    60000,
    70000,
    80000,
    90000,
    100000,
];

/// `BBS_user_level_names` (`bbs.c:86-103`), one per threshold.
const USER_LEVEL_NAMES: [&str; 16] = [
    "新手上路",
    "初来乍练",
    "白手起家",
    "略懂一二",
    "小有作为",
    "对答如流",
    "精于此道",
    "博大精深",
    "登峰造极",
    "论坛砥柱",
    "☆☆☆☆☆",
    "★☆☆☆☆",
    "★★☆☆☆",
    "★★★☆☆",
    "★★★★☆",
    "★★★★★",
];

/// The immortal `life` values, which age neither (`user_info_display.c:156`).
const IMMORTAL_LIFE: [i32; 4] = [333, 365, 666, 999];

/// `get_astro_name` (`bbs.c:107-119`). A birthday of 0 is the epoch, so a user who never
/// filled the field in reads as 摩羯.
pub fn astro_name(birthday: i64) -> &'static str {
    let Some(birth) = chrono::Local.timestamp_opt(birthday, 0).single() else {
        return ASTRO_NAMES[0];
    };
    let month = birth.month0() as usize;
    if (birth.day() as i32) < ASTRO_DATES[month] {
        ASTRO_NAMES[month]
    } else {
        ASTRO_NAMES[month + 1]
    }
}

/// `get_user_level` (`bbs.c:121-154`).
pub fn user_level(point: i32) -> usize {
    USER_LEVEL_POINTS
        .iter()
        .rposition(|&start| point >= start)
        .unwrap_or(0)
}

/// `get_user_level_name` (`bbs.c:156-159`), whose C array index is unchecked.
pub fn user_level_name(level: usize) -> &'static str {
    USER_LEVEL_NAMES[level]
}

/// `strftime("%F %H:%M")` of an epoch second (`user_info_display.c:97-108`).
fn clock_minute(ts: i64) -> String {
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|moment| moment.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// `user_info_display` (`:41`): the whole profile of one roster row, ending the way C's
/// `display_data(…, 1, …)` does — with a press-any-key.
pub async fn user_info_display(term: &mut Terminal, session: &mut Session, user: &UserInfo) {
    let profile = profile_screen(term, session, user);

    // `split_data_lines(user_info_f, SCREEN_COLS + 1, …, MIN(SCREEN_ROWS - 1,
    // USER_INFO_MAX_LINE), 1, NULL)` (`:183`): the `+1` is C's wrap budget, and the
    // nineteen offset slots it asks for are eighteen display lines.
    let max_lines = (SCREEN_ROWS - 1).min(USER_INFO_MAX_LINE) - 1;
    let lines = term.wrap(&profile, SCREEN_COLS + 1, true, max_lines);

    screen::clearscr(term);
    let mut hooks = ProfileHooks { session };
    screen::display_data(
        term,
        &profile,
        &lines,
        1,
        &mut hooks,
        DATA_READ_HELP,
        MAX_USER_IDLE_TIME,
    )
    .await;
}

/// The buffer C formats into `user_info_f` (`:170-181`), assembled in the same order.
fn profile_screen(term: &Terminal, session: &Session, user: &UserInfo) -> Vec<u8> {
    // `get_user_article_cnt` (`user_list.c:1632`), whose `-1` for a uid the stat table does
    // not have is `unwrap_or(-1)` here.
    let article_cnt = session
        .state
        .board()
        .user_article_count(user.uid)
        .unwrap_or(-1);

    // `:77-95`, where the gender gate only chooses the colour.
    let gender = user.gender.to_ascii_uppercase();
    let astro = astro_name(user.birthday);
    let astro_str = if user.gender_pub && gender == 'M' {
        format!("\x1b[1;36m{astro}座\x1b[m")
    } else if user.gender_pub && gender == 'F' {
        format!("\x1b[1;35m{astro}座\x1b[m")
    } else {
        format!("\x1b[1;33m{astro}座\x1b[m")
    };

    let str_last_login = clock_minute(user.last_login_dt);
    let str_last_logout = if user.last_logout_dt <= user.last_login_dt {
        str_last_login.clone()
    } else {
        clock_minute(user.last_logout_dt)
    };

    let online = session.state.online();
    let sessions = online.sessions_of(user.uid, 0, MAX_SESSIONS_PER_USER);
    let is_online = !sessions.is_empty();

    // `action_str` (`:110-141`): `[标题] ` per session. C tests the room for the bracket, the
    // title, the space, the newline and the NUL before appending any of them, and gives up on
    // the rest of the sessions when it runs out.
    let mut action_str: Vec<u8> = Vec::with_capacity(LINE_BUFFER_LEN);
    for (i, session_row) in sessions.iter().enumerate() {
        let title = match session_row.current_action_title {
            Some(title) => title,
            None => session_row.current_action.as_str(),
        };
        if action_str.len() + title.len() + 4 >= LINE_BUFFER_LEN {
            tracing::error!("action_str of user(uid={}) truncated at i={}", user.uid, i);
            break;
        }
        action_str.extend_from_slice(b"[");
        action_str.extend_from_slice(title.as_bytes());
        action_str.extend_from_slice(b"] ");
    }
    action_str.push(b'\n');

    // `:143-154`: the first session's address, masked one octet deeper for a caller who is
    // not staff.
    let login_ip = match sessions.first() {
        Some(session_row) => ipmask::mask(
            &session_row.ip,
            if session.privs.checklevel(P_ADMIN_M | P_ADMIN_S) {
                1
            } else {
                2
            },
            b'*',
        ),
        None => String::new(),
    };

    // `:156-163`: one day of life per day since the last visit, except for the immortals.
    let life = if IMMORTAL_LIFE.contains(&user.life) {
        user.life
    } else {
        let now = chrono::Local::now().timestamp();
        user.life - ((now - user.last_login_dt) / 86400 + 1) as i32
    };

    let level = user_level(user.exp);
    let level_name = user_level_name(level);

    // `lml_render(intro, intro_f, sizeof(intro_f), SCREEN_COLS, 0)` (`:168`).
    let intro = lml::render(
        user.intro.as_bytes(),
        USER_INTRO_MAX_LEN + 1,
        SCREEN_COLS as i64,
        false,
        term.fixed_width(),
    );

    let mut buf: Vec<u8> = Vec::with_capacity(USER_INFO_MAX_LEN);
    buf.extend(
        format!(
            "\n{} ({}) 上站 [{}] 发文 [{}]\n\
             上次在 [{}] 从 [{}] 访问本站 经验值 [{}]\n\
             离线于 [{}] 生命 [{}] 等级 [{}({})] 星座 [{}]\n",
            user.username,
            user.nickname,
            user.visit_count,
            article_cnt,
            str_last_login,
            if is_online { &login_ip } else { "未知" },
            user.exp,
            if is_online {
                "在线或因断线不详"
            } else {
                &str_last_logout
            },
            life,
            level_name,
            level + 1,
            astro_str,
        )
        .as_bytes(),
    );
    buf.extend(
        if is_online {
            "目前在线，状态如下: \n"
        } else {
            ""
        }
        .as_bytes(),
    );
    buf.extend(b"\x1b[1m");
    if is_online {
        buf.extend(&action_str);
    }
    buf.extend(b"\x1b[m");
    buf.extend(
        if intro.is_empty() {
            "\x1b[0;36m没有个人说明档\x1b[m"
        } else {
            "\x1b[0;36m个人说明档如下: \x1b[m"
        }
        .as_bytes(),
    );
    buf.push(b'\n');
    buf.extend(&intro);
    buf.push(b'\n');

    // `snprintf` leaves the NUL its room, which is 8191 bytes of text.
    buf.truncate(USER_INFO_MAX_LEN - 1);
    buf
}

/// `display_user_info_key_handler` (`:36-39`), which reports nothing and so leaves the
/// profile with no status row of its own. Everything else is the session's pager bookkeeping.
struct ProfileHooks<'a> {
    session: &'a mut Session,
}

impl PagerHooks for ProfileHooks<'_> {
    async fn on_activity(&mut self) {
        Session::record_activity(self.session).await;
    }

    async fn key_handler(&mut self, _ch: &mut u32, _ctx: &mut DisplayCtx) -> i32 {
        0
    }

    fn help_enabled(&mut self) -> bool {
        PagerHooks::help_enabled(self.session)
    }

    fn set_help_enabled(&mut self, enabled: bool) {
        PagerHooks::set_help_enabled(self.session, enabled);
    }

    async fn show_help(&mut self, term: &mut Terminal, help_filename: &str) -> u32 {
        self.session.display_file(term, help_filename, 1).await
    }
}

/// `user_list_search` (`user_list_display.c:476`): type a prefix, get the grid of names that
/// start with it, and either keep typing, press Enter for the exact one, or leave.
pub async fn user_list_search(term: &mut Terminal, session: &mut Session) {
    // C's `char username[BBS_username_max_len + 1]`, which is both the buffer and the prefix
    // the next `get_data` starts from (`:482`, `:492`).
    let mut username: Vec<u8> = Vec::new();

    screen::clearscr(term);

    while !session.state.is_exiting() {
        screen::clrline(term, 3, SCREEN_ROWS);
        let Some(typed) = screen::get_data(
            term,
            2,
            1,
            "查找谁: ",
            &username,
            USERNAME_MAX_LEN + 1,
            USERNAME_MAX_LEN,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            return;
        };
        username = typed;
        if username.is_empty() {
            // C's `username[0] == '\0'`, which is also what an `ESC` leaves behind (`:501`).
            return;
        }

        let prefix = String::from_utf8_lossy(&username).into_owned();
        if !browse::valid_username(&prefix) {
            // The same rule the `/`-style author search runs (`section_list_display.c:1215`),
            // and the same refusal: a row of text and a key (`:518-525`).
            term.moveto(3, 1);
            term.clrtoeol();
            term.print("用户名格式非法");
            screen::press_any_key(term).await;
            continue;
        }

        screen::clrline(term, 3, SCREEN_ROWS);

        let roster = session.state.roster();
        let matches = roster.by_username_prefix(&prefix, MAX_USER_CNT);

        // `ret`, which in C is the count the query wrote (`:529`), the `> 1` grid below
        // narrowing it to an exact-match test.
        let found = if matches.len() > 1 {
            for (i, row) in matches
                .iter()
                .take(USERS_PER_LINE * MAX_USER_LINES)
                .enumerate()
            {
                term.moveto(
                    4 + i / USERS_PER_LINE,
                    3 + i % USERS_PER_LINE * (USERNAME_MAX_LEN + 3),
                );
                term.print(&row.username);
            }
            term.moveto(SCREEN_ROWS, 1);
            if matches.len() > USERS_PER_LINE * MAX_USER_LINES {
                term.print("还有更多...");
            }
            term.moveto(3, 1);
            term.print("存在多个匹配的用户，按\x1b[1;33mEnter\x1b[m精确查找");
            let _ = term.flush().await;

            match term.read_key_wait(MAX_USER_IDLE_TIME).await {
                KEY_NULL | KEY_TIMEOUT => return,
                KEY_ESC => return,
                CR => matches[0].username.eq_ignore_ascii_case(&prefix),
                ch => {
                    // One more character appended to the prefix, and the search starts over
                    // (`:564-571`). C assigns the key code to a `char`, which only ever holds
                    // a byte the `isalnum` test already accepted.
                    if username.len() + 1 <= USERNAME_MAX_LEN
                        && (key::is_alnum(ch) || ch == b'_' as u32)
                    {
                        username.push(ch as u8);
                    }
                    continue;
                }
            }
        } else {
            !matches.is_empty()
        };

        screen::clrline(term, 3, SCREEN_ROWS);
        if !found {
            term.moveto(3, 1);
            term.print("没有找到符合条件的用户");
            screen::press_any_key(term).await;
            return;
        }

        user_info_display(term, session, matches[0]).await;
        return;
    }
}

#[cfg(test)]
#[path = "../tests/test_userinfo.rs"]
mod tests;
