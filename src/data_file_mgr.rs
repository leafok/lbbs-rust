/* SPDX-License-Identifier: GPL-3.0-or-later */
//! 数据文件管理: the eight fixed texts of the site, picked by number and edited in place.
//!
//! Port of `src/data_file_mgr.c` — `data_file_mgr` (`:55-273`) — reached from the top menu's
//! `DATAMGR` item (`bbs_cmd.c:28`), the only menu action of C's that rewrites files under the
//! site root.
//!
//! An edit lands on the packaged file and, beside it, on a copy with `.user` on the end: the
//! comment at `:225` says the second one exists so that the edit survives an upgrade. Neither
//! is read back — the loader is [`crate::state::AppState::load_data_files`], run at startup and
//! by `RELOADCONF`, which is what the closing line of `:248` tells the reader.
//!
//! # Deviations from C
//!
//! * `fstat` + `mmap` + `close` + `munmap` (`:97-132`) is one read, and the 4 MB `malloc`
//!   (`:134-139`) is only the length the save is checked against; none of C's error paths in
//!   between — including its `OOM` — is a thing that can happen here.
//! * [`editor::editor_display`]'s return code is dropped, as C drops the result of the call at
//!   `:145`, and `editor_data_cleanup` (`:252-255`) is the [`EditorData`] going out of scope.
//! * C's `atoi` of `:83` is [`crate::config::c_atoi`], and its `i < 0 || i >= data_file_cnt`
//!   test is one `try_from`: a negative number and a number past the list give the same answer.
//! * `DATA_FILE_MAX_LEN` is a `usize` rather than the single member of an `enum` (`:30-33`), and
//!   `data_file_cnt` (`:53`) is that table's length.
//! * C's one `write()` of the saved text is not a loop, so a short write would leave the tail of
//!   the file out; `write_all` is the loop C would have needed. Its `close()` error has no
//!   counterpart — the handle is closed when it goes out of scope.

use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::config::c_atoi;
use crate::editor::{self, EditorData};
use crate::paths::{
    CONF_BBSNET, CONF_BWF, DATA_ACTIVE_BOARD, DATA_EDITOR_HELP, DATA_GOODBYE, DATA_LOGIN_ERROR,
    DATA_READ_HELP, DATA_WELCOME, MAX_USER_IDLE_TIME, SCREEN_ROWS,
};
use crate::screen;
use crate::session::Session;
use crate::term::Terminal;
use crate::term::key::{KEY_NULL, KEY_TIMEOUT, to_upper};

/// `DATA_FILE_MAX_LEN` (`:32`), the size of the buffer C saves the edited text into.
const DATA_FILE_MAX_LEN: usize = 1024 * 1024 * 4;

/// `data_file_list` (`:42-51`): the number the screen prints is the index plus one.
const DATA_FILE_LIST: [(&str, &str); 8] = [
    ("进站欢迎", DATA_WELCOME),
    ("离站告别", DATA_GOODBYE),
    ("登陆错误", DATA_LOGIN_ERROR),
    ("活动看板", DATA_ACTIVE_BOARD),
    ("浏览帮助", DATA_READ_HELP),
    ("编辑帮助", DATA_EDITOR_HELP),
    ("敏感词表", CONF_BWF),
    ("穿梭配置", CONF_BBSNET),
];

/// The command letters of the two prompts, which like [`KEY_CR`] cannot be written as byte
/// patterns against a `u32` key code.
const KEY_CR: u32 = b'\r' as u32;
const KEY_C: u32 = b'C' as u32;
const KEY_E: u32 = b'E' as u32;
const KEY_P: u32 = b'P' as u32;
const KEY_S: u32 = b'S' as u32;

/// `data_file_mgr` (`:55-273`). The menu parameter is unused, as it is in C, and every way out of
/// the page is the `return REDRAW` of `:272`, which the dispatcher answers itself.
pub async fn data_file_mgr(term: &mut Terminal, session: &mut Session) {
    screen::clearscr(term);
    term.moveto(1, 1);
    term.print("数据文件列表: ");
    for (i, (name, _)) in DATA_FILE_LIST.iter().enumerate() {
        term.moveto(3 + i, 4);
        term.print(&format!("{:>2}. {}", i + 1, name));
    }

    // `get_data(SCREEN_ROWS, 1, ..., buf, sizeof(buf), 2)` (`:82`): two display cells into a
    // three-byte buffer. C throws the return code away and reads `buf` regardless, so the
    // timeout that [`screen::get_data`] reports as `None` has to be the empty answer C started
    // with — no number, and the menu is redrawn.
    let typed = screen::get_data(
        term,
        SCREEN_ROWS,
        1,
        "请输入需要编辑的文件编号: ",
        b"",
        3,
        2,
        MAX_USER_IDLE_TIME,
    )
    .await
    .unwrap_or_default();
    let chosen = match usize::try_from(c_atoi(&String::from_utf8_lossy(&typed)) - 1) {
        Ok(i) if i < DATA_FILE_LIST.len() => i,
        // `:84-87`: nothing typed, `0`, or a number past the list.
        _ => return,
    };
    let path = DATA_FILE_LIST[chosen].1;
    let user_path = format!("{path}.user");

    // `:90-95`: the site's own copy if there is one, the file as installed if there is not, and
    // the name in the log is the installed one, because its absence is the news.
    let content = match std::fs::read(session.root.join(&user_path)) {
        Ok(content) => content,
        Err(_) => match std::fs::read(session.root.join(path)) {
            Ok(content) => content,
            Err(err) => {
                tracing::error!("open({path}) error ({err})");
                return;
            }
        },
    };
    let mut data = EditorData::load(&content, term.fixed_width());

    let mut saved: Vec<u8> = Vec::new();
    let mut previewed = false;
    // `for (ch = 'E'; ...)`: the first round is the editor, and the key that leaves it is the
    // one the prompt below the editor is answered with.
    let mut ch: u32 = KEY_E;

    while !session.state.is_exiting() {
        if ch == KEY_E {
            editor::editor_display(term, session, &mut data).await;
            previewed = false;
        }

        screen::clearscr(term);
        term.moveto(1, 1);
        term.print(if previewed {
            "(S)保存, (P)预览, (C)取消 or (E)再编辑? [P]: "
        } else {
            "(P)预览, (C)取消 or (E)再编辑? [P]: "
        });
        if let Err(err) = term.flush().await {
            tracing::error!("io_flush() error: {err}");
        }

        ch = term.read_key_wait(MAX_USER_IDLE_TIME).await;
        match to_upper(ch) {
            KEY_NULL | KEY_TIMEOUT => return,
            KEY_CR | KEY_P => {
                let Some(bytes) = data.save(DATA_FILE_MAX_LEN) else {
                    // `:169-174`: the text outgrew the 4 MB buffer.
                    tracing::error!("editor_data_save() error");
                    return;
                };
                saved = bytes;
                // `split_data_lines` and `display_data` of `:175-176`, which is what
                // [`screen::display_file`] does as one step.
                screen::display_file(term, session, &saved, 1).await;
                previewed = true;
                continue;
            }
            KEY_S => {
                // `:179-184`: saving without having looked at the result is not an answer.
                if !previewed {
                    continue;
                }
            }
            KEY_C => {
                notice(term, "取消更改...").await;
                return;
            }
            KEY_E => {
                ch = KEY_E;
                continue;
            }
            // `default: // Invalid selection` (`:194-195`).
            _ => continue,
        }

        break;
    }

    // `if (SYS_server_exit) goto cleanup;` (`:201-204`): an edit that ends because the daemon is
    // going down does not write.
    if session.state.is_exiting() {
        return;
    }

    if let Err(err) = write_copy(&session.root.join(&user_path), &user_path, &saved) {
        tracing::error!("{err}");
        return;
    }
    // "Make another copy of the data file so that the user editored copy will be kept after
    // upgrade / reinstall" (`:225`): the same bytes, in this order.
    if let Err(err) = write_copy(&session.root.join(path), path, &saved) {
        tracing::error!("{err}");
        return;
    }

    notice(term, "修改已完成，需要重新加载配置后才会生效...").await;
}

/// `clearscr(); moveto(1, 1); prints(message); press_any_key();` (`:186-189` and `:246-249`).
async fn notice(term: &mut Terminal, message: &str) {
    screen::clearscr(term);
    term.moveto(1, 1);
    term.print(message);
    if let Err(err) = term.flush().await {
        tracing::error!("io_flush() error: {err}");
    }
    screen::press_any_key(term).await;
}

/// The `open(O_WRONLY | O_CREAT | O_TRUNC, 0640)`, `write` and `close` of `:206-244`, with C's
/// two messages and the path as the list spells it. A file that is already there keeps the mode
/// it has — `open`'s mode argument is only used on create in C too.
fn write_copy(file: &Path, name: &str, data: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o640);
    let mut handle = match options.open(file) {
        Ok(handle) => handle,
        Err(err) => return Err(format!("open({name}) error ({err})")),
    };
    if let Err(err) = handle.write_all(data) {
        return Err(format!("write(len={}) error ({err})", data.len()));
    }
    if let Err(err) = handle.flush() {
        return Err(format!("close(fd) error ({err})"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/test_data_file_mgr.rs"]
mod tests;
