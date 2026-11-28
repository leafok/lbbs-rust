use super::*;
use crate::board::Board;
use crate::favorlog::FavorLog;
use crate::term::{ByteIo, Pipe};
use std::time::Duration;

/// 2023-11-14, far enough back that the date column is its `%m/%Y` form in any time zone and
/// on any day the tests run.
const OLD: i64 = 1_700_000_000;

fn post(aid: i32, sid: i16, title: &str) -> Article {
    Article {
        aid,
        sid,
        uid: 7,
        username: format!("user{aid}"),
        nickname: format!("nick{aid}"),
        title: title.to_string(),
        sub_dt: OLD,
        visible: true,
        ..Article::default()
    }
}

/// The two boards a row can name: the short one leaves the title its full budget, the
/// 20-cell one is exactly as wide as its field.
fn pool(articles: &[Article]) -> Board {
    let mut board = Board::default();
    assert!(
        board
            .create_section(1, "test", "测试板", "master")
            .is_some()
    );
    assert!(
        board
            .create_section(2, "long-board-name-here", "长板", "master")
            .is_some()
    );
    for article in articles {
        board.append_article(article).expect("append");
    }
    board
}

fn sname_of(sid: i16) -> String {
    if sid == 2 {
        "long-board-name-here".to_string()
    } else {
        "test".to_string()
    }
}

fn line(article: &Article, display_sname: bool, view_log: &ViewLog, uid: i32) -> String {
    let row = Row {
        article: article.clone(),
        sname: sname_of(article.sid),
    };
    row_line(&row, display_sname, view_log, uid, false, session::now())
}

/// The row the drawer writes, printed as `%7d` beside a star that is never in the pool.
fn row_marker(aid: i32) -> String {
    format!("  {aid:>7} N user{aid}")
}

/// The star list a page reads. `uid` 0 keeps it in memory: `article_favor_flush_inc`
/// (`article_favor.c:132-135`) does nothing without a user, so the page query that starts
/// with a flush never reaches the unreachable database of [`session::offline_session`].
fn starred(aids: &[i32], articles: &[Article]) -> Session {
    let mut session = session::offline_session();
    session.uid = 2;
    session.privs.uid = 2;
    session.username = "tester".to_string();
    session.favor = FavorLog::from_base(0, aids.to_vec());
    *session.state.board_mut() = pool(articles);
    session
}

fn pair() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

/// Run the page over `session` with `keys` queued up front — the key loop polls at 100 ms and
/// never drains the terminal, so a script needs no pacing — and give back everything written.
/// The closing flush is the menu's: the screen gives up mid-draw when a query fails, and the
/// bytes it had buffered only reach the client when the redraw after it flushes.
async fn run(session: &mut Session, keys: &[u8]) -> String {
    let (mut term, mut client) = pair();
    client.push(keys);

    tokio::time::timeout(
        Duration::from_secs(10),
        article_favor_display(&mut term, session),
    )
    .await
    .expect("the page ends on its keys");
    let _ = term.flush().await;

    String::from_utf8_lossy(&client.take_available()).into_owned()
}

fn count(screen: &str, needle: &str) -> usize {
    screen.match_indices(needle).count()
}

#[test]
fn a_starred_thread_is_number_flag_author_date_and_title() {
    // The author column is 12 cells, so the title keeps 47 of the line's 59.
    let article = post(12345, 1, &"t".repeat(50));
    assert_eq!(
        line(&article, false, &ViewLog::anonymous(), 2),
        format!("  {:>7} N user12345    11/2023 ● {}", 12345, "t".repeat(45))
    );
}

#[test]
fn the_board_name_column_wears_the_padding_instead() {
    // Same title, 20 cells of name: the budget is 39, and the short name still pads out.
    let article = post(12345, 1, &"t".repeat(50));
    assert_eq!(
        line(&article, true, &ViewLog::anonymous(), 2),
        format!(
            "  {:>7} N test{} 11/2023 ● {}",
            12345,
            " ".repeat(16),
            "t".repeat(37)
        )
    );
}

#[test]
fn a_board_name_of_the_full_field_width_eats_the_space_before_the_date() {
    let article = post(12345, 2, &"t".repeat(50));
    assert_eq!(
        line(&article, true, &ViewLog::anonymous(), 2),
        format!(
            "  {:>7} N long-board-name-here 11/2023 ● {}",
            12345,
            "t".repeat(37)
        )
    );
}

#[test]
fn a_thread_head_transships_and_the_re_prefix_stays() {
    // Unlike the board list, `article_favor_draw_items` never collapses a `Re: `.
    let mut article = post(12345, 1, "Re: 讨论");
    article.transship = true;
    let text = line(&article, false, &ViewLog::anonymous(), 2);
    assert!(
        text.ends_with("● [转载]Re: 讨论"),
        "the prefixes stack: {text}"
    );
}

#[test]
fn the_flag_reads_the_digest_the_lock_and_the_view_log() {
    let unseen = ViewLog::anonymous();
    let seen = ViewLog::from_base(2, vec![12345]);
    let plain = post(12345, 1, "t");
    // The reader's own post counts as read whatever the view log says (`:80-83`).
    let mine = Article {
        uid: 2,
        ..plain.clone()
    };

    let mut digest = plain.clone();
    digest.excerption = true;
    let mut locked = plain.clone();
    locked.lock = true;
    let locked_mine = Article {
        lock: true,
        ..mine.clone()
    };

    for (article, view_log, uid, expected) in [
        (&digest, &unseen, 2, 'M'),
        (&digest, &seen, 2, 'm'),
        (&locked, &seen, 2, 'x'),
        (&locked, &unseen, 2, 'N'),
        (&plain, &seen, 2, ' '),
        (&plain, &unseen, 2, 'N'),
        (&mine, &unseen, 2, ' '),
        (&locked_mine, &unseen, 2, 'x'),
    ] {
        let text = line(article, false, view_log, uid);
        let flag = text
            .chars()
            .nth(10)
            .unwrap_or_else(|| panic!("no flag column in {text}"));
        assert_eq!(flag, expected, "flag of {article:?}");
    }
}

#[tokio::test]
async fn an_empty_star_list_is_page_one_of_one() {
    let mut session = starred(&[], &[]);
    let screen = run(&mut session, b"\x1b").await;

    assert_eq!(count(&screen, "[主题收藏]"), 1);
    assert_eq!(count(&screen, "第\x1b[36m1\x1b[33m/\x1b[36m1\x1b[33m页"), 1);
    assert!(!screen.contains("\x1b[4;1H"), "no row is drawn: {screen}");
}

#[tokio::test]
async fn n_switches_the_column_and_redraws_the_screen() {
    let articles = [post(101, 1, "t"), post(102, 2, "t")];
    let mut session = starred(&[101, 102], &articles);
    // The escape byte on its own decodes as `KEY_ESC`, which leaves the list.
    let screen = run(&mut session, b"n\x1b").await;

    assert!(screen.contains(USERNAME_TITLE));
    assert!(screen.contains(SNAME_TITLE));
    // The key line names the column `n` will switch to, so both words appear.
    assert!(screen.contains("版块名称[\x1b[1;32mn"));
    assert!(screen.contains("用户名[\x1b[1;32mn"));
    assert_eq!(count(&screen, "[主题收藏]"), 2);
    assert!(session.display_sname, "the switch survives the screen");
}

#[tokio::test]
async fn the_cursor_walks_down_and_up_the_rows() {
    let articles = [post(101, 1, "t"), post(102, 1, "t")];
    let mut session = starred(&[101, 102], &articles);
    // Down, up, escape.
    let screen = run(&mut session, b"\x1b[B\x1b[A\x1b").await;

    assert!(screen.contains("\x1b[4;1H>"), "the list opens on row 4");
    assert!(screen.contains("\x1b[4;1H \x1b[5;1H>"));
    assert!(screen.contains("\x1b[5;1H \x1b[4;1H>"));
    // Moving the cursor is not a redraw: the rows were written once.
    assert_eq!(count(&screen, &row_marker(101)), 1);
}

#[tokio::test]
async fn dash_unstars_the_row_it_sits_on() {
    let articles = [post(101, 1, "t"), post(102, 1, "t")];
    let mut session = starred(&[101, 102], &articles);
    let screen = run(&mut session, b"-\x1b").await;

    assert!(!session.favor.is_favor(101));
    assert!(session.favor.is_favor(102));
    // Once for the page as drawn, once for the page after the removal.
    assert_eq!(count(&screen, &row_marker(101)), 1);
    assert_eq!(count(&screen, &row_marker(102)), 2);
}

#[tokio::test]
async fn unstarring_the_last_row_of_the_last_page_closes_the_list() {
    // The refresh after the removal keeps the `page_id` the key loop had, and 20 stars no
    // longer have a page 1: `query_favor_articles` reports the page out of range and the
    // screen ends instead of clamping to the new tail.
    let articles: Vec<Article> = (1..=21).map(|aid| post(aid, 1, "t")).collect();
    let aids: Vec<i32> = (1..=21).collect();
    let mut session = starred(&aids, &articles);
    let screen = run(&mut session, b"$-").await;

    assert_eq!(count(&screen, &row_marker(1)), 1, "the first page is drawn");
    assert_eq!(count(&screen, &row_marker(21)), 1, "and so is the tail");
    assert!(
        !session.favor.is_favor(21),
        "the removal still reached the list"
    );
}

#[tokio::test]
async fn a_star_whose_article_left_the_pool_closes_the_list() {
    // Nothing is typed: the failed lookup returns from the screen by itself.
    let mut session = starred(&[999], &[post(101, 1, "t")]);
    let screen = run(&mut session, b"").await;

    assert!(screen.contains("移除"), "the screen is drawn: {screen}");
    assert!(!screen.contains("999"), "but no row is");
}

#[tokio::test]
async fn the_help_page_leaves_the_list_redrawn_behind_it() {
    // `data/read_help.txt` is not under the offline session's root, so `display_file` reports
    // `KEY_NULL` without asking for a key: `h` alone must be enough to reach the redraw.
    let mut session = starred(&[101], &[post(101, 1, "t")]);
    let screen = run(&mut session, b"h\x1b").await;

    assert_eq!(count(&screen, "[主题收藏]"), 2);
    assert_eq!(count(&screen, &row_marker(101)), 2);
}
