/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The site shuttle: the station list behind `@RunMBEM` and the relay that carries a session
//! through to the station picked from it.
//!
//! Ported from `src/bbs_net.c` — the constants at `:48-61`, the station table and its reader at
//! `:63-233`, `progress_bar` at `:253-294`, `connect_timeout` at `:296-329`, `bbsnet_connect` at
//! `:331-1426`, `bbsnet_refresh` at `:1428-1449`, `bbsnet_selchange` at `:1453-1474`, and the
//! key loop of `bbs_net` at `:1476-1573` — together with `exec_mbem`, the `@mod:` dispatcher
//! `menu.c:1281` reaches with the item's name (`src/menu_proc.c:53-121`, item
//! `conf/menu.conf:129`).
//!
//! The screen comes out of [`MenuSet::hand_built`] and [`MenuNav`], because C builds its
//! `MENU_SET` with `calloc` and then calls `display_menu`/`menu_control` on it like any other
//! menu; the rows below the frame are what the port adds.
//!
//! # Deviations from C
//!
//! * `configure.ac:76-78` turns `dlopen` on with `--enable-shared`, which is how the reference
//!   tree is configured (`config.h` answers `_ENABLE_SHARED 1`), so C looks the name after the
//!   `#` up as a symbol in the named `.so` and prints `dlerror`'s text when it is not there, or
//!   `加载库文件 [...] 失败!!` when the library itself will not load. The port has no dynamic
//!   loading, so it answers with the table the `#else` branch of `menu_proc.c:97-121` compiles to:
//!   the two names that branch knows, `bbs_net` here and `chicken_main` in [`crate::chicken`], and
//!   the `未知入口` line for anything else. With no `#` in the parameter, C hands a NULL `strstr`
//!   never found to `strcasecmp`; the port reports the empty entry name instead of crashing.
//! * The relay is one `tokio::select!` over the two reads instead of an `epoll`/`poll` round over
//!   three descriptors, so `stdout_write_wait` and the `O_NONBLOCK` toggling of `STDIN`/`STDOUT`
//!   (`:505-524`) have no counterpart: output goes through [`Terminal`], which owns the buffer
//!   and the socket. A key typed during the connect wait is also looked at once per 100 ms round
//!   rather than the instant `poll` reports it.
//! * `igetch(0)` answering `0` is C's "nothing to decode", which the port reports as
//!   [`key::KEY_RETRY`]; when it answers `0` because the client went away C spins in that
//!   `do … while`, and the port ends the connect instead.
//! * The SSH leg keeps C's ten-second budget and its messages but not its plumbing: libssh is put
//!   in non-blocking mode and each call is retried until it stops answering `SSH_AGAIN`, while
//!   russh is awaited once per stage ([`stage`]) with the progress bar running beside it. russh
//!   never reads the user's SSH configuration, which is what
//!   `SSH_OPTIONS_PROCESS_CONFIG = 0` asked libssh for, and the host-key list is russh's own
//!   default rather than `+ssh-ed25519,ecdsa-sha2-nistp256,ssh-rsa`.
//! * `var/known_hosts` is resolved under the server root; C gave libssh a path relative to the
//!   working directory. C asks `ssh_session_is_known_server` once the handshake is over and acts
//!   on its answer, while russh decides during the handshake, so a refused key ends the connect
//!   from inside [`ShuttleClient::check_server_key`] and the verdict travels out to be reported
//!   there. russh's own `check_known_hosts_path` calls a host whose recorded key has a different
//!   algorithm "unknown", which would add the new key; the port reads the recorded entries itself
//!   so `SSH_KNOWN_HOSTS_OTHER` is refused as C refuses it. `SSH_KNOWN_HOSTS_ERROR`, which C's
//!   `switch` has no arm for and so walks past into the password prompt, is refused too.
//! * Conversion runs through the session's UTF-8 rather than through two iconv descriptors:
//!   [`Terminal::read_raw`] hands over what the client sent already transcoded, and
//!   [`Terminal::print_bytes`] takes the station's output back in UTF-8 and lets [`Terminal`]
//!   transcode it for the client. What C drops (`//IGNORE`), the station's encoder replaces, and
//!   what C would copy through unchanged (`//TRANSLIT`, and any byte iconv rejects) becomes the
//!   replacement character. The `ESC`-plus-one-byte pass-through of `io_buf_conv` (`io.c:1141-1167`)
//!   is not needed: ANSI sequences are ASCII, and both charsets carry those through untouched.
//! * A station charset other than `UTF-8` or `GBK` takes C's `iconv_open() error` path.
//! * `load_bbsnet_conf`'s `-1` (two `calloc` failures) and `-3` (an exhausted item pool, which
//!   the loop bound already prevents) cannot happen; `-2`, the file not opening, is the only
//!   failure the port reports.
//! * Lines longer than `LINE_BUFFER_LEN` are read whole, where C's `fgets` would split them into
//!   two records and reject the tail half.
//! * `%-20s` pads by bytes, the port pads by display cells: they differ only for a host name
//!   containing characters wider than one cell.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use encoding_rs::{Decoder, GBK};
use russh::keys::{PublicKey, PublicKeyOrCertificate, known_hosts};
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::browse;
use crate::menu::{MenuItem, MenuNav, MenuSet};
use crate::paths::{
    CHARSET_MAX_LEN, CONF_BBSNET, LINE_BUFFER_LEN, MAX_USER_IDLE_TIME, SCREEN_COLS, SCREEN_ROWS,
    SSH_KNOWN_HOSTS,
};
use crate::screen::{self, Echo};
use crate::session::{self, Session};
use crate::term::key::{
    self, CR, KEY_DOWN, KEY_END, KEY_ESC, KEY_HOME, KEY_LEFT, KEY_NULL, KEY_PGDN, KEY_PGUP,
    KEY_RETRY, KEY_RIGHT, KEY_TIMEOUT, KEY_UP,
};
use crate::term::{Charset, RawRead, Terminal};

/// `MAXSTATION` (`:49`): the number of stations the table, and so the menu, can hold.
pub const MAX_STATION: usize = 26 * 2;
/// `STATION_PER_LINE` (`:50`): the columns of the grid, and how far the arrow keys step.
pub const STATION_PER_LINE: usize = 4;
/// `ORG_NAME_MAX_LEN` … `PORT_STR_MAX_LEN` (`:51-54`), the field widths of the config file.
pub const ORG_NAME_MAX_LEN: usize = 40;
pub const SITE_NAME_MAX_LEN: usize = 40;
pub const HOSTNAME_MAX_LEN: usize = 253;
pub const PORT_STR_MAX_LEN: usize = 5;
/// `USERNAME_MAX_LEN` and `PASSWORD_MAX_LEN` (`:55-56`): the remote login prompt's buffers.
/// They are `bbs_net`'s own constants, narrower in one case and wider in the other than the
/// site's [`crate::paths::USERNAME_MAX_LEN`].
pub const REMOTE_USERNAME_MAX_LEN: usize = 20;
pub const REMOTE_PASSWORD_MAX_LEN: usize = 20;
/// `REMOTE_CONNECT_TIMEOUT` (`:57`): the budget every connect stage shares. The
/// `SSH_CONNECT_TIMEOUT` beside it is never read.
pub const REMOTE_CONNECT_TIMEOUT: u64 = 10;
/// `PROGRESS_BAR_LEN` (`:59`): the width of the bar between the rules.
pub const PROGRESS_BAR_LEN: usize = 30;

/// The name `load_bbsnet_conf` gives its one menu (`:110`), and the only way back out of it is
/// the `Ctrl+C` the frame advertises.
pub const MENU_NAME: &str = "BBSNET";
/// What `user_online_update` is told while the shuttle is on screen (`:1479` and `:341`).
pub const ACTION: &str = "BBS_NET";

/// `MENU_CONF_DELIM` (`:75`): the delimiters `strtok_r` splits a station line on.
const CONFIG_DELIM: [u8; 4] = [b' ', b'\t', b'\r', b'\n'];
/// `igetch(100)` in the key loop, and the `poll` timeout in the relay.
const POLL: Duration = Duration::from_millis(100);
/// `REMOTE_CONNECT_TIMEOUT * 1000`, the unit `connect_timeout` works in.
const CONNECT_BUDGET_MS: i64 = REMOTE_CONNECT_TIMEOUT as i64 * 1000;
/// What libssh's `ssh_channel_request_pty` (`:862`) really asks for: it is a wrapper for
/// `ssh_channel_request_pty_size(channel, "xterm", 80, 24)`, so the station gets the site's own
/// screen size rather than the one the visitor is using.
const PTY_TERM: &str = "xterm";

/// `struct _bbsnet_conf` (`:63-72`), one line of `conf/bbsnet.conf`. The port keeps the port
/// number as the token C stored in it, because both the display and the lookup print or resolve
/// that string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Station {
    pub org_name: String,
    pub site_name: String,
    pub host_name: String,
    pub port: String,
    pub use_ssh: bool,
    pub charset: String,
}

/// `MENU_ITEM *p_menu_item` as `load_bbsnet_conf` fills it (`:199-217`): the grid position, the
/// one- or two-letter hot key, and the cyan initial in front of the station name.
fn station_item(id: usize, station: &Station) -> MenuItem {
    let hotkey = hotkey_name(id);
    MenuItem {
        row: (2 + id / STATION_PER_LINE) as i16,
        col: (5 + (id % STATION_PER_LINE) * 20) as i16,
        action: id.to_string(),
        action_menu_id: None,
        action_cmd: None,
        submenu: false,
        priv_mask: 0,
        level: 0,
        name: hotkey.to_string(),
        text: format!("\x1b[1;36m{hotkey}.\x1b[m {}", station.site_name),
    }
}

/// `p_menu_item->name[0] = menu_item_id < MAXSTATION / 2 ? 'A' + … : 'a' + …` (`:207-208`).
fn hotkey_name(id: usize) -> char {
    if id < MAX_STATION / 2 {
        (b'A' + id as u8) as char
    } else {
        (b'a' + (id - MAX_STATION / 2) as u8) as char
    }
}

/// `load_bbsnet_conf` (`:79-233`): the stations to show, in file order. The `Err` is C's `-2`;
/// a line with anything wrong in it is skipped after its own message, exactly as C `continue`s
/// past it without spending a `menu_item_id`.
pub fn load_conf(root: &Path) -> io::Result<Vec<Station>> {
    let bytes = std::fs::read(root.join(CONF_BBSNET))?;
    let mut stations = Vec::new();

    for (fin_line, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let mut tokens = line
            .split(|b| CONFIG_DELIM.contains(b))
            .filter(|f| !f.is_empty());
        // `strtok_r` returns NULL for a blank line, which is the same as having no fields at all.
        let Some(org_name) = tokens.next() else {
            continue;
        };
        if org_name[0] == b'#' {
            continue;
        }
        let mut fields: [Option<&[u8]>; 6] = [Some(org_name), None, None, None, None, None];
        for slot in fields[1..].iter_mut() {
            *slot = tokens.next();
        }

        let text = |field: Option<&[u8]>, max: usize, name: &str| -> Option<String> {
            match field {
                // C tests the missing field and the over-long one in the same `if`, and so gives
                // them the same message.
                None => {
                    tracing::error!("Error {name} in BBSNET config line {}", fin_line + 1);
                    None
                }
                Some(value) if value.len() > max => {
                    tracing::error!("Error {name} in BBSNET config line {}", fin_line + 1);
                    None
                }
                Some(value) => Some(String::from_utf8_lossy(value).into_owned()),
            }
        };

        let Some(org_name) = text(Some(org_name), ORG_NAME_MAX_LEN, "org_name") else {
            continue;
        };
        let Some(site_name) = text(fields[1], SITE_NAME_MAX_LEN, "site_name") else {
            continue;
        };
        let Some(host_name) = text(fields[2], HOSTNAME_MAX_LEN, "host_name") else {
            continue;
        };
        let Some(port) = text(fields[3], PORT_STR_MAX_LEN, "port") else {
            continue;
        };
        // `strtol` with the whole token consumed, then the range check (`:157-162`): the message
        // quotes the number either way, so `22x` is reported as `22` and `007` is kept as written
        // while resolving as seven.
        let (parsed, complete) = strtol_decimal(port.as_bytes());
        if !complete || parsed <= 0 || parsed > 65535 {
            tracing::error!(
                "Invalid port {parsed} in BBSNET config line {}",
                fin_line + 1
            );
            continue;
        }
        // `strlen(p) != 1` is its own error: the flag is a single letter.
        let use_ssh = match fields[4] {
            Some(flag) if flag.len() == 1 => flag[0].to_ascii_uppercase() == b'Y',
            _ => {
                tracing::error!("Error use_ssh in BBSNET config line {}", fin_line + 1);
                continue;
            }
        };
        let Some(charset) = text(fields[5], CHARSET_MAX_LEN, "charset") else {
            continue;
        };

        stations.push(Station {
            org_name,
            site_name,
            host_name,
            port,
            use_ssh,
            charset,
        });
        if stations.len() == MAX_STATION {
            break;
        }
    }

    Ok(stations)
}

/// `bbsnet_refresh` (`:1428-1449`): the frame the stations are drawn inside. Column 80 is where
/// C puts the right-hand border, one cell past the last column of the screen.
async fn refresh(term: &mut Terminal) {
    screen::clearscr(term);

    let rule = format!(" {} ", "-".repeat(SCREEN_COLS - 2));
    term.moveto(1, 1);
    term.print(&rule);
    for row in 2..19 {
        term.moveto(row, 1);
        term.print("|");
        term.moveto(row, SCREEN_COLS);
        term.print("|");
    }
    term.moveto(19, 1);
    term.print(&format!("|{}|", "-".repeat(SCREEN_COLS - 2)));
    term.moveto(22, 1);
    term.print(&rule);
    term.moveto(23, 1);
    term.print(" [\x1b[1;32mCtrl+C\x1b[m]退出");
    let _ = term.flush().await;
}

/// `bbsnet_selchange` (`:1453-1474`): the two detail rows under the frame, which follow the
/// cursor rather than the key that was pressed.
async fn selchange(term: &mut Terminal, station: &Station) {
    // The two rows are laid out before anything is written: the padding needs the display
    // width, which the terminal is only asked for read-only.
    let padded = |field: &str, cells: usize| -> String {
        let budget = cells as i64 - term.width(field.as_bytes(), true) as i64;
        " ".repeat(browse::padding(budget))
    };
    let justified = |field: &str, cells: usize| -> String {
        let used = term.width(field.as_bytes(), true);
        format!("{field}{}", " ".repeat(cells.saturating_sub(used)))
    };

    let detail = format!(
        "|\x1b[1m单位: \x1b[1;33m{}\x1b[m{}  站名: \x1b[1;33m{}\x1b[m{}  类型: \x1b[1;33m{}\x1b[m",
        station.org_name,
        padded(&station.org_name, 20),
        station.site_name,
        padded(&station.site_name, 20),
        if station.use_ssh { "SSH" } else { "Telnet" }
    );
    let target = format!(
        "|\x1b[1m连往: \x1b[1;33m{}\x1b[m  端口: \x1b[1;33m{}\x1b[m                 编码: \x1b[1;33m{}\x1b[m",
        justified(&station.host_name, 20),
        justified(&station.port, 5),
        station.charset
    );

    term.moveto(20, 1);
    term.clrtoeol();
    term.print(&detail);
    term.moveto(20, SCREEN_COLS);
    term.print("|");

    term.moveto(21, 1);
    term.clrtoeol();
    term.print(&target);
    term.moveto(21, SCREEN_COLS);
    term.print("|");
    let _ = term.flush().await;
}

/// `progress_bar` (`:253-294`): the percentage of the connect budget that has gone, between two
/// rules and centred inside them.
async fn progress_bar(term: &mut Terminal, percent: i64, len: usize) {
    let len = len.clamp(4, LINE_BUFFER_LEN - 3);
    let percent = percent.clamp(0, 100);
    let pos = len * percent as usize / 100;

    let rule = format!(" {} ", "-".repeat(len));
    let bar = format!(
        "{}{:>3}%{}",
        " ".repeat((len - 4) / 2),
        percent,
        " ".repeat((len - 4 + 1) / 2)
    );

    term.moveto(4, 1);
    term.print(&format!("{rule}\r\n"));
    term.print(&format!(
        "|\x1b[46m{}\x1b[44m{}\x1b[m|\r\n",
        &bar[..pos],
        &bar[pos..]
    ));
    term.print(&format!("{rule}\r\n"));
    let _ = term.flush().await;
}

/// The clock `connect_timeout` (`:296-329`) reads, with the percentage it last painted.
struct Clock {
    begin: Instant,
    progress_last: i64,
}

impl Clock {
    fn start() -> Clock {
        Clock {
            begin: Instant::now(),
            progress_last: 0,
        }
    }

    /// `connect_timeout`: repaint when the percentage moves, and answer `true` once the budget
    /// is gone, which is when C shows `连接超时！` and waits for a key.
    async fn tick(&mut self, term: &mut Terminal) -> bool {
        let duration_ms = self.begin.elapsed().as_millis() as i64;
        if duration_ms >= CONNECT_BUDGET_MS {
            progress_bar(term, 100, PROGRESS_BAR_LEN).await;
            term.print("\x1b[1;31m连接超时！\x1b[m\r\n");
            screen::press_any_key(term).await;
            return true;
        }

        let progress = duration_ms * 100 / CONNECT_BUDGET_MS + 1;
        if progress != self.progress_last {
            self.progress_last = progress;
            progress_bar(term, progress, PROGRESS_BAR_LEN).await;
        }

        false
    }
}

/// What a connect stage answers: the value C's `ssh_*` call finally returns, or nothing because
/// the ten-second budget ran out first.
enum Stage<T> {
    Done(T),
    TimedOut,
}

/// C retries a non-blocking libssh call until it stops answering `SSH_AGAIN`, checking the bar
/// between tries (`:736-917`); the port awaits the stage once and keeps the bar moving beside it.
async fn stage<F: Future>(term: &mut Terminal, clock: &mut Clock, fut: F) -> Stage<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Stage::Done(out),
            _ = tokio::time::sleep(POLL) => {
                if clock.tick(term).await {
                    return Stage::TimedOut;
                }
            }
        }
    }
}

/// `getaddrinfo(host, service, AF_INET)` (`:462-467`, `:489-500`): C's hints ask for IPv4 only,
/// so the first v4 answer is the address used.
async fn resolve_v4(host: &str, port: u16) -> io::Result<SocketAddr> {
    let mut addrs = tokio::net::lookup_host((host, port)).await?;
    addrs
        .find(|addr| addr.is_ipv4())
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no IPv4 address"))
}

/// C's `hints.ai_family = AF_INET`, the `bind`/`connect` half of what `resolve_v4` already asked
/// the resolver for. `SockAddr` builds the `sockaddr_in` — including the BSD length byte that
/// Linux has no field for — on every platform, which is what this replaces.
fn v4_only(addr: SocketAddr) -> io::Result<SockAddr> {
    match addr {
        SocketAddr::V4(_) => Ok(SockAddr::from(addr)),
        SocketAddr::V6(_) => Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "not IPv4")),
    }
}

/// Why the socket could not be prepared (`:471-505`): the two `log_error` calls C makes for them
/// name the stage, so the port keeps them apart.
enum SocketError {
    Socket(io::Error),
    Bind(io::Error),
    NonBlocking(io::Error),
}

/// `socket`/`bind`/`fcntl(O_NONBLOCK)` (`:471-505`), the socket left bound to the site's own
/// address so that the station sees the connection coming from where the session came in.
///
/// The handle is still unregistered when this returns: C's first loop retries `connect()` on it
/// before anything waits on the runtime, and `registered` moves it into the reactor afterwards.
/// `Socket` owns the descriptor throughout, which is what the paired `libc::close` calls on each
/// failure path used to do by hand.
fn bound_socket(local: SocketAddr) -> Result<Socket, SocketError> {
    let addr = v4_only(local).map_err(SocketError::Bind)?;
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .map_err(SocketError::Socket)?;
    if let Err(err) = sock.bind(&addr) {
        return Err(SocketError::Bind(err));
    }
    set_lowdelay(&sock);
    if let Err(err) = sock.set_nonblocking(true) {
        return Err(SocketError::NonBlocking(err));
    }
    Ok(sock)
}

/// The `poll(POLLOUT)` of C's second loop (`:600-677`) waits through the runtime, which needs to
/// own the registration; `set_nonblocking` above is what lets it take the socket.
fn registered(sock: Socket) -> io::Result<TcpStream> {
    TcpStream::from_std(std::net::TcpStream::from(sock))
}

/// `connect()` in C's first loop (`:579-597`): `Ok` is the connection completing at once, and
/// [`connect_pending`] decides whether a failure still means the loop should watch the socket.
fn connect_now(sock: &Socket, remote: SocketAddr) -> io::Result<()> {
    sock.connect(&v4_only(remote)?)
}

/// The `errno`s C's first loop reads as "the connect is still going": `EAGAIN`, `EALREADY` and
/// `EINPROGRESS`.
#[cfg(unix)]
const CONNECT_PENDING: [i32; 3] = [libc::EAGAIN, libc::EALREADY, libc::EINPROGRESS];

/// The same three as Winsock numbers — `WSAEWOULDBLOCK`, `WSAEINPROGRESS`, `WSAEALREADY`. A socket
/// failure on Windows carries the Winsock code from `WSAGetLastError`, not the CRT `errno` values
/// that `libc` re-exports there, which is why the `unix` list above cannot be reused.
#[cfg(windows)]
const CONNECT_PENDING: [i32; 3] = [10035, 10036, 10037];

#[cfg(not(any(unix, windows)))]
const CONNECT_PENDING: [i32; 0] = [];

/// Whether a failed `connect()` only means the answer has not come back yet.
///
/// This compares the OS error code rather than the `io::ErrorKind` because no kind covers C's
/// three: `ErrorKind` classifies `EINPROGRESS` as `InProgress`, leaves `EALREADY` uncategorised,
/// and Windows files all three under codes `ErrorKind::WouldBlock` does not stand for.
fn connect_pending(err: &io::Error) -> bool {
    err.raw_os_error()
        .is_some_and(|code| CONNECT_PENDING.contains(&code))
}

/// `netinet/in.h`'s low-delay bit of the ToS byte, spelled out because the value is the same on
/// every platform and `libc::IP_TOS` (3 on Darwin, 1 on Linux) is not it. `set_tos_v4` applies it
/// as the `IPPROTO_IP`/`IP_TOS` pair C uses.
const IPTOS_LOWDELAY: u32 = 0x10;

/// `setsockopt(IP_TOS, IPTOS_LOWDELAY)` (`:683-688`): a failure is only worth the log line C
/// writes for it.
///
/// C sets it after the connect is confirmed. The option is per-socket and no traffic has been
/// sent by then on either path, so the only difference is that the SYN carries the bit too.
fn set_lowdelay(sock: &Socket) {
    if let Err(err) = sock.set_tos_v4(IPTOS_LOWDELAY) {
        tracing::error!("setsockopt IP_TOS={IPTOS_LOWDELAY} error ({err})");
    }
}

/// C's `do { ch = igetch(0); } while (ch == 0)` (`:663-666`): look at the terminal without
/// waiting, and take again when the bytes read did not make a key.
async fn peek_key(term: &mut Terminal) -> u32 {
    loop {
        let ch = term.read_key(Duration::ZERO).await;
        if ch != KEY_RETRY {
            return ch;
        }
    }
}

/// `ssh_session_is_known_server`'s four answers (`:762-786`), of which two refuse the station.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyVerdict {
    Accepted,
    /// C's `NOT_FOUND`/`UNKNOWN` fall-through: the key was recorded, and the log line says so.
    Learned,
    AddFailed,
    Changed,
}

fn decide_key(path: &Path, host: &str, port: u16, key: &PublicKey) -> KeyVerdict {
    let recorded = match known_hosts::known_host_keys_path(host, port, path) {
        Ok(recorded) => recorded,
        // A file this cannot read is no licence to believe whatever the station presents.
        Err(_) => return KeyVerdict::Changed,
    };
    if recorded.iter().any(|(_, known)| known == key) {
        return KeyVerdict::Accepted;
    }
    if recorded.is_empty() {
        return match known_hosts::learn_known_hosts_path(host, port, key, path) {
            Ok(()) => KeyVerdict::Learned,
            Err(_) => KeyVerdict::AddFailed,
        };
    }
    KeyVerdict::Changed
}

/// `ssh_options_set` plus the one question libssh is asked during the handshake (`:721-731`):
/// the verdict travels out through the handler, because the key arrives mid-handshake and a
/// refusal ends it.
#[derive(Clone)]
struct ShuttleClient {
    known_hosts: PathBuf,
    host: String,
    port: u16,
    verdict: Arc<Mutex<Option<KeyVerdict>>>,
}

impl russh::client::Handler for ShuttleClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let verdict = decide_key(
            &self.known_hosts,
            &self.host,
            self.port,
            &server_key.public_key(),
        );
        let accepted = matches!(verdict, KeyVerdict::Accepted | KeyVerdict::Learned);
        *self.verdict.lock().unwrap() = Some(verdict);
        Ok(accepted)
    }
}

/// The station end of the relay: the socket C keeps as `sock`, or the libssh channel it keeps as
/// `outbound_channel`/`outbound_session`.
enum Outbound {
    Tcp(TcpStream),
    Ssh {
        /// russh's session task lives as long as its handle does, so the relay owns both halves
        /// of the channel and the handle beside them.
        handle: russh::client::Handle<ShuttleClient>,
        read: ChannelReadHalf,
        write: ChannelWriteHalf<russh::client::Msg>,
    },
}

impl Outbound {
    /// `read(sock)` or `ssh_channel_read_nonblocking(outbound_channel, …, 0)`: an empty answer is
    /// end of stream, which is C's `0` and its `SSH_EOF`.
    async fn read(&mut self) -> io::Result<Vec<u8>> {
        match self {
            Outbound::Tcp(stream) => {
                let mut buf = vec![0u8; LINE_BUFFER_LEN];
                let n = stream.read(&mut buf).await?;
                buf.truncate(n);
                Ok(buf)
            }
            Outbound::Ssh { read, .. } => loop {
                match read.wait().await {
                    Some(ChannelMsg::Data { data }) => return Ok(data.to_vec()),
                    // `is_stderr` was 0, so libssh would not have returned it.
                    Some(ChannelMsg::ExtendedData { .. }) => continue,
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        return Ok(Vec::new());
                    }
                    Some(_) => continue,
                }
            },
        }
    }

    /// `write(sock, …)` or `ssh_channel_write(outbound_channel, …)` (`:1175-1225`).
    async fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Outbound::Tcp(stream) => stream.write_all(data).await,
            Outbound::Ssh { write, .. } => write
                .data_bytes(data.to_vec())
                .await
                .map_err(io::Error::other),
        }
    }

    /// C's cleanup (`:1388-1402`): `ssh_channel_send_eof`, `ssh_channel_close` and then
    /// `ssh_disconnect` on the session the channel belonged to.
    async fn shutdown(&mut self) {
        match self {
            Outbound::Tcp(stream) => {
                let _ = stream.shutdown().await;
            }
            Outbound::Ssh { handle, write, .. } => {
                let _ = write.eof().await;
                let _ = write.close().await;
                // `ssh_disconnect(session)` (`:1400`) is libssh's own disconnect, whose reason
                // text this port does not reproduce: only the code is matched.
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "", "")
                    .await;
            }
        }
    }
}

/// The station half of the two `iconv_open` calls (`:918-937`). C names the descriptor by the
/// session charset and the station's own spelling, with `//IGNORE` on the way out and
/// `//TRANSLIT` on the way in unless the two names match; the port has one implementation of each
/// direction, so only the two charsets it can actually convert are accepted.
fn station_charset(name: &str) -> Option<Charset> {
    match name.to_ascii_lowercase().as_str() {
        "utf-8" => Some(Charset::Utf8),
        "gbk" => Some(Charset::Gbk),
        _ => None,
    }
}

/// The two iconv descriptors of `:919-937`, met in the middle: the session side is always UTF-8
/// here, so only the station's charset has to be converted to and from.
struct Codec {
    remote: Charset,
    /// GBK decodes with its own state, so a character split across reads is rejoined.
    gbk: Option<Decoder>,
}

impl Codec {
    fn to_remote(&self, utf8: &[u8]) -> Vec<u8> {
        match self.remote {
            Charset::Utf8 => utf8.to_vec(),
            Charset::Gbk => GBK.encode(&String::from_utf8_lossy(utf8)).0.into_owned(),
        }
    }

    fn to_local(&mut self, remote: &[u8]) -> Vec<u8> {
        match self.remote {
            Charset::Utf8 => remote.to_vec(),
            Charset::Gbk => {
                let decoder = self
                    .gbk
                    .get_or_insert_with(|| GBK.new_decoder_without_bom_handling());
                let mut text = String::new();
                let room = decoder
                    .max_utf8_buffer_length(remote.len())
                    .unwrap_or(remote.len() * 3);
                text.reserve(room);
                let _ = decoder.decode_to_string(remote, &mut text, false);
                text.into_bytes()
            }
        }
    }
}

/// `bbsnet_connect` (`:331-1426`). `1` is a relay that ended, `0` a connect the user cancelled or
/// the station refused, and `-1` an error: the three answers C leaves in `ret`.
async fn connect(term: &mut Terminal, session: &mut Session, station: &Station) -> i32 {
    if let Err(e) = session.online_update(Some(ACTION)).await {
        tracing::error!("user_online_update({ACTION}) error: {e}");
    }

    // `remote_user`/`remote_pass` (`:391-392`): only an SSH station asks for them, and only while
    // this scope lives.
    let mut remote_user = String::new();
    let mut remote_pass = String::new();
    if station.use_ssh {
        screen::clearscr(term);
        if !session.is_ssh {
            term.moveto(1, 1);
            term.print("只有在以SSH方式登陆本站时，才能使用SSH站点穿梭。");
            screen::press_any_key(term).await;
            return 0;
        }

        term.moveto(1, 1);
        term.print(&format!(
            "\x1b[1;32m正在准备往 {} ({}:{}) 的SSH连接... \x1b[m\r\n",
            station.site_name, station.host_name, station.port
        ));
        term.print("请输入用户名: ");
        let Some(line) = screen::str_input(
            term,
            REMOTE_USERNAME_MAX_LEN + 1,
            Echo::On,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            return -1;
        };
        remote_user = String::from_utf8_lossy(&line).into_owned();
        if remote_user.is_empty() {
            return 0;
        }

        term.moveto(3, 1);
        term.print("请输入密码: ");
        let Some(line) = screen::str_input(
            term,
            REMOTE_PASSWORD_MAX_LEN + 1,
            Echo::Off,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            return -1;
        };
        remote_pass = String::from_utf8_lossy(&line).into_owned();
        if remote_pass.is_empty() {
            return 0;
        }
    }

    screen::clearscr(term);
    term.moveto(1, 1);
    term.print(&format!(
        "\x1b[1;32m正在测试往 {} ({}:{}) 的{}连接，请稍候... \x1b[m\r\n",
        station.site_name,
        station.host_name,
        station.port,
        if station.use_ssh { "SSH" } else { "Telnet" }
    ));
    term.print("\x1b[1;32m连接进行中，按\x1b[1;33mCtrl+C\x1b[1;32m中断。\x1b[m\r\n");
    progress_bar(term, 0, PROGRESS_BAR_LEN).await;
    let mut clock = Clock::start();

    let local = match resolve_v4(&session.config.bbs_address, 0).await {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!("getaddrinfo({}) error: {e}", session.config.bbs_address);
            return -1;
        }
    };
    let sock = match bound_socket(local) {
        Ok(sock) => sock,
        Err(SocketError::Socket(e)) => {
            tracing::error!("socket() error: {e}");
            return -1;
        }
        Err(SocketError::Bind(e)) => {
            tracing::error!("bind({local}) error: {e}");
            return -1;
        }
        Err(SocketError::NonBlocking(e)) => {
            tracing::error!("fcntl(F_SETFL) error: {e}");
            return -1;
        }
    };

    // `AI_NUMERICSERV` on the stored token, which is read the way `strtol` read it at load time:
    // a token that is not a number never reaches the resolver, as it never reaches the table.
    let (parsed, complete) = strtol_decimal(station.port.as_bytes());
    let remote = match (complete, u16::try_from(parsed)) {
        (true, Ok(port)) => resolve_v4(&station.host_name, port).await,
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a port number",
        )),
    };
    let remote = match remote {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!(
                "getaddrinfo({} {}) error: {e}",
                station.host_name,
                station.port
            );
            term.print("\x1b[1;31m查找主机名失败！\x1b[m\r\n");
            screen::press_any_key(term).await;
            return -1;
        }
    };

    let mut connected = false;
    while !session.state.is_exiting() {
        if clock.tick(term).await {
            break;
        }
        match connect_now(&sock, remote) {
            Ok(()) => {
                connected = true;
                break;
            }
            Err(e) if connect_pending(&e) => break,
            // `socket2` already restarts on `EINTR`, so this only fires where a retry is not
            // possible; C's loop just calls `connect()` again.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::error!("connect(socket) error: {e}");
                term.print("\x1b[1;31m连接失败！\x1b[m\r\n");
                screen::press_any_key(term).await;
                return -1;
            }
        }
    }

    // C waits on the same descriptor in both loops; the runtime can only do the second one, so
    // the socket is registered once the `connect()` retries are done.
    let stream = match registered(sock) {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("poll(socket) error: {e}");
            return -1;
        }
    };

    // C's second loop (`:600-677`): the socket becomes writable when the answer arrives, and a
    // bad answer leaves it writable but unconnected, which is why a refused station is only
    // reported when the budget runs out.
    while !connected && !session.state.is_exiting() && !clock.tick(term).await {
        let ready = tokio::time::timeout(POLL, stream.writable()).await;
        match ready {
            // `getsockopt(SO_ERROR)` (`:646-653`): what a non-blocking connect answered.
            Ok(Ok(())) => match stream.take_error() {
                Ok(None) => connected = true,
                Ok(Some(_)) => {}
                Err(e) => {
                    tracing::error!("getsockopt() error: {e}");
                    return -1;
                }
            },
            Ok(Err(e)) => {
                tracing::error!("poll(socket) error: {e}");
                return -1;
            }
            Err(_) => {}
        }

        let ch = peek_key(term).await;
        if ch == KEY_NULL {
            return 0;
        }
        if ch == key::ctrl(b'C') || ch == KEY_ESC {
            return 0;
        }
    }
    if !connected {
        return -1;
    }

    // `getsockname` (`:690-703`): the port the kernel chose for the bound socket.
    let bound = match stream.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!("getsockname() error: {e}");
            return -1;
        }
    };

    let mut outbound = if station.use_ssh {
        match ssh_setup(
            term,
            session,
            &mut clock,
            stream,
            station,
            remote,
            &remote_user,
            &remote_pass,
        )
        .await
        {
            Ok(outbound) => outbound,
            Err(code) => return code,
        }
    } else {
        Outbound::Tcp(stream)
    };

    // C wipes the credential buffers at `cleanup` (`:1371-1372`); russh has its own copy of the
    // password by now, so nothing below reads these again.
    remote_user.clear();
    remote_pass.clear();

    term.print("\x1b[1;31m连接成功！\x1b[m\r\n");
    // `iflush()` here and everywhere in this file is the *output* flush (`io.c:296-380`): nothing
    // in C's shuttle ever discards typed input, so whatever was pressed while the bar was
    // running is still waiting to reach the station.
    let _ = term.flush().await;
    tracing::info!(
        "BBSNET connect to {}:{} from {}:{} by [{}]",
        remote.ip(),
        remote.port(),
        bound.ip(),
        bound.port(),
        session.username
    );

    let local_charset = term.charset();
    let Some(remote_charset) = station_charset(&station.charset) else {
        // The descriptor that fails first is the outbound one, and it is named with the
        // `//IGNORE` suffix unless the two charsets already match.
        let suffix = if local_charset.name().eq_ignore_ascii_case(&station.charset) {
            ""
        } else {
            "//IGNORE"
        };
        tracing::error!(
            "iconv_open({}->{}{}) error",
            local_charset.name(),
            station.charset,
            suffix
        );
        outbound.shutdown().await;
        return -1;
    };

    let begin = session::now();
    session.last_access_tm = begin;
    relay(term, session, &mut outbound, remote_charset).await;

    // C refreshes the access time before it measures the session, so a relay that ends on the
    // idle timeout does not also expire the user's own idle clock.
    session.last_access_tm = session::now();
    let used = session.last_access_tm - begin;
    tracing::info!(
        "BBSNET disconnect, {} days {} hours {} minutes {} seconds used",
        used / 86400,
        used % 86400 / 3600,
        used % 3600 / 60,
        used % 60
    );
    outbound.shutdown().await;

    1
}

/// `ssh_connect` through `ssh_channel_request_shell` (`:712-917`), each stage inside the same
/// budget as the TCP connect. The `Err` is the code `bbsnet_connect` answers with.
async fn ssh_setup(
    term: &mut Terminal,
    session: &Session,
    clock: &mut Clock,
    stream: TcpStream,
    station: &Station,
    remote: SocketAddr,
    remote_user: &str,
    remote_pass: &str,
) -> Result<Outbound, i32> {
    let verdict = Arc::new(Mutex::new(None));
    let handler = ShuttleClient {
        known_hosts: session.root.join(SSH_KNOWN_HOSTS),
        host: station.host_name.clone(),
        port: remote.port(),
        verdict: verdict.clone(),
    };

    let mut handle = match stage(
        term,
        clock,
        russh::client::connect_stream(Arc::new(russh::client::Config::default()), stream, handler),
    )
    .await
    {
        Stage::Done(Ok(handle)) => handle,
        Stage::Done(Err(e)) => {
            // The handshake stops on a refused key, so the verdict has to be read before the
            // failure is reported: it says whether C's switch would have shown a message.
            let seen = *verdict.lock().unwrap();
            match seen {
                Some(KeyVerdict::AddFailed) => {
                    tracing::error!(
                        "ssh_session_update_known_hosts({}) error",
                        station.host_name
                    );
                    term.print("\x1b[1;31m无法添加服务器证书\x1b[m\r\n");
                    screen::press_any_key(term).await;
                }
                Some(KeyVerdict::Changed) => {
                    tracing::error!(
                        "ssh_session_is_known_server({}) error: changed",
                        station.host_name
                    );
                    term.print("\x1b[1;31m服务器证书已变更\x1b[m\r\n");
                    screen::press_any_key(term).await;
                }
                _ => tracing::error!("ssh_connect() error: {e}"),
            }
            return Err(-1);
        }
        Stage::TimedOut => return Err(-1),
    };

    let learned = *verdict.lock().unwrap() == Some(KeyVerdict::Learned);
    if learned {
        tracing::info!(
            "SSH key of ({}) is added into {SSH_KNOWN_HOSTS}",
            station.host_name
        );
    }

    let auth = stage(
        term,
        clock,
        handle.authenticate_password(remote_user, remote_pass),
    )
    .await;
    match auth {
        Stage::Done(Ok(result)) if result.success() => {}
        // `SSH_AUTH_DENIED` is C's `else` arm, so any other answer is treated as one.
        Stage::Done(Ok(result)) => {
            tracing::debug!("ssh_userauth_password() error: {result:?}");
            term.print("\x1b[1;31m身份验证失败！\x1b[m\r\n");
            screen::press_any_key(term).await;
            return Err(0);
        }
        Stage::Done(Err(e)) => {
            tracing::error!("ssh_userauth_password() error: {e}");
            return Err(-1);
        }
        Stage::TimedOut => return Err(-1),
    }

    let channel: Channel<russh::client::Msg> =
        match stage(term, clock, handle.channel_open_session()).await {
            Stage::Done(Ok(channel)) => channel,
            Stage::Done(Err(e)) => {
                tracing::error!("ssh_channel_open_session() error: {e}");
                return Err(-1);
            }
            Stage::TimedOut => return Err(-1),
        };

    if let Stage::Done(Err(e)) = stage(
        term,
        clock,
        channel.request_pty(
            false,
            PTY_TERM,
            SCREEN_COLS as u32,
            SCREEN_ROWS as u32,
            0,
            0,
            &[],
        ),
    )
    .await
    {
        tracing::error!("ssh_channel_request_pty() error: {e}");
        return Err(-1);
    }
    if let Stage::Done(Err(e)) = stage(term, clock, channel.request_shell(false)).await {
        tracing::error!("ssh_channel_request_shell() error: {e}");
        return Err(-1);
    }

    let (read, write) = channel.split();
    Ok(Outbound::Ssh {
        handle,
        read,
        write,
    })
}

/// The `epoll`/`poll` round of `:959-1359`, reduced to the two reads it waits on. Everything
/// that ends the round early ends it the way C's `break` does: as a normal disconnect.
async fn relay(
    term: &mut Terminal,
    session: &mut Session,
    outbound: &mut Outbound,
    remote_charset: Charset,
) {
    let mut codec = Codec {
        remote: remote_charset,
        gbk: None,
    };

    loop {
        if session.state.is_exiting() {
            break;
        }

        let local = term.read_raw(POLL);
        let remote = outbound.read();
        tokio::select! {
            bytes = local => match bytes {
                RawRead::Closed => break,
                RawRead::Idle => {
                    if session::now() - session.last_access_tm
                        >= MAX_USER_IDLE_TIME as i64
                    {
                        tracing::debug!("User input timeout");
                        break;
                    }
                }
                RawRead::Bytes(bytes) => {
                    session.last_access_tm = session::now();
                    if let Err(e) = session.online_update(Some(ACTION)).await {
                        tracing::error!("user_online_update({ACTION}) error: {e}");
                    }
                    if outbound.write(&codec.to_remote(&bytes)).await.is_err() {
                        break;
                    }
                }
            },
            bytes = remote => match bytes {
                Ok(bytes) if !bytes.is_empty() => {
                    term.print_bytes(&codec.to_local(&bytes));
                    if term.flush().await.is_err() {
                        break;
                    }
                }
                // `read(sock)` answering `0`, or `SSH_EOF`: the station hung up.
                _ => break,
            },
        }
    }
}

/// `Ctrl('C')`, the other key C cancels the menu with.
const CTRL_C: u32 = key::ctrl(b'C');

/// `strtol(p, &endptr, 10)` (`:157`): the sign and the run of digits are the value, and the
/// answer says whether anything else was left behind. glibc saturates a token too big for a
/// `long`, which the range check then rejects.
fn strtol_decimal(token: &[u8]) -> (i64, bool) {
    let signed = matches!(token.first(), Some(b'+') | Some(b'-'));
    let negative = token.first() == Some(&b'-');
    let digits = &token[if signed { 1 } else { 0 }..];

    let mut value: i64 = 0;
    let mut used = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            break;
        }
        let digit = i64::from(c - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(digit)
        } else {
            value.saturating_mul(10).saturating_add(digit)
        };
        used += 1;
    }

    (value, used == digits.len())
}

/// `bbs_net` (`:1476-1573`): the station grid, and the keys that move through it.
pub async fn bbs_net(term: &mut Terminal, session: &mut Session) -> i32 {
    let stations = match load_conf(&session.root) {
        Ok(stations) => stations,
        Err(e) => {
            screen::clearscr(term);
            term.moveto(1, 1);
            term.print("加载穿梭配置失败！");
            screen::press_any_key(term).await;
            tracing::error!("load_bbsnet_conf({CONF_BBSNET}) error: {e}");
            return -1;
        }
    };
    let items: Vec<MenuItem> = stations
        .iter()
        .enumerate()
        .map(|(id, station)| station_item(id, station))
        .collect();
    let set = MenuSet::hand_built(MENU_NAME, items);
    let mut nav = MenuNav::new(&set);

    let chosen = |id: usize| -> Station {
        // `bbsnet_conf[menu_item_pos[0]]`: an empty table still has the zeroed entry C would use.
        stations.get(id).cloned().unwrap_or_default()
    };

    refresh(term).await;
    nav.draw(term, session).await;
    selchange(term, &chosen(nav.cursor())).await;

    while !session.state.is_exiting() {
        let ch = term.read_key(POLL).await;

        if ch != KEY_NULL && ch != KEY_TIMEOUT {
            session.last_access_tm = session::now();
        }

        match ch {
            KEY_NULL => {
                tracing::debug!("KEY_NULL");
                break;
            }
            KEY_TIMEOUT => {
                if session::now() - session.last_access_tm >= MAX_USER_IDLE_TIME as i64 {
                    tracing::debug!("User input timeout");
                    break;
                }
                continue;
            }
            KEY_ESC | CTRL_C => break,
            CR => {
                if connect(term, session, &chosen(nav.cursor())).await < 0 {
                    tracing::debug!("bbsnet_connect() error");
                }
                // C clears the screen twice here: once to throw the relay's output away, and
                // once again inside the frame it redraws.
                screen::clearscr(term);
                let _ = term.flush().await;
                refresh(term).await;
                nav.draw(term, session).await;
                selchange(term, &chosen(nav.cursor())).await;
            }
            KEY_UP | KEY_DOWN => {
                let step = if ch == KEY_UP {
                    key::KEY_UP
                } else {
                    key::KEY_DOWN
                };
                // The grid is four stations wide, so an arrow key walks a whole row.
                for _ in 0..STATION_PER_LINE {
                    nav.control(term, session, step).await;
                }
                selchange(term, &chosen(nav.cursor())).await;
            }
            KEY_LEFT | KEY_RIGHT => {
                let step = if ch == KEY_LEFT {
                    key::KEY_UP
                } else {
                    key::KEY_DOWN
                };
                nav.control(term, session, step).await;
                selchange(term, &chosen(nav.cursor())).await;
            }
            KEY_HOME | KEY_PGUP => {
                nav.control(term, session, key::KEY_PGUP).await;
                selchange(term, &chosen(nav.cursor())).await;
            }
            KEY_END | KEY_PGDN => {
                nav.control(term, session, key::KEY_PGDN).await;
                selchange(term, &chosen(nav.cursor())).await;
            }
            _ => {
                nav.control(term, session, ch).await;
                selchange(term, &chosen(nav.cursor())).await;
            }
        }
    }

    0
}

/// `exec_mbem`'s entry table (`menu_proc.c:53-121`), matched statically where C asks `dlsym`: the
/// parameter is the menu item's name, `N@mod:lib/libbbs.so#bbs_net` for the shuttle (`:129`) and
/// `C@mod:game/so/libgame.so#chicken_main` for the chicken (`:238`), and only the part after the
/// first `#` is looked at.
pub async fn exec_mbem(term: &mut Terminal, session: &mut Session, param: &str) {
    let Some((_, rest)) = param.split_once("@mod:") else {
        return;
    };
    // C looks for the first `#` after `@mod:` and compares everything past it.
    let entry = match rest.split_once('#') {
        Some((_, entry)) => entry,
        // C passes the NULL `strstr` never found to `strcasecmp`.
        None => "",
    };

    if entry.eq_ignore_ascii_case("bbs_net") {
        bbs_net(term, session).await;
        return;
    }
    if entry.eq_ignore_ascii_case("chicken_main") {
        crate::chicken::chicken_main(term, session).await;
        return;
    }

    screen::clearscr(term);
    term.print(&format!("未知入口 [{entry}] !!\r\n"));
    screen::press_any_key(term).await;
}

#[cfg(test)]
mod tests {
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
            seen.contains(
                "连往: \x1b[1;33mbbs.test            \x1b[m  端口: \x1b[1;33m2323 \x1b[m"
            ),
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
            seen.contains(
                "连往: \x1b[1;33m                    \x1b[m  端口: \x1b[1;33m     \x1b[m"
            ),
            "{seen}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
