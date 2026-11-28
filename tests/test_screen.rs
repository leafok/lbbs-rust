use super::*;
use crate::paths::MAX_SPLIT_FILE_LINES;
use crate::term::{ByteIo, Pipe};

/// A terminal wired to a test peer plus the peer itself.
fn pair() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

/// `igetch_reset` swallows anything queued in its first 100 ms window, so keys have
/// to arrive after it has gone quiet.
const AFTER_RESET: Duration = Duration::from_millis(250);

#[derive(Default)]
struct TestHooks {
    msg: String,
    help_enabled: bool,
    help_shown: Option<String>,
}

impl PagerHooks for TestHooks {
    async fn on_activity(&mut self) {}

    async fn key_handler(&mut self, _ch: &mut u32, ctx: &mut DisplayCtx) -> i32 {
        ctx.msg = self.msg.clone();
        0
    }

    fn help_enabled(&mut self) -> bool {
        self.help_enabled
    }

    fn set_help_enabled(&mut self, enabled: bool) {
        self.help_enabled = enabled;
    }

    async fn show_help(&mut self, _term: &mut Terminal, help_filename: &str) -> u32 {
        self.help_shown = Some(help_filename.to_string());
        0
    }
}

fn thirty_lines() -> Vec<u8> {
    // `split_data_lines` is a do/while, so the closing newline of the last line
    // yields a 31st empty display line and the percentages are out of 31.
    (1..=30)
        .map(|i| format!("line {i}\n"))
        .collect::<String>()
        .into_bytes()
}

/// Whether a byte run appears anywhere in the wire traffic.
fn sent(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn clear_row_range_spans_inclusively() {
    let (mut term, mut client) = pair();
    clrline(&mut term, 2, 4);
    clrtobot(&mut term, 5);
    clearscr(&mut term);
    term.flush().await.unwrap();
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        "\x1b[2;0H\x1b[K\x1b[3;0H\x1b[K\x1b[4;0H\x1b[K\
             \x1b[5;0H\x1b[J\x1b[5;0H\x1b[2J\x1b[1;1H"
    );
}

#[tokio::test]
async fn show_top_pads_against_the_whole_middle_field() {
    let (mut term, mut client) = pair();
    show_top(&mut term, "左", "中", "右");
    term.flush().await.unwrap();
    // Each ideograph costs two cells, so the gaps are 44-2-2 and 36-2.
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        format!(
            "\x1b[1;0H\x1b[K\x1b[1;44;33m左\x1b[37m{}中\x1b[33m{}右\x1b[m",
            " ".repeat(40),
            " ".repeat(34)
        )
    );
}

#[tokio::test]
async fn show_bottom_carries_time_message_and_user() {
    let (mut term, mut client) = pair();
    let now = chrono::Local::now().timestamp();
    show_bottom(&mut term, "看板", "tester", now - 3720);
    term.flush().await.unwrap();
    let out = String::from_utf8(client.take_available()).unwrap();
    assert!(
        out.starts_with("\x1b[24;0H\x1b[K\x1b[1;44;33m时间[\x1b[36m"),
        "{out}"
    );
    assert!(out.contains("\x1b[33m]看板"), "{out}");
    assert!(
        out.contains("用户[\x1b[36mtester\x1b[33m][\x1b[36m1\x1b[33m:\x1b[36m02\x1b[33m]\x1b[m"),
        "{out}"
    );
}

#[tokio::test]
async fn press_any_key_shows_the_prompt_and_returns_the_key() {
    let (mut term, mut client) = pair();
    let task = tokio::spawn(async move { press_any_key(&mut term).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"q");
    assert_eq!(task.await.unwrap(), b'q' as u32);
    let out = String::from_utf8(client.take_available()).unwrap();
    assert!(
        out.ends_with("\x1b[24;0H\x1b[K                           \x1b[1;33m按任意键继续...\x1b[m"),
        "{out}"
    );
}

#[tokio::test]
async fn pager_draws_a_page_then_the_status_row() {
    let (mut term, mut client) = pair();
    let data = thirty_lines();
    let lines = text::split_data_lines(&data, SCREEN_COLS, true, true, MAX_SPLIT_FILE_LINES);
    let mut hooks = TestHooks {
        msg: "| 测试 |".to_string(),
        ..Default::default()
    };

    let task = tokio::spawn(async move {
        display_data(&mut term, &data, &lines, 0, &mut hooks, "help.txt", 5).await
    });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"\x1b[D");
    assert_eq!(task.await.unwrap(), KEY_LEFT);

    let out = client.take_available();
    assert!(
        out.starts_with(b"\x1b[1;0H\x1b[K\x1b[2;0H\x1b[K"),
        "page starts with a clear"
    );
    assert!(sent(&out, b"\x1b[2;0H\x1b[Kline 2\n"));
    assert!(sent(&out, b"\x1b[23;0H\x1b[Kline 23\n"));
    assert!(
        !sent(&out, b"\x1b[24;0H\x1b[Kline 24\n"),
        "row 24 is the status row"
    );

    // 第23行 of 30 is 76%, and the row is padded out to the full 80 cells.
    assert!(
            sent(
                &out,
                b"\x1b[24;0H\x1b[1;44;33m\xe7\xac\xac\x1b[32m1\x1b[33m-\x1b[32m23\x1b[33m\xe8\xa1\x8c (\x1b[32m74%\x1b[33m) | \xe6\xb5\x8b\xe8\xaf\x95 |"
            ),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
    // 80 cells minus the 23 the prefix and message occupy, then the colour reset.
    assert!(sent(&out, format!("{}\x1b[m", " ".repeat(57)).as_bytes()));
}

/// The file pager measures a row by its cells and not its bytes. The C loader wraps the
/// file with `skip_ctrl_seq` (`file_loader.c:87`), which is what keeps a row of
/// `data/welcome.txt` that spends 15 of its 79 bytes on colour sequences on one screen
/// row; counted as bytes it breaks in the middle and the logo arrives in fragments.
#[tokio::test]
async fn display_file_wraps_a_logo_row_by_cells_not_bytes() {
    let (mut term, mut client) = pair();
    let logo = format!("\x1b[1;31m{}  \x1b[0m\n", "A".repeat(70));
    assert!(
        logo.len() > SCREEN_COLS && term.width(logo.as_bytes(), true) < SCREEN_COLS,
        "the fixture must be one row too wide to measure by bytes"
    );
    let second = format!("\x1b[32m{}\x1b[m\n", "B".repeat(20));
    let data = format!("{logo}{second}").into_bytes();

    let mut hooks = TestHooks::default();
    let task = tokio::spawn(async move { display_file(&mut term, &mut hooks, &data, 0).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"\x1b[D");
    assert_eq!(task.await.unwrap(), KEY_LEFT);

    let out = client.take_available();
    assert!(
        sent(&out, format!("\x1b[1;0H\x1b[K{logo}").as_bytes()),
        "the logo row must reach the screen unbroken: {}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        sent(&out, format!("\x1b[2;0H\x1b[K{second}").as_bytes()),
        "the row after it must still be on row 2: {}",
        String::from_utf8_lossy(&out)
    );
    // Three display lines, one page.
    assert!(
            sent(
                &out,
                b"\x1b[24;0H\x1b[1;44;33m\xe7\xac\xac\x1b[32m1\x1b[33m-\x1b[32m3\x1b[33m\xe8\xa1\x8c (\x1b[32m100%\x1b[33m)"
            ),
            "{}",
            String::from_utf8_lossy(&out)
        );
}

#[tokio::test]
async fn pager_scrolls_one_line_with_a_newline() {
    let (mut term, mut client) = pair();
    let data = thirty_lines();
    let lines = text::split_data_lines(&data, SCREEN_COLS, true, true, MAX_SPLIT_FILE_LINES);
    let mut hooks = TestHooks::default();

    let task = tokio::spawn(async move {
        display_data(&mut term, &data, &lines, 0, &mut hooks, "help.txt", 5).await
    });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"\r\x1b[D");
    assert_eq!(task.await.unwrap(), KEY_LEFT);

    let out = client.take_available();
    // CR clears the bottom row, emits a bare newline, and redraws a single line.
    assert!(sent(&out, b"\x1b[24;0H\x1b[K\n"));
    assert!(sent(&out, b"\x1b[23;0H\x1b[Kline 24\n"));
    assert!(
            sent(
                &out,
                b"\x1b[24;0H\x1b[1;44;33m\xe7\xac\xac\x1b[32m2\x1b[33m-\x1b[32m24\x1b[33m\xe8\xa1\x8c (\x1b[32m77%\x1b[33m) "
            ),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
}

#[tokio::test]
async fn pager_returns_on_timeout_and_at_the_end_of_input() {
    let (mut term, _client) = pair();
    let data = thirty_lines();
    let lines = text::split_data_lines(&data, SCREEN_COLS, true, true, MAX_SPLIT_FILE_LINES);
    let mut hooks = TestHooks::default();
    let task = tokio::spawn(async move {
        display_data(&mut term, &data, &lines, 0, &mut hooks, "help.txt", 0).await
    });
    // `igetch_t(0)` reports a timeout without waiting, so the pager cannot loop.
    assert_eq!(task.await.unwrap(), KEY_TIMEOUT);
}

#[tokio::test]
async fn str_input_echoes_and_backspaces() {
    let (mut term, mut client) = pair();
    let task = tokio::spawn(async move { str_input_ex(&mut term, b"ab", 8, Echo::On, 5).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"cd\x08e\r");
    assert_eq!(task.await.unwrap().unwrap(), b"abce".to_vec());
    // The prefilled buffer belongs to the caller's drawing, only typing is echoed.
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        "cd\x1b[D \x1b[De\r\n"
    );
}

#[tokio::test]
async fn str_input_rings_the_bell_at_the_limit() {
    let (mut term, mut client) = pair();
    let task = tokio::spawn(async move { str_input(&mut term, 4, Echo::On, 5).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"abcd\r");
    assert_eq!(task.await.unwrap().unwrap(), b"abc".to_vec());
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        "abc\x07\r\n"
    );
}

#[tokio::test]
async fn str_input_hides_the_password() {
    let (mut term, mut client) = pair();
    let task = tokio::spawn(async move { str_input(&mut term, 8, Echo::Off, 5).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push("中a\x08\r".as_bytes());
    // Two cells charged per multibyte character, one per deleted character.
    assert_eq!(task.await.unwrap().unwrap(), "中".as_bytes().to_vec());
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        "***\x1b[D \x1b[D\r\n"
    );
}

#[tokio::test]
async fn the_client_is_told_to_stop_echoing() {
    let (mut term, mut client) = pair();
    set_input_echo(&mut term, false).await;
    // `IAC WILL ECHO`, `IAC WILL SGA` — option bytes, and the only thing a `NOECHO` prompt
    // needs to be more than a mask the client can still talk over.
    assert!(
        client
            .take_available()
            .starts_with(&[0xff, 0xfb, 0x01, 0xff, 0xfb, 0x03]),
        "the negotiation must go out raw"
    );

    let (mut term, mut client) = pair();
    set_input_echo(&mut term, true).await;
    // C's other arm is the bare byte 131 that no telnet option defines.
    assert_eq!(client.take_available(), vec![0x83]);
}

#[tokio::test]
async fn get_data_inserts_at_the_cursor() {
    let (mut term, mut client) = pair();
    let task =
        tokio::spawn(async move { get_data(&mut term, 5, 2, "名称: ", b"ac", 16, 10, 5).await });
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"\x1b[Dx\r");
    assert_eq!(task.await.unwrap().unwrap(), b"axc".to_vec());
    // The prompt row is drawn, the cursor moves left a cell, then the tail from the
    // insertion point is repainted with the padding shortened by one.
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        format!(
            "\x1b[5;2H名称: ac{}\x1b[5;10H\x1b[5;9H\x1b[5;9Hxc{}\x1b[5;10H",
            " ".repeat(8),
            " ".repeat(7)
        )
    );
}

#[tokio::test]
async fn get_data_deleting_a_wide_character_frees_a_column() {
    let (mut term, mut client) = pair();
    let task =
        tokio::spawn(
            async move { get_data(&mut term, 3, 0, "", "中x".as_bytes(), 16, 6, 5).await },
        );
    tokio::time::sleep(AFTER_RESET).await;
    client.push(b"\x1b[D\x08\r");
    assert_eq!(task.await.unwrap().unwrap(), "x".as_bytes().to_vec());
    // LEFT steps back over 'x', BACKSPACE erases the whole three-byte character and
    // redraws from the freed column with two more padding cells.
    assert_eq!(
        String::from_utf8(client.take_available()).unwrap(),
        format!(
            "\x1b[3;0H中x{}\x1b[3;3H\x1b[3;2H\x1b[3;0Hx{}\x1b[3;0H",
            " ".repeat(3),
            " ".repeat(5)
        )
    );
}
