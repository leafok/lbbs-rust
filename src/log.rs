/* SPDX-License-Identifier: GPL-3.0-or-later */
//! Logging, mirroring `src/log.c`.
//!
//! The C server keeps two `FILE`s opened in append mode — `fp_common_log` for `log_common()`
//! and `fp_error_log` for `log_error()`/`log_debug()` — and every line goes through
//! `log_head()` first:
//!
//! ```text
//! [2026-11-19 09:41:02] [1234] [INFO] 欢迎光临 Example BBS
//! [2026-11-19 09:41:02] [1234] [ERROR] [net_server.c:948] accept(socket_server) error: 24
//! ```
//!
//! The timestamp is UTC (`gmtime_r`), the pid is the writer's, and only the error stream carries
//! the `file:line` suffix. Each line is flushed immediately, so a crash cannot lose the tail of
//! the log, and `log_restart()` reopens both files so a rotated log is picked up.
//!
//! That is the whole of it, so the port implements it directly as a [`tracing::Subscriber`]
//! rather than bending a general-purpose formatter to the same shape: [`LogSink`] routes on the
//! level and writes the head itself. Three deliberate differences:
//!
//! * `WARN` has no equivalent in the C log levels. The port's recoverable failures are the ones
//!   the original reported through `log_error()`, so warnings join the error stream tagged
//!   `[WARN] [file:line]`.
//! * `TRACE` maps to `[DEBUG]`: the C macros offered nothing finer.
//! * The error head names the Rust module path and line rather than a `.c` file.
//!
//! Spans are inert. Nothing in the port uses `#[instrument]`, and the C log has no notion of a
//! context to attach, so [`Subscriber::new_span`] hands out one placeholder id and the record
//! methods drop the values.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::Utc;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::{Event, Id, Level, Metadata, Subscriber};

use crate::paths::{LINE_BUFFER_LEN, LOG_FILE_ERROR, LOG_FILE_INFO};

/// `log_begin()` failed: which stream, with the C return code it gave (`-1` for the common log,
/// `-2` for the error log).
#[derive(Debug)]
pub enum LogError {
    Common(io::Error),
    Error(io::Error),
}

impl LogError {
    fn path(&self) -> &'static str {
        match self {
            LogError::Common(_) => LOG_FILE_INFO,
            LogError::Error(_) => LOG_FILE_ERROR,
        }
    }

    /// `main.c` propagated `log_begin()`'s return straight out of `main()`.
    pub fn exit_code(&self) -> i32 {
        match self {
            LogError::Common(_) => -1,
            LogError::Error(_) => -2,
        }
    }

    fn cause(&self) -> &io::Error {
        match self {
            LogError::Common(err) | LogError::Error(err) => err,
        }
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The C message, with strerror replaced by the OS error string.
        write!(f, "fopen({}) error: {}", self.path(), self.cause())
    }
}

impl std::error::Error for LogError {}

/// Where one stream currently goes.
#[derive(Debug)]
enum Target {
    File(File),
    /// `log_common_redir(STDERR_FILENO)`: both streams land on stderr in `main.c`. A redirected
    /// stream is what `log_restart()` leaves alone.
    Console,
    /// The lines captured by a unit test instead of a file or the console.
    #[cfg(test)]
    Buffered(Arc<Mutex<Vec<u8>>>),
}

/// An append-mode stream, or the console after a `--display-*` redirect.
///
/// The mutex is what makes concurrent tasks safe: the C daemon had one `FILE` per process, and
/// `fputs()` plus `fflush()` was atomic enough because nothing else wrote to it. Here the loader
/// task, the listeners and every client task share both streams.
#[derive(Debug)]
struct Stream {
    path: PathBuf,
    target: Mutex<Target>,
}

/// A poisoned log lock means a task died while writing one line; the streams stay usable, which
/// is what C's unguarded `FILE` would have done.
fn locked<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Stream {
    fn open(path: &Path) -> io::Result<Stream> {
        let file = OpenOptions::new().append(true).create(true).open(path)?;
        Ok(Stream {
            path: path.to_path_buf(),
            target: Mutex::new(Target::File(file)),
        })
    }

    fn locked(&self) -> MutexGuard<'_, Target> {
        locked(&self.target)
    }

    /// `log_restart()` for this stream.
    fn reopen(&self) -> io::Result<()> {
        let mut guard = self.locked();
        if matches!(*guard, Target::Console) {
            return Ok(());
        }
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)?;
        // Assigning only after the open succeeds keeps the previous descriptor, which the
        // replacement drops as soon as the guard is released.
        *guard = Target::File(file);
        Ok(())
    }

    fn write_line(&self, line: &[u8]) {
        let mut guard = self.locked();
        // A log line that cannot be written must not reach the client's socket, and C had no way
        // to report it either: log_printf() returned -3 and the caller ignored it.
        match &mut *guard {
            Target::File(file) => {
                let _ = file.write_all(line).and_then(|()| file.flush());
            }
            Target::Console => {
                let _ = io::stderr()
                    .write_all(line)
                    .and_then(|()| io::stderr().flush());
            }
            #[cfg(test)]
            Target::Buffered(lines) => locked(lines).extend_from_slice(line),
        }
    }

    fn redirect_to_console(&self) {
        *self.locked() = Target::Console;
    }
}

/// The two streams, opened by [`begin`] and held by `main` so the reload path can reopen them.
///
/// Cloning is cheap and every clone writes the same files, which is what lets the loader task,
/// the listeners and each client task share one logger.
#[derive(Clone, Debug)]
pub struct LogSink {
    common: Arc<Stream>,
    error: Arc<Stream>,
}

impl LogSink {
    /// `log_common_redir(STDERR_FILENO)` and `log_error_redir(STDERR_FILENO)`, which `main.c`
    /// only applies in the foreground and only when the matching flag was given.
    pub fn redirect_to_console(&self) {
        self.common.redirect_to_console();
        self.error.redirect_to_console();
    }

    /// `log_common_redir(STDERR_FILENO)`, which `--display-log` alone asks for.
    pub fn redirect_common_to_console(&self) {
        self.common.redirect_to_console();
    }

    /// `log_error_redir(STDERR_FILENO)`, which `--display-error-log` alone asks for.
    pub fn redirect_error_to_console(&self) {
        self.error.redirect_to_console();
    }

    /// `log_restart()`: `0`, `-1` when the common log failed to reopen, `-2` when the error log
    /// did. The failure cannot be reported through the stream that just broke, so it goes to
    /// stderr — C's `log_error()` would have tried the error log and quietly failed.
    pub fn restart(&self) -> i32 {
        if let Err(err) = self.common.reopen() {
            eprintln!("fopen({}) error: {}", self.common.path.display(), err);
            return -1;
        }
        if let Err(err) = self.error.reopen() {
            eprintln!("fopen({}) error: {}", self.error.path.display(), err);
            return -2;
        }
        0
    }

    /// Make this the process-wide subscriber. `log_begin()` needed no equivalent step because the
    /// C log functions looked the streams up in globals.
    pub fn install(&self) -> Result<(), InstallError> {
        tracing::subscriber::set_global_default(self.clone())
            .map_err(|err| InstallError(err.to_string()))
    }

    /// `fp_log = (log_level == LOG_LEVEL_COMMON ? fp_common_log : fp_error_log)`.
    fn routes(&self, meta: &Metadata<'_>) -> &Arc<Stream> {
        if meta.level() == &Level::INFO {
            &self.common
        } else {
            &self.error
        }
    }
}

/// A logging setup that could not be installed because something else already owns the
/// process-wide subscriber.
#[derive(Debug)]
pub struct InstallError(pub String);

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "set_logger error: {}", self.0)
    }
}

impl std::error::Error for InstallError {}

impl Subscriber for LogSink {
    fn enabled(&self, _meta: &Metadata<'_>) -> bool {
        // C compiled the debug calls out only with --without-debug; the port keeps every level
        // and lets the streams decide where a line goes.
        true
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> Id {
        Id::from_u64(SPAN_PLACEHOLDER)
    }

    fn record(&self, _span: &Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let Some(message) = message_of(event) else {
            // C has no field-only logging calls, so an event with no message prints nothing.
            return;
        };
        let line = format_line(
            meta.level(),
            meta.module_path().unwrap_or("?"),
            meta.line().unwrap_or(0),
            &message,
        );
        self.routes(meta).write_line(&line);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Every span shares this id because none of them is ever looked up again.
const SPAN_PLACEHOLDER: u64 = 1;

/// `log_begin(LOG_FILE_INFO, LOG_FILE_ERROR)`, resolved against the server root that `main` has
/// already moved into.
pub fn begin(root: &Path) -> Result<LogSink, LogError> {
    let common = Stream::open(&root.join(LOG_FILE_INFO)).map_err(LogError::Common)?;
    let error = Stream::open(&root.join(LOG_FILE_ERROR)).map_err(LogError::Error)?;
    Ok(LogSink {
        common: Arc::new(common),
        error: Arc::new(error),
    })
}

/// A sink with both streams on stderr, for the paths that print a message and exit before any
/// log file exists.
pub fn console() -> LogSink {
    let stream = |path: &str| {
        Arc::new(Stream {
            path: PathBuf::from(path),
            target: Mutex::new(Target::Console),
        })
    };
    LogSink {
        common: stream(LOG_FILE_INFO),
        error: stream(LOG_FILE_ERROR),
    }
}

/// `[ts] [pid] [LEVEL] [file:line] message\n`, with `log_printf()`'s `LINE_BUFFER_LEN` cap.
///
/// The C overflow branch keeps the head and the beginning of the message, forces the byte before
/// the limit to `\n`, and returns -2 to say the line was cut.
pub(crate) fn format_line(
    level: &Level,
    module_path: &str,
    line_no: u32,
    message: &str,
) -> Vec<u8> {
    // log_head() adds the file and line only for the two error-stream levels, and INFO is its
    // first branch; DEBUG is the catch-all.
    let (tag, location) = match *level {
        Level::INFO => ("INFO", String::new()),
        level => {
            let tag = match level {
                Level::ERROR => "ERROR",
                Level::WARN => "WARN",
                _ => "DEBUG",
            };
            (tag, format!(" [{}:{}]", module_path, line_no))
        }
    };

    let stamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let head = format!(
        "[{}] [{}] [{}]{} ",
        stamp,
        std::process::id(),
        tag,
        location
    );

    let mut out = Vec::with_capacity(head.len() + message.len() + 1);
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(message.as_bytes());
    out.push(b'\n');

    if head.len() + message.len() + 1 >= LINE_BUFFER_LEN {
        // Truncation happens on bytes, as the C buffer did, so a multibyte character may be cut
        // in the middle and the log then holds what the equivalent of vsnprintf() left.
        out.truncate(LINE_BUFFER_LEN - 1);
        let last = out.len() - 1;
        out[last] = b'\n';
    }
    out
}

struct Message(Option<String>);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && self.0.is_none() {
            // The message field of an inline-formatted call is the fmt::Arguments of that call,
            // whose Debug impl writes exactly what the C vsnprintf() produced.
            self.0 = Some(format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = Some(value.to_string());
        }
    }
}

fn message_of(event: &Event<'_>) -> Option<String> {
    let mut visitor = Message(None);
    event.record(&mut visitor);
    visitor.0
}

/// Both streams captured into memory, for the tests that assert on what a module logs. The
/// subscriber becomes the thread default, so an `async` body would have to stay on one thread;
/// the C log has no equivalent to this — the lines simply went into `log/bbs.log`.
#[cfg(test)]
pub(crate) fn captured(body: impl FnOnce()) -> (String, String) {
    let common = Arc::new(Mutex::new(Vec::new()));
    let error = Arc::new(Mutex::new(Vec::new()));
    let sink = LogSink {
        common: buffered(&common, "test-common"),
        error: buffered(&error, "test-error"),
    };
    tracing::subscriber::with_default(sink, body);
    (text(&common), text(&error))
}

#[cfg(test)]
fn buffered(lines: &Arc<Mutex<Vec<u8>>>, name: &str) -> Arc<Stream> {
    Arc::new(Stream {
        path: PathBuf::from(name),
        target: Mutex::new(Target::Buffered(Arc::clone(lines))),
    })
}

#[cfg(test)]
fn text(lines: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&locked(lines)).to_string()
}

#[cfg(test)]
#[path = "../tests/test_log.rs"]
mod tests;
