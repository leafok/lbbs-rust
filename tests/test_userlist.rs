use super::*;

use std::time::Duration;

use crate::db::{OnlineRow, UserRow};
use crate::roster::{OnlineList, Roster};
use crate::term::{ByteIo, Pipe};

/// `1970-01-01 00:00:00 UTC`, the origin every duration here is measured from.
const EPOCH: i64 = 0;
const HOUR: i64 = 3600;
const DAY: i64 = 86400;

fn pair() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

fn user_row(uid: i32, username: &str) -> UserRow {
    UserRow {
        uid,
        username: username.to_owned(),
        nickname: Some(format!("nick{uid}")),
        gender: "M".to_owned(),
        gender_pub: true,
        life: 100,
        exp: Some(1000),
        visit_count: 7,
        signup_dt: None,
        last_login_dt: Some(EPOCH),
        last_logout_dt: Some(EPOCH),
        birthday: None,
        introduction: None,
    }
}

fn roster(count: usize) -> Roster {
    Roster::load(
        &(1..=count)
            .map(|i| user_row(i as i32, &format!("user{i:02}")))
            .collect::<Vec<_>>(),
    )
}

fn online_row(sid: &str, uid: i32, action: &str) -> OnlineRow {
    OnlineRow {
        sid: sid.to_owned(),
        uid,
        ip: "202.38.64.1".to_owned(),
        current_action: action.to_owned(),
        login_tm: EPOCH,
        last_tm: Some(EPOCH),
    }
}

/// A session with both tables published and no database behind them.
fn session_with(roster: Roster, online: OnlineList) -> Session {
    let session = crate::session::offline_session();
    session.state.publish_roster(roster);
    session.state.publish_online(online);
    session
}

/// Everything the screen wrote while a script of keys was typed into it.
///
/// `user_list_select` polls at 100 ms and never drains, so a script of movement and exit
/// keys can be queued up front. A key that opens another screen cannot: the pager and the
/// line editor start by discarding what they were not given, so those tests feed the keys
/// one screen at a time with [`feed`].
async fn run(session: &mut Session, online_user: bool, keys: &[u8]) -> String {
    let (mut term, mut client) = pair();
    if !keys.is_empty() {
        client.push(keys);
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        user_list_display(&mut term, session, online_user),
    )
    .await
    .expect("the screen ends on its keys");
    String::from_utf8_lossy(&client.take_available()).into_owned()
}

/// The window a screen must stay silent before its answer is typed.
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

/// Keys typed screen by screen, each waiting for the one it answers.
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

/// The key line of `user_list_draw_screen`, which is what a redraw of the list can be
/// counted by: no other screen prints it.
fn drawings(screen: &str) -> usize {
    screen.matches("查找[\x1b[1;32ms\x1b[0;37m]").count()
}

#[test]
fn the_years_of_a_visit_are_counted_from_1970() {
    // `tm_year > 70`, which is any duration that reaches 1971.
    assert_eq!(since_visit(366 * DAY), "1年1天");
    // The same test one day short of 1971 falls to the days branch.
    assert_eq!(since_visit(364 * DAY), "364天");
    assert_eq!(since_visit(DAY + HOUR), "1天");
    assert_eq!(since_visit(2 * HOUR + 3 * 60), "2:03");
    assert_eq!(since_visit(65), "1'05\"");
    assert_eq!(since_visit(0), "0'00\"");
    // Before the epoch `gmtime` is in 1969: day 364 of that year, which the days branch
    // prints without a sign.
    assert_eq!(since_visit(-1), "364天");
}

#[test]
fn a_session_age_never_shows_years() {
    // 400 days is five days into 1971, and the years are not split out here.
    assert_eq!(since_login(400 * DAY), "35d0h");
    assert_eq!(since_login(DAY), "1d0h");
    // One second short of a day is still day 0, so the hour branch answers for it.
    assert_eq!(since_login(DAY - 1), "23:59");
    assert_eq!(since_login(90 * 60), "1:30");
    assert_eq!(since_login(7), "0'07\"");
}

#[test]
fn the_idle_field_does_not_pad_its_seconds() {
    assert_eq!(since_activity(61), "1'1\"");
    assert_eq!(since_activity(59 * 60 + 59), "59'59\"");
    assert_eq!(since_activity(9), "9\"");
    assert_eq!(since_activity(0), "0\"");
}

#[test]
fn the_idle_field_wraps_past_the_hour() {
    // `tm_min` is the minute of the hour, not the minutes elapsed, so an hour of reading
    // the same article reports no idle time at all. C prints the same.
    assert_eq!(since_activity(3600), "0\"");
    assert_eq!(since_activity(3661), "1'1\"");
}

#[test]
fn a_duration_chrono_cannot_read_is_an_empty_field() {
    // C's `gmtime` answers `NULL` here; chrono's `from_timestamp` answers `None`.
    assert!(since_visit(i64::MAX).is_empty());
    assert!(since_login(i64::MIN).is_empty());
    assert!(since_activity(i64::MAX).is_empty());
}

#[test]
fn the_columns_pad_to_their_width_whatever_the_name() {
    let fixed = true;
    // Twelve cells for the username, twenty for the nickname.
    assert_eq!(field("user01", 12, fixed), "user01      ");
    assert_eq!(field("用户", 12, fixed), "用户        ");
    // An over-long name still pads, by the overshoot: `%*s` of a negative width.
    assert_eq!(field("a-name-too-long", 12, fixed), "a-name-too-long   ");
}

#[test]
fn a_registered_row_is_the_pool_number_and_two_names_and_an_age() {
    let users = roster(2);
    let first = users.page(0).expect("page 0")[0].clone();
    // Ten minutes since the visit, a twelve-cell username column and a twenty-cell
    // nickname column.
    assert_eq!(
        registered_line(&first, true, 10 * 60),
        "       1  user01       nick1                10'00\""
    );
    // The number is the pool slot, which is not the uid.
    let second = users.page(0).expect("page 0")[1].clone();
    assert_eq!(
        registered_line(&second, true, 10 * 60),
        "       2  user02       nick2                10'00\""
    );
}

#[test]
fn an_online_row_adds_the_two_durations_and_the_action() {
    let users = roster(1);
    let rows = vec![online_row("s1", 1, "VIEW_ARTICLE")];
    let online = OnlineList::load(&rows, &users);
    let session_row = &online.users[0];
    // Ninety minutes on, ninety minutes idle: the session column is eight wide and the
    // idle column six. The idle field is `tm_min` and `tm_sec`, so ninety minutes reads
    // as thirty — see `the_idle_field_wraps_past_the_hour`.
    assert_eq!(
        online_line(session_row, true, 90 * 60),
        "       1  user01       nick1                1:30     30'0\"  阅读文章"
    );
}

#[tokio::test]
async fn the_registered_page_draws_its_titles_and_its_rows() {
    let mut session = session_with(
        roster(USER_LIMIT_PER_PAGE as usize + 2),
        OnlineList::default(),
    );
    let screen = run(&mut session, false, b"\x1b").await;

    assert!(screen.contains("\x1b[2J"), "cleared first: {screen}");
    assert!(
        screen.contains("\x1b[1;44;33m[已注册用户]\x1b[37m"),
        "the title row: {screen}"
    );
    assert!(screen.contains("www.bbs.test"), "the site name: {screen}");
    assert!(screen.contains("查找[\x1b[1;32ms\x1b[0;37m]"), "{screen}");
    assert!(screen.contains("上次登陆距今"), "{screen}");
    assert!(!screen.contains("在线时长"), "{screen}");
    assert!(screen.contains("\x1b[4;1H       1  user01"), "{screen}");
    // Page 1 of 2, and the cursor on the first row.
    assert!(
        screen.contains("[第\x1b[36m1\x1b[33m/\x1b[36m2\x1b[33m页]"),
        "{screen}"
    );
    assert!(screen.contains("\x1b[24;0H"), "the status row: {screen}");
    assert_eq!(drawings(&screen), 1, "{screen}");
    assert!(screen.contains("\x1b[4;1H>"), "{screen}");
}

#[tokio::test]
async fn the_online_page_is_the_other_title_and_the_other_columns() {
    let users = roster(3);
    let rows = vec![online_row("s1", 2, "MENU")];
    let online = OnlineList::load(&rows, &users.clone());
    let mut session = session_with(users, online);
    let screen = run(&mut session, true, b"\x1b").await;

    assert!(screen.contains("[线上使用者]"), "{screen}");
    assert!(screen.contains("在线时长 空闲   最后活动"), "{screen}");
    assert!(screen.contains("菜单选择"), "{screen}");
    assert!(!screen.contains("上次登陆距今"), "{screen}");
}

#[tokio::test]
async fn an_empty_table_is_page_one_of_one_with_nothing_to_choose() {
    let mut session = session_with(Roster::default(), OnlineList::default());
    let screen = run(&mut session, false, b"\r\x1b").await;

    assert!(
        screen.contains("[第\x1b[36m1\x1b[33m/\x1b[36m1\x1b[33m页]"),
        "{screen}"
    );
    // With no rows the cursor is not drawn, and Enter does not open a profile.
    assert!(!screen.contains("\x1b[4;1H>"), "{screen}");
    assert!(!screen.contains("上站 ["), "{screen}");
}

#[tokio::test]
async fn the_down_key_moves_the_cursor_and_the_up_key_puts_it_back() {
    let mut session = session_with(roster(3), OnlineList::default());
    let screen = run(&mut session, false, b"\x1b[B\x1b[A\x1b").await;

    // The cursor is drawn where it goes and wiped where it was, and no page is re-read:
    // the rows are drawn once, before the first key.
    assert!(screen.contains("\x1b[5;1H>"), "{screen}");
    assert!(screen.contains("\x1b[5;1H "), "{screen}");
    assert!(screen.contains("\x1b[4;1H>"), "{screen}");
    assert_eq!(drawings(&screen), 1, "{screen}");
}

#[tokio::test]
async fn down_at_the_last_row_of_the_page_turns_it() {
    let mut session = session_with(
        roster(USER_LIMIT_PER_PAGE as usize + 1),
        OnlineList::default(),
    );
    // 20 rows on page 0: nineteen downs reach the last, and the twentieth turns the page.
    let mut keys = [b"\x1b[B".as_slice(); USER_LIMIT_PER_PAGE as usize].concat();
    keys.extend_from_slice(b"\x1b");
    let screen = run(&mut session, false, &keys).await;

    assert!(screen.contains("[第\x1b[36m2\x1b[33m/"), "{screen}");
    assert!(
        screen.contains("user21"),
        "the first row of page 2: {screen}"
    );
    // A page turn re-reads the rows without redrawing the titles.
    assert_eq!(
        drawings(&screen),
        1,
        "the page turn draws no titles: {screen}"
    );
}

#[tokio::test]
async fn up_at_the_first_row_rotates_to_the_tail() {
    let mut session = session_with(roster(2), OnlineList::default());
    let screen = run(&mut session, false, b"\x1b[A\x1b").await;

    // One page, so rotation lands on the last row of the list, not the last of a page.
    assert!(screen.contains("\x1b[5;1H>"), "{screen}");
    assert!(
        screen.contains("[第\x1b[36m1\x1b[33m/\x1b[36m1\x1b[33m页]"),
        "{screen}"
    );
}

#[tokio::test]
async fn home_and_end_set_the_page_before_they_move() {
    // The two keys are the VT forms the decoder answers: `\x1b[4~` and `\x1b[1~`. The
    // `\x1b[F`/`\x1b[H` of the application cursor keys are not in `igetch`'s table.
    let users = USER_LIMIT_PER_PAGE as usize + 3;

    let mut session = session_with(roster(users), OnlineList::default());
    let screen = run(&mut session, false, b"\x1b[4~\x1b").await;

    // END: last page, and its last row: 23 users over two pages, so page 2 holds 3.
    assert!(screen.contains("[第\x1b[36m2\x1b[33m/"), "{screen}");
    assert!(screen.contains("\x1b[6;1H>"), "{screen}");
    assert!(
        screen.contains("user23"),
        "the tail of the roster: {screen}"
    );

    let mut session = session_with(roster(users), OnlineList::default());
    let screen = run(&mut session, false, b"\x1b[4~\x1b[1~\x1b").await;

    // HOME from page 2: the page is set to 0, then `P` runs, then `k`, and at the head of
    // an empty direction nothing moves.
    assert!(
        screen.contains("[第\x1b[36m1\x1b[33m/\x1b[36m2\x1b[33m页]"),
        "{screen}"
    );
    assert!(screen.contains("\x1b[4;1H>"), "{screen}");
}

#[tokio::test]
async fn enter_opens_the_profile_of_the_row_under_the_cursor() {
    let (mut term, client) = pair();
    let mut session = session_with(roster(2), OnlineList::default());
    // Down to the second row, Enter for its profile, `q` out of the pager, ESC out of the
    // list.
    let typed = feed(client, &[b"\x1b[B", b"\r", b"q", b"\x1b"]);
    tokio::time::timeout(
        Duration::from_secs(30),
        user_list_display(&mut term, &mut session, false),
    )
    .await
    .expect("the screen ends on its keys");
    let screen = screen_text(typed.await.unwrap());

    assert!(screen.contains("user02 (nick2) 上站 [7]"), "{screen}");
    assert!(
        screen.contains("按任意键继续"),
        "the pager's own row: {screen}"
    );
    // `user_list_draw_screen` runs again on the way back, titles and all.
    assert_eq!(drawings(&screen), 2, "{screen}");
}

#[tokio::test]
async fn s_runs_the_name_search_inside_the_list() {
    let (mut term, client) = pair();
    let mut session = session_with(roster(3), OnlineList::default());
    // `s` opens the search, which prompts on row 2; one name matches exactly, so its
    // profile follows at once; `q` leaves the profile and ESC leaves the list.
    let typed = feed(client, &[b"s", b"user02\r", b"q", b"\x1b"]);
    tokio::time::timeout(
        Duration::from_secs(30),
        user_list_display(&mut term, &mut session, false),
    )
    .await
    .expect("the screen ends on its keys");
    let screen = screen_text(typed.await.unwrap());

    assert!(screen.contains("查找谁: "), "{screen}");
    assert!(screen.contains("user02 (nick2) 上站 [7]"), "{screen}");
    assert_eq!(drawings(&screen), 2, "the list is drawn again: {screen}");
}

#[tokio::test]
async fn h_shows_the_reader_help_and_redraws_the_screen() {
    let (mut term, client) = pair();
    let mut session = session_with(roster(2), OnlineList::default());
    let typed = feed(client, &[b"h", b"q", b"\x1b"]);
    tokio::time::timeout(
        Duration::from_secs(30),
        user_list_display(&mut term, &mut session, false),
    )
    .await
    .expect("the screen ends on its keys");
    let screen = screen_text(typed.await.unwrap());

    // Once before the help, once after it, whatever `data/read_help.txt` has to say.
    assert_eq!(drawings(&screen), 2, "{screen}");
    assert!(screen.contains("[已注册用户]"), "{screen}");
}

#[tokio::test]
async fn f5_re_reads_the_page_a_later_publish_shows() {
    let (mut term, client) = pair();
    let mut session = session_with(roster(1), OnlineList::default());
    // A second user joins the roster while the first page is on the screen; F5 is what
    // makes the screen notice.
    let state = session.state.clone();
    let typing = tokio::spawn(async move {
        tokio::time::sleep(QUIET).await;
        state.publish_roster(roster(2));
        client.push(b"\x1b[15~");
        tokio::time::sleep(QUIET).await;
        client.push(b"\x1b");
        client
    });
    tokio::time::timeout(
        Duration::from_secs(30),
        user_list_display(&mut term, &mut session, false),
    )
    .await
    .expect("the screen ends on its keys");
    let mut client = typing.await.unwrap();
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();

    assert!(screen.contains("user01"), "{screen}");
    assert!(screen.contains("user02"), "F5 re-read the roster: {screen}");
}

#[tokio::test]
async fn a_client_that_goes_away_ends_the_list() {
    let mut session = session_with(roster(2), OnlineList::default());
    // The whole client end dropped: every write fails and every read is end of stream,
    // which is what `igetch` answers with `KEY_NULL` and C's `user_list_select` treats as
    // the exit key. Nothing can be read back, so the test is that the screen returns.
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    drop(client);
    tokio::time::timeout(
        Duration::from_secs(10),
        user_list_display(&mut term, &mut session, false),
    )
    .await
    .expect("the screen leaves when the client does");
    assert!(term.is_closed());
}

#[tokio::test]
async fn a_server_already_exiting_draws_no_rows() {
    let mut session = session_with(roster(2), OnlineList::default());
    session.state.exit();
    let screen = run(&mut session, false, b"").await;

    // C draws the titles and reads the page before it tests `SYS_server_exit`, and the
    // only `iflush` of the loop body sits after the rows: what the screen put in the
    // buffer never reaches the wire, and the rows are never drawn.
    assert!(screen.is_empty(), "{screen}");
}

/// Both pages against the reference site's real tables, where the nicknames are CJK, the
/// roster is longer than one page and the pool numbers come from a loaded `user` table:
/// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn a_real_site_fills_both_roster_pages() {
    let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let root = std::path::Path::new(&conf)
        .ancestors()
        .nth(2)
        .expect("LBBS_REF_CONF must be <root>/conf/bbsd.conf")
        .to_path_buf();
    let text = std::fs::read_to_string(&conf).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let bbs_name = config.bbs_name.clone();
    let db = crate::db::Db::connect(&config.db).await.unwrap();
    let state = std::sync::Arc::new(AppState::new(config, db));
    crate::loader::Loader::new(state.clone(), &root)
        .initial_load()
        .await
        .expect("the reference site loads");

    for online_user in [false, true] {
        let mut session = Session::new(
            state.clone(),
            &root,
            "lbbs-smoke".to_string(),
            "127.0.0.1".to_string(),
            0,
            false,
        );
        let (total_page, listed) = if online_user {
            let list = state.online();
            (list.page_count(), list.users.len())
        } else {
            let roster = state.roster();
            (roster.page_count(), roster.count() as usize)
        };
        let page = total_page.max(1);

        // Down one row, to the last page, and out: keys that stay inside the list, so
        // they can all be queued before the screen starts.
        let (mut term, mut client) = pair();
        client.push(b"\x1b[B\x1b[4~\x1b");
        tokio::time::timeout(
            Duration::from_secs(30),
            user_list_display(&mut term, &mut session, online_user),
        )
        .await
        .expect("the page ends on its keys");
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();

        assert!(
            screen.contains(if online_user {
                "[线上使用者]"
            } else {
                "[已注册用户]"
            }),
            "{screen}"
        );
        assert!(screen.contains(&bbs_name), "{screen}");
        // 环顾四周 hides the session running the test: uid 0 is a guest, which
        // [`crate::roster::OnlineList::load`] counts but does not list, so a reference
        // site with nobody on it shows an empty page here.
        if listed > 0 {
            assert!(
                screen.contains("\x1b[4;1H       1  "),
                "the pool number of the first row: {screen}"
            );
            assert!(screen.contains("\x1b[4;1H>"), "the cursor: {screen}");
        }
        assert!(
            screen.contains(&format!(
                "[第\x1b[36m{page}\x1b[33m/\x1b[36m{page}\x1b[33m页]"
            )),
            "END lands on the last of {page} pages: {screen}"
        );
    }
}
