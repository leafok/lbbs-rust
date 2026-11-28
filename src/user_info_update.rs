/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The two texts a reader edits about themselves: the introduction and one of three signatures.
//!
//! Port of `src/user_info_update.c` — `user_intro_edit` (`:38-199`) and `user_sign_edit`
//! (`:201-374`) — reached from the toolbox of the top menu (`conf/menu.conf:155-156`), the only
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
//! * The two connections C opens per save (`:53`, `:159`) and the `goto cleanup` ladder between
//!   them become one method each on [`crate::db::Db`] and a `match` on its `Result`; a pooled
//!   connection is not closed on the way out.
//! * C splices the text in after `mysql_real_escape_string` (`:168`, `:343`); the port binds it,
//!   which lands the same bytes in the column. The `sign_%d` name is the part that cannot be
//!   bound, and [`crate::db::SignSlot`] is what keeps it to three spelled-out statements.
//! * `check_badwords` returning negative is C's `BWF not loaded` (`bwf.c:141-145`) or an offset
//!   vector too small for one match (`bwf.c:167-171`); the port's mask answers with a count and
//!   cannot fail, so `:117-122` and `:292-297` have no counterpart. A site with no patterns is
//!   not that case: `bwf_compile` compiles the empty pattern string (`bwf.c:112`) rather than
//!   leaving it unset, and C counts zero matches.
//! * `lml_render`'s negative return is the one given up on in [`crate::lml::render`], so C's
//!   unchecked calls at `:124` and `:299` are plain bindings here.
//! * The over-length test (`:126-136`, `:301-311`) hands the rendered text to `split_data_lines`
//!   with `BBS_user_*_max_line + 2` slots — one more display line than it is willing to count —
//!   and then looks only at whether the count it gets back is above ten. [`edit_round`] asks for
//!   the same two numbers for the same reason, including what that count is made of: the
//!   do/while of `str_process.c:190` yields a line after a closing newline, so ten
//!   newline-terminated lines are eleven and the limit admits nine.
//! * Both functions now have `data_file_mgr.c:201`'s guard (`:154-157`, `:329-332`): a loop the
//!   shutdown ended reaches `cleanup` with `ret` still `0` and no `UPDATE` at all. The text that
//!   the last `editor_data_save` filled in is dropped with the editor, which is why
//!   [`Outcome::Update`] still covers both ways out of the loop and the callers look at
//!   [`crate::session::AppState::is_exiting`] before they believe it.

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
    /// C's `break` (`:151`, `:326`), which carries the text `editor_data_save` filled in. The
    /// fall-out of a loop that the shutdown ended before its body ran again is this too, and
    /// then it carries that round's leftover.
    Update(Vec<u8>),
    /// A `goto cleanup`: `0` for the key that timed out and for `(C)`, `-3` for the text that
    /// did not fit the save buffer.
    Cleanup(i32),
}

/// `user_intro_edit` (`:38-199`). The return is C's `ret`, because `menu_proc.c:404` tests it for
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

    // `:154-157`: the shutdown that ended the loop is not an answer. C goes to `cleanup` with
    // the `ret` it was given at the top of the function, and the text goes with it.
    let Some(saved) = kept_for_writing(session, saved) else {
        return 0;
    };

    // `:159-183`.
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

/// `user_sign_edit` (`:201-374`): the same round, with a number in front of it.
pub async fn user_sign_edit(term: &mut Terminal, session: &mut Session) -> i32 {
    let uid = session.privs.uid;

    // `:218-225`.
    screen::clearscr(term);
    // `get_data(1, 1, prompt, buf, sizeof(buf), 1)` (`:219`): one display cell out of a two-byte
    // buffer. C throws the return code away, so a timeout is answered with the empty `buf` the
    // stack had and `atoi` (`:220`) reads that as no slot at all.
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

    // `:329-332`.
    let Some(saved) = kept_for_writing(session, saved) else {
        return 0;
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

/// `:154-157` and `:329-332`: on the way out the daemon takes the text back. `None` is the
/// `goto cleanup` that answers a shutdown with no `UPDATE` and no notice.
fn kept_for_writing(session: &Session, saved: Vec<u8>) -> Option<Vec<u8>> {
    (!session.state.is_exiting()).then_some(saved)
}

/// The `for (ch = 'E'; !SYS_server_exit;)` of `:89-152`, which `:264-327` repeats word for word
/// apart from the two numbers and the word in the over-length message. `noun` is that word.
async fn edit_round(
    term: &mut Terminal,
    session: &mut Session,
    data: &mut EditorData,
    max_len: usize,
    max_lines: usize,
    noun: &str,
) -> Outcome {
    // `intro` (`:47`) and `sign` (`:210`): buffers C leaves uninitialised, which is what an
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
                    // `:110-115`, `:285-290`: the text outgrew the save buffer.
                    tracing::error!("editor_data_save() error");
                    return Outcome::Cleanup(-3);
                };
                session.state.badwords().mask(&mut bytes, b'*');
                saved = bytes;

                // `lml_render(intro, intro_f, sizeof(intro_f), SCREEN_COLS, 0)` (`:124`, `:299`),
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
            // `default: // Invalid selection` (`:147-148`, `:322-323`).
            _ => continue,
        }

        break;
    }

    Outcome::Update(saved)
}

/// `clearscr(); moveto(1, 1); prints(message); press_any_key();` — the notice of `:129-132`, the
/// two of `:139-142` and `:304-307`, and the closing line of `:185-188` and `:360-363`.
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
#[path = "../tests/test_user_info_update.rs"]
mod tests;
