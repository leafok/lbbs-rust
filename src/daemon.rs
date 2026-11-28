/* SPDX-License-Identifier: GPL-3.0-or-later */
//! Command line, daemonization and server-root derivation, mirroring `src/main.c` and
//! `init_daemon()` from `src/init.c`.
//!
//! The C entry parses its arguments through a table of option descriptors, each with a handler
//! that either sets a flag or prints something and exits (`main.c:60-67`):
//!
//! ```text
//! -f  --foreground        Run in foreground
//! -h  --help              Display help message
//! -v  --version           Display version information
//!     --display-log       Display standard log information
//!     --display-error-log Display error log information
//! -C  --compile-config    Display compile configuration
//! ```
//!
//! Everything else is an error: `Invalid arguments`, the usage block, and exit code 1. The
//! handlers run in argument order, so `bbsd -h -f` prints the usage and leaves before the
//! `-f` is ever seen.
//!
//! After the arguments, the C process detaches from the terminal, moves into the server root
//! derived from its own location, and prints `APP_INFO` to stderr before the log is opened. The
//! two steps cannot be copied literally:
//!
//! * `setlocale(LC_ALL, "en_US.UTF-8")` is kept as [`apply_locale`], because one string the
//!   server writes is formatted by libc: the `strftime("%c %Z")` line of an article cache header
//!   ([`crate::cache::submission_time`]). Left in the `C` locale that line reads
//!   `Sun Aug 31 03:41:55 2003 CST` where C writes `Sun 31 Aug 2003 03:41:55 AM CST`, which is
//!   every cache file differing from the one the C loader wrote. Every other string the port
//!   handles is UTF-8 bytes it never hands to libc.
//! * Rust has no preprocessor build timestamp, so `app_info()` reports the package version and
//!   target architecture without the `on Nov 19 2026 09:41:02` tail.

use std::io::Write;
use std::path::{Path, PathBuf};

/// What `arg_usage()` prints as the program name (`main.c:83`): the literal string, not
/// `argv[0]`, so a call through a path or a symlink still reads the way the C help does. The
/// server root, by contrast, really is derived from `argv[0]` (`main.c:217`).
pub const PROG_NAME: &str = "bbsd";

/// `arg_options` (`main.c:60-67`), in order and with the same descriptions.
pub const OPTIONS: [(&str, &str, &str); 6] = [
    ("-f", "--foreground", "Run in foreground"),
    ("-h", "--help", "Display help message"),
    ("-v", "--version", "Display version information"),
    ("", "--display-log", "Display standard log information"),
    ("", "--display-error-log", "Display error log information"),
    ("-C", "--compile-config", "Display compile configuration"),
];

/// What the arguments asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Args {
    /// Start the daemon with these settings.
    Run(Flags),
    /// `arg_help()`: print the usage and exit 0.
    Help,
    /// `arg_version()`: print `APP_INFO` and the copyright and exit 0.
    Version,
    /// `arg_compile_info()`: print the build configuration and exit 0.
    CompileConfig,
    /// `arg_error()`: `Invalid arguments`, the usage and exit 1.
    Invalid,
}

/// The three flags the C globals `daemon_service`, `std_log_redir` and `error_log_redir` held.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `daemon_service`, inverted: the default is to detach.
    pub foreground: bool,
    pub display_log: bool,
    pub display_error_log: bool,
}

impl Args {
    /// The flags of [`Args::Run`], or `None` for the options that print and leave.
    pub fn into_flags(self) -> Option<Flags> {
        match self {
            Args::Run(flags) => Some(flags),
            _ => None,
        }
    }
}

/// `main.c:188-208`: the two nested scans, with the handlers applied as they are matched.
pub fn parse(argv: &[String]) -> Args {
    let mut flags = Flags::default();

    for arg in argv.iter().skip(1) {
        let mut matched = false;
        for (short, long, _) in OPTIONS {
            if (!short.is_empty() && arg == short) || arg == long {
                match long {
                    "--foreground" => flags.foreground = true,
                    "--help" => return Args::Help,
                    "--version" => return Args::Version,
                    "--display-log" => flags.display_log = true,
                    "--display-error-log" => flags.display_error_log = true,
                    "--compile-config" => return Args::CompileConfig,
                    _ => return Args::Invalid,
                }
                matched = true;
                break;
            }
        }
        if !matched {
            return Args::Invalid;
        }
    }

    Args::Run(flags)
}

/// `arg_usage()`, including the two `%*s` paddings that line the descriptions up.
pub fn usage(prog: &str) -> String {
    let mut out = format!("Usage: {prog} [-fhvC] [...]\n\n");
    for (short, long, description) in OPTIONS {
        // fprintf("%s%*s%s%*s%s\n", short, 8 - len(short), "", long, 24 - len(long), "", desc)
        let short_pad = 8_usize.saturating_sub(short.chars().count());
        let long_pad = 24_usize.saturating_sub(long.chars().count());
        out.push_str(short);
        out.push_str(&" ".repeat(short_pad));
        out.push_str(long);
        out.push_str(&" ".repeat(long_pad));
        out.push_str(description);
        out.push('\n');
    }
    out.push_str("\n    If meet any bug, please report to <leaflet@leafok.com>\n\n");
    out
}

/// `APP_INFO` (`common.c:17`): `PACKAGE_STRING " build (x86_64) on " __DATE__ " " __TIME__`.
pub fn app_info() -> String {
    format!(
        "lbbs-rust {} build ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH
    )
}

/// `COPYRIGHT_INFO` (`common.c:26-30`), reproduced with the original author line kept.
pub const COPYRIGHT_INFO: &str = concat!(
    "Copyright (C) 2004-2026  Leaflet <leaflet@leafok.com>\n",
    "This program comes with ABSOLUTELY NO WARRANTY.\n",
    "This is free software, and you are welcome to redistribute it \n",
    "under certain conditions; see the GNU General Public License \n",
    "version 3 for details.\n"
);

/// `arg_compile_info()`. The C answers come from configure; the Rust equivalents are what the
/// crate actually depends on, so the six original keys are answered in place.
pub fn compile_config() -> String {
    let mut out = format!("{}\n", app_info());
    for (key, value) in [
        // No shared objects: the crate links statically.
        ("--enable-shared", "no"),
        // The three states of `Type=notify` are written by `systemd::notify` rather than by
        // libsystemd, and only a Linux system answers them.
        (
            "--enable-systemd",
            if cfg!(target_os = "linux") {
                "yes"
            } else {
                "no"
            },
        ),
        (
            "--with-debug",
            if cfg!(debug_assertions) { "yes" } else { "no" },
        ),
        // tokio's IO driver is epoll(7) on Linux.
        (
            "--with-epoll",
            if cfg!(target_os = "linux") {
                "yes"
            } else {
                "no"
            },
        ),
        // sqlx speaks the MySQL protocol directly, no client library.
        ("--with-mariadb", "no"),
        // The pools are Arc<RwLock> in one process instead of shm segments.
        ("--with-sysv", "no"),
    ] {
        out.push_str(&flag_line(key, value));
    }
    out
}

/// One `arg_compile_info()` row. C wrote the paddings into its format string — `\t\t` after every
/// name but `\t` after `--enable-systemd` — and what that lands on is the column the eight-column
/// tab stops put the bracket in: 24.
fn flag_line(key: &str, value: &str) -> String {
    let mut column = key.len();
    let mut out = String::from(key);
    while column < 24 {
        column += 8 - (column % 8);
        out.push('\t');
    }
    out.push_str(&format!("[{value}]\n"));
    out
}

/// `main.c:217-229`: `chdir(dirname(argv[0]))` and then `chdir("..")`.
///
/// The daemon loads every path relative to the server root, which the C executable assumes is
/// one level above the directory it was started from.
#[derive(Debug)]
pub enum RootError {
    /// `chdir(<dirname>) error: <errno>`
    ExecDir(String, i32),
    /// `chdir(..) error: <errno>`
    Parent(i32),
    /// The current directory could not be read back after both moves.
    Getcwd(i32),
}

impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootError::ExecDir(dir, errno) => write!(f, "chdir({dir}) error: {errno}"),
            RootError::Parent(errno) => write!(f, "chdir(..) error: {errno}"),
            RootError::Getcwd(errno) => write!(f, "getcwd() error: {errno}"),
        }
    }
}

impl RootError {
    /// C returned -1 from `main()` for both failures.
    pub fn exit_code(&self) -> i32 {
        -1
    }
}

pub fn enter_server_root(argv0: &str) -> Result<PathBuf, RootError> {
    // dirname("bbsd") is "." and dirname("/usr/bin/bbsd") is "/usr/bin". Path::parent() gives
    // Some("") for the former, which set_current_dir would reject.
    let exec_dir = match Path::new(argv0).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };

    let exec_dir_str = exec_dir.to_string_lossy().to_string();
    std::env::set_current_dir(&exec_dir)
        .map_err(|err| RootError::ExecDir(exec_dir_str, errno(&err)))?;
    std::env::set_current_dir("..").map_err(|err| RootError::Parent(errno(&err)))?;

    let root = std::env::current_dir().map_err(|err| RootError::Getcwd(errno(&err)))?;
    Ok(root.canonicalize().unwrap_or(root))
}

/// `main.c:231-238`: the locale C installs, and refuses to start without. It is the locale
/// libc's `strftime` writes an article cache header in, which is the one place the port hands a
/// formatted string rather than a byte it copied.
#[cfg(unix)]
pub fn apply_locale() -> Result<(), &'static str> {
    let name = c"en_US.UTF-8";
    if unsafe { libc::setlocale(libc::LC_ALL, name.as_ptr()) }.is_null() {
        return Err("setlocale(LC_ALL, en_US.UTF-8) error");
    }
    Ok(())
}

/// There is no locale to install on Windows: `strftime` is not what formats the header there (see
/// [`crate::cache`]), and the UCRT rejects `en_US.UTF-8` as a name, which would otherwise fail the
/// check above and stop the daemon at startup for a reason that no longer applies.
#[cfg(not(unix))]
pub fn apply_locale() -> Result<(), &'static str> {
    Ok(())
}

/// C reported `errno` by number in these messages, so the OS error code is pulled back out.
fn errno(err: &std::io::Error) -> i32 {
    err.raw_os_error().unwrap_or(-1)
}

/// `init_daemon()` (`init.c:29-62`): the classic double fork, `setsid()` and `umask(022)`.
///
/// The parent of each fork exits with status 0, so a shell that started `bbsd &` returns
/// immediately while the daemon goes on without a controlling terminal. `_exit()` rather than
/// `exit()` because the C used it: the parent must not flush anything it had buffered.
///
/// The tokio runtime is built only after this returns, which is what keeps `fork()` away from
/// the runtime's worker threads.
#[cfg(unix)]
pub fn init_daemon() -> Result<(), DaemonError> {
    match unsafe { fork() } {
        Err(err) => return Err(err),
        Ok(pid) if pid > 0 => unsafe { libc::_exit(0) },
        Ok(_) => {}
    }

    // Become a session leader so the terminal it came from can no longer signal it.
    let sid = unsafe { libc::setsid() };
    if sid == -1 {
        return Err(DaemonError(errno(&std::io::Error::last_os_error())));
    }

    match unsafe { fork() } {
        Err(err) => return Err(err),
        Ok(pid) if pid > 0 => unsafe { libc::_exit(0) },
        Ok(_) => {}
    }

    // The process is no longer a session leader, so it can never reacquire a controlling
    // terminal; only the file mode is left to set.
    unsafe { libc::umask(0o022) };

    Ok(())
}

/// Windows has no `fork`, no session to leave and no `umask`, so `-d` keeps the process in the
/// foreground on the console it was started from. That is what a service wrapper or a scheduled
/// task wants in any case, and the failures this function could report are all detaching failures.
#[cfg(not(unix))]
pub fn init_daemon() -> Result<(), DaemonError> {
    Ok(())
}

/// `init_daemon()` failed; `main.c` ignored the code and carried on, which is the one place the
/// port refuses to do the same.
#[derive(Debug)]
pub struct DaemonError(pub i32);

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fork()/setsid() error: {}", self.0)
    }
}

impl std::error::Error for DaemonError {}

#[cfg(unix)]
unsafe fn fork() -> Result<i32, DaemonError> {
    let pid = unsafe { libc::fork() };
    if pid == -1 {
        return Err(DaemonError(errno(&std::io::Error::last_os_error())));
    }
    Ok(pid)
}

/// Write a message where C used `fprintf(stderr, …)` for the argument handlers and `APP_INFO`.
pub fn print_stderr(text: &str) {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
#[path = "../tests/test_daemon.rs"]
mod tests;
