use super::*;

/// The two `char title_input[...]` arrays, sized the way C declared them: `sizeof` a string
/// literal carries its terminator, which is the byte the reply buffer has and the new-post
/// buffer does not.
#[test]
fn the_title_buffers_are_the_c_arrays() {
    assert_eq!(TITLE_POST_BUF_LEN, 161);
    assert_eq!(TITLE_REPLY_BUF_LEN, 165);
    assert_eq!(
        TITLE_REPLY_BUF_LEN,
        TITLE_POST_BUF_LEN + "Re: ".len(),
        "a reply fits its prefix and keeps the terminator"
    );
}
use crate::config::{Config, DbConfig};
use crate::state::AppState;
use crate::term::{ByteIo, Pipe, Terminal};
use std::path::Path;
use std::sync::Arc;

fn target(aid: i32, tid: i32, uid: i32, excerption: bool) -> Deletable {
    Deletable {
        aid,
        tid,
        uid,
        excerption,
    }
}

/// The whole of `article_del`'s arithmetic: `m_del` says whether a moderator did it, and
/// the experience bill is bigger for a root than for a reply, and bigger again when
/// somebody else's post is taken down.
#[test]
fn the_author_pays_half_of_what_a_moderator_pays() {
    let own_root = target(10, 0, 7, false);
    let own_reply = target(11, 10, 7, false);
    let foreign_root = target(12, 0, 8, false);
    let foreign_reply = target(13, 12, 8, false);

    assert_eq!(delete_cost(&own_root, 7), (0, -20, 'D'));
    assert_eq!(delete_cost(&own_reply, 7), (0, -5, 'D'));
    assert_eq!(delete_cost(&foreign_root, 7), (1, -50, 'X'));
    assert_eq!(delete_cost(&foreign_reply, 7), (1, -15, 'X'));

    // The bill follows the post's owner, not the reader's uid.
    assert_eq!(delete_cost(&own_reply, 8), (1, -15, 'X'));
}

/// The digest bill (`article_op.c:179-192`): a topic root is worth five times a reply, a
/// reprint less than an original, and taking the flag off charges the author exactly what
/// putting it on earned. A reply is ten either way, reprint or not.
#[test]
fn the_digest_prices_a_topic_over_a_reply_and_an_original_over_a_reprint() {
    let row = |tid: i32, transship: bool| db::ExcerptionRow {
        uid: 7,
        tid,
        sid: 3,
        transship,
        excerption: false,
    };

    assert_eq!(excerption_cost(&row(0, false), true), 50);
    assert_eq!(excerption_cost(&row(0, true), true), 20);
    assert_eq!(excerption_cost(&row(40, false), true), 10);
    assert_eq!(excerption_cost(&row(40, true), true), 10);

    for tid in [0, 40] {
        for transship in [false, true] {
            let set = row(tid, transship);
            let unset = db::ExcerptionRow {
                excerption: true,
                ..set
            };
            assert_eq!(excerption_cost(&unset, false), -excerption_cost(&set, true));
        }
    }
}

/// `:215-233` and `:248-265`: the pool update takes the board lock, gives it back, and takes it
/// again to undo itself when the commit fails. `780e8ef` is the second `gives it back` — a
/// toggle the pool refused while the lock was held used to leave the board locked for every
/// session in the process. The port holds [`crate::state::AppState::board_mut`] in a scope, so
/// what is worth pinning is that a refused toggle ends the guard rather than the board.
#[tokio::test]
async fn a_refused_digest_toggle_leaves_the_board_readable() {
    use crate::board::{Board, BoardError};

    let session = detached_session();
    let mut board = Board::default();
    assert!(
        board
            .create_section(1, "test", "测试板", "master")
            .is_some()
    );
    *session.state.board_mut() = board;

    for round in 1..=2 {
        let err = session
            .state
            .board_mut()
            .set_article_excerption(1, 999, true)
            .expect_err("the pool holds no such aid");
        assert!(
            matches!(err, BoardError::NotFound),
            "round {round}: {err:?} is C's `ret = -3`"
        );
    }
    assert!(
        session.state.board().section(1).is_some(),
        "and the reader that comes after the failures still gets the pool"
    );
}

#[test]
fn a_deletable_is_copied_out_of_the_arena() {
    let article = Article {
        aid: 42,
        tid: 40,
        uid: 9,
        excerption: true,
        ..Article::default()
    };
    assert_eq!(Deletable::from(&article), target(42, 40, 9, true));
}

#[test]
fn uppercase_only_moves_the_ascii_letters() {
    assert_eq!(upper(b'y' as u32), b'Y' as u32);
    assert_eq!(upper(b'N' as u32), b'N' as u32);
    // The sentinels are above a byte, so C's `toupper` leaving them alone is what the
    // `switch` relies on.
    assert_eq!(upper(KEY_NULL), KEY_NULL);
    assert_eq!(upper(KEY_TIMEOUT), KEY_TIMEOUT);
    assert_eq!(upper(KEY_ESC), KEY_ESC);
    assert_eq!(upper(b'\r' as u32), b'\r' as u32);
}

fn detached_session() -> Session {
    let db = DbConfig {
        host: "db.example".into(),
        username: "nobody".into(),
        password: "not-a-real-secret".into(),
        database: "boarddb".into(),
        timezone: "+08:00".into(),
    };
    let config = Config {
        bbs_id: "test".into(),
        bbs_name: "MyBBS".into(),
        bbs_server: "www.bbs.test".into(),
        bbs_address: "127.0.0.1".into(),
        telnet_port: 2323,
        ssh_port: 2322,
        max_client: 10,
        max_client_per_ip: 2,
        start_dt: "2023-11-14".into(),
        sys_id: 1,
        db: db.clone(),
    };
    let state = Arc::new(AppState::new(config, crate::db::Db::detached(&db)));
    Session::new(
        state,
        Path::new("."),
        "lbbs-test".into(),
        "127.0.0.1".into(),
        2323,
        false,
    )
}

/// The window a screen must stay silent before its answer is typed. A prompt flushes and then
/// `reset_input` drains at 100 ms per poll, and anything that arrives inside a drain is
/// dropped the way C drops it, so silence is the only outward sign that a prompt is really
/// waiting for a key.
const QUIET: std::time::Duration = std::time::Duration::from_millis(250);

/// How long one step waits for its screen. Typing anyway once this passes keeps a flow that
/// has lost a key short: it still ends, and the assertion names the screen that is missing.
const PROMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Copy what the server has written into `seen`, waiting for the screen to change and then to
/// go quiet: the change is the new prompt, the quiet is its input reset finishing.
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

/// Keys typed prompt by prompt: every step waits for the screen it answers.
///
/// Typing on a schedule instead makes the result depend on how busy the machine is. An answer
/// can then land inside the drain of the prompt that follows, be dropped with C's logic, and
/// leave the flow waiting [`MAX_USER_IDLE_TIME`] for a key that already came and went.
///
/// The bytes seen while waiting come back with the pipe, because watching the screen is what
/// tells the script where it is; [`screen_text`] appends whatever arrives after that.
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

/// The one step a flow needs when it stops at a gate.
fn answer(client: Pipe, keys: &'static [u8]) -> tokio::task::JoinHandle<(Vec<u8>, Pipe)> {
    feed(client, &[keys])
}

/// Everything the session drew: what the script watched, plus what came after its last key.
fn screen_text(typed: (Vec<u8>, Pipe)) -> String {
    let (mut screen, mut client) = typed;
    screen.extend(client.take_available());
    String::from_utf8_lossy(&screen).into_owned()
}

/// A `Y` answers the gate and nothing else does — the keys C's `switch` lists, and one it
/// does not, which loops back for another try.
#[tokio::test]
async fn only_y_opens_the_gate() {
    for (keys, accepted) in [
        (&b"y"[..], true),
        (&b"Y"[..], true),
        (&b"x\r"[..], false),
        (&b"n"[..], false),
        (&b"N"[..], false),
        (&b"\x1b"[..], false),
        (&b"\r"[..], false),
    ] {
        let (client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let session = detached_session();

        let keyed = answer(client, keys);
        let asked = ask(
            &mut term,
            &session,
            "真的要删除文章？(Y)是, (N)否 [N]: ",
            b'Y' as u32,
        )
        .await;

        assert_eq!(asked, accepted, "{keys:?}");

        let screen = screen_text(keyed.await.unwrap());
        assert!(
            screen.contains("真的要删除文章？(Y)是, (N)否 [N]: "),
            "{keys:?}: {screen}"
        );
    }
}

/// A client that goes away reads as `KEY_NULL`, and C treats that as a refusal rather
/// than as an answer.
#[tokio::test]
async fn a_hung_up_reader_refuses() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let session = detached_session();

    client.hang_up();
    assert!(!ask(&mut term, &session, "prompt: ", b'Y' as u32).await);
}

/// The first branch of `article_del`: an excerpted post is refused on the screen, before
/// the confirmation is asked and before any connection is opened.
#[tokio::test]
async fn an_excerpted_post_is_refused_without_a_query() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = detached_session();

    let keyed = answer(client, b" ");
    let deleted = article_del(&mut term, &mut session, target(1, 0, 7, true))
        .await
        .expect("the refusal is not a database error");

    assert!(!deleted, "C returns 0 from this branch");
    let screen = screen_text(keyed.await.unwrap());
    assert!(
        screen.contains("该文章无法被删除，请联系版主。"),
        "{screen}"
    );
    assert!(
        !screen.contains("真的要删除文章"),
        "the gate is behind this branch: {screen}"
    );
}

/// `Ctrl-Q` shows one line and stops for a key: the article's web address, on the row C moves
/// to before it prints. The session is the detached one, so this also proves the screen reads
/// nothing from the database.
#[tokio::test]
async fn the_meta_screen_is_the_web_link_of_the_article() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let session = detached_session();

    let keyed = answer(client, b" ");
    display_article_meta(&mut term, &session, 4242).await;

    let screen = screen_text(keyed.await.unwrap());
    assert!(
        screen.contains("\x1b[2J"),
        "the screen is cleared first: {screen}"
    );
    assert!(screen.contains("\x1b[3;1HWeb版 文章链接: "), "{screen}");
    assert!(
        screen.contains("\x1b[4;1Hhttps://www.bbs.test/bbs/view_article.php?id=4242#4242"),
        "the aid is written twice, into the query and into the fragment: {screen}"
    );
    assert!(
        screen.contains("按任意键继续"),
        "the screen waits for a key: {screen}"
    );
}

fn target_article() -> Quoted {
    Quoted {
        aid: 42,
        tid: 40,
        title: "原标题".into(),
        username: "someone".into(),
        nickname: "某人".into(),
    }
}

#[test]
fn the_quoted_fields_come_from_the_arena_row() {
    let article = Article {
        aid: 42,
        tid: 40,
        title: "原标题".into(),
        username: "someone".into(),
        nickname: "某人".into(),
        ..Article::default()
    };
    assert_eq!(
        Quoted::from(&article),
        target_article(),
        "replying reads the five fields C copies and nothing else"
    );
}

/// The board's `S_POST` bit is the first question the write flows ask, and it is answered
/// out of the pools: none of them reaches the database from here.
#[tokio::test]
async fn a_board_that_forbids_posts_says_so_and_stops() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = detached_session();

    let keyed = answer(client, b" ");
    let posted = article_post(&mut term, &mut session, 3, "test", "测试版")
        .await
        .expect("the refusal is not a database error");

    assert!(!posted, "C returns 0 from this branch");
    let screen = screen_text(keyed.await.unwrap());
    assert!(screen.contains("您没有权限在本版块发表文章"), "{screen}");
}

#[tokio::test]
async fn a_reply_to_a_read_only_board_stops_at_the_same_gate() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = detached_session();

    let keyed = answer(client, b" ");
    let replied = article_reply(
        &mut term,
        &mut session,
        3,
        "test",
        "测试版",
        &target_article(),
    )
    .await
    .expect("the refusal is not a database error");

    assert!(!replied);
    let screen = screen_text(keyed.await.unwrap());
    assert!(screen.contains("您没有权限在本版块发表文章"), "{screen}");
}

/// The excerpt half of `article_modify`, in front of its `SELECT`.
#[tokio::test]
async fn an_excerpted_post_is_refused_the_same_way() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = detached_session();

    let keyed = answer(client, b" ");
    let modified = article_modify(&mut term, &mut session, 42, true)
        .await
        .expect("the refusal is not a database error");

    assert!(!modified);
    let screen = screen_text(keyed.await.unwrap());
    assert!(
        screen.contains("该文章无法被编辑，请联系版主。"),
        "{screen}"
    );
}

/// The whole of the writing half, up to the point where the answer is `C`: title prompt,
/// header commands, editor, the send question. The database is never consulted, which is
/// what makes this test possible without a server.
#[tokio::test]
async fn a_post_cancelled_after_editing_is_not_sent() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = detached_session();
    session.privs.g_priv |= S_POST;

    let keyed = feed(
        client,
        &[
            "标题\r".as_bytes(),
            "\r".as_bytes(),
            "正文\r\u{17}".as_bytes(),
            "C".as_bytes(),
            " ".as_bytes(),
        ],
    );
    let posted = article_post(&mut term, &mut session, 3, "test", "测试版")
        .await
        .expect("cancelling is not a database error");

    assert!(!posted, "a cancelled post is C's 0");
    let screen = screen_text(keyed.await.unwrap());
    // The Enter inside the editor is what flushes the typed text onto the screen: like C,
    // `Ctrl-W` returns without repainting.
    assert!(
        screen.contains("正文"),
        "the editor painted what was typed: {screen}"
    );
    assert!(screen.contains("(S)发送, (C)取消"), "{screen}");
    assert!(screen.contains("取消..."), "{screen}");
}

/// `clearscr` + the message + a key, in that order.
#[tokio::test]
async fn a_notice_is_cleared_onto_the_first_row() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));

    let keyed = answer(client, b"\r");
    notice(&mut term, "删除成功，请在1秒后刷新列表。").await;

    let screen = screen_text(keyed.await.unwrap());
    assert!(
        screen.starts_with("\x1b[2J"),
        "clearscr comes first: {screen:?}"
    );
    assert!(
        screen.contains("\x1b[1;1H删除成功，请在1秒后刷新列表。"),
        "{screen:?}"
    );
}

/// The digest ladder against the live schema, in a transaction that is dropped instead of
/// committed: `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
/// [`article_excerption_set`] commits what it writes, so its statements are run one by one
/// here. Nothing survives it: the row lock the first statement takes goes back with the
/// rollback, and the flag the last query reads is the one that was there before any of them.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn the_digest_ladder_rolls_everything_back() {
    let path = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let text = std::fs::read_to_string(path).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = crate::db::Db::connect(&config.db).await.unwrap();

    // The oldest post whose author is still a user, so the `exp` statement has a row to move:
    // C's ladder would otherwise be tested against a statement that affects nothing. The
    // join is on a primary key, so it cannot duplicate the row.
    let (aid, tid, sid, uid, transship, excerption): (i32, i32, i16, i32, bool, bool) =
        sqlx::query_as(
            "SELECT bbs.AID, TID, SID, bbs.UID, transship, excerption FROM bbs
                     INNER JOIN user_pubinfo ON user_pubinfo.UID = bbs.UID
                 WHERE visible ORDER BY bbs.AID LIMIT 1",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();

    let (before,): (Option<i32>,) = sqlx::query_as("SELECT exp FROM user_pubinfo WHERE UID = ?")
        .bind(uid)
        .fetch_one(db.pool())
        .await
        .unwrap();

    // Both directions, so the unset arm's extra statements are reached too.
    for set in [true, false] {
        let mut tx = db.begin().await.unwrap();
        let locked = db::lock_article_for_excerption(&mut tx, aid)
            .await
            .unwrap()
            .expect("the query above picked a visible article");
        assert_eq!(
            locked,
            db::ExcerptionRow {
                uid,
                tid,
                sid,
                transship,
                excerption
            },
            "the five columns decode the same way on both sides of the row lock"
        );

        db::set_excerption(&mut tx, aid, set).await.unwrap();
        if !set {
            db::clear_generated_digest(&mut tx, aid).await.unwrap();
            if tid == 0 {
                db::delete_ex_dir(&mut tx, aid).await.unwrap();
            }
        }
        db::reassign_attachments(&mut tx, aid, if set { 0 } else { uid })
            .await
            .unwrap();
        let cost = excerption_cost(&locked, set);
        // The prices, spelled out here the way C spells them inside its statement, so a
        // changed number is caught while the `exp` row is really being moved and not only by
        // the helper's own test.
        let price = match (tid == 0, transship) {
            (true, false) => 50,
            (true, true) => 20,
            (false, _) => 10,
        };
        assert_eq!(cost, if set { price } else { -price });
        db::add_exp(&mut tx, uid, cost).await.unwrap();
        db::log_article_op(&mut tx, aid, uid, if set { 'E' } else { 'O' }, "127.0.0.1")
            .await
            .unwrap();

        // The ladder really wrote the two rows it bills, while the transaction is open:
        // `exp = exp + ?` moves by exactly the price, and a NULL `exp` stays NULL the way it
        // does for C.
        let (flag,): (bool,) = sqlx::query_as("SELECT excerption FROM bbs WHERE AID = ?")
            .bind(aid)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(flag, set, "the UPDATE wrote the row the lock held");
        let (after,): (Option<i32>,) = sqlx::query_as("SELECT exp FROM user_pubinfo WHERE UID = ?")
            .bind(uid)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            after,
            before.map(|exp| exp + cost),
            "the author was billed {cost}"
        );
        drop(tx);

        let (flag,): (bool,) = sqlx::query_as("SELECT excerption FROM bbs WHERE AID = ?")
            .bind(aid)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(
            flag, excerption,
            "the dropped transaction took the flag back"
        );
    }
}
