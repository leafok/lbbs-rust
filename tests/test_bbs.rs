use super::*;
use crate::term::{ByteIo, Pipe};

/// Run the shutdown item with `answer` typed a quarter second in, which is past the
/// `reset_input` (`igetch_reset`) that opens the question and so cannot be discarded by it.
async fn shutdown(answer: &[u8]) -> (MenuReturn, bool, Vec<u8>) {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let (ret, ()) = tokio::join!(
        bbs_do_choice(&mut term, &mut session, Action::Shutdown, "", false),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(answer);
        }
    );

    (ret, session.state.is_exiting(), client.take_available())
}

/// `clearscr` then `moveto(1, 1)` then the question with the `N` it starts out holding.
const PROMPT: &str = "\x1b[1;1H真的要关闭系统吗[y/N]? N";

/// The panel rotates at the root only. The three depths are the ones a session actually
/// holds there: `MenuNav::new` is the root, and every `@LIST`/submenu entry is one deeper.
#[test]
fn the_active_board_panel_rotates_only_at_the_root_menu() {
    assert!(panel_due(0, PANEL_TICK_SECS));
    assert!(panel_due(0, PANEL_TICK_SECS + 1));
    assert!(!panel_due(0, PANEL_TICK_SECS - 1));
    for depth in 1..=3 {
        for elapsed in [0, PANEL_TICK_SECS, PANEL_TICK_SECS + 60] {
            assert!(!panel_due(depth, elapsed), "depth {depth} never rotates");
        }
    }
}

/// The menu a user spends their time in is the one `bbs_center` builds, so the root the
/// gate tests has to be the root the cursor moves on.
#[test]
fn a_fresh_nav_is_at_the_depth_the_panel_gate_ticks_at() {
    let menus = crate::menu::MenuSet::default();
    let nav = MenuNav::new(&menus);
    assert!(panel_due(nav.depth(), PANEL_TICK_SECS));
}

fn screen_of(painted: &[u8]) -> String {
    String::from_utf8_lossy(painted).into_owned()
}

#[tokio::test]
async fn shutdown_asks_before_it_orders_the_exit() {
    let (ret, exiting, painted) = shutdown(b"\r").await;
    assert_eq!(ret, MenuReturn::Redraw);
    assert!(
        screen_of(&painted).starts_with("\x1b[2J"),
        "`clearscr` comes first: {}",
        screen_of(&painted)
    );
    assert!(
        screen_of(&painted).contains(PROMPT),
        "{}",
        screen_of(&painted)
    );
    assert!(!exiting, "Enter keeps the default answer, which is `N`");
}

/// The answer is `char buf[2]` shown one cell wide and the cell already holds the `N`, so
/// `get_data`'s printable arm (`screen.c:596`) rings the bell instead of taking the key:
/// typing over the default is not a way to close the system. Erasing it first is, and
/// `io.c:1079-1083` folds Ctrl-H and DEL onto the one erase key C then tests for.
#[tokio::test]
async fn shutdown_takes_the_y_only_after_the_default_is_deleted() {
    for answer in [b"y\r".as_slice(), b"Y\r".as_slice()] {
        let (_, exiting, painted) = shutdown(answer).await;
        assert!(
            !exiting,
            "{:?} must leave it running: {}",
            String::from_utf8_lossy(answer),
            screen_of(&painted)
        );
        assert!(
            screen_of(&painted).contains('\u{7}'),
            "{:?} must be refused with a bell",
            String::from_utf8_lossy(answer)
        );
    }

    for answer in [
        b"\x7fy\r".as_slice(),
        b"\x7fY\r".as_slice(),
        b"\x08y\r".as_slice(),
    ] {
        let (_, exiting, painted) = shutdown(answer).await;
        assert!(
            exiting,
            "{:?} must close the system: {}",
            String::from_utf8_lossy(answer),
            screen_of(&painted)
        );
    }
}

/// `@RELOADCONF` says what it did and takes a key for it, then still hands the menu back —
/// and the ask itself stays standing for the daemon's reload loop.
#[tokio::test]
async fn reload_conf_confirms_on_screen_before_it_returns_to_the_menu() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let (ret, ()) = tokio::join!(
        bbs_do_choice(&mut term, &mut session, Action::ReloadConf, "", false),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b" ");
        }
    );

    assert_eq!(ret, MenuReturn::Redraw);
    assert!(
        session.state.take_reload_request(),
        "the reload is still asked for"
    );
    let screen = screen_of(&client.take_available());
    assert!(
        screen.starts_with("\x1b[2J\x1b[1;1H已发送指令\r\n"),
        "{screen}"
    );
    assert!(screen.contains("按任意键继续"), "{screen}");
}

/// An erased answer is an empty one — `buf[0]` is the terminator `get_data` left there,
/// which is neither a `Y` nor anything else.
#[tokio::test]
async fn shutdown_with_the_answer_erased_leaves_the_system_running() {
    for answer in [b"\x7f\r".as_slice(), b"\x7fn\r".as_slice()] {
        let (ret, exiting, _) = shutdown(answer).await;
        assert_eq!(ret, MenuReturn::Redraw);
        assert!(
            !exiting,
            "{:?} must leave it running",
            String::from_utf8_lossy(answer)
        );
    }
}

/// The two roster items reach the page with the flag that titles it — which is all the
/// menu has to say about them — and both answer with `REDRAW`. The tables are empty, so
/// what identifies the variant is the column header the flag picks.
#[tokio::test]
async fn the_roster_items_open_the_two_pages() {
    for (action, header) in [
        (Action::ListUser, "上次登陆距今"),
        (Action::ListOnlineUser, "在线时长"),
    ] {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        let (ret, ()) = tokio::join!(
            bbs_do_choice(&mut term, &mut session, action, "", false),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(b"\x1b");
            }
        );

        assert_eq!(ret, MenuReturn::Redraw);
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(
            screen.contains(header),
            "{} must draw the {header} column: {screen}",
            action.name()
        );
    }
}

/// `@FAVOR_TOPIC` (`menu_proc.c:372-380`) passes its parameter to the screen, which ignores
/// it, and redraws the menu whatever the list does on the way out. With nothing starred the
/// page has only its header and key line to show.
#[tokio::test]
async fn the_topic_item_opens_the_star_list() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let (ret, ()) = tokio::join!(
        bbs_do_choice(&mut term, &mut session, Action::FavorTopic, "Topic", false),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b"\x1b");
        }
    );

    assert_eq!(ret, MenuReturn::Redraw);
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(screen.contains("[主题收藏]"), "{screen}");
    assert!(screen.contains("移除[\x1b[1;32m-\x1b[0;37m"), "{screen}");
}

/// `@DATAMGR` ignores its menu parameter, so the dispatch only has to reach the list and read
/// the number back: an answer that is not one of the eight rows opens nothing, and the return
/// is the `REDRAW` C gives on every one of its ways out.
#[tokio::test]
async fn the_data_item_draws_the_list_it_asks_a_number_for() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    session.root = std::env::temp_dir().join(format!("lbbs-datamgr-menu-{}", std::process::id()));

    let (ret, ()) = tokio::join!(
        bbs_do_choice(&mut term, &mut session, Action::DataMgr, "", false),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b"9\r");
        }
    );

    assert_eq!(ret, MenuReturn::Redraw);
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(screen.contains("数据文件列表: "), "{screen}");
    assert!(screen.contains(" 1. 进站欢迎"), "{screen}");
    assert!(screen.contains(" 8. 穿梭配置"), "{screen}");
    assert!(!screen.contains("再编辑"), "{screen}");
}

/// `@EDIT_INTRO` and `@EDIT_SIGN` both begin with a query, so on a session with nothing behind
/// it they fail before either screen is drawn — and the menu still comes back, which is C's
/// `REDRAW` after the `log_error` of a negative return (`menu_proc.c:402-420`).
#[tokio::test]
async fn the_profile_items_give_the_menu_back_when_their_query_fails() {
    let (_client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let ret = bbs_do_choice(&mut term, &mut session, Action::EditIntro, "", false).await;

    assert_eq!(ret, MenuReturn::Redraw);

    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    let (ret, ()) = tokio::join!(
        bbs_do_choice(&mut term, &mut session, Action::EditSign, "", false),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b"0\r");
        }
    );

    assert_eq!(ret, MenuReturn::Redraw);
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(
        screen.contains("请输入需要编辑的签名档编号（1-3）: "),
        "{screen}"
    );
    assert!(!screen.contains("再编辑"), "{screen}");
}

/// `@RunMBEM` answers with the entry named after the `#` in its own parameter, so the shuttle
/// item needs no menu id and no privilege beyond the item's. With no stations configured the
/// frame still draws, the pager still complains about the empty selection, and the key that
/// leaves it hands the menu back.
#[tokio::test]
async fn the_mbem_item_opens_the_shuttle() {
    let root = std::env::temp_dir().join(format!("lbbs-runmbem-{}", std::process::id()));
    std::fs::create_dir_all(root.join("conf")).unwrap();
    std::fs::write(root.join("conf/bbsnet.conf"), "# 暂无站点\n").unwrap();

    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    session.root = root.clone();

    let (ret, ()) = tokio::join!(
        bbs_do_choice(
            &mut term,
            &mut session,
            Action::RunMbem,
            "N@mod:lib/libbbs.so#bbs_net",
            false
        ),
        async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b"\r\x03");
        }
    );

    assert_eq!(ret, MenuReturn::Redraw);
    let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(screen.contains(" [\x1b[1;32mCtrl+C\x1b[m]退出"), "{screen}");
    assert!(screen.contains("没有可选项"), "{screen}");
    std::fs::remove_dir_all(&root).unwrap();
}

/// A session takes the echo away from the client before it asks anything. The `*` mask at the
/// login prompt only hides what this side prints; without the negotiation a telnet client
/// still echoes what it typed itself, and the password walks across the screen in clear.
#[tokio::test]
async fn a_session_negotiates_the_echo_before_it_asks_anything() {
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();

    // The client stays mute, so the session is still waiting on the charset question when the
    // deadline cuts it off; nothing past that prompt is reached, database included.
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        bbs_main(&mut term, &mut session),
    )
    .await;

    let wire = client.take_available();
    let body = wire
        .strip_prefix(&[0xff, 0xfb, 0x01, 0xff, 0xfb, 0x03][..])
        .unwrap_or_else(|| panic!("the session must open with the negotiation: {wire:?}"));
    // UTF-8 is the default at this point, so the prompt that follows is still plain text.
    assert!(
        String::from_utf8_lossy(body).contains("Choose character set"),
        "the negotiation must come first: {body:?}"
    );
}

/// The shipped `var/bbs_top_menu.conf` is a title, a screen body, and no items at all. C
/// enters `show_top10_menu`'s read loop only when `display_menu` returns 0, so such a menu
/// costs the single press `press_any_key` already asked for; the loop would ask for a second.
#[tokio::test]
async fn an_empty_top10_menu_ends_with_the_press_it_asks_for() {
    let set = crate::menu::MenuSet::parse(
        b"%menu M_TOP10\ntitle 0, 0, \"Top\"\nscreen 2, 0, S_TOP10\n%\n%S_TOP10\nten\n%\n",
    )
    .unwrap();
    let mut session = crate::session::offline_session();
    session.state.publish_top10(set);
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));

    let keyed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\r");
        client
    });

    // A menu still stuck in its read loop never returns: the press it takes is the only one
    // this test offers.
    let visited = tokio::time::timeout(
        Duration::from_secs(2),
        show_top10_menu(&mut term, &mut session),
    )
    .await;
    assert!(visited.is_ok(), "the empty menu must end after one press");

    let mut client = keyed.await.unwrap();
    let painted = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(painted.contains("没有可选项"), "{painted}");
}

/// The other half of `menu_proc.c:295-330`: a menu that *does* open a read loop closes it on
/// the idle clock, exactly like `bbs_center` does. The timeout alone did not end the visit —
/// only a timeout with `BBS_max_user_idle_time` gone by since the last key does — and a loop
/// that never ends holds one of `MaxClient` slots with nobody at the keyboard.
#[tokio::test]
async fn the_top10_menu_closes_when_the_idle_clock_runs_out() {
    let set = crate::menu::MenuSet::parse(
        "%S_TOP\none\n%\n%menu M_TOP10\nscreen 0, 0, S_TOP\n\
              @LIST_ONLINE_USER 0, 0, 0, 0, \"\", \"在线看板\"\n%\n"
            .as_bytes(),
    )
    .unwrap();
    let mut session = crate::session::offline_session();
    session.state.publish_top10(set);
    // The clock is already past its limit, so the first timeout has to close the screen; the
    // client is never asked for a key.
    session.last_access_tm = session::now() - IDLE_LIMIT;

    let (_client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let closed = tokio::time::timeout(
        Duration::from_secs(2),
        show_top10_menu(&mut term, &mut session),
    )
    .await;
    assert!(closed.is_ok(), "the idle timeout must end the visit");
}

/// A key typed inside the menu restarts that clock, which is what keeps an active reader at
/// the top-ten list: five seconds of age is nothing against the limit, and the press has to
/// be what pulls the clock back.
#[tokio::test]
async fn a_key_inside_the_top10_menu_keeps_it_open() {
    let set = crate::menu::MenuSet::parse(
        "%S_TOP\none\n%\n%menu M_TOP10\nscreen 0, 0, S_TOP\n\
              @LIST_ONLINE_USER 0, 0, 0, 0, \"\", \"在线看板\"\n%\n"
            .as_bytes(),
    )
    .unwrap();
    let mut session = crate::session::offline_session();
    session.state.publish_top10(set);
    let stale = session::now() - 5;
    session.last_access_tm = stale;

    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    // The menu opens, so the item it drew is on the wire; the Down key that follows is
    // activity, which moves `last_access_tm` up from the back-dated value.
    let keyed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\x1b[B");
        client
    });

    let _ = tokio::time::timeout(
        Duration::from_millis(600),
        show_top10_menu(&mut term, &mut session),
    )
    .await;
    let mut client = keyed.await.unwrap();
    let painted = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(
        painted.contains("在线看板"),
        "the menu must be open: {painted}"
    );
    assert!(
        session.last_access_tm > stale,
        "the key has to refresh the idle clock, left at {}",
        session.last_access_tm
    );
}

/// What C's `kill(0, SIGTERM)` achieved: the flag was raised, every child's key wait answered
/// at once, and each session ran its own exit path before the parent's 15 s wait was up. A
/// session that ignores the flag is only ever abandoned by the drain, so the daemon reports
/// "Give up waiting for N sessions" where C reported the children gone.
#[tokio::test]
async fn a_session_waits_on_the_client_not_on_the_shutdown() {
    let (client, server) = Pipe::new();
    let mut session = crate::session::offline_session();
    let state = session.state.clone();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    term.set_exit_watch(state.exit_watch());

    let parked = tokio::spawn(async move {
        bbs_main(&mut term, &mut session).await;
    });
    // The client stays mute, so the session is inside a key wait when the notify lands.
    tokio::time::sleep(Duration::from_millis(250)).await;
    state.exit();

    let ended = tokio::time::timeout(Duration::from_secs(2), parked).await;
    assert!(ended.is_ok(), "the session must end on the notify");
    drop(client);
}

/// `bbs_main.c:378-379` hands the greeting to `press_any_key_ex`, and `screen.c:101-104` moves
/// to the bottom row and clears that row *before* printing. Printing the greeting at whatever
/// cursor position the welcome left, and then clearing the row for an empty prompt, is what
/// made the line never reach the client.
#[tokio::test]
async fn the_ssh_greeting_is_the_bottom_row_prompt() {
    let (mut client, server) = Pipe::new();
    let mut session = crate::session::offline_session();
    session.is_ssh = true;
    let mut term = Terminal::new(ByteIo::Pipe(server));

    let (ran, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(3), bbs_main(&mut term, &mut session)),
        async {
            // The charset rule, the width rule, then the greeting: every key has to land
            // after the `igetch_reset` opening the wait it answers.
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(400)).await;
                client.push(b"\r");
            }
        }
    );
    assert!(
        ran.is_err(),
        "the session continues into its menu after the greeting"
    );

    let wire = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(
        wire.contains(
            "\x1b[24;0H\x1b[K\u{1b}[1m 欢迎使用ssh方式访问 \u{1b}[1;33m按任意键继续...\u{1b}[m"
        ),
        "the greeting must come from the prompt that clears the row: {wire:?}"
    );
}
