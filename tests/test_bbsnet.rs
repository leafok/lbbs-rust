use super::*;

use std::sync::{Arc, Mutex};

use crate::config::{Config, DbConfig};
use crate::state::AppState;
use crate::term::{ByteIo, Pipe};

const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOKuTzmcyfM/BOgoRA6lr27l5NOnboaGuJ1cUVD4M7je";
const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIK3QrmKusG+6Z9z/du4ZeNu0F2sF7siNGSBGZz5nSVd+";

/// A terminal wired to a test end of the same pipe.
fn pair() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

/// A session with no database behind it, on the address the site would bind to. The action is
/// pre-set to the shuttle's own so `user_online_update` never reaches for a pool.
fn shuttle_session(root: &Path, is_ssh: bool) -> Session {
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
    let mut session = Session::new(
        state,
        root,
        "lbbs-test".into(),
        "127.0.0.1".into(),
        2323,
        is_ssh,
    );
    session.current_action = ACTION.to_string();
    session.current_action_tm = session::now();
    session
}

/// A server root holding one `conf/bbsnet.conf`, unique to this process and test name.
fn root_with_conf(name: &str, body: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-{name}-{}", std::process::id()));
    std::fs::create_dir_all(root.join("conf")).unwrap();
    std::fs::write(root.join(CONF_BBSNET), body).unwrap();
    root
}

fn station(host: &str, port: u16, use_ssh: bool, charset: &str) -> Station {
    Station {
        org_name: "测试单位".into(),
        site_name: "回声站".into(),
        host_name: host.into(),
        port: port.to_string(),
        use_ssh,
        charset: charset.into(),
    }
}

/// Everything the screen has written so far, with the ANSI moves kept: the tests look for the
/// cells C puts where, not for a rendered page.
async fn until(client: &mut Pipe, needle: &[u8]) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        seen.extend(client.take_available());
        if seen.windows(needle.len()).any(|window| window == needle) {
            return seen;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "screen never wrote {:?}; saw {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&seen)
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The screen has asked for a key; only then is it safe to type one. A prompt first drops
/// whatever the client had already sent (`reset_input`, and it keeps dropping until the
/// client has been quiet for 100 ms), so an answer sent with the prompt lands in that drain.
async fn type_after(client: &mut Pipe, needle: &[u8], keys: &[u8]) -> Vec<u8> {
    let seen = until(client, needle).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    client.push(keys);
    seen
}

#[test]
fn conf_keeps_valid_stations_and_skips_broken_lines() {
    let root = root_with_conf(
        "load",
        "\
# 注释整行跳过
	水木社区   新水木 bbs.newsmth.net\t23\tN\tUTF-8

LBBS MyBBS 127.0.0.1 0022 y GBK
坏端口 站 127.0.0.1 22x Y UTF-8
缺字段 站 127.0.0.1 23
错标记 站 127.0.0.1 23 YY UTF-8
太长 编码超长 127.0.0.1 23 N UTF-8-and-then-some-more-than-twenty-bytes
",
    );

    let stations = load_conf(&root).unwrap();
    assert_eq!(stations.len(), 2);
    // Tabs, runs of spaces and the trailing newline are all one delimiter to `strtok_r`.
    assert_eq!(
        stations[0],
        Station {
            org_name: "水木社区".into(),
            site_name: "新水木".into(),
            host_name: "bbs.newsmth.net".into(),
            port: "23".into(),
            use_ssh: false,
            charset: "UTF-8".into(),
        }
    );
    // `toupper(p[0]) == 'Y'`, and the port keeps the token rather than the number.
    assert_eq!(
        stations[1],
        Station {
            org_name: "LBBS".into(),
            site_name: "MyBBS".into(),
            host_name: "127.0.0.1".into(),
            port: "0022".into(),
            use_ssh: true,
            charset: "GBK".into(),
        }
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn strtol_accepts_what_the_resolver_would() {
    assert_eq!(strtol_decimal(b"23"), (23, true));
    assert_eq!(strtol_decimal(b"0022"), (22, true));
    assert_eq!(strtol_decimal(b"+7"), (7, true));
    assert_eq!(strtol_decimal(b"-1"), (-1, true));
    // `22x` is the value `strtol` stopped at, and the leftover is what makes it invalid.
    assert_eq!(strtol_decimal(b"22x"), (22, false));
    assert_eq!(strtol_decimal(b"telnet"), (0, false));
    assert_eq!(strtol_decimal(b"99999999999999999999"), (i64::MAX, true));
}

#[test]
fn grid_walks_four_stations_to_a_row() {
    let station = Station {
        org_name: "单位".into(),
        site_name: "站名".into(),
        host_name: "bbs.test".into(),
        port: "23".into(),
        use_ssh: false,
        charset: "UTF-8".into(),
    };

    let first = station_item(0, &station);
    assert_eq!((first.row, first.col), (2, 5));
    assert_eq!(first.name, "A");
    assert_eq!(first.text, "\x1b[1;36mA.\x1b[m 站名");

    // The row only turns over after the fourth station.
    assert_eq!(
        (station_item(3, &station).row, station_item(3, &station).col),
        (2, 65)
    );
    assert_eq!(
        (station_item(4, &station).row, station_item(4, &station).col),
        (3, 5)
    );
    assert_eq!(station_item(25, &station).name, "Z");
    // `'a' + 26 - 13`, the lower-case half of the hot keys.
    assert_eq!(station_item(26, &station).name, "a");
    assert_eq!(station_item(MAX_STATION - 1, &station).name, "z");
    assert_eq!(
        (
            station_item(MAX_STATION - 1, &station).row,
            station_item(MAX_STATION - 1, &station).col
        ),
        (14, 65)
    );
}

#[tokio::test]
async fn frame_draws_the_rules_c_writes() {
    let (mut term, mut client) = pair();
    refresh(&mut term).await;
    let seen = String::from_utf8_lossy(&client.take_available()).into_owned();

    // 78 dashes between a space on either side, at the top and again at row 22.
    let rule = format!(" {} ", "-".repeat(78));
    assert_eq!(seen.matches(&rule).count(), 2);
    assert!(seen.starts_with("\x1b[2J"), "{seen}");
    assert!(seen.contains("\x1b[1;1H"), "{seen}");
    // The side borders: column 80 is one cell past the screen, where C puts them.
    assert!(seen.contains("\x1b[2;1H|\x1b[2;80H|"), "{seen}");
    assert!(seen.contains("\x1b[18;1H|\x1b[18;80H|"), "{seen}");
    // Row 19 is the one rule that closes with the border characters instead of spaces.
    assert!(
        seen.contains(&format!("\x1b[19;1H|{}|", "-".repeat(78))),
        "{seen}"
    );
    assert!(
        seen.contains("\x1b[23;1H [\x1b[1;32mCtrl+C\x1b[m]退出"),
        "{seen}"
    );
}

#[tokio::test]
async fn detail_rows_pad_by_display_cells() {
    let (mut term, mut client) = pair();
    let station = Station {
        org_name: "示例站点".into(),
        site_name: "LBBS".into(),
        host_name: "bbs.test".into(),
        port: "2323".into(),
        use_ssh: true,
        charset: "GBK".into(),
    };
    selchange(&mut term, &station).await;
    let seen = String::from_utf8_lossy(&client.take_available()).into_owned();

    // Eight cells of CJK in a 20-cell field, so `%*s` owes twelve spaces rather than the six
    // a byte count of 示例站点 would give, and C's own two spaces come after them.
    assert!(
            seen.contains("\x1b[20;1H\x1b[K|\x1b[1m单位: \x1b[1;33m示例站点\x1b[m              \
                           站名: \x1b[1;33mLBBS\x1b[m                  类型: \x1b[1;33mSSH\x1b[m\x1b[20;80H|"),
            "{seen}"
        );
    // `%-20s` pads the host name out and `%-5s` leaves the four-digit port one short.
    assert!(
        seen.contains("连往: \x1b[1;33mbbs.test            \x1b[m  端口: \x1b[1;33m2323 \x1b[m"),
        "{seen}"
    );
    assert!(
        seen.ends_with("编码: \x1b[1;33mGBK\x1b[m\x1b[21;80H|"),
        "{seen}"
    );
}

#[tokio::test]
async fn progress_bar_inverts_the_part_it_has_reached() {
    let (mut term, mut client) = pair();
    progress_bar(&mut term, 0, PROGRESS_BAR_LEN).await;
    let empty = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(empty.starts_with("\x1b[4;1H"), "{empty}");
    // `(30 - 4) / 2` of room on either side of `  0%`.
    assert!(
        empty.contains(&format!(
            "|\x1b[46m\x1b[44m{:<13}{:>3}%{:<13}\x1b[m|\r\n",
            "", 0, ""
        )),
        "{empty}"
    );

    progress_bar(&mut term, 50, PROGRESS_BAR_LEN).await;
    let half = String::from_utf8_lossy(&client.take_available()).into_owned();
    let buf = format!("{:<13}{:>3}%{:<13}", "", 50, "");
    let (cyan, blue) = buf.split_at(PROGRESS_BAR_LEN / 2);
    assert!(
        half.contains(&format!("|\x1b[46m{cyan}\x1b[44m{blue}\x1b[m|\r\n")),
        "{half}"
    );
    assert!(
        half.contains(&format!(" {} \r\n", "-".repeat(PROGRESS_BAR_LEN))),
        "{half}"
    );
}

#[tokio::test]
async fn an_ssh_station_needs_an_ssh_login() {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-sshgate-{}", std::process::id()));
    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);
    let station = station("127.0.0.1", 2322, true, "UTF-8");

    // C returns 0 before it resolves anything, so this cannot reach the network.
    let (code, mut seen) = tokio::join! {
        connect(&mut term, &mut session, &station),
        type_after(&mut client, "只有在以SSH方式登陆本站时".as_bytes(), b"\r")
    };
    assert_eq!(code, 0);
    seen.extend(client.take_available());
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(
        seen.contains("只有在以SSH方式登陆本站时，才能使用SSH站点穿梭。"),
        "{seen}"
    );
    // No username prompt, because the gate answers before the credentials are asked for.
    assert!(!seen.contains("请输入用户名"), "{seen}");
}

#[tokio::test]
async fn a_name_that_will_not_resolve_is_reported() {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-dns-{}", std::process::id()));
    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);
    let station = station("no-such-station.invalid", 23, false, "UTF-8");

    let (code, mut seen) = tokio::join! {
        connect(&mut term, &mut session, &station),
        type_after(&mut client, "查找主机名失败".as_bytes(), b"\r")
    };
    assert_eq!(code, -1);
    seen.extend(client.take_available());
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(
        seen.contains("正在测试往 回声站 (no-such-station.invalid:23) 的Telnet连接"),
        "{seen}"
    );
    assert!(
        seen.contains("\x1b[1;32m连接进行中，按\x1b[1;33mCtrl+C\x1b[1;32m中断。"),
        "{seen}"
    );
}

#[tokio::test]
async fn the_relay_carries_both_ways_in_the_station_charset() {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-relay-{}", std::process::id()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // The station side echoes what the shuttle forwards and hangs up, which is how the relay
    // learns the conversation is over.
    let station_read = Arc::new(Mutex::new(Vec::new()));
    let read_log = station_read.clone();
    let echo = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).await.unwrap();
        read_log.lock().unwrap().extend_from_slice(&buf[..n]);
        // `世界` in GBK, which the shuttle has to turn back into UTF-8.
        peer.write_all(&GBK.encode("世界").0).await.unwrap();
        peer.shutdown().await.unwrap();
    });

    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);
    let station = station("127.0.0.1", port, false, "GBK");

    let (code, seen) = tokio::join! {
        connect(&mut term, &mut session, &station),
        async {
            // Nothing typed before the connection is up: a key the connect wait reads is a
            // key C throws away.
            tokio::time::sleep(Duration::from_millis(300)).await;
            client.push("你好".as_bytes());
            until(&mut client, "世界".as_bytes()).await
        }
    };

    assert_eq!(code, 1);
    echo.await.unwrap();
    // Out went GBK, and what came back was re-encoded for the session's own charset.
    assert_eq!(*station_read.lock().unwrap(), GBK.encode("你好").0.as_ref());
    assert!(seen.windows(6).any(|window| window == "世界".as_bytes()));
}

#[tokio::test]
async fn a_station_charset_the_converter_lacks_is_refused() {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-charset-{}", std::process::id()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = tokio::spawn(async move {
        let (peer, _) = listener.accept().await.unwrap();
        // Hold the connection open until the shuttle gives it up.
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(peer);
    });

    let (mut term, mut client) = pair();
    term.set_charset(Charset::Utf8);
    let mut session = shuttle_session(&root, false);
    let station = station("127.0.0.1", port, false, "BIG5");

    // The failure comes where C opens its descriptors, after the connect and its message, and
    // C's answer for it is `-1` with nothing more on screen.
    assert_eq!(connect(&mut term, &mut session, &station).await, -1);
    let seen =
        String::from_utf8_lossy(&until(&mut client, "连接成功".as_bytes()).await).into_owned();
    assert!(!seen.contains("按任意键"), "{seen}");
    accepted.abort();
}

#[test]
fn known_hosts_decides_as_libssh_does() {
    let dir = std::env::temp_dir().join(format!("lbbs-bbsnet-keys-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("known_hosts");
    let key_a = russh::keys::parse_public_key_base64(KEY_A).unwrap();
    let key_b = russh::keys::parse_public_key_base64(KEY_B).unwrap();

    // An unseen host is recorded and accepted, and the recording is what the log line claims.
    assert_eq!(
        decide_key(&path, "bbs.test", 22, &key_a),
        KeyVerdict::Learned
    );
    assert!(std::fs::read_to_string(&path).unwrap().contains("bbs.test"));
    assert_eq!(
        decide_key(&path, "bbs.test", 22, &key_a),
        KeyVerdict::Accepted
    );
    // A different key for a recorded host is C's `SSH_KNOWN_HOSTS_CHANGED`.
    assert_eq!(
        decide_key(&path, "bbs.test", 22, &key_b),
        KeyVerdict::Changed
    );
    // The port is part of the identity, so a station on 2322 is still unknown.
    assert_eq!(
        decide_key(&path, "bbs.test", 2322, &key_b),
        KeyVerdict::Learned
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_unknown_entry_names_itself() {
    let root = std::env::temp_dir().join(format!("lbbs-bbsnet-entry-{}", std::process::id()));
    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);

    // `money_balance` is a symbol of `libgame.so` (`utils/game/money.c:23`) that C's `dlsym`
    // branch would find, and that the `#else` table this port mirrors does not know.
    let (_, mut seen) = tokio::join! {
        exec_mbem(&mut term, &mut session, "C@mod:game/so/libgame.so#money_balance"),
        type_after(&mut client, "未知入口 [money_balance]".as_bytes(), b" ")
    };
    seen.extend(client.take_available());
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(seen.contains("未知入口 [money_balance] !!\r\n"), "{seen}");

    // With no `#` at all C hands a NULL pointer to `strcasecmp`; the port reports the empty
    // entry instead of crashing on it.
    let (_, mut seen) = tokio::join! {
        exec_mbem(&mut term, &mut session, "X@mod:lib/libbbs.so"),
        type_after(&mut client, "未知入口 []".as_bytes(), b" ")
    };
    seen.extend(client.take_available());
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(seen.contains("未知入口 [] !!\r\n"), "{seen}");

    // A parameter with no `@mod:` is not this entry's business, and nothing is drawn.
    exec_mbem(&mut term, &mut session, "N").await;
    assert!(client.take_available().is_empty());
}

#[tokio::test]
async fn the_menu_follows_the_cursor_and_exits_on_ctrl_c() {
    let root = root_with_conf(
        "menu",
        "\
单位甲 甲站 bbs-a.test 23 N UTF-8
单位乙 乙站 bbs-b.test 22 Y GBK
单位丙 丙站 bbs-c.test 23 N UTF-8
",
    );
    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);

    // Four steps of one item each is what C's `KEY_DOWN` handler does to walk a row of the
    // grid, and `KEY_RIGHT` is the same call once.
    client.push(b"\x1b[B\x1b[C\x03");
    assert_eq!(bbs_net(&mut term, &mut session).await, 0);
    let seen = String::from_utf8_lossy(&client.take_available()).into_owned();

    assert!(seen.contains("\x1b[2;5H\x1b[1;36mA.\x1b[m 甲站"), "{seen}");
    assert!(seen.contains("\x1b[2;25H\x1b[1;36mB.\x1b[m 乙站"), "{seen}");
    assert!(seen.contains("\x1b[2;45H\x1b[1;36mC.\x1b[m 丙站"), "{seen}");
    // The cursor is two columns left of the item it marks.
    assert!(seen.contains("\x1b[2;3H>"), "{seen}");

    // Four steps past C wrap round to B, and the detail rows follow the cursor.
    let start = seen.find("单位甲").unwrap();
    let after_down = seen[start..].find("单位乙").unwrap() + start;
    let after_right = seen[after_down..].find("单位丙").unwrap() + after_down;
    assert!(
        seen[after_down..after_right].contains("bbs-b.test"),
        "{seen}"
    );
    assert!(seen[after_down..after_right].contains("SSH"), "{seen}");
    assert!(seen[after_down..after_right].contains("GBK"), "{seen}");
    // One more step reaches the third station, and the cursor with it.
    assert!(seen[after_right..].starts_with("单位丙"), "{seen}");
    assert!(seen[after_right..].contains("bbs-c.test"), "{seen}");
    assert!(seen[after_right..].contains("Telnet"), "{seen}");
    assert!(seen.ends_with("\x1b[21;80H|"), "{seen}");
    assert!(seen.contains("\x1b[2;23H \x1b[2;43H>"), "{seen}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn an_empty_table_still_offers_the_zeroed_station() {
    let root = root_with_conf("empty", "# 只有注释\n");
    let (mut term, mut client) = pair();
    let mut session = shuttle_session(&root, false);

    // The hand-built menu has no rows to page through, so C's `display_menu` complains and
    // waits; the cursor then still selects `bbsnet_conf[0]`, which was never written to.
    let (code, mut seen) = tokio::join! {
        bbs_net(&mut term, &mut session),
        async {
            // The first key answers the pager prompt, the second leaves the menu.
            let mut seen = type_after(&mut client, "没有可选项".as_bytes(), b"\r\x03").await;
            seen.extend(client.take_available());
            seen
        }
    };
    assert_eq!(code, 0);
    seen.extend(client.take_available());
    let seen = String::from_utf8_lossy(&seen).into_owned();
    assert!(seen.contains("没有可选项"), "{seen}");
    // Empty fields, and the padding `%*s` owes a full 20 cells for each of them.
    assert!(seen.contains("\x1b[20;1H\x1b[K|\x1b[1m单位: \x1b[1;33m\x1b[m                      \
                               站名: \x1b[1;33m\x1b[m                      类型: \x1b[1;33mTelnet\x1b[m"), "{seen}");
    assert!(
        seen.contains("连往: \x1b[1;33m                    \x1b[m  端口: \x1b[1;33m     \x1b[m"),
        "{seen}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}
