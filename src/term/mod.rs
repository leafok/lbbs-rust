//! Session transport: byte pipes, charset transcoding, buffered output, and the
//! `igetch`/`prints` semantics the original `src/io.c` implements.

pub mod key;

use encoding_rs::{Decoder, DecoderResult, GBK};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};

use crate::paths::LINE_BUFFER_LEN;
use crate::text;

pub use key::{Decoded, KEY_NULL, KEY_RETRY, KEY_TIMEOUT};

/// How long an escape that ends the input buffer may wait for the bytes behind it, and how
/// often that wait is retried while they trickle in. `io.c:1103` uses the same hundred
/// milliseconds for the continuation bytes of a multi-byte character.
const ESC_GRACE: Duration = Duration::from_millis(100);
const ESC_GRACE_ROUNDS: usize = 3;

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

/// `SYS_server_exit` as C's `io.c` reads it. A `select()` there was always cut short by the
/// flag — `while (!SYS_server_exit && ch == KEY_TIMEOUT && …)` at `io.c:1104`, and the same test
/// in the buffering loops at `:308`, `:456`, `:526` — because the parent's `kill(0, SIGTERM)`
/// reached every child and the handler only raised the flag. A task cannot be interrupted from
/// outside, so the port raises the flag *and* wakes a waiter: the wait then answers as its own
/// timeout, which is what lets the caller reach the exit test in its loop.
#[derive(Clone, Default)]
pub struct ExitWatch {
    exiting: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
}

impl ExitWatch {
    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::Relaxed)
    }

    /// Raises the flag and wakes the waits registered by [`ExitWatch::wait`].
    pub fn exit(&self) {
        self.exiting.store(true, Ordering::Relaxed);
        self.shutdown.notify_waiters();
    }

    /// Resolves once [`ExitWatch::exit`] has been called. The waiter registers before the flag
    /// is read, since `notify_waiters()` only reaches waiters already registered.
    pub async fn wait(&self) {
        loop {
            let notified = self.shutdown.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_exiting() {
                return;
            }
            notified.await;
        }
    }
}

/// One client session's terminal.
pub struct Terminal {
    io: ByteIo,
    /// The daemon's shutdown flag, so a wait ends with it rather than with its own deadline.
    exit: ExitWatch,
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
            exit: ExitWatch::default(),
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

    /// Hand the transport the daemon's shutdown flag, as a C child inherited it in shared memory.
    pub fn set_exit_watch(&mut self, exit: ExitWatch) {
        self.exit = exit;
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
        text::split_data_lines(
            buf,
            max_display_len,
            skip_ctrl_seq,
            self.fixed_width,
            max_lines,
        )
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
            // A sequence split across packets is still one sequence: the bytes in flight are
            // awaited before anything is given up on. `igetch` decodes the bytes its drained
            // read happened to have and returns 0, so a key whose tail lagged behind simply
            // went missing and had to be pressed a second time.
            Decoded::Partial => self.await_sequence().await,
            Decoded::Retry => {
                self.input.clear();
                KEY_RETRY
            }
        }
    }

    /// Settles a buffer that ends inside an escape sequence: `KEY_ESC` while the escape is
    /// alone, since that is the key the user pressed, and `KEY_RETRY` once a second byte has
    /// joined it and no known sequence follows.
    async fn await_sequence(&mut self) -> u32 {
        for _ in 0..ESC_GRACE_ROUNDS {
            match self.refill(ESC_GRACE).await {
                Refill::Data => match key::decode(&self.input) {
                    Decoded::Key(code, n) => {
                        self.input.drain(..n.min(self.input.len()));
                        return code;
                    }
                    Decoded::Retry => {
                        self.input.clear();
                        return KEY_RETRY;
                    }
                    Decoded::Partial => continue,
                },
                Refill::Timeout => break,
                Refill::Closed => {
                    self.closed = true;
                    return KEY_NULL;
                }
            }
        }

        let lone_escape =
            self.input.first() == Some(&(key::KEY_ESC as u8)) && self.input.len() == 1;
        self.input.clear();
        if lone_escape { key::KEY_ESC } else { KEY_RETRY }
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
            if ch != KEY_TIMEOUT || self.exit.is_exiting() {
                // `io.c:1104`: `press_any_key` answers the moment the flag is up, which is how a
                // session parked here for its full idle limit still ends inside the drain.
                return ch;
            }
        }
    }

    /// One round of the site shuttle's raw read (`bbsnet_connect:1106`): whole bytes, not
    /// decoded keys.
    ///
    /// What an earlier `igetch` left in [`Terminal::input`] stays there, as it does in C's
    /// `io.c` conversion buffer: the shuttle reads the transport, and those bytes reach the
    /// next key reader after it.
    pub async fn read_raw(&mut self, timeout: Duration) -> RawRead {
        let mut buf = [0u8; LINE_BUFFER_LEN];
        let kept = self.input.len();
        loop {
            let exit = self.exit.clone();
            let done = {
                let read = tokio::time::timeout(timeout, read_bytes(&mut self.io, &mut buf));
                tokio::pin!(read);
                tokio::select! {
                    biased;
                    res = &mut read => res,
                    _ = exit.wait() => return RawRead::Idle,
                }
            };
            match done {
                Ok(Ok(0)) => {
                    self.closed = true;
                    return RawRead::Closed;
                }
                Ok(Ok(n)) => {
                    self.push_input(&buf[..n]);
                    return RawRead::Bytes(self.input.split_off(kept));
                }
                Ok(Err(err)) if err.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(_)) => {
                    self.closed = true;
                    return RawRead::Closed;
                }
                Err(_) => return RawRead::Idle,
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
            let exit = self.exit.clone();
            let done = {
                let read = tokio::time::timeout(timeout, read_bytes(&mut self.io, &mut buf));
                tokio::pin!(read);
                // `biased` so a key that lands together with the shutdown is still a key: C's
                // `igetch` returned whatever the completed `select()` had read, and only tested
                // `SYS_server_exit` around a wait that produced nothing (`io.c:1104`). Dropping a
                // completed read here would swallow that byte, and the client would have to type
                // the same key twice against a server that is going down anyway.
                tokio::select! {
                    biased;
                    res = &mut read => res,
                    _ = exit.wait() => return Refill::Timeout,
                }
            };
            match done {
                Ok(Ok(0)) => return Refill::Closed,
                Ok(Ok(n)) => {
                    self.push_input(&buf[..n]);
                    return Refill::Data;
                }
                Ok(Err(err)) if err.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(_)) => return Refill::Closed,
                Err(_) => return Refill::Timeout,
            }
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
                // `io_buf_conv` (io.c:1142-1176) keeps a `0xff` and the byte behind it away
                // from iconv, which is how the `IAC` triples with which a telnet client
                // answers the session's negotiation reach `igetch` whole - as the wide code
                // [`key::decode`] reads them. Converted as text they would come out as
                // replacement characters, whose three UTF-8 bytes the line editor takes for a
                // truncated Chinese character and completes with the keys typed behind them.
                let mut held = std::mem::take(&mut self.pending_lead);
                if !held.is_empty() {
                    held.extend_from_slice(bytes);
                }
                let mut src: &[u8] = if held.is_empty() { bytes } else { &held };

                // Stateful so a character split across packets decodes correctly.
                let decoder = self
                    .gbk
                    .get_or_insert_with(|| GBK.new_decoder_without_bom_handling());
                // `decode_to_*` writes into spare capacity and reports OutputFull when there
                // is none, so the worst case for the whole buffer is reserved up front.
                let room = decoder
                    .max_utf8_buffer_length(src.len())
                    .unwrap_or(src.len() * 3);
                let mut out: Vec<u8> = Vec::with_capacity(room);
                let mut decoded = String::with_capacity(room);

                while !src.is_empty() {
                    let cut = src
                        .iter()
                        .position(|&b| b == key::KEY_CONTROL as u8)
                        .unwrap_or(src.len());
                    let (segment, control) = src.split_at(cut);

                    decoded.clear();
                    let mut rest = segment;
                    while !rest.is_empty() {
                        let (result, read) =
                            decoder.decode_to_string_without_replacement(rest, &mut decoded, false);
                        // The `//IGNORE` of io.c:1200-1217: the malformed sequence has been
                        // consumed by the call above and nothing was written for it, so the
                        // text the decoder mapped behind it is kept and the scan goes on.
                        rest = &rest[read..];
                        if !matches!(result, DecoderResult::Malformed(..)) {
                            break;
                        }
                    }
                    out.extend_from_slice(decoded.as_bytes());

                    if control.is_empty() {
                        break;
                    }
                    if control.len() == 1 {
                        // The escape is split across packets. C leaves the unconsumed byte in
                        // `stdin_buf` to be converted again; the port holds it here.
                        self.pending_lead = control.to_vec();
                        break;
                    }
                    out.extend_from_slice(&control[..2]);
                    src = &control[2..];
                }

                self.input.extend_from_slice(&out);
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

/// The three answers a raw read gets: `read(2)` returning bytes, `EAGAIN`, or end of stream.
pub enum RawRead {
    Bytes(Vec<u8>),
    Idle,
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
#[path = "../../tests/test_term.rs"]
mod tests;
