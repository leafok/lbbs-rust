/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The two texts a reader edits about themselves: the introduction and one of three signatures.
//!
//! Port of `src/user_info_update.c` — `user_intro_edit` (`:38-194`) and `user_sign_edit`
//! (`:196-364`) — reached from the toolbox of the top menu (`conf/menu.conf:155-156`), the only
//! two items there that write to the database. C calls them from `menu_proc.c:402-420`, which
//! logs a negative return and redraws the menu whichever way they go; [`crate::bbs`] does the
//! same.
//!
//! Both screens are the editor of [`crate::editor`] over one `user_pubinfo` column answering the
//! same `(S)保存, (C)取消 or (E)再编辑` question, so [`edit_round`] is that loop once. What sets
//! them apart is what precedes it — the signature screen asks which of the three slots first —
//! the word in the message a too-long text gets, and the closing line.
//!
//! Both save what the badword filter left behind: `check_badwords` masks the buffer in place
//! (`bwf.c:131-195`) before anything is measured or stored, so a reader cannot put a censored
//! word into their own introduction through this screen. The mask is `memset` over the matched
//! byte range, one `*` per byte rather than per character.
//!
//! # Deviations from C
//!
//! * The two connections C opens per save (`:53`, `:154`) and the `goto cleanup` ladder between
//!   them become one method each on [`crate::db::Db`] and a `match` on its `Result`; a pooled
//!   connection is not closed on the way out.
//! * C splices the text in after `mysql_real_escape_string` (`:163`, `:333`); the port binds it,
//!   which lands the same bytes in the column. The `sign_%d` name is the part that cannot be
//!   bound, and [`crate::db::SignSlot`] is what keeps it to three spelled-out statements.
//! * `check_badwords` returning negative is C's `BWF not loaded` (`bwf.c:141-145`) or an offset
//!   vector too small for one match (`bwf.c:167-171`); the port's mask answers with a count and
//!   cannot fail, so `:117-122` and `:287-292` have no counterpart. A site with no patterns is
//!   not that case: `bwf_compile` compiles the empty pattern string (`bwf.c:112`) rather than
//!   leaving it unset, and C counts zero matches.
//! * `lml_render`'s negative return is the one given up on in [`crate::lml::render`], so C's
//!   unchecked calls at `:124` and `:294` are plain bindings here.
//! * The over-length test (`:126-136`, `:296-306`) hands the rendered text to `split_data_lines`
//!   with `BBS_user_*_max_line + 2` slots — one more display line than it is willing to count —
//!   and then looks only at whether the count it gets back is above ten. [`edit_round`] asks for
//!   the same two numbers for the same reason, including what that count is made of: the
//!   do/while of `str_process.c:190` yields a line after a closing newline, so ten
//!   newline-terminated lines are eleven and the limit admits nine.
//! * Neither function has `data_file_mgr.c:201`'s guard: falling out of the `for` because the
//!   daemon is going down still reaches the `UPDATE`, with whatever the last save left in the
//!   buffer and with an empty one if there was no such round. The port reproduces that, which is
//!   why [`Outcome::Update`] covers both ways out.

use crate::config::c_atoi;
use crate::db::SignSlot;
use crate::editor::{self, EditorData};
use crate::loader::USER_LIST_INTERVAL;
use crate::paths::{MAX_USER_IDLE_TIME, SCREEN_COLS, USER_INTRO_MAX_LEN, USER_INTRO_MAX_LINE};
use crate::screen;
use crate::session::Session;
use crate::term::Terminal;
use crate::term::key::{KEY_NULL, KEY_TIMEOUT, to_upper};
use crate::{db, lml, text};

/// `BBS_user_sign_max_len` and `BBS_user_sign_max_line` (`:33-34`). The introduction's two live
/// in [`crate::paths`] beside the rest of `include/user_list.h`, and `BBS_user_sign_cnt` (`:35`)
/// is the third arm of [`SignSlot::from_index`].
const USER_SIGN_MAX_LEN: usize = 4096;
const USER_SIGN_MAX_LINE: usize = 10;

/// The command letters of the answer, which like [`KEY_CR`] cannot be written as byte patterns
/// against a `u32` key code.
const KEY_CR: u32 = b'\r' as u32;
const KEY_C: u32 = b'C' as u32;
const KEY_E: u32 = b'E' as u32;
const KEY_S: u32 = b'S' as u32;

/// How the editor loop of `:89-152` ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// C's `break` (`:151`, `:321`), which carries the text `editor_data_save` filled in. The
    /// fall-out of a loop that the shutdown ended before its body ran again is this too, and
    /// then it carries that round's leftover.
    Update(Vec<u8>),
    /// A `goto cleanup`: `0` for the key that timed out and for `(C)`, `-3` for the text that
    /// did not fit the save buffer.
    Cleanup(i32),
}

/// `user_intro_edit` (`:38-194`). The return is C's `ret`, because `menu_proc.c:404` tests it for
/// its `log_error`; the menu is redrawn either way.
pub async fn user_intro_edit(term: &mut Terminal, session: &mut Session) -> i32 {
    let uid = session.privs.uid;

    // `:53-87`: the query, the fetch, and the `row[0] ? row[0] : ""` of `:76`. No row at all is
    // C's `mysql_fetch_row() failed` (`:79-83`), a failed statement its `Query user_pubinfo
    // error` (`:62-67`), and `mysql_store_result` returning NULL (`:68-73`) is a third thing a
    // pool cannot answer separately.
    let stored = match session.state.db.introduction(uid).await {
        Ok(Some(cell)) => cell.unwrap_or_default(),
        Ok(None) => {
            tracing::error!("mysql_fetch_row() failed");
            return -2;
        }
        Err(err) => {
            tracing::error!("Query user_pubinfo error: {err}");
            return -2;
        }
    };
    let mut data = EditorData::load(stored.as_bytes(), term.fixed_width());

    let saved = match edit_round(
        term,
        session,
        &mut data,
        USER_INTRO_MAX_LEN,
        USER_INTRO_MAX_LINE,
        "说明档",
    )
    .await
    {
        Outcome::Update(saved) => saved,
        Outcome::Cleanup(ret) => return ret,
    };

    // `:154-178`.
    if let Err(err) =
        db::update_introduction(&session.state.db, uid, &String::from_utf8_lossy(&saved)).await
    {
        tracing::error!("Update user_pubinfo error: {err}");
        return -2;
    }

    notice(
        term,
        &format!(
            "说明档修改完成，会在{}秒内生效",
            USER_LIST_INTERVAL.as_secs()
        ),
    )
    .await;
    1
}

/// `user_sign_edit` (`:196-364`): the same round, with a number in front of it.
pub async fn user_sign_edit(term: &mut Terminal, session: &mut Session) -> i32 {
    let uid = session.privs.uid;

    // `:213-220`.
    screen::clearscr(term);
    // `get_data(1, 1, prompt, buf, sizeof(buf), 1)` (`:214`): one display cell out of a two-byte
    // buffer. C throws the return code away, so a timeout is answered with the empty `buf` the
    // stack had and `atoi` (`:215`) reads that as no slot at all.
    let typed = screen::get_data(
        term,
        1,
        1,
        "请输入需要编辑的签名档编号（1-3）: ",
        b"",
        2,
        1,
        MAX_USER_IDLE_TIME,
    )
    .await
    .unwrap_or_default();
    let Some(slot) = SignSlot::from_index(c_atoi(&String::from_utf8_lossy(&typed))) else {
        // C logs nothing of its own for a number that is not a slot; the caller's
        // `user_sign_edit(%d) error` is the only line it leaves behind.
        return -1;
    };

    let stored = match session.state.db.signature_row(uid, slot).await {
        Ok(Some(cell)) => cell.unwrap_or_default(),
        Ok(None) => {
            tracing::error!("mysql_fetch_row() failed");
            return -2;
        }
        Err(err) => {
            tracing::error!("Query user_pubinfo error: {err}");
            return -2;
        }
    };
    let mut data = EditorData::load(stored.as_bytes(), term.fixed_width());

    let saved = match edit_round(
        term,
        session,
        &mut data,
        USER_SIGN_MAX_LEN,
        USER_SIGN_MAX_LINE,
        "签名档",
    )
    .await
    {
        Outcome::Update(saved) => saved,
        Outcome::Cleanup(ret) => return ret,
    };

    if let Err(err) = db::update_signature(
        &session.state.db,
        uid,
        slot,
        &String::from_utf8_lossy(&saved),
    )
    .await
    {
        tracing::error!("Update user_pubinfo error: {err}");
        return -2;
    }

    notice(term, "签名档修改完成").await;
    1
}

/// The `for (ch = 'E'; !SYS_server_exit;)` of `:89-152`, which `:259-322` repeats word for word
/// apart from the two numbers and the word in the over-length message. `noun` is that word.
async fn edit_round(
    term: &mut Terminal,
    session: &mut Session,
    data: &mut EditorData,
    max_len: usize,
    max_lines: usize,
    noun: &str,
) -> Outcome {
    // `intro` (`:47`) and `sign` (`:205`): buffers C leaves uninitialised, which is what an
    // update forced out by the shutdown writes from.
    let mut saved: Vec<u8> = Vec::new();
    let mut ch: u32 = KEY_E;

    while !session.state.is_exiting() {
        if ch == KEY_E {
            editor::editor_display(term, session, data).await;
        }

        screen::clearscr(term);
        term.moveto(1, 1);
        term.print("(S)保存, (C)取消 or (E)再编辑? [S]: ");
        if let Err(err) = term.flush().await {
            tracing::error!("io_flush() error: {err}");
        }

        ch = term.read_key_wait(MAX_USER_IDLE_TIME).await;
        match to_upper(ch) {
            KEY_NULL | KEY_TIMEOUT => return Outcome::Cleanup(0),
            KEY_CR | KEY_S => {
                let Some(mut bytes) = data.save(max_len) else {
                    // `:110-115`, `:280-285`: the text outgrew the save buffer.
                    tracing::error!("editor_data_save() error");
                    return Outcome::Cleanup(-3);
                };
                session.state.badwords().mask(&mut bytes, b'*');
                saved = bytes;

                // `lml_render(intro, intro_f, sizeof(intro_f), SCREEN_COLS, 0)` (`:124`, `:294`),
                // whose output buffer is twice the save buffer plus the byte C ends it with.
                let rendered = lml::render(
                    &saved,
                    max_len * 2 + 1,
                    SCREEN_COLS as i64,
                    false,
                    term.fixed_width(),
                );

                if text::split_data_lines(
                    &rendered,
                    SCREEN_COLS + 1,
                    true,
                    term.fixed_width(),
                    max_lines + 2,
                )
                .count()
                    > max_lines
                {
                    notice(
                        term,
                        &format!("{noun}长度超过限制 ({max_lines}行)，请返回修改"),
                    )
                    .await;
                    ch = KEY_E;
                    continue;
                }
            }
            KEY_C => {
                notice(term, "取消更改...").await;
                return Outcome::Cleanup(0);
            }
            KEY_E => {
                ch = KEY_E;
                continue;
            }
            // `default: // Invalid selection` (`:147-148`, `:317-318`).
            _ => continue,
        }

        break;
    }

    Outcome::Update(saved)
}

/// `clearscr(); moveto(1, 1); prints(message); press_any_key();` — the notice of `:129-132`, the
/// two of `:139-142` and `:299-302`, and the closing line of `:180-183` and `:350-353`.
async fn notice(term: &mut Terminal, message: &str) {
    screen::clearscr(term);
    term.moveto(1, 1);
    term.print(message);
    if let Err(err) = term.flush().await {
        tracing::error!("io_flush() error: {err}");
    }
    screen::press_any_key(term).await;
}

#[cfg(test)]
mod tests {
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
    /// `:297` count against their limit of ten.
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

    /// `data_file_mgr.c:201` has a `if (SYS_server_exit) goto cleanup;` before its writes and
    /// `user_info_update.c` has none: a loop the shutdown ends between two rounds still reaches
    /// its `UPDATE`, with the text the last save filled — the rejected over-length one, here.
    #[tokio::test]
    async fn a_shutdown_between_rounds_still_writes_the_last_text() {
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

    /// The same fall-out with nothing behind it is C's read of an uninitialised `intro`, whose
    /// escaped length is zero: the update carries no text, which is why the port starts from an
    /// empty `Vec` rather than refusing to write.
    #[tokio::test]
    async fn a_shutdown_before_the_first_round_writes_an_empty_text() {
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

    /// The number gate of `user_sign_edit` (`:213-220`) stands in front of every query, which is
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

    /// `:215-220` reads the number with `atoi`, so anything that is not one of three names no
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
}
