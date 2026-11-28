use super::*;
use crate::bwf::Badwords;
use crate::term::{ByteIo, Pipe};
use std::time::Duration;

/// The editor's leave key, which is what the answer line after it is read with.
const CTRL_W: u8 = 0x17;

/// The round run on its own, which is the only way to reach it without a database: both
/// callers stop at their first query on a session that has none.
async fn round(initial: &[u8], script: &[&[u8]]) -> (Outcome, String) {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    let mut data = EditorData::load(initial, term.fixed_width());

    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            edit_round(
                &mut term,
                &mut session,
                &mut data,
                USER_INTRO_MAX_LEN,
                USER_INTRO_MAX_LINE,
                "说明档"
            ),
            async {
                for step in script {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    client.push(step);
                }
            }
        )
    })
    .await
    .expect("the loop ends on its keys")
    .0;

    let _ = term.flush().await;
    (
        outcome,
        String::from_utf8_lossy(&client.take_available()).into_owned(),
    )
}

fn count(screen: &str, needle: &str) -> usize {
    screen.match_indices(needle).count()
}

fn text(outcome: &Outcome) -> Vec<u8> {
    match outcome {
        Outcome::Update(saved) => saved.clone(),
        Outcome::Cleanup(ret) => panic!("the loop cleaned up with {ret}"),
    }
}

/// `(C)` is the `goto cleanup` of `:138-143` that leaves `ret` at zero: no update, and the
/// text the editor holds is never even saved out of it.
#[tokio::test]
async fn cancelling_leaves_the_text_unsaved() {
    let (outcome, screen) = round("欢迎回来\n".as_bytes(), &[&[CTRL_W], b"C", b" "]).await;

    assert_eq!(outcome, Outcome::Cleanup(0));
    assert!(
        screen.contains("(S)保存, (C)取消 or (E)再编辑? [S]: "),
        "{screen}"
    );
    assert!(screen.contains("取消更改..."), "{screen}");
}

/// `CR` and `(S)` are one case of the switch (`:107-108`), and an answer that passes the line
/// count is the `break` C follows with its update — with the text as the editor holds it.
#[tokio::test]
async fn an_answer_of_save_hands_the_text_to_the_update() {
    let (outcome, screen) = round("欢迎回来\n".as_bytes(), &[&[CTRL_W], b"S"]).await;

    assert_eq!(text(&outcome), "欢迎回来\n".as_bytes());
    // The question is the last thing the loop draws; the closing notice is the caller's.
    assert_eq!(count(&screen, "再编辑? [S]: "), 1, "{screen}");
}

/// A key that is none of the four (`:147-148`) redraws the same question, and `(E)` goes back
/// through the editor (`:144-146`) before it asks again.
#[tokio::test]
async fn an_invalid_answer_asks_the_same_question_again() {
    let (outcome, screen) = round(
        "欢迎回来\n".as_bytes(),
        &[&[CTRL_W], b"X", b"E", &[CTRL_W], b"C", b" "],
    )
    .await;

    assert_eq!(outcome, Outcome::Cleanup(0));
    // Once for the round that left the editor, once for the invalid key, once after `(E)`.
    assert_eq!(
        count(&screen, "(S)保存, (C)取消 or (E)再编辑? [S]: "),
        3,
        "{screen}"
    );
}

/// Eleven display lines are one more than the ten the screen allows, and the `ch = 'E'` of
/// `:134-135` sends the reader back to the editor rather than out to the update.
#[tokio::test]
async fn too_many_lines_returns_to_the_editor() {
    let long = "行1234567890\n".repeat(11);
    let (outcome, screen) = round(
        long.as_bytes(),
        &[&[CTRL_W], b"S", b" ", &[CTRL_W], b"C", b" "],
    )
    .await;

    assert_eq!(outcome, Outcome::Cleanup(0));
    assert!(
        screen.contains("说明档长度超过限制 (10行)，请返回修改"),
        "{screen}"
    );
    // The rejected round still asked the question twice: the loop went back through the
    // editor instead of breaking out of it.
    assert_eq!(count(&screen, "再编辑? [S]: "), 2, "{screen}");
}

/// Nine lines are what the ten allowed are spent on: the do/while of `split_data_lines`
/// (`str_process.c:190`) yields a display line *after* the closing newline too, so an editor
/// buffer of nine newline-terminated lines counts as ten and is the longest save C accepts.
#[tokio::test]
async fn nine_lines_are_within_the_limit() {
    let nine = "行1234567890\n".repeat(9);
    let (outcome, screen) = round(nine.as_bytes(), &[&[CTRL_W], b"S"]).await;

    assert_eq!(text(&outcome), nine.as_bytes());
    assert!(!screen.contains("超过限制"), "{screen}");
}

/// The tenth line closes the text with the newline that gives the eleventh, which `:127` and
/// `:302` count against their limit of ten.
#[tokio::test]
async fn a_tenth_line_is_the_empty_one_after_it() {
    let ten = "行1234567890\n".repeat(10);
    let (outcome, screen) = round(
        ten.as_bytes(),
        &[&[CTRL_W], b"S", b" ", &[CTRL_W], b"C", b" "],
    )
    .await;

    assert_eq!(outcome, Outcome::Cleanup(0));
    assert!(
        screen.contains("说明档长度超过限制 (10行)，请返回修改"),
        "{screen}"
    );
}

/// `data_file_mgr.c:201` was the model for the guard `user_info_update.c:154-157` now has of
/// its own. The loop itself still hands the last saved text out — the rejected over-length
/// one, here — and it is the caller that no longer believes it.
#[tokio::test]
async fn a_shutdown_between_rounds_hands_the_last_text_to_the_caller() {
    let ten = "行1234567890\n".repeat(10);
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    let state = session.state.clone();
    let mut data = EditorData::load(ten.as_bytes(), term.fixed_width());

    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            edit_round(
                &mut term,
                &mut session,
                &mut data,
                USER_INTRO_MAX_LEN,
                USER_INTRO_MAX_LINE,
                "说明档"
            ),
            async {
                // Asked for before the first round ends, and the loop only looks at it
                // again when a `continue` takes it back to the top.
                state.exit();
                for step in [&[CTRL_W][..], &b"S"[..], &b" "[..]] {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    client.push(step);
                }
            }
        )
    })
    .await
    .expect("the loop ends on its keys")
    .0;

    assert_eq!(outcome, Outcome::Update(ten.as_bytes().to_vec()));
}

/// The same fall-out with nothing behind it: C's `intro` was never written to, so the loop
/// reports an empty save. `:154-157` is what keeps that empty text out of the column.
#[tokio::test]
async fn a_shutdown_before_the_first_round_saves_nothing() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    session.state.exit();
    let mut data = EditorData::load("欢迎回来\n".as_bytes(), term.fixed_width());

    let outcome = edit_round(
        &mut term,
        &mut session,
        &mut data,
        USER_INTRO_MAX_LEN,
        USER_INTRO_MAX_LINE,
        "说明档",
    )
    .await;
    drop(client);

    assert_eq!(outcome, Outcome::Update(Vec::new()));
    assert_eq!(
        kept_for_writing(&session, Vec::new()),
        None,
        "and the caller drops it"
    );
}

/// The two callers' one shared line of arithmetic: while the daemon is running the saved
/// text reaches the `UPDATE`, and once it is going down nothing does.
#[tokio::test]
async fn only_a_running_daemon_writes_the_text() {
    let session = crate::session::offline_session();
    assert_eq!(
        kept_for_writing(&session, b"line one\n".to_vec()).as_deref(),
        Some(&b"line one\n"[..])
    );
    session.state.exit();
    assert_eq!(kept_for_writing(&session, b"line one\n".to_vec()), None);
}

/// The save buffer of `:109` is the same 4 KB as the column, and a text past it is the `-3`
/// of `:110-115`: the length of the text is what fails, not the number of lines it makes.
#[tokio::test]
async fn a_text_past_the_save_buffer_gives_up() {
    let long = "x\n".repeat(2049);
    let (outcome, screen) = round(long.as_bytes(), &[&[CTRL_W], b"S"]).await;

    assert_eq!(outcome, Outcome::Cleanup(-3));
    assert!(!screen.contains("超过限制"), "{screen}");
}

/// `check_badwords(intro, '*')` (`:117`) masks the buffer the update reads from, so the
/// column gets the censored text rather than what was typed.
#[tokio::test]
async fn the_saved_text_is_the_masked_one() {
    let dict = std::env::temp_dir().join(format!("lbbs-infoupd-bwf-{}", std::process::id()));
    std::fs::write(&dict, "敏感\n").unwrap();

    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    session
        .state
        .publish_badwords(Badwords::load(&dict).unwrap());
    let mut data = EditorData::load("这里有敏感词\n".as_bytes(), term.fixed_width());

    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(
            edit_round(
                &mut term,
                &mut session,
                &mut data,
                USER_INTRO_MAX_LEN,
                USER_INTRO_MAX_LINE,
                "说明档"
            ),
            async {
                for step in [&[CTRL_W][..], &b"S"[..]] {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    client.push(step);
                }
            }
        )
    })
    .await
    .expect("the loop ends on its keys")
    .0;

    assert_eq!(text(&outcome), "这里有******词\n".as_bytes());
    std::fs::remove_file(&dict).unwrap();
}

/// The number gate of `user_sign_edit` (`:218-225`) stands in front of every query, which is
/// what makes the two negative returns tellable apart on a session with no database: `-1` is
/// the number, `-2` the query.
#[tokio::test]
async fn a_number_and_a_slot_leave_the_screen_by_different_doors() {
    for (typed, expected) in [
        ("0\r", -1),
        ("4\r", -1),
        ("\x1b", -1),
        ("2\r", -2),
        ("3\r", -2),
    ] {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        let ret = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(user_sign_edit(&mut term, &mut session), async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(typed.as_bytes());
            })
        })
        .await
        .expect("the prompt ends on its answer")
        .0;

        assert_eq!(ret, expected, "{typed:?}");
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(
            screen.contains("请输入需要编辑的签名档编号（1-3）: "),
            "{screen}"
        );
        assert!(!screen.contains("再编辑"), "{screen}");
    }
}

/// `user_intro_edit` has no prompt of its own, so a session with nothing behind it is out at
/// the first statement — before the editor screen is ever drawn.
#[tokio::test]
async fn the_introduction_screen_starts_at_its_query() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let ret = tokio::time::timeout(
        Duration::from_secs(15),
        user_intro_edit(&mut term, &mut session),
    )
    .await
    .expect("the query gives up on its own");

    assert_eq!(ret, -2);
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(!screen.contains("再编辑"), "{screen}");
}

/// `:220-225` reads the number with `atoi`, so anything that is not one of three names no
/// slot: the empty answer a timeout leaves, a negative, and a number past the list.
#[test]
fn only_three_numbers_are_slots() {
    assert_eq!(SignSlot::from_index(c_atoi("1")), Some(SignSlot::One));
    assert_eq!(SignSlot::from_index(c_atoi("2")), Some(SignSlot::Two));
    assert_eq!(SignSlot::from_index(c_atoi("3")), Some(SignSlot::Three));
    for typed in ["", "0", "4", "-1", "x", "12"] {
        assert_eq!(SignSlot::from_index(c_atoi(typed)), None, "{typed:?}");
    }
}
