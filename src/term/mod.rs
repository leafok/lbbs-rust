//! Session transport: byte pipes, charset transcoding, buffered output, and the
//! `igetch`/`prints` semantics the original `src/io.c` implements.

pub mod key;

use encoding_rs::{Decoder, GBK};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::text;

pub use key::{Decoded, KEY_NULL, KEY_RETRY, KEY_TIMEOUT};

/// Client character set, chosen by the `(U)UTF-8/(G)GBK` prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Utf8,
    Gbk,
}

impl Charset {
    /// Answers to the `(U)UTF-8/(G)GBK? [U]` prompt.
    pub fn from_answer(c: u8) -> Option<Charset> {
        match c.to_ascii_uppercase() {
            b'U' => Some(Charset::Utf8),
            b'G' => Some(Charset::Gbk),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Charset::Utf8 => "UTF-8",
            Charset::Gbk => "GBK",
        }
    }
}

/// A bidirectional byte stream. The telnet side owns a socket; the SSH side routes data
/// through a channel pipe so both backends share one session implementation.
pub enum ByteIo {
    Tcp(TcpStream),
    Pipe(Pipe),
}

/// Duplex byte pipe used by the SSH backend and by tests.
pub struct Pipe {
    inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    pending: Vec<u8>,
}

impl Pipe {
    pub fn new() -> (Pipe, Pipe) {
        let (to_first, from_first) = mpsc::unbounded_channel();
        let (to_second, from_second) = mpsc::unbounded_channel();
        (
            Pipe {
                inbound: from_first,
                outbound: to_second,
                pending: Vec::new(),
            },
            Pipe {
                inbound: from_second,
                outbound: to_first,
                pending: Vec::new(),
            },
        )
    }

    /// Queue bytes for the far end to read.
    pub fn push(&self, data: &[u8]) {
        let _ = self.outbound.send(data.to_vec());
    }

    /// Everything the far end has written so far, without waiting for more.
    pub fn take_available(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.pending);
        while let Ok(data) = self.inbound.try_recv() {
            out.extend_from_slice(&data);
        }
        out
    }

    /// Tell the far end the conversation is over, as closing the telnet socket does.
    ///
    /// Bytes already queued still reach the reader; after them every read is end of stream and
    /// every write fails. The SSH transport calls this when the client's channel goes away,
    /// because unlike a socket it cannot make a blocked read return by itself.
    pub fn hang_up(&mut self) {
        self.inbound.close();
    }
}

/// `Ok(0)` means end of stream, the way `read`/`ssh_channel_read_nonblocking` report it.
async fn read_bytes(io: &mut ByteIo, buf: &mut [u8]) -> io::Result<usize> {
    match io {
        ByteIo::Tcp(stream) => stream.read(buf).await,
        ByteIo::Pipe(pipe) => pipe.read(buf).await,
    }
}

async fn write_bytes(io: &mut ByteIo, buf: &[u8]) -> io::Result<()> {
    match io {
        ByteIo::Tcp(stream) => stream.write_all(buf).await,
        ByteIo::Pipe(pipe) => pipe.write(buf).await,
    }
}

impl Pipe {
    /// The far end's bytes, waiting for them if there are none. `Ok(0)` is end of stream,
    /// which is also what the SSH transport reports when the client's channel closes.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty()
            && let Some(data) = self.inbound.recv().await
        {
            self.pending = data;
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }

    async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.outbound
            .send(data.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session gone"))
    }
}

/// One client session's terminal.
pub struct Terminal {
    io: ByteIo,
    charset: Charset,
    /// The `(V)变宽/(F)定宽` choice handed to `src/str_process.c` as `UTF8_fixed_width`.
    fixed_width: bool,
    /// Client-charset bytes already transcoded to UTF-8, awaiting the key decoder.
    input: Vec<u8>,
    /// A multi-byte lead whose tail has not arrived yet.
    pending_lead: Vec<u8>,
    /// GBK decodes with its own state, so a split character needs no lead buffer.
    gbk: Option<Decoder>,
    /// UTF-8 waiting for the wire.
    output: Vec<u8>,
    closed: bool,
}

impl Terminal {
    pub fn new(io: ByteIo) -> Terminal {
        Terminal {
            io,
            charset: Charset::Utf8,
            fixed_width: true,
            input: Vec::new(),
            pending_lead: Vec::new(),
            gbk: None,
            output: Vec::new(),
            closed: false,
        }
    }

    pub fn charset(&self) -> Charset {
        self.charset
    }

    pub fn set_charset(&mut self, charset: Charset) {
        self.charset = charset;
        self.input.clear();
        self.pending_lead.clear();
        self.gbk = None;
    }

    pub fn fixed_width(&self) -> bool {
        self.fixed_width
    }

    pub fn set_fixed_width(&mut self, fixed: bool) {
        self.fixed_width = fixed;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Display width in terminal cells, using this session's width mode.
    pub fn width(&self, buf: &[u8], skip_ctrl_seq: bool) -> usize {
        text::display_width(buf, skip_ctrl_seq, self.fixed_width)
    }

    /// Line wrapping in this session's width mode.
    pub fn wrap(
        &self,
        buf: &[u8],
        max_display_len: usize,
        skip_ctrl_seq: bool,
        max_lines: usize,
    ) -> text::Wrapped {
        text::split_data_lines(buf, max_display_len, skip_ctrl_seq, self.fixed_width, max_lines)
    }

    /// `prints`: queue UTF-8 text. `igetch` flushes before reading, which is what keeps
    /// the buffer bounded, so this stays synchronous. C's version is `vfprintf`, so a `%` in
    /// the text is a format — the two call sites 51b1c7f quoting-ified have none here.
    pub fn print(&mut self, text: &str) {
        self.print_bytes(text.as_bytes());
    }

    /// `prints` for bytes that are not guaranteed to be valid UTF-8, such as the raw
    /// buffer the line editor echoes.
    pub fn print_bytes(&mut self, text: &[u8]) {
        self.output.extend_from_slice(text);
    }

    pub fn bell(&mut self) {
        self.print("\u{7}");
    }

    /// `moveto`: the original emits `\033[{row};{col}H` with a zero-based column, which
    /// callers rely on when they pass 0 and the terminal clamps to the first column.
    pub fn moveto(&mut self, row: usize, col: usize) {
        self.moveto_signed(row as i32, col as i32);
    }

    /// `moveto` for callers that hold coordinates as signed integers. A menu cursor two columns
    /// left of an item at column 1 goes negative, and the original's `%d` prints the sign; a
    /// negative row is the degenerate `\r` that `screen.c:55` emits instead of a sequence.
    pub fn moveto_signed(&mut self, row: i32, col: i32) {
        use std::fmt::Write as _;
        if row < 0 {
            self.print("\r");
            return;
        }
        let mut buf = String::with_capacity(16);
        let _ = write!(buf, "\x1b[{row};{col}H");
        self.print(&buf);
    }

    /// `clrtoeol`.
    pub fn clrtoeol(&mut self) {
        self.print("\x1b[K");
    }

    /// `iflush`: transcode and write everything pending.
    pub async fn flush(&mut self) -> io::Result<()> {
        if self.output.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.output);
        let wire: Vec<u8> = match self.charset {
            Charset::Utf8 => pending,
            // The original pipes the buffer through `iconv UTF-8→GBK//TRANSLIT`, which
            // replaces what it cannot map; a lossy decode then encode is the same thing.
            Charset::Gbk => {
                let text = String::from_utf8_lossy(&pending);
                GBK.encode(&text).0.into_owned()
            }
        };
        match write_bytes(&mut self.io, &wire).await {
            Ok(()) => Ok(()),
            Err(err) => {
                self.closed = true;
                Err(err)
            }
        }
    }

    /// `igetch`: wait up to `timeout` for one key.
    ///
    /// `KEY_TIMEOUT` when nothing arrived, `KEY_NULL` when the client went away, one of
    /// the codes in [`key`] otherwise.
    pub async fn read_key(&mut self, timeout: Duration) -> u32 {
        // Output must be on the wire before the client can answer.
        if self.flush().await.is_err() {
            return KEY_NULL;
        }

        if self.input.is_empty() {
            match self.refill(timeout).await {
                Refill::Data => {}
                Refill::Timeout => return KEY_TIMEOUT,
                Refill::Closed => {
                    self.closed = true;
                    return KEY_NULL;
                }
            }
            if self.input.is_empty() {
                // A partial multi-byte sequence decoded to nothing: `igetch` returns 0.
                return KEY_RETRY;
            }
        }

        match key::decode(&self.input) {
            Decoded::Key(code, n) => {
                self.input.drain(..n.min(self.input.len()));
                code
            }
            // No sequence state survives a call, so an escape split across packets is
            // dropped whole and reported as a bare ESC.
            Decoded::Partial => {
                self.input.clear();
                key::KEY_ESC
            }
            Decoded::Retry => {
                self.input.clear();
                KEY_RETRY
            }
        }
    }

    /// `igetch_t`: wait up to `seconds`, retrying through timeouts.
    pub async fn read_key_wait(&mut self, seconds: u64) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return KEY_TIMEOUT;
            }
            let ch = self.read_key(remaining).await;
            if ch != KEY_TIMEOUT {
                return ch;
            }
        }
    }

    /// `igetch_reset`: drop whatever the client already sent.
    pub async fn reset_input(&mut self) {
        loop {
            match self.read_key(Duration::from_millis(100)).await {
                KEY_NULL | KEY_TIMEOUT => return,
                _ => continue,
            }
        }
    }

    async fn refill(&mut self, timeout: Duration) -> Refill {
        let mut buf = [0u8; 1024];
        loop {
            match tokio::time::timeout(timeout, read_bytes(&mut self.io, &mut buf)).await {
                Ok(Ok(0)) => return Refill::Closed,
                Ok(Ok(n)) => {
                    self.push_input(&buf[..n]);
                    return Refill::Data;
                }
                Ok(Err(err)) if err.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(_)) => return Refill::Closed,
                Err(_) => return Refill::Timeout,
            };
        }
    }

    fn push_input(&mut self, bytes: &[u8]) {
        match self.charset {
            Charset::Utf8 => {
                if !self.pending_lead.is_empty() {
                    self.input.append(&mut self.pending_lead);
                }
                self.input.extend_from_slice(bytes);
                // Hold back a truncated multi-byte tail so it is not decoded as garbage.
                let tail = incomplete_utf8_len(&self.input);
                if tail > 0 {
                    let split = self.input.len() - tail;
                    self.pending_lead = self.input.split_off(split);
                }
            }
            Charset::Gbk => {
                // Stateful so a character split across packets decodes correctly.
                let decoder = self
                    .gbk
                    .get_or_insert_with(|| GBK.new_decoder_without_bom_handling());
                let mut text = String::new();
                // `decode_to_string` writes into spare capacity and reports OutputFull
                // when there is none, so the worst case must be reserved up front.
                let room = decoder
                    .max_utf8_buffer_length(bytes.len())
                    .unwrap_or(bytes.len() * 3);
                text.reserve(room);
                let _ = decoder.decode_to_string(bytes, &mut text, false);
                self.input.extend_from_slice(text.as_bytes());
            }
        }
    }
}

/// Bytes at the end of `buf` that start a UTF-8 sequence which is not complete.
fn incomplete_utf8_len(buf: &[u8]) -> usize {
    let mut i = buf.len();
    let mut needed = 0usize;
    while i > 0 {
        i -= 1;
        let b = buf[i];
        if b & 0xc0 == 0x80 {
            needed += 1;
            continue;
        }
        let total = if b & 0x80 == 0 {
            return 0;
        } else if b & 0xe0 == 0xc0 {
            2
        } else if b & 0xf0 == 0xe0 {
            3
        } else if b & 0xf8 == 0xf0 {
            4
        } else {
            return 0;
        };
        return if needed + 1 < total { needed + 1 } else { 0 };
    }
    0
}

enum Refill {
    Data,
    Timeout,
    Closed,
}

/// Cut a string to at most `cells` display columns.
pub fn truncate_cells(s: &str, cells: usize, fixed: bool) -> &str {
    let bytes = s.as_bytes();
    let split = text::split_line(bytes, cells, true, fixed);
    std::str::from_utf8(&bytes[..split.bytes.min(bytes.len())]).unwrap_or("")
}

/// `printf("%-*s")`: pad or truncate to exactly `cells` columns.
pub fn pad_field(s: &str, cells: usize, fixed: bool) -> String {
    let shown = truncate_cells(s, cells, fixed);
    let used = text::display_width(shown.as_bytes(), true, fixed);
    let mut out = shown.to_string();
    for _ in used..cells {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn arrow_key_split_across_packets_is_lost_like_igetch() {
        let (mut term, client) = session();
        client.push(b"\x1b[");
        client.push(b"Ax");
        assert_eq!(
            term.read_key(Duration::from_millis(50)).await,
            key::KEY_ESC
        );
        assert_eq!(term.read_key(Duration::from_millis(50)).await, b'A' as u32);
        assert_eq!(term.read_key(Duration::from_millis(50)).await, b'x' as u32);
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
        assert_eq!(GBK.decode_without_bom_handling(&buf[..n]).0.as_ref(), "中文");
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
        assert_eq!(incomplete_utf8_len(&["中".as_bytes()[0]], ), 1);
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
}
