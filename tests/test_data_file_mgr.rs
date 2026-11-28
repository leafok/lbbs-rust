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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let root = root("save");
    std::fs::write(root.join(DATA_WELCOME), "欢迎回来\n").unwrap();
    #[cfg(unix)]
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
    // takes off it, which is why the world bits are what the test asks for. There is no mode
    // to ask on Windows, so only the copies above are checked there.
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(root.join(DATA_WELCOME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    #[cfg(unix)]
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
