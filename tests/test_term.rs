use super::*;

fn session() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

#[tokio::test]
async fn reads_keys_until_disconnect() {
    let (mut term, client) = session();
    client.push(b"AB\r\n");
    assert_eq!(term.read_key(Duration::from_millis(50)).await, b'A' as u32);
    assert_eq!(term.read_key(Duration::from_millis(50)).await, b'B' as u32);
    assert_eq!(term.read_key(Duration::from_millis(50)).await, key::CR);
    drop(client);
    assert_eq!(term.read_key(Duration::from_millis(50)).await, KEY_NULL);
    assert!(term.is_closed());
}

#[tokio::test]
async fn timeout_when_nothing_arrives() {
    let (mut term, client) = session();
    assert_eq!(term.read_key(Duration::from_millis(20)).await, KEY_TIMEOUT);
    client.push(b"x");
    assert_eq!(term.read_key(Duration::from_millis(50)).await, b'x' as u32);
}

#[tokio::test]
async fn pending_output_is_flushed_before_reading() {
    let (mut term, mut client) = session();
    term.print("菜单");
    client.push(b"\r");
    assert_eq!(term.read_key(Duration::from_millis(50)).await, key::CR);
    let mut buf = vec![0u8; 64];
    assert_eq!(client.read(&mut buf).await.unwrap(), 6);
    assert_eq!(&buf[..6], "菜单".as_bytes());
}

/// `igetch` decoded only the bytes its own read happened to return, so an arrow split
/// across packets came back as a bare escape and lost the key typed behind it with it. The
/// wait for the rest of the sequence keeps such a key one key.
#[tokio::test]
async fn arrow_key_split_across_packets_is_rejoined() {
    let (mut term, client) = session();
    client.push(b"\x1b[");
    let (key, ()) = tokio::join!(term.read_key(Duration::from_secs(1)), async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        client.push(b"Ax");
    });
    assert_eq!(key, key::KEY_UP, "the split arrow is one key");
    assert_eq!(
        term.read_key(Duration::from_millis(50)).await,
        b'x' as u32,
        "the text typed behind it is not swallowed with it"
    );

    // The reported case: a `Del` whose tail lagged, with the `Enter` typed after it.
    client.push(b"\x1b[3");
    let (del, ()) = tokio::join!(term.read_key(Duration::from_secs(1)), async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        client.push(b"~\r");
    });
    assert_eq!(del, key::KEY_DEL);
    assert_eq!(term.read_key(Duration::from_millis(50)).await, key::CR);
}

/// A lone escape waits for a tail that never arrives and then answers as the escape key,
/// which is `igetch`'s `out == 0 && in_esc` case (`io.c:1075-1078`).
#[tokio::test]
async fn lone_escape_is_reported_once_the_grace_ends() {
    let (mut term, client) = session();
    client.push(b"\x1b");
    let started = tokio::time::Instant::now();
    assert_eq!(term.read_key(Duration::from_secs(1)).await, key::KEY_ESC);
    assert!(
        started.elapsed() >= ESC_GRACE,
        "the escape waits for the byte that would tell it apart from a sequence"
    );
}

/// Once a second byte has joined a sequence that matches no known key, `igetch` returns 0
/// rather than the escape (`in_esc` is cleared at `io.c:529`), so the editor inserts
/// nothing and the keys that follow are read normally.
#[tokio::test]
async fn unknown_sequence_is_dropped_without_a_key() {
    let (mut term, client) = session();
    client.push(b"\x1b[?25l");
    assert_eq!(term.read_key(Duration::from_millis(400)).await, KEY_RETRY);
    client.push(b"y");
    assert_eq!(term.read_key(Duration::from_millis(50)).await, b'y' as u32);
}

#[tokio::test]
async fn gbk_output_is_transcoded() {
    let (mut term, mut client) = session();
    term.set_charset(Charset::Gbk);
    term.print("中文");
    term.flush().await.unwrap();
    let mut buf = vec![0u8; 64];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], [0xd6, 0xd0, 0xce, 0xc4]);
    assert_eq!(
        GBK.decode_without_bom_handling(&buf[..n]).0.as_ref(),
        "中文"
    );
}

#[tokio::test]
async fn gbk_input_rejoins_a_split_character() {
    let (mut term, client) = session();
    term.set_charset(Charset::Gbk);
    let gbk: Vec<u8> = GBK.encode("中").0.into_owned();
    client.push(&gbk[..1]);
    // Half a character is consumed but yields no key, which `igetch` reports as 0.
    assert_eq!(term.read_key(Duration::from_millis(20)).await, KEY_RETRY);
    client.push(&gbk[1..]);
    assert_eq!(
        term.read_key(Duration::from_millis(50)).await,
        "中".as_bytes()[0] as u32
    );
    // The lead byte is what the line editor reassembles the rest from.
    assert_eq!(term.input, &"中".as_bytes()[1..]);
}

#[test]
fn incomplete_utf8_is_detected() {
    assert_eq!(incomplete_utf8_len(b"abc"), 0);
    assert_eq!(incomplete_utf8_len("中".as_bytes()), 0);
    assert_eq!(incomplete_utf8_len(&["中".as_bytes()[0]],), 1);
    let two = vec!["中".as_bytes()[0], 'a' as u8];
    assert_eq!(incomplete_utf8_len(&two), 0);
    let partial = vec!["文".as_bytes()[0], "文".as_bytes()[1]];
    assert_eq!(incomplete_utf8_len(&partial), 2);
}

#[test]
fn fields_are_padded_to_cell_width() {
    assert_eq!(pad_field("ab", 5, true), "ab   ");
    assert_eq!(pad_field("中文", 5, true), "中文 ");
    // A wide character is never split to fill the last cell.
    assert_eq!(pad_field("中文", 3, true), "中 ");
}

#[tokio::test]
async fn reset_input_drains_pending_keys() {
    let (mut term, client) = session();
    client.push(b"abc");
    term.reset_input().await;
    assert_eq!(term.read_key(Duration::from_millis(20)).await, KEY_TIMEOUT);
}

/// The daemon's `kill(0, SIGTERM)` reached a C child in the middle of its `select()`, and
/// `io.c:1104` answered that wait at once. Without the same cut-short, a session parked at
/// "press any key" for its idle limit never reaches the exit test its caller is in, and the
/// drain ends up reporting it as a straggler instead of closing it.
#[tokio::test]
async fn a_long_wait_ends_when_the_daemon_exits() {
    let (mut term, _client) = session();
    let exit = ExitWatch::default();
    term.set_exit_watch(exit.clone());

    let parked = tokio::spawn(async move { term.read_key_wait(600).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    exit.exit();

    let answered = tokio::time::timeout(Duration::from_secs(1), parked).await;
    assert!(answered.is_ok(), "the wait must not outlive the notify");
    assert_eq!(
        answered.unwrap().unwrap(),
        KEY_TIMEOUT,
        "the flag answers the wait as its own timeout, the way `igetch` did"
    );
}

/// A key typed at the moment the daemon exits is still the key: the read branch is polled
/// first, so the shutdown cannot eat the byte the client sent.
#[tokio::test]
async fn a_key_landing_with_the_exit_is_still_the_key() {
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let exit = ExitWatch::default();
    exit.exit();
    term.set_exit_watch(exit);

    client.push(b"A");
    assert_eq!(term.read_key(Duration::from_secs(1)).await, b'A' as u32);
    assert_eq!(term.read_key(Duration::from_secs(1)).await, KEY_TIMEOUT);
}

/// `io_buf_conv` (io.c:1142-1176) hands a `0xff` and the byte after it to `igetch`
/// without converting them, so the `IAC` triple with which a telnet client answers the
/// session's negotiation is one unreadable key in either charset - and not, under GBK,
/// replacement characters whose bytes the line editor completes with real keystrokes.
#[tokio::test]
async fn a_control_escape_is_not_converted_as_text() {
    for charset in [Charset::Utf8, Charset::Gbk] {
        let (mut term, client) = session();
        term.set_charset(charset);
        client.push(&[0xff, 0xfb, 0x01, b'a', b'b']);
        let first = term.read_key(Duration::from_millis(200)).await;
        assert!(
            first > 255,
            "{charset:?}: {first:#x} is a wide code, not text"
        );
        assert_eq!(
            term.read_key(Duration::from_millis(200)).await,
            b'a' as u32,
            "{charset:?}"
        );
        assert_eq!(
            term.read_key(Duration::from_millis(200)).await,
            b'b' as u32,
            "{charset:?}"
        );
    }
}

/// C keeps an unconsumed byte in `stdin_buf` for the next read (`io.c:1185-1191`), so a
/// control escape split across packets is still one escape and not a lone `KEY_CONTROL`
/// followed by an option byte read as text.
#[tokio::test]
async fn a_control_escape_split_across_packets_is_kept_whole() {
    let (mut term, client) = session();
    term.set_charset(Charset::Gbk);
    client.push(&[0xff]);
    // Half an escape is not a key: `igetch` has the byte and returns 0 to be asked again.
    assert_eq!(term.read_key(Duration::from_millis(50)).await, KEY_RETRY);

    client.push(&[0xfb, 0x01, b'a']);
    let first = term.read_key(Duration::from_secs(1)).await;
    assert!(first > 255, "the triple is a wide code again: {first:#x}");
    assert_eq!(term.read_key(Duration::from_millis(200)).await, b'a' as u32);
}

/// `io_conv_init` opens the input descriptor with `//IGNORE` (io.c:1259-1261) and
/// `io_buf_conv` then skips what iconv reports as a malformed sequence (io.c:1200-1217).
/// Dropping it is what leaves the keys typed after it intact: a replacement character
/// would be three bytes and the line editor would treat them as a truncated character.
#[tokio::test]
async fn unconvertible_input_bytes_are_dropped() {
    let (mut term, client) = session();
    term.set_charset(Charset::Gbk);
    client.push(&[0xa1, b'!']);
    assert_eq!(term.read_key(Duration::from_millis(200)).await, b'!' as u32);
    assert!(
        term.input.is_empty(),
        "the malformed byte leaves nothing behind: {:x?}",
        term.input
    );
}
