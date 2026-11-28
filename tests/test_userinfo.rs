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
