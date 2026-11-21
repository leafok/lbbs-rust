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
    let mut handle = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(file)
    {
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
mod tests {
    use super::*;
    use crate::term::{ByteIo, Pipe};
    use std::path::PathBuf;
    use std::time::Duration;

    /// The editor's leave key, which is what the prompt after it is answered with.
    const CTRL_W: u8 = 0x17;
    const ESC: u8 = 0x1b;

    /// A site root with nothing in it but the directories the eight paths need.
    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("lbbs-datamgr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        root
    }

    /// The offline session, moved out to `root` so the page has a site to read and write.
    fn test_session(root: &Path) -> Session {
        let mut session = crate::session::offline_session();
        session.root = root.to_path_buf();
        session
    }

    /// The screen the page leaves behind. Every step of the script is pushed a quarter second
    /// after the one before it, because [`screen::get_data`] and [`screen::press_any_key`] both
    /// empty the input before they ask: a script fed at once is a script nobody was asking for.
    async fn run(root: &Path, script: &[&[u8]]) -> String {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = test_session(root);

        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(data_file_mgr(&mut term, &mut session), async {
                for step in script {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    client.push(step);
                }
            });
        })
        .await
        .expect("the page ends on its keys");
        let _ = term.flush().await;
        String::from_utf8_lossy(&client.take_available()).into_owned()
    }

    fn count(screen: &str, needle: &str) -> usize {
        screen.match_indices(needle).count()
    }

    /// A number that is not one of the eight is C's first `return REDRAW`: the list is drawn, the
    /// answer is read and nothing is opened, so nothing is created either.
    #[tokio::test]
    async fn a_number_past_the_list_is_answered_with_the_list() {
        let root = root("number");
        std::fs::write(root.join(DATA_WELCOME), "欢迎回来\n").unwrap();

        let screen = run(&root, &[b"9\r"]).await;
        assert!(screen.contains("数据文件列表: "), "{screen}");
        assert!(screen.contains(" 1. 进站欢迎"), "{screen}");
        assert!(screen.contains(" 8. 穿梭配置"), "{screen}");
        assert!(screen.contains("请输入需要编辑的文件编号: "), "{screen}");
        assert_eq!(
            std::fs::read(root.join(DATA_WELCOME)).unwrap(),
            "欢迎回来\n".as_bytes()
        );
        assert!(!root.join(format!("{DATA_WELCOME}.user")).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An entry that is not a file on this site — the shuttle configuration of the last row — is
    /// C's second `return REDRAW`, with `open(data_file_list[i].path)` named in the log.
    #[tokio::test]
    async fn a_file_that_is_not_there_writes_nothing() {
        let root = root("missing");
        std::fs::write(root.join(DATA_WELCOME), "欢迎回来\n").unwrap();

        let screen = run(&root, &[b"8\r"]).await;
        assert!(!screen.contains("再编辑"), "{screen}");
        assert!(!root.join("conf/bbsnet.conf.user").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The `.user` copy is the one that opens, and `(C)` leaves both files alone.
    #[tokio::test]
    async fn the_users_copy_is_the_one_opened() {
        let root = root("user-copy");
        std::fs::write(root.join(DATA_WELCOME), "PACKAGED\n").unwrap();
        std::fs::write(root.join(format!("{DATA_WELCOME}.user")), "USERCOPY\n").unwrap();

        let screen = run(&root, &[b"1\r", &[CTRL_W], b"C", b" "]).await;
        assert!(screen.contains("USERCOPY"), "{screen}");
        assert!(!screen.contains("PACKAGED"), "{screen}");
        assert!(screen.contains("取消更改..."), "{screen}");
        assert_eq!(
            std::fs::read(root.join(DATA_WELCOME)).unwrap(),
            b"PACKAGED\n"
        );
        assert_eq!(
            std::fs::read(root.join(format!("{DATA_WELCOME}.user"))).unwrap(),
            b"USERCOPY\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The whole round trip: `(S)` before anything has been previewed is not an answer, `CR`
    /// previews what the editor holds, `ESC` leaves the pager, and `(S)` after that writes both
    /// copies and says a reload is what makes it count.
    #[tokio::test]
    async fn a_saved_edit_lands_on_both_copies() {
        use std::os::unix::fs::PermissionsExt;

        let root = root("save");
        std::fs::write(root.join(DATA_WELCOME), "欢迎回来\n").unwrap();
        std::fs::set_permissions(
            root.join(DATA_WELCOME),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let screen = run(
            &root,
            &[
                b"1\r",
                b"HELLO\r",
                &[CTRL_W],
                b"S",
                b"\r",
                &[ESC],
                b"S",
                b" ",
            ],
        )
        .await;

        assert!(
            screen.contains("(P)预览, (C)取消 or (E)再编辑?"),
            "{screen}"
        );
        // The `(S)` typed before anything was previewed only asked the same question again: the
        // line that offers it is written once, after the pager has run.
        assert_eq!(count(&screen, "(S)保存, (P)预览"), 1, "{screen}");
        assert!(
            screen.contains("修改已完成，需要重新加载配置后才会生效..."),
            "{screen}"
        );

        let packaged = std::fs::read(root.join(DATA_WELCOME)).unwrap();
        let user = std::fs::read(root.join(format!("{DATA_WELCOME}.user"))).unwrap();
        assert_eq!(packaged, user);
        let text = String::from_utf8_lossy(&packaged).into_owned();
        assert!(
            text.contains("HELLO") && text.contains("欢迎回来"),
            "{text:?}"
        );

        // The mode `0640` is what C asks `open` for, and a mode argument only counts on create:
        // the packaged file that was already there keeps the `0600` it was given, and the `.user`
        // copy the page creates is the one that answers `0640` — minus whatever the harness umask
        // takes off it, which is why the world bits are what the test asks for.
        assert_eq!(
            std::fs::metadata(root.join(DATA_WELCOME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.join(format!("{DATA_WELCOME}.user")))
                .unwrap()
                .permissions()
                .mode()
                & 0o007,
            0
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `(E)` re-enters the editor, and the answer line after it is the plain one again — the
    /// `previewed = 0` of `:146`.
    #[tokio::test]
    async fn editing_again_clears_the_saved_answer() {
        let root = root("re-edit");
        std::fs::write(root.join(DATA_WELCOME), "欢迎回来\n").unwrap();

        let screen = run(
            &root,
            &[
                b"1\r",
                &[CTRL_W],
                b"\r",
                &[ESC],
                b"E",
                &[CTRL_W],
                b"C",
                b" ",
            ],
        )
        .await;

        // The previewed prompt is the plain one with `(S)保存, ` in front of it, so of the three
        // times the plain phrase was written only one carried that prefix: the line redrawn after
        // the second editor run is the plain one again.
        assert_eq!(
            count(&screen, "(P)预览, (C)取消 or (E)再编辑? [P]: "),
            3,
            "{screen}"
        );
        assert_eq!(count(&screen, "(S)保存, (P)预览"), 1, "{screen}");
        assert!(screen.contains("取消更改..."), "{screen}");
        // The cancel still wrote nothing, in either spelling of the path.
        assert_eq!(
            std::fs::read(root.join(DATA_WELCOME)).unwrap(),
            "欢迎回来\n".as_bytes()
        );
        assert!(!root.join(format!("{DATA_WELCOME}.user")).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
