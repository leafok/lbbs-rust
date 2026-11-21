//! The write path of a board — a port of `src/article_del.c`, `src/article_post.c` and the one
//! article operation of `src/article_op.c`.
//!
//! Both files split the same way: a screen half that asks questions, and a SQL half that runs
//! one transaction. The transaction stands in for C's `SET autocommit=0` / `BEGIN` / `COMMIT`
//! ladder and behaves the same: an error that leaves it uncommitted rolls back, which is what
//! C's `goto cleanup` reaching `mysql_close` did.
//!
//! Writing a post leaves the in-memory pools alone, because C let them be: a new article becomes
//! visible when [`crate::loader`] next reads it out of the database, which is what the
//! "新文章通常会在%d秒后可见" message promises. The digest flag is the exception — C set it on the
//! shared list in the same breath as the `UPDATE`, so [`article_excerption_set`] does too.
//!
//! # Deviations from C
//!
//! * The arena fields `article_del` needs are copied into [`Deletable`] by the caller: a board
//!   guard cannot be held across the `.await`s of the transaction.
//! * `article_modify` sends its `UPDATE bbs SET CID = …` twice (`article_post.c:701` re-runs the same `sql`
//!   under an "Update content error" label). The port sends it once — the statement is
//!   idempotent and the second copy is a leftover.
//! * Titles and bodies are bound as SQL parameters instead of going through
//!   `mysql_real_escape_string`, so a quote in a title cannot change the statement.
//! * `time(&now)` + `localtime_r` for the modification stamp uses the session's zone the same
//!   way C uses the server's; both format through `strftime`, so the string is the host's.
//! * The footers are measured with what `snprintf` *would* have written. `article_modify` still
//!   appends its stamp into a fixed `LINE_BUFFER_LEN` (`article_post.c:617`) and adds the return value to
//!   `len_content`, so a stamp cut short leaves the length past the body; `article_post` (`:276`)
//!   and `article_reply` (`:1149`) size the write to the room that is left, and a2f6c10 clamps
//!   their counter back to the buffer. The port hands each footer the room that is actually left.
//! * `get_data` returns `-1` when the reader goes away, and the title trimming that follows it
//!   writes its terminator at `title_input[-1]` (`article_post.c:127`). The port returns from the flow.
//! * The notification a reply sends quotes `title_f` (`article_post.c:1295`), which is the SQL-escaped title
//!   the `INSERT` used at `:1230`, and then escapes the whole message again — a `"` in the
//!   subject arrives as `\"`. The port binds the title once, so the message says what the
//!   article is called.
//! * `article_post` and `article_modify` return C's `-1` (an allocation that did not happen, a
//!   body that did not fit the buffer) as `Ok(false)`: the callers log the difference and the
//!   screen is the same either way.

use crate::board::Article;
use crate::db;
use crate::editor;
use crate::lml;
use crate::loader::SECTION_LIST_INTERVAL;
use crate::paths::{LINE_BUFFER_LEN, MAX_USER_IDLE_TIME};
use crate::privilege::{S_GETEXP, S_POST};
use crate::screen;
use crate::session::Session;
use crate::term::key::{KEY_ESC, KEY_NULL, KEY_TIMEOUT};
use crate::term::Terminal;
use crate::text;

/// The four arena fields `article_del` reads (`:40`, `:155`, `:166-173`, `:191`, `:202`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deletable {
    pub aid: i32,
    pub tid: i32,
    pub uid: i32,
    pub excerption: bool,
}

impl From<&Article> for Deletable {
    fn from(article: &Article) -> Self {
        Deletable {
            aid: article.aid,
            tid: article.tid,
            uid: article.uid,
            excerption: article.excerption,
        }
    }
}

/// The three figures a delete derives from "who deletes what": `m_del`, the experience delta,
/// and the `bbs_article_op` letter (`:155`, `:166-173`, `:191`). An author pays 20 for a root
/// and 5 for a reply; a moderator takes 50 and 15 and leaves `m_del = 1` behind.
pub fn delete_cost(target: &Deletable, uid: i32) -> (i32, i32, char) {
    let own = target.uid == uid;
    let exp_change = match (own, target.tid == 0) {
        (true, true) => -20,
        (true, false) => -5,
        (false, true) => -50,
        (false, false) => -15,
    };

    (i32::from(!own), exp_change, if own { 'D' } else { 'X' })
}

/// `article_del`. `Ok(true)` is C's `ret = 1`; `Ok(false)` is any path that left the database
/// alone, which is what C returns `0` for.
pub async fn article_del(
    term: &mut Terminal,
    session: &mut Session,
    target: Deletable,
) -> Result<bool, sqlx::Error> {
    if target.excerption {
        notice(term, "该文章无法被删除，请联系版主。").await;
        return Ok(false);
    }

    if !ask(term, session, "真的要删除文章？(Y)是, (N)否 [N]: ", b'Y' as u32).await {
        return Ok(false);
    }

    let (m_del, exp_change, op_kind) = delete_cost(&target, session.privs.uid);
    let mut tx = session.state.db.begin().await?;

    let status = db::lock_article_status(&mut tx, target.aid).await?;
    // C fetches one row and leaves both flags at their initialised 0 when there is none, so an
    // article that vanished since the list was drawn reads as already deleted.
    let (visible, excerption) = match status {
        Some(row) => (row.visible, row.excerption),
        None => (false, false),
    };

    if !visible {
        drop(tx);
        notice(term, "该文章已被删除，请稍后刷新列表。").await;
        return Ok(false);
    }

    if excerption {
        drop(tx);
        notice(term, "该文章无法被删除，请联系版主。").await;
        return Ok(false);
    }

    db::hide_thread(&mut tx, target.aid, m_del).await?;
    db::add_exp(&mut tx, session.privs.uid, exp_change).await?;
    db::log_article_op(&mut tx, target.aid, session.privs.uid, op_kind, &session.hostaddr).await?;

    if target.tid != 0 {
        db::dec_reply_count(&mut tx, target.tid).await?;
    }

    tx.commit().await?;

    notice(
        term,
        &format!("删除成功，请在{}秒后刷新列表。", SECTION_LIST_INTERVAL.as_secs()),
    )
    .await;

    Ok(true)
}

/// The experience a digest change bills, read off the row the transaction holds
/// (`article_op.c:179-192`): fifty for an original topic, twenty for a reprinted one, ten for a
/// reply, all of it taken back at the same rate when the flag comes off. The author earns or pays
/// it; the moderator who pressed the key is only in the audit row.
pub fn excerption_cost(row: &db::ExcerptionRow, set: bool) -> i32 {
    let base = match (row.tid == 0, row.transship) {
        (true, false) => 50,
        (true, true) => 20,
        (false, _) => 10,
    };

    if set { base } else { -base }
}

/// `article_excerption_set` (`article_op.c:34-269`): put an article into the 精华 digest, or take
/// it back out. The caller hands over the toggle it read off the list, which is what C's `m`
/// handler does with `p_articles[selected_index]->excerption`.
///
/// Nothing is drawn. C runs this between two repaints of the list, and the marker on the row is
/// the whole response — including the silence of an article that already carried the flag.
///
/// `Ok(true)` is C's `ret = 1`: the database now says what was asked. `Ok(false)` is any path
/// that left it alone — C's `-1`, `-2` and `-3`, and the failed `COMMIT` that C rolls back and
/// reports as `0` — each of which logs C's own message on the way out.
///
/// This is the one write path here that touches the in-memory pools, because C's did:
/// `section_list_set_article_excerption` ran on the shared list under its rw lock, so the marker
/// was there on the next draw rather than at the next load pass. The write guard of
/// [`crate::state::AppState::board_mut`] is that critical section, and the loader's `E`/`O` replay
/// of the audit row this transaction inserts then finds the flag already in place and changes
/// nothing.
pub async fn article_excerption_set(
    session: &Session,
    sid: i16,
    aid: i32,
    set: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = session.state.db.begin().await?;

    let Some(row) = db::lock_article_for_excerption(&mut tx, aid).await? else {
        tracing::error!("Article not found: aid={aid}");
        return Ok(false);
    };

    if row.sid != sid {
        tracing::error!("Inconsistent SID of article (aid={aid}): {sid}!={}", row.sid);
        return Ok(false);
    }

    if row.excerption != set {
        db::set_excerption(&mut tx, aid, set).await?;

        if !set {
            db::clear_generated_digest(&mut tx, aid).await?;

            if row.tid == 0 {
                db::delete_ex_dir(&mut tx, aid).await?;
            }
        }

        db::reassign_attachments(&mut tx, aid, if set { 0 } else { row.uid }).await?;

        if session.privs.checkpriv(sid, S_GETEXP) {
            db::add_exp(&mut tx, row.uid, excerption_cost(&row, set)).await?;
        }

        db::log_article_op(
            &mut tx,
            aid,
            session.privs.uid,
            if set { 'E' } else { 'O' },
            &session.hostaddr,
        )
        .await?;
    }

    // C took the pool before committing and gave it back when a commit failed (`:214-250`), so
    // both the order and the revert are the original's. A pool that no longer holds the article
    // is C's `ret = -3`: no commit, and the dropped transaction undoes the SQL.
    if let Err(err) = session.state.board_mut().set_article_excerption(sid, aid, set) {
        tracing::error!(
            "section_list_set_article_excerption(sid={sid}, aid={aid}, set={}) error: {err:?}",
            i32::from(set)
        );
        return Ok(false);
    }

    if let Err(err) = tx.commit().await {
        tracing::error!("Mysqli error: {err}");
        let _ = session
            .state
            .board_mut()
            .set_article_excerption(sid, aid, row.excerption);
        return Ok(false);
    }

    Ok(true)
}

/// `TITLE_INPUT_MAX_LEN` (`article_post.c:31`): the display columns the title prompt holds.
const TITLE_INPUT_MAX_LEN: usize = 72;

/// `article_post` (`article_post.c:36-437`): a new root article, from the title menu through
/// [`editor::editor_display`] to one transaction. `Ok(true)` is C's `ret = 1`, which is what
/// tells the board list to re-query the page; anything that left the database alone — the
/// permission refusal, a cancel, a disconnect or a timeout — is `Ok(false)`.
///
/// C returns `-1` for its own failures (an allocation that did not happen, a body that did
/// not fit) and the caller only logs them; the port logs the same message and returns `false`,
/// because the distinction changes nothing on the screen.
pub async fn article_post(
    term: &mut Terminal,
    session: &mut Session,
    sid: i16,
    sname: &str,
    stitle: &str,
) -> Result<bool, sqlx::Error> {
    if !session.privs.checkpriv(sid, S_POST) {
        notice(term, "您没有权限在本版块发表文章\n").await;
        return Ok(false);
    }

    let fixed = term.fixed_width();
    let mut data = editor::EditorData::load(b"", fixed);
    // `p_article_new->title`, which the header shows and the `INSERT` stores, and
    // `title_input`, the bytes the prompt holds between the two.
    let mut title: Vec<u8> = Vec::new();
    let mut title_input: Vec<u8> = Vec::new();
    let mut transship = false;
    let mut reply_note = true;
    let mut sign_id = 0i32;
    // C's loop variable doubles as the answer to every prompt below, and the header loop starts
    // with `'T'` so that the first pass skips its own command line.
    let mut ch: u32 = KEY_T;

    while !session.state.is_exiting() {
        screen::clearscr(term);
        term.moveto(21, 1);
        term.print(&format!(
            "发表文章于 {stitle}[{sname}] 讨论区，类型: {}，回复通知: {}",
            if transship { "转载" } else { "原创" },
            if reply_note { "开启" } else { "关闭" }
        ));
        term.moveto(22, 1);
        term.print(&format!(
            "标题: {}",
            if title.is_empty() {
                "[无]".to_string()
            } else {
                String::from_utf8_lossy(&title).into_owned()
            }
        ));
        term.moveto(23, 1);
        term.print(&format!("使用第 \x1b[1;32m{sign_id}\x1b[m 个签名"));

        if upper(ch) != KEY_T {
            term.print(
                "    按\x1b[1;32m0\x1b[m~\x1b[1;32m3\x1b[m选签名档(0表示不使用)",
            );
            term.moveto(24, 1);
            term.print(&format!(
                "\x1b[1;32mT\x1b[m改标题, \x1b[1;32mC\x1b[m取消, \x1b[1;32mZ\x1b[m设为{}, \x1b[1;32mN\x1b[m{}, \x1b[1;32mEnter\x1b[m继续: ",
                if transship { "原创" } else { "转载" },
                if reply_note {
                    "关闭回复通知"
                } else {
                    "开启回复通知"
                }
            ));
            if let Err(err) = term.flush().await {
                tracing::error!("io_flush() error: {err}");
            }
            term.reset_input().await;
            ch = 0;
        }

        loop {
            if session.state.is_exiting() {
                break;
            }

            match upper(ch) {
                KEY_NULL | KEY_TIMEOUT => return Ok(false),
                KEY_CR => {}
                KEY_T => {
                    let buf_len = crate::paths::ARTICLE_TITLE_MAX_LEN + 1;
                    let Some(typed) = screen::get_data(
                        term,
                        24,
                        1,
                        "标题: ",
                        &title_input,
                        buf_len,
                        TITLE_INPUT_MAX_LEN,
                        MAX_USER_IDLE_TIME as u64,
                    )
                    .await
                    else {
                        // C carries on with `len = -1` and writes one byte before the buffer;
                        // the reader is gone either way.
                        tracing::debug!("get_data(title) timeout");
                        return Ok(false);
                    };
                    title_input = typed;

                    let start = title_input
                        .iter()
                        .position(|&b| b != b' ')
                        .unwrap_or(title_input.len());
                    let mut end = title_input.len();
                    while end > start && title_input[end - 1] == b' ' {
                        end -= 1;
                    }
                    let trimmed = &title_input[start..end];
                    if !trimmed.is_empty() {
                        let mut masked = trimmed.to_vec();
                        if session.state.badwords().mask(&mut masked, b'*') > 0 {
                            // The stars go back into the prompt, and the title stays as it was.
                            title_input = masked;
                            continue;
                        }
                        title = masked.clone();
                        title_input = masked;
                    }
                    ch = 0;
                }
                KEY_C => {
                    notice(term, "取消...").await;
                    return Ok(false);
                }
                KEY_Z => transship = !transship,
                KEY_NO => reply_note = !reply_note,
                KEY_0..=KEY_3 => sign_id = (ch - KEY_0) as i32,
                // An unrecognised key is simply another chance: `ch` is replaced and the
                // command line is not repainted, because the loop never left.
                _ => {
                    ch = term.read_key_wait(MAX_USER_IDLE_TIME as u64).await;
                    continue;
                }
            }

            break;
        }

        if ch != KEY_CR || title.is_empty() {
            continue;
        }

        ch = KEY_E;
        while !session.state.is_exiting() && upper(ch) == KEY_E {
            editor::editor_display(term, session, &mut data).await;

            screen::clearscr(term);
            term.moveto(1, 1);
            term.print("(S)发送, (C)取消, (T)更改标题 or (E)再编辑? [S]: ");
            if let Err(err) = term.flush().await {
                tracing::error!("io_flush() error: {err}");
            }
            term.reset_input().await;

            loop {
                if session.state.is_exiting() {
                    break;
                }

                ch = term.read_key_wait(MAX_USER_IDLE_TIME as u64).await;
                match upper(ch) {
                    KEY_NULL | KEY_TIMEOUT => return Ok(false),
                    KEY_C => {
                        notice(term, "取消...").await;
                        return Ok(false);
                    }
                    KEY_CR | KEY_S | KEY_T | KEY_E => break,
                    _ => continue,
                }
            }
        }

        if upper(ch) != KEY_T {
            break;
        }
    }

    if session.state.is_exiting() {
        // "Do not save data on shutdown" (`:219`).
        return Ok(false);
    }

    let Some(mut content) = data.save(lml::ARTICLE_CONTENT_MAX_LEN) else {
        tracing::error!("editor_data_save() error");
        return Ok(false);
    };

    session.state.badwords().mask(&mut content, b'*');

    if sign_id > 0 {
        let slot = match sign_id {
            1 => db::SignSlot::One,
            2 => db::SignSlot::Two,
            _ => db::SignSlot::Three,
        };
        if let Some(sign) = session.state.db.signature(session.privs.uid, slot).await? {
            let tail = format!("\n\n--\n{sign}\n");
            let room = lml::ARTICLE_CONTENT_MAX_LEN
                .saturating_sub(1)
                .saturating_sub(content.len());
            content.extend_from_slice(&tail.as_bytes()[..tail.len().min(room)]);
        }
    }

    let content_display_length = text::display_width(&content, true, fixed) as i32;

    let mut tx = session.state.db.begin().await?;

    // A new post has no `AID` yet, so the body goes in attached to `0` and is linked back
    // after the article row exists.
    let cid = db::insert_content(&mut tx, 0, &content).await?;

    let row = db::NewArticleRow {
        sid,
        tid: 0,
        uid: session.privs.uid,
        username: session.username.clone(),
        nickname: session.nickname.clone(),
        title: String::from_utf8_lossy(&title).into_owned(),
        cid,
        transship,
        reply_note,
        exp: session.exp,
        length: content_display_length,
        ip: session.hostaddr.clone(),
    };
    let aid = db::insert_article(&mut tx, &row).await?;
    db::link_content(&mut tx, aid, cid).await?;

    if session.privs.checkpriv(sid, S_GETEXP) {
        db::add_exp(&mut tx, session.privs.uid, if transship { 5 } else { 15 }).await?;
    }

    db::log_article_op(&mut tx, aid, session.privs.uid, 'A', &session.hostaddr).await?;

    tx.commit().await?;

    notice(
        term,
        &format!(
            "发送完成，新文章通常会在{}秒后可见",
            SECTION_LIST_INTERVAL.as_secs()
        ),
    )
    .await;

    Ok(true)
}

/// `article_modify` (`article_post.c:439-750`): an author's rewrite of their own article.
/// The body is read out of the database, stripped of its control sequences, edited, and stored
/// as a new `bbs_content` row that the article is pointed at.
///
/// `reply_note` starts from the article's own setting (`:529`) and the `(N)` key flips it as
/// often as the user likes before saving; the saved row gets whichever way it is left.
pub async fn article_modify(
    term: &mut Terminal,
    session: &mut Session,
    aid: i32,
    excerption: bool,
) -> Result<bool, sqlx::Error> {
    if excerption {
        notice(term, "该文章无法被编辑，请联系版主。").await;
        return Ok(false);
    }

    let fixed = term.fixed_width();
    let Some(row) = db::editable_content(&session.state.db, aid, true).await? else {
        tracing::error!("Article content not found (aid={aid})");
        return Ok(false);
    };

    let mut content = row.content.unwrap_or_default().into_bytes();
    // The stored body carries the colours the last render added; the editor starts from the
    // text underneath them.
    text::str_filter(&mut content, false);
    let mut data = editor::EditorData::load(&content, fixed);
    let mut reply_note = row.reply_note.unwrap_or(0) != 0;
    let mut ch: u32 = KEY_E;

    while !session.state.is_exiting() && upper(ch) == KEY_E {
        editor::editor_display(term, session, &mut data).await;

        loop {
            if session.state.is_exiting() {
                break;
            }

            screen::clearscr(term);
            term.moveto(1, 1);
            term.print(&format!(
                "(S)保存, (C)取消, (N){}回复通知 or (E)再编辑? [S]: ",
                if reply_note { "关闭" } else { "开启" }
            ));
            if let Err(err) = term.flush().await {
                tracing::error!("io_flush() error: {err}");
            }
            term.reset_input().await;

            ch = term.read_key_wait(MAX_USER_IDLE_TIME as u64).await;
            match upper(ch) {
                KEY_NULL | KEY_TIMEOUT => return Ok(false),
                KEY_C => {
                    notice(term, "取消...").await;
                    return Ok(false);
                }
                // `(N)` repaints the same question over the new setting; anything the switch
                // does not know is another chance to answer it.
                KEY_NO => {
                    reply_note = !reply_note;
                    continue;
                }
                KEY_CR | KEY_S | KEY_E => {}
                _ => continue,
            }

            break;
        }
    }

    if session.state.is_exiting() {
        return Ok(false);
    }

    let Some(mut content) = data.save(lml::ARTICLE_CONTENT_MAX_LEN - LINE_BUFFER_LEN) else {
        tracing::error!("editor_data_save() error");
        return Ok(false);
    };

    session.state.badwords().mask(&mut content, b'*');

    let stamp = chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S (UTC %z)")
        .to_string();
    let tail = format!("\n--\n※ 作者已于 {stamp} 修改本文※\n");
    // C appends through `snprintf(…, LINE_BUFFER_LEN, …)` and then adds the value it *would*
    // have written, so a footer that did not fit leaves the length past the buffer. The port
    // caps the footer at the same 1023 bytes and counts the bytes that are really there.
    content.extend_from_slice(&tail.as_bytes()[..tail.len().min(LINE_BUFFER_LEN - 1)]);

    let content_display_length = text::display_width(&content, true, fixed) as i32;

    let mut tx = session.state.db.begin().await?;

    // The modified body is a new row, pointed at the article that already exists (`:666`).
    let new_cid = db::insert_content(&mut tx, aid, &content).await?;
    db::apply_edit(&mut tx, aid, new_cid, content_display_length, reply_note).await?;
    db::log_article_op(&mut tx, aid, session.privs.uid, 'M', &session.hostaddr).await?;

    tx.commit().await?;

    notice(
        term,
        &format!(
            "修改完成，新内容通常会在{}秒后可见",
            SECTION_LIST_INTERVAL.as_secs()
        ),
    )
    .await;

    Ok(true)
}

/// `ARTICLE_QUOTE_DEFAULT_LINES` (`article_post.c:32`): how much of the original a concise
/// quote carries.
const ARTICLE_QUOTE_DEFAULT_LINES: usize = 20;

/// `BBS_msg_max_len` (`bbs.h:25`), the size of the reply notification `article_reply` builds.
const MSG_MAX_LEN: usize = 1024;

/// The arena fields `article_reply` quotes from — `aid`, the thread it belongs to, and the
/// three strings that go into the header, the quote banner and the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quoted {
    pub aid: i32,
    pub tid: i32,
    pub title: String,
    pub username: String,
    pub nickname: String,
}

impl From<&Article> for Quoted {
    fn from(article: &Article) -> Self {
        Quoted {
            aid: article.aid,
            tid: article.tid,
            title: article.title.clone(),
            username: article.username.clone(),
            nickname: article.nickname.clone(),
        }
    }
}

/// `article_reply` (`article_post.c:752-1371`): a reply to a listed article, with the original
/// quoted into the editor. The thread root is the `tid` when there is one, and that is the
/// article whose `lock` flag refuses the reply (`:814-852`).
pub async fn article_reply(
    term: &mut Terminal,
    session: &mut Session,
    sid: i16,
    sname: &str,
    stitle: &str,
    target: &Quoted,
) -> Result<bool, sqlx::Error> {
    if !session.privs.checkpriv(sid, S_POST) {
        notice(term, "您没有权限在本版块发表文章\n").await;
        return Ok(false);
    }

    let fixed = term.fixed_width();
    let mut title: Vec<u8> = Vec::new();
    let mut title_input = format!("Re: {}", target.title).into_bytes();
    // The prefilled title is cut to what the prompt can show before it is ever drawn (`:798`).
    let split = text::split_line(&title_input, TITLE_INPUT_MAX_LEN, false, fixed);
    title_input.truncate(split.bytes.min(title_input.len()));

    let root_aid = if target.tid == 0 { target.aid } else { target.tid };
    if db::topic_lock(&session.state.db, root_aid).await? {
        notice(term, "该主题谢绝回复").await;
        return Ok(false);
    }

    let Some(row) = db::editable_content(&session.state.db, target.aid, false).await? else {
        tracing::error!("Article content not found (aid={})", target.aid);
        return Ok(false);
    };
    // The quote is what a reader would have seen: rendered, then stripped back to text (`:889-893`).
    let body = row.content.unwrap_or_default();
    let mut quoted = lml::render(
        body.as_bytes(),
        lml::ARTICLE_CONTENT_MAX_LEN,
        (editor::MAX_EDITOR_DATA_LINE_LENGTH - 3) as i64,
        true,
        fixed,
    );
    text::str_filter(&mut quoted, false);

    let mut data = editor::EditorData::load(b"", fixed);
    let mut sign_id = 0i32;
    // A reply starts with notification off, unlike a new post (`:764` versus `:51`).
    let mut reply_note = false;
    let mut full_quote = false;
    let mut ch: u32 = KEY_T;

    while !session.state.is_exiting() {
        screen::clearscr(term);
        term.moveto(21, 1);
        term.print(&format!(
            "回复文章于 {stitle}[{sname}] 讨论区, 回复通知: {}, 引用模式: {}",
            if reply_note { "开启" } else { "关闭" },
            if full_quote { "完整" } else { "精简" }
        ));
        term.moveto(22, 1);
        term.print(&format!(
            "标题: {}",
            if title.is_empty() {
                "[无]".to_string()
            } else {
                String::from_utf8_lossy(&title).into_owned()
            }
        ));
        term.moveto(23, 1);
        term.print(&format!("使用第 \x1b[1;32m{sign_id}\x1b[m 个签名"));

        if upper(ch) != KEY_T {
            term.print("    按\x1b[1;32m0\x1b[m~\x1b[1;32m3\x1b[m选签名档(0表示不使用)");
            term.moveto(24, 1);
            term.print(&format!(
                "\x1b[1;32mT\x1b[m改标题, \x1b[1;32mC\x1b[m取消, \x1b[1;32mN\x1b[m{}, \x1b[1;32mQ\x1b[m{}, \x1b[1;32mEnter\x1b[m继续: ",
                if reply_note {
                    "关闭回复通知"
                } else {
                    "开启回复通知"
                },
                if full_quote { "精简引用" } else { "完整引用" }
            ));
            if let Err(err) = term.flush().await {
                tracing::error!("io_flush() error: {err}");
            }
            term.reset_input().await;
            ch = 0;
        }

        loop {
            if session.state.is_exiting() {
                break;
            }

            match upper(ch) {
                KEY_NULL | KEY_TIMEOUT => return Ok(false),
                KEY_CR => {}
                KEY_T => {
                    let buf_len = crate::paths::ARTICLE_TITLE_MAX_LEN + 4;
                    let Some(typed) = screen::get_data(
                        term,
                        24,
                        1,
                        "标题: ",
                        &title_input,
                        buf_len,
                        TITLE_INPUT_MAX_LEN,
                        MAX_USER_IDLE_TIME as u64,
                    )
                    .await
                    else {
                        tracing::debug!("get_data(title) timeout");
                        return Ok(false);
                    };
                    title_input = typed;

                    let start = title_input
                        .iter()
                        .position(|&b| b != b' ')
                        .unwrap_or(title_input.len());
                    let mut end = title_input.len();
                    while end > start && title_input[end - 1] == b' ' {
                        end -= 1;
                    }
                    let trimmed = &title_input[start..end];
                    if !trimmed.is_empty() {
                        let mut masked = trimmed.to_vec();
                        if session.state.badwords().mask(&mut masked, b'*') > 0 {
                            title_input = masked;
                            continue;
                        }
                        title = masked.clone();
                        title_input = masked;
                    }
                    ch = 0;
                }
                KEY_C => {
                    notice(term, "取消...").await;
                    return Ok(false);
                }
                KEY_NO => reply_note = !reply_note,
                KEY_Q => full_quote = !full_quote,
                KEY_0..=KEY_3 => sign_id = (ch - KEY_0) as i32,
                _ => {
                    ch = term.read_key_wait(MAX_USER_IDLE_TIME as u64).await;
                    continue;
                }
            }

            break;
        }

        if ch != KEY_CR || title.is_empty() {
            continue;
        }

        // The banner and the quoted lines, every one of them prefixed with `: ` (`:997-1022`).
        let mut content =
            format!("\n\n【 在 {} ({}) 的大作中提到: 】\n", target.username, target.nickname)
                .into_bytes();
        let max_lines = if full_quote {
            editor::MAX_EDITOR_DATA_LINES
        } else {
            ARTICLE_QUOTE_DEFAULT_LINES
        };
        let wrapped = text::split_data_lines(
            &quoted,
            editor::MAX_EDITOR_DATA_LINE_LENGTH - 2,
            false,
            fixed,
            max_lines,
        );
        for index in 0..wrapped.count() {
            content.extend_from_slice(b": ");
            content.extend_from_slice(wrapped.line(&quoted, index));
            if content.last() != Some(&b'\n') {
                content.push(b'\n');
            }
        }
        if content.last() != Some(&b'\n') {
            content.push(b'\n');
        }

        data = editor::EditorData::load(&content, fixed);

        ch = KEY_E;
        while !session.state.is_exiting() && upper(ch) == KEY_E {
            editor::editor_display(term, session, &mut data).await;

            screen::clearscr(term);
            term.moveto(1, 1);
            term.print("(S)发送, (C)取消, (T)更改标题 or (E)再编辑? [S]: ");
            if let Err(err) = term.flush().await {
                tracing::error!("io_flush() error: {err}");
            }
            term.reset_input().await;

            loop {
                if session.state.is_exiting() {
                    break;
                }

                ch = term.read_key_wait(MAX_USER_IDLE_TIME as u64).await;
                match upper(ch) {
                    KEY_NULL | KEY_TIMEOUT => return Ok(false),
                    KEY_C => {
                        notice(term, "取消...").await;
                        return Ok(false);
                    }
                    KEY_CR | KEY_S | KEY_T | KEY_E => break,
                    _ => continue,
                }
            }
        }

        if upper(ch) != KEY_T {
            break;
        }
    }

    if session.state.is_exiting() {
        return Ok(false);
    }

    let Some(mut content) = data.save(lml::ARTICLE_CONTENT_MAX_LEN) else {
        tracing::error!("editor_data_save() error");
        return Ok(false);
    };

    session.state.badwords().mask(&mut content, b'*');

    if sign_id > 0 {
        let slot = match sign_id {
            1 => db::SignSlot::One,
            2 => db::SignSlot::Two,
            _ => db::SignSlot::Three,
        };
        if let Some(sign) = session.state.db.signature(session.privs.uid, slot).await? {
            let tail = format!("\n\n--\n{sign}\n");
            let room = lml::ARTICLE_CONTENT_MAX_LEN
                .saturating_sub(1)
                .saturating_sub(content.len());
            content.extend_from_slice(&tail.as_bytes()[..tail.len().min(room)]);
        }
    }

    let content_display_length = text::display_width(&content, true, fixed) as i32;

    let mut tx = session.state.db.begin().await?;

    let cid = db::insert_content(&mut tx, 0, &content).await?;

    let row = db::NewArticleRow {
        sid,
        tid: root_aid,
        uid: session.privs.uid,
        username: session.username.clone(),
        nickname: session.nickname.clone(),
        title: String::from_utf8_lossy(&title).into_owned(),
        cid,
        transship: false,
        reply_note,
        exp: session.exp,
        length: content_display_length,
        ip: session.hostaddr.clone(),
    };
    let aid = db::insert_article(&mut tx, &row).await?;

    db::touch_topic(
        &mut tx,
        root_aid,
        session.privs.uid,
        &session.username,
        &session.nickname,
    )
    .await?;
    db::link_content(&mut tx, aid, cid).await?;

    // Both the thread root and the article that was replied to ask to be told, unless they are
    // the reader themself.
    let notice_uids =
        db::reply_notice_uids(&mut tx, target.tid, target.aid, session.privs.uid).await?;
    let message = format!(
        "有人回复了您所发表/回复的文章，快来[article {aid}]看看[/article]《{}》吧！\n",
        String::from_utf8_lossy(&title)
    );
    let message = crate::board::trunc_bytes(&message, MSG_MAX_LEN - 1);
    for to_uid in notice_uids {
        db::insert_message(
            &mut tx,
            session.config.sys_id,
            to_uid,
            &message,
            &session.hostaddr,
        )
        .await?;
    }

    if session.privs.checkpriv(sid, S_GETEXP) {
        db::add_exp(&mut tx, session.privs.uid, 3).await?;
    }

    db::log_article_op(&mut tx, aid, session.privs.uid, 'A', &session.hostaddr).await?;

    tx.commit().await?;

    notice(
        term,
        &format!(
            "发送完成，新文章通常会在{}秒后可见",
            SECTION_LIST_INTERVAL.as_secs()
        ),
    )
    .await;

    Ok(true)
}

/// `clearscr(); moveto(1, 1); prints(prompt); iflush();` and the key loop that follows it
/// (`:50-78`). `yes` is the uppercase letter that accepts; `CR`, `ESC`, `N`, a timeout and a
/// hung-up client all refuse, as they do in C.
async fn ask(term: &mut Terminal, session: &Session, prompt: &str, yes: u32) -> bool {
    screen::clearscr(term);
    term.moveto(1, 1);
    term.print(prompt);
    if let Err(err) = term.flush().await {
        tracing::error!("io_flush() error: {err}");
        return false;
    }
    term.reset_input().await;

    loop {
        if session.state.is_exiting() {
            return false;
        }

        let ch = upper(term.read_key_wait(MAX_USER_IDLE_TIME as u64).await);
        if ch == yes {
            return true;
        }
        if matches!(ch, KEY_NULL | KEY_TIMEOUT | KEY_ESC | KEY_CR | KEY_NO) {
            return false;
        }
    }
}

/// The two ASCII members of C's yes/no `switch`, which the `KEY_*` sentinels cannot join in a
/// pattern.
const KEY_CR: u32 = b'\r' as u32;
const KEY_NO: u32 = b'N' as u32;

/// The command letters of [`article_post`]'s two prompts, which like [`KEY_CR`] cannot be
/// written as byte patterns against a `u32` key code.
const KEY_T: u32 = b'T' as u32;
const KEY_E: u32 = b'E' as u32;
const KEY_C: u32 = b'C' as u32;
const KEY_S: u32 = b'S' as u32;
const KEY_Z: u32 = b'Z' as u32;
const KEY_Q: u32 = b'Q' as u32;
const KEY_0: u32 = b'0' as u32;
const KEY_3: u32 = b'3' as u32;

/// `toupper` over a key code: the two ASCII ranges move, everything else — including the
/// `KEY_*` sentinels — is passed through, which is what the C `switch` sees.
fn upper(ch: u32) -> u32 {
    if ch < 0x100 && (b'a'..=b'z').contains(&(ch as u8)) {
        ch - 0x20
    } else {
        ch
    }
}

/// `clearscr(); moveto(1, 1); prints(message); press_any_key();`
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
    use crate::config::{Config, DbConfig};
    use crate::state::AppState;
    use crate::term::{ByteIo, Pipe, Terminal};
    use std::path::Path;
    use std::sync::Arc;

    fn target(aid: i32, tid: i32, uid: i32, excerption: bool) -> Deletable {
        Deletable { aid, tid, uid, excerption }
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
            bbs_name: "枫林在线".into(),
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
        Session::new(state, Path::new("."), "lbbs-test".into(), "127.0.0.1".into(), 2323, false)
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
            let asked = ask(&mut term, &session, "真的要删除文章？(Y)是, (N)否 [N]: ", b'Y' as u32).await;

            assert_eq!(asked, accepted, "{keys:?}");

            let screen = screen_text(keyed.await.unwrap());
            assert!(screen.contains("真的要删除文章？(Y)是, (N)否 [N]: "), "{keys:?}: {screen}");
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
        assert!(screen.contains("该文章无法被删除，请联系版主。"), "{screen}");
        assert!(!screen.contains("真的要删除文章"), "the gate is behind this branch: {screen}");
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
        let replied = article_reply(&mut term, &mut session, 3, "test", "测试版", &target_article())
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
        assert!(screen.contains("该文章无法被编辑，请联系版主。"), "{screen}");
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
        assert!(screen.contains("正文"), "the editor painted what was typed: {screen}");
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
        assert!(screen.starts_with("\x1b[2J"), "clearscr comes first: {screen:?}");
        assert!(screen.contains("\x1b[1;1H删除成功，请在1秒后刷新列表。"), "{screen:?}");
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

        let (before,): (Option<i32>,) =
            sqlx::query_as("SELECT exp FROM user_pubinfo WHERE UID = ?")
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
            let (after,): (Option<i32>,) =
                sqlx::query_as("SELECT exp FROM user_pubinfo WHERE UID = ?")
                    .bind(uid)
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
            assert_eq!(after, before.map(|exp| exp + cost), "the author was billed {cost}");
            drop(tx);

            let (flag,): (bool,) = sqlx::query_as("SELECT excerption FROM bbs WHERE AID = ?")
                .bind(aid)
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(flag, excerption, "the dropped transaction took the flag back");
        }
    }
}
