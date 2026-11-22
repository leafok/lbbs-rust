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
    // `get_user_article_cnt` (`user_list.c:1633`), whose `-1` for a uid the stat table does
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
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::db::{OnlineRow, UserRow};
    use crate::roster::{OnlineList, Roster};
    use crate::term::{ByteIo, Pipe};

    /// A terminal wired to a test peer plus the peer itself.
    fn pair() -> (Terminal, Pipe) {
        let (client, server) = Pipe::new();
        (Terminal::new(ByteIo::Pipe(server)), client)
    }

    /// The window a screen must stay silent before its answer is typed: a prompt flushes and
    /// then drains at 100 ms per poll, dropping whatever lands inside a drain, so silence is
    /// the only outward sign that it is really waiting.
    const QUIET: Duration = Duration::from_millis(250);
    const PROMPT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Copy what the screen has written into `seen`, waiting for it to change and then to go
    /// quiet: the change is the new screen, the quiet is its input reset finishing.
    async fn settle(client: &mut Pipe, seen: &mut Vec<u8>) {
        let deadline = tokio::time::Instant::now() + PROMPT_TIMEOUT;
        let mut changed = false;
        loop {
            tokio::time::sleep(QUIET).await;
            let more = client.take_available();
            if !more.is_empty() {
                changed = true;
                seen.extend_from_slice(&more);
                continue;
            }
            if changed || tokio::time::Instant::now() >= deadline {
                return;
            }
        }
    }

    /// Keys typed screen by screen, each waiting for the one it answers. The bytes seen while
    /// waiting come back with the pipe, because watching the screen is what tells the script
    /// where it is.
    fn feed(client: Pipe, steps: &[&'static [u8]]) -> tokio::task::JoinHandle<(Vec<u8>, Pipe)> {
        let steps = steps.to_vec();
        tokio::spawn(async move {
            let mut client = client;
            let mut seen = Vec::new();
            for keys in steps {
                settle(&mut client, &mut seen).await;
                client.push(keys);
            }
            (seen, client)
        })
    }

    /// Everything the session drew: what the script watched, plus what came after its last key.
    fn screen_text(typed: (Vec<u8>, Pipe)) -> String {
        let (mut screen, mut client) = typed;
        screen.extend_from_slice(&client.take_available());
        String::from_utf8_lossy(&screen).into_owned()
    }

    fn user_row(uid: i32, username: &str) -> UserRow {
        UserRow {
            uid,
            username: username.to_owned(),
            nickname: Some("昵称".to_owned()),
            gender: "M".to_owned(),
            gender_pub: true,
            life: 100,
            exp: Some(1000),
            visit_count: 7,
            signup_dt: None,
            // 2023-11-14, and a logout not after the login.
            last_login_dt: Some(1_700_000_000),
            last_logout_dt: Some(1_700_000_000),
            birthday: None,
            introduction: Some("简介".to_owned()),
        }
    }

    fn online_row(sid: &str, uid: i32, ip: &str, action: &str) -> OnlineRow {
        OnlineRow {
            sid: sid.to_owned(),
            uid,
            ip: ip.to_owned(),
            current_action: action.to_owned(),
            login_tm: 1_700_000_000,
            last_tm: Some(1_700_000_000),
        }
    }

    fn sessions(uid: i32, count: usize) -> Vec<OnlineRow> {
        (0..count)
            .map(|i| online_row(&format!("s{i}"), uid, "202.38.64.1", "USER_ONLINE"))
            .collect()
    }

    fn roster() -> Roster {
        Roster::load(&[
            user_row(10, "amy"),
            user_row(11, "Bob"),
            user_row(12, "bobby"),
            user_row(13, "bobsled"),
        ])
    }

    /// A session with both tables published and no database behind them.
    fn session_with(roster: Roster, online: OnlineList) -> Session {
        let session = crate::session::offline_session();
        session.state.publish_roster(roster);
        session.state.publish_online(online);
        session
    }

    /// The buffer C formats into `user_info_f`, which is what the screen then pages.
    fn profile(roster: Roster, online: OnlineList, uid: i32) -> String {
        let (term, _client) = pair();
        let session = session_with(roster, online);
        let user = session
            .state
            .roster()
            .by_uid(uid)
            .cloned()
            .expect("roster row");
        String::from_utf8_lossy(&profile_screen(&term, &session, &user)).into_owned()
    }

    #[tokio::test]
    async fn a_birthday_before_the_cusp_is_the_previous_sign() {
        // Either side of January's entry in `astro_dates`.
        let before = chrono::Local
            .with_ymd_and_hms(1980, 1, 20, 12, 0, 0)
            .unwrap()
            .timestamp();
        let after = chrono::Local
            .with_ymd_and_hms(1980, 1, 21, 0, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(astro_name(before), "摩羯");
        assert_eq!(astro_name(after), "水瓶");

        // A December past its date wraps to the thirteenth name.
        let late = chrono::Local
            .with_ymd_and_hms(1980, 12, 23, 0, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(astro_name(late), "摩羯");

        // The value C reads for a user who never filled the field in, whichever side of the
        // new year the server's zone puts the epoch on.
        assert_eq!(astro_name(0), "摩羯");
    }

    #[test]
    fn the_level_is_the_last_threshold_the_point_reached() {
        assert_eq!(user_level(i32::MIN), 0);
        assert_eq!(user_level(49), 0);
        assert_eq!(user_level(50), 1);
        assert_eq!(user_level(1000), 4);
        assert_eq!(user_level(99_999), 14);
        assert_eq!(user_level(100_000), 15);
        assert_eq!(user_level(i32::MAX), 15);
        assert_eq!(user_level_name(user_level(1000)), "小有作为");
    }

    #[tokio::test]
    async fn an_offline_profile_reports_the_logout_minute_and_an_unknown_address() {
        let screen = profile(roster(), OnlineList::default(), 10);
        assert!(screen.starts_with("\n"), "{screen}");
        assert!(
            screen.contains("amy (昵称) 上站 [7] 发文 [-1]\n"),
            "{screen}"
        );
        assert!(
            screen.contains("从 [未知] 访问本站 经验值 [1000]\n"),
            "{screen}"
        );

        // The logout is not after the login, so C prints the login minute twice.
        let login = clock_minute(1_700_000_000);
        assert!(
            screen.contains(&format!("上次在 [{login}] 从 [")),
            "{screen}"
        );
        assert!(
            screen.contains(&format!("离线于 [{login}] 生命 [")),
            "{screen}"
        );

        // 1000 points is rank 4, labelled with its name and `level + 1`.
        assert!(screen.contains("等级 [小有作为(5)]"), "{screen}");
        assert!(
            screen.contains("星座 [\x1b[1;36m摩羯座\x1b[m]\n"),
            "{screen}"
        );
        // Offline: no status block, and the bold section wraps nothing.
        assert!(!screen.contains("目前在线"), "{screen}");
        assert!(screen.contains("\x1b[1m\x1b[m"), "{screen}");
        assert!(screen.ends_with("简介\n"), "{screen}");
    }

    #[tokio::test]
    async fn the_gender_gate_only_chooses_the_colour() {
        let mut row = user_row(10, "amy");
        row.gender = "f".to_owned();
        let screen = profile(Roster::load(&[row.clone()]), OnlineList::default(), 10);
        assert!(
            screen.contains("星座 [\x1b[1;35m摩羯座\x1b[m]"),
            "female is violet: {screen}"
        );

        row.gender_pub = false;
        let screen = profile(Roster::load(&[row]), OnlineList::default(), 10);
        assert!(
            screen.contains("星座 [\x1b[1;33m摩羯座\x1b[m]"),
            "an unpublished gender is amber whatever it is: {screen}"
        );
    }

    #[tokio::test]
    async fn an_empty_intro_is_reported_as_absent() {
        // C tests `lml_render`'s return length, which whitespace alone does not zero.
        let mut row = user_row(10, "amy");
        row.introduction = Some("   \n".to_owned());
        let screen = profile(Roster::load(&[row]), OnlineList::default(), 10);
        assert!(
            screen.contains("\x1b[0;36m个人说明档如下: \x1b[m"),
            "the render is not empty: {screen}"
        );

        let mut row = user_row(10, "amy");
        row.introduction = None;
        let screen = profile(Roster::load(&[row]), OnlineList::default(), 10);
        assert!(
            screen.contains("\x1b[0;36m没有个人说明档\x1b[m"),
            "no introduction, no section: {screen}"
        );
    }

    #[tokio::test]
    async fn an_immortal_keeps_its_life_count_and_the_rest_age() {
        let mut row = user_row(10, "amy");
        row.life = 999;
        let screen = profile(Roster::load(&[row.clone()]), OnlineList::default(), 10);
        assert!(screen.contains("生命 [999]"), "{screen}");

        // A mortal loses one per day since the last visit, which for a 2023 login is well
        // below what the row holds; the exact number belongs to the clock.
        row.life = 100;
        let screen = profile(Roster::load(&[row]), OnlineList::default(), 10);
        assert!(!screen.contains("生命 [100]"), "{screen}");
    }

    #[tokio::test]
    async fn an_online_profile_lists_the_sessions_and_masks_the_address() {
        let roster = roster();
        let rows = vec![
            online_row("s1", 10, "202.38.64.1", "MENU"),
            online_row("s2", 10, "202.38.64.2", "VIEW_ARTICLE"),
        ];
        let screen = profile(roster.clone(), OnlineList::load(&rows, &roster), 10);

        // Two octets hidden from a caller with no staff level.
        assert!(
            screen.contains("访问本站 经验值"),
            "the address line: {screen}"
        );
        assert!(screen.contains("从 [202.38.*.*] 访问本站"), "{screen}");
        assert!(screen.contains("离线于 [在线或因断线不详]"), "{screen}");
        assert!(
            screen.contains("目前在线，状态如下: \n\x1b[1m[菜单选择] [阅读文章] \n\x1b[m"),
            "{screen}"
        );
    }

    #[tokio::test]
    async fn staff_see_one_octet_masked() {
        let roster = roster();
        let rows = vec![online_row("s1", 10, "202.38.64.1", "MENU")];
        let (term, _client) = pair();
        let mut session = session_with(roster.clone(), OnlineList::load(&rows, &roster));
        session.privs.level |= P_ADMIN_M;
        let user = session.state.roster().by_uid(10).cloned().unwrap();
        let screen = String::from_utf8_lossy(&profile_screen(&term, &session, &user)).into_owned();

        assert!(screen.contains("从 [202.38.64.*] 访问本站"), "{screen}");
    }

    #[tokio::test]
    async fn ten_sessions_are_asked_for_and_the_eleventh_is_not() {
        let roster = roster();
        let online = OnlineList::load(&sessions(10, MAX_SESSIONS_PER_USER + 1), &roster);
        let screen = profile(roster, online, 10);

        // `BBS_max_sessions_per_user` caps the query, not the drawing.
        assert_eq!(screen.matches("[环顾四周] ").count(), MAX_SESSIONS_PER_USER);
    }

    #[tokio::test]
    async fn the_profile_is_cut_at_the_buffer_limit() {
        let mut row = user_row(10, "amy");
        row.introduction = Some("说".repeat(3000));
        let screen = profile(Roster::load(&[row]), OnlineList::default(), 10);

        // `lml_render` fills `intro_f` to `sizeof` it, which is 4096 bytes; the header, the
        // ten brackets and that never reach the 8191 the `snprintf` allows.
        assert!(screen.len() <= USER_INFO_MAX_LEN - 1, "{}", screen.len());
        assert!(screen.len() < "说".repeat(3000).len(), "{}", screen.len());
        assert!(screen.ends_with('\n'));
    }

    /// `:183` asks for `MIN(SCREEN_ROWS - 1, USER_INFO_MAX_LINE)` offsets, and since `9e1eff6`
    /// `:33` defines that constant as the `BBS_user_intro_max_line + 9` slots the array of `:62`
    /// has: nineteen of them, which is what the count and the storage agree on. The `+1` of the
    /// same line is the wrap budget, and C's loop stops one line short of the slots it was given.
    #[tokio::test]
    async fn the_displayed_profile_stops_at_the_line_limit_and_ends_with_a_key() {
        let (mut term, client) = pair();
        let mut row = user_row(10, "amy");
        // Enough introduction lines to overflow the nineteen slots C asks for.
        row.introduction = Some(
            (1..=40)
                .map(|i| format!("第{i}行"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut session = session_with(Roster::load(&[row]), OnlineList::default());
        let user = session.state.roster().by_uid(10).cloned().unwrap();

        let typed = feed(client, &[b"q"]);
        user_info_display(&mut term, &mut session, &user).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("\x1b[2J"), "cleared first: {screen}");
        assert!(screen.contains("amy (昵称) 上站 [7]"), "{screen}");
        assert_eq!(
            USER_INFO_MAX_LINE, 19,
            "the ten introduction lines of `:33` and the nine header lines above them"
        );
        // C asks for nineteen offset slots, which is eighteen display lines: the header, the
        // label and thirteen lines of the introduction, and no more.
        assert!(screen.contains("第1行"), "{screen}");
        assert!(screen.contains("第13行"), "{screen}");
        assert!(!screen.contains("第14行"), "{screen}");
        // The screen ends on the pager's own row, which holds nothing but the message: the
        // nineteen slots C asked for never fill a page, so there is no progress to count.
        assert!(!screen.contains("行 ("), "{screen}");
        assert!(screen.contains("按任意键继续"), "{screen}");
    }

    #[tokio::test]
    async fn typing_a_prefix_narrows_the_grid_and_enter_opens_the_profile() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        let typed = feed(client, &[b"bob\r", b"\r", b"q"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("查找谁: "), "{screen}");
        assert!(
            screen.contains("存在多个匹配的用户，按\x1b[1;33mEnter\x1b[m精确查找"),
            "{screen}"
        );
        // Five names a line: column 3, then 3 + 12 + 3, from row 4 down.
        assert!(screen.contains("\x1b[4;3HBob"), "{screen}");
        assert!(screen.contains("\x1b[4;18Hbobby"), "{screen}");
        assert!(screen.contains("bobsled"), "{screen}");
        assert!(!screen.contains("\x1b[5;3H"), "one row of names: {screen}");
        // `strcasecmp("Bob", "bob")` is the exact match, so the profile of that row follows.
        assert!(screen.contains("Bob (昵称) 上站 [7]"), "{screen}");
        assert!(!screen.contains("没有找到符合条件的用户"), "{screen}");
    }

    #[tokio::test]
    async fn enter_without_an_exact_match_reports_nothing() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        let typed = feed(client, &[b"bo\r", b"\r", b"q"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("存在多个匹配的用户"), "{screen}");
        assert!(screen.contains("没有找到符合条件的用户"), "{screen}");
        assert!(!screen.contains("上站 ["), "no profile follows: {screen}");
    }

    #[tokio::test]
    async fn typing_at_the_grid_adds_the_character_and_searches_again() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        // "b" is a prefix of three names, so the grid asks; "x" is appended to it and the
        // second prompt opens with `bx` already typed.
        let typed = feed(client, &[b"b\r", b"x", b"\x1b"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("查找谁: b"), "{screen}");
        assert!(screen.contains("存在多个匹配的用户"), "{screen}");
        assert!(screen.contains("查找谁: bx"), "{screen}");
        assert!(!screen.contains("没有找到符合条件的用户"), "{screen}");
    }

    #[tokio::test]
    async fn a_prefix_nothing_starts_with_says_so() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        let typed = feed(client, &[b"zzz\r", b"q"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("没有找到符合条件的用户"), "{screen}");
        assert!(!screen.contains("存在多个匹配的用户"), "{screen}");
    }

    #[tokio::test]
    async fn an_illegal_prefix_is_refused_in_place() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        // A digit first, which is what the format rule rejects. The refusal is not an exit,
        // so the last key typed is the escape that ends the search.
        let typed = feed(client, &[b"7\r", b"q", b"\x1b"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("用户名格式非法"), "{screen}");
        assert!(!screen.contains("没有找到符合条件的用户"), "{screen}");
        assert_eq!(screen.matches("查找谁: ").count(), 2, "{screen}");
    }

    #[tokio::test]
    async fn a_hundred_and_one_matches_say_there_are_more() {
        let rows = (1..=102)
            .map(|i| user_row(i, &format!("nam{i:03}")))
            .collect::<Vec<_>>();
        let (mut term, client) = pair();
        let mut session = session_with(Roster::load(&rows), OnlineList::default());

        let typed = feed(client, &[b"nam\r", b"\x1b"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        // One hundred names are drawn and the hundred and first only announces itself.
        assert!(screen.contains("\x1b[23;63Hnam100"), "{screen}");
        assert!(!screen.contains("nam101"), "{screen}");
        assert!(screen.contains("\x1b[24;1H还有更多..."), "{screen}");
    }

    #[tokio::test]
    async fn escape_leaves_the_search_without_a_message() {
        let (mut term, client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        let typed = feed(client, &[b"am\x1b"]);
        user_list_search(&mut term, &mut session).await;
        let screen = screen_text(typed.await.unwrap());

        assert!(screen.contains("查找谁:"), "{screen}");
        // The editor echoes one character at a time, each from the cursor it just left, so
        // the two keys are on the screen but never as the whole word.
        assert!(screen.contains("\x1b[2;9Ha"), "{screen}");
        assert!(screen.contains("\x1b[2;10Hm"), "{screen}");
        // `ESC` empties the buffer and `get_data` returns it, which is the exit at the top of
        // C's loop; it is not the refusal that re-prompts.
        assert_eq!(screen.matches("查找谁: ").count(), 1, "{screen}");
        assert!(!screen.contains("没有找到符合条件的用户"), "{screen}");
        assert!(!screen.contains("存在多个匹配的用户"), "{screen}");
    }

    #[tokio::test]
    async fn a_closed_client_ends_the_search_at_once() {
        let (mut term, mut client) = pair();
        let mut session = session_with(roster(), OnlineList::default());

        // A prefix is typed and the pipe goes away before the next prompt. C searches that
        // stale prefix once more; the port leaves, so the screen never asks again.
        client.push(b"bob\r");
        client.hang_up();
        tokio::time::timeout(
            Duration::from_secs(5),
            user_list_search(&mut term, &mut session),
        )
        .await
        .expect("the search does not spin on a closed pipe");
    }
}
