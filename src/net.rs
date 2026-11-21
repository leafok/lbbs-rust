/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The accept loops of `src/net_server.c`, with the fork replaced by a task.
//!
//! C listens on one socket (or two: telnet and SSH behind the same `epoll`), and for every
//! connection that passes the two limits it `fork_server()`s a child that serves exactly one
//! user and then exits. The accounting that `SYS_child_process_count` and the
//! `hash_dict_sockaddr_count` did is [`Limits`] here, and `fork` is `tokio::spawn`, which
//! leaves three differences worth naming:
//!
//! * A child counted itself, so `SYS_child_process_count - 1` was the number of *user*
//!   processes. [`Limits::entered`] returns that number directly and the comparison reads the
//!   same as the original's.
//! * C closes the accepted socket without a word when it refuses it. The port does the same,
//!   because a client that has been refused gets the same RST either way.
//! * On exit C signals every child and then `waitpid`s until the count drops, escalating to
//!   `SIGKILL` after 15 s and giving up 1 s later. The port waits on the same two clocks
//!   ([`Limits::drain`]) but has nothing to kill: a task whose client stopped reading is
//!   dropped, not reaped, which is why the second timeout logs without a `kill(-1, SIGKILL)`.
//!
//! The reload and exit signals are the daemon's ([`crate::state::AppState`]); the listeners
//! only ever stop for them.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::bbs;
use crate::session::{clamp_bytes, sid_for};
use crate::state::AppState;
use crate::term::{ByteIo, Terminal};

/// `WAIT_CHILD_PROCESS_EXIT_TIMEOUT` (`net_server.c:73`): how long the drain waits for the
/// sessions to notice `SYS_server_exit` on their own.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// `WAIT_CHILD_PROCESS_KILL_TIMEOUT`: the extra second before the daemon stops waiting.
pub const FORCE_EXIT_TIMEOUT: Duration = Duration::from_secs(1);

/// `user_online.SID varchar(32)` (`db.rs:37`): the width the identifier has to fit.
const SID_MAX_LEN: usize = 32;

/// The two accept limits and the live counts they are checked against.
///
/// The semaphore is the total: `acquire_owned` both checks and reserves, so two connections
/// arriving at once cannot both be told they are the last one. C had that race too — it read
/// `SYS_child_process_count` in the parent, but only ever one accept ran at a time. The
/// per-IP map is behind a [`Mutex`] for the same reason the counter was not atomic: it is two
/// lookups and an update.
pub struct Gateway {
    pub state: Arc<AppState>,
    /// The directory `data/` and `var/` hang off, as in `main.c:217-229`.
    pub root: std::path::PathBuf,
    limits: Mutex<PerIp>,
    total: Arc<Semaphore>,
    /// `SYS_child_process_count - 1`, kept beside the permits so the drain can watch it fall.
    live: Arc<AtomicUsize>,
    /// The `user_online.SID` sequence: `getpid()` for C, a counter per process here.
    next_sid: AtomicU64,
}

/// `hash_dict_sockaddr_count`, plus the count the dictionary's value held.
#[derive(Default)]
struct PerIp {
    by_addr: HashMap<IpAddr, usize>,
}

impl Gateway {
    pub fn new(state: Arc<AppState>, root: std::path::PathBuf) -> Arc<Self> {
        let (max_client, _) = state.limits();
        Arc::new(Gateway {
            // The reload path can raise or lower the limit; the permit count is what startup
            // read, and a reload that changes it is reported by `crate::daemon`'s caller.
            total: Arc::new(Semaphore::new(max_client.max(1))),
            limits: Mutex::new(PerIp::default()),
            live: Arc::new(AtomicUsize::new(0)),
            next_sid: AtomicU64::new(1),
            state,
            root,
        })
    }

    /// Live user sessions, the number the reject messages quote.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// The `user_online.SID` of the next session. C used `getpid()`, which is unique among the
    /// live children; a counter is unique among the live tasks.
    pub(crate) fn next_sid(&self) -> String {
        let id = self.next_sid.fetch_add(1, Ordering::Relaxed);
        clamp_bytes(&sid_for(id), SID_MAX_LEN)
    }

    /// The `SYS_child_process_count - 1 < BBS_max_client` and
    /// `j < BBS_max_client_per_ip` tests, in the order the original ran them, so a connection
    /// that breaks both is reported for the total as C reported it.
    async fn admit(self: &Arc<Self>, addr: SocketAddr) -> Admission {
        let (max_client, max_per_ip) = self.state.limits();
        let permit = match Arc::clone(&self.total).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return Admission::Rejected(Reject::Total(self.live(), max_client));
            }
        };
        // The C comparison is against the limit *before* this connection, which is what the
        // semaphore's `live()` reports while the permit is in flight.
        let mut per_ip = self.limits.lock().await;
        let same_ip = per_ip.by_addr.get(&addr.ip()).copied().unwrap_or(0);
        if same_ip >= max_per_ip {
            return Admission::Rejected(Reject::PerIp(same_ip, max_per_ip));
        }
        per_ip.by_addr.insert(addr.ip(), same_ip + 1);
        drop(per_ip);

        self.live.fetch_add(1, Ordering::Relaxed);
        Admission::Accepted(Admitted {
            _permit: permit,
            gateway: Arc::clone(self),
            addr,
            same_ip,
        })
    }

    /// `hash_dict_inc(hash_dict_sockaddr_count, addr, -1)` on the child's exit.
    fn release(&self, addr: IpAddr) {
        let per_ip = self.limits.try_lock();
        if let Ok(mut per_ip) = per_ip
            && let Some(count) = per_ip.by_addr.get_mut(&addr)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_ip.by_addr.remove(&addr);
            }
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
    }

    /// `while (!SYS_server_exit || SYS_child_process_count > 0)` in the parent: the two
    /// escalation clocks of `net_server.c:771-807`.
    ///
    /// Returns once no session is left or the second timeout expired, in which case the
    /// sessions are still running and the caller leaves them as `kill(-1, SIGKILL)` left the
    /// children — nothing waits for them again.
    pub async fn drain(&self) {
        let started = match self.live() {
            0 => return,
            _ => Instant::now(),
        };
        let mut stage = 0;
        let mut stage_at = started;

        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if self.live() == 0 {
                return;
            }

            let now = Instant::now();
            match stage {
                0 => {
                    // `kill(0, SIGTERM)`: the flag every menu loop and key read polls.
                    tracing::info!("Notify {} sessions to exit", self.live());
                    self.state.exit();
                    stage = 1;
                    stage_at = now;
                }
                1 if now - stage_at >= DRAIN_TIMEOUT => {
                    // There is no way to stop a task from outside, so this is where C sent
                    // SIGKILL and the port simply starts ignoring the stragglers.
                    tracing::error!("Give up waiting for {} sessions", self.live());
                    stage = 2;
                    stage_at = now;
                }
                2 if now - stage_at >= FORCE_EXIT_TIMEOUT => {
                    tracing::error!(
                        "Main process prepare to exit without waiting for {} sessions any longer",
                        self.live()
                    );
                    return;
                }
                _ => {}
            }
        }
    }
}

/// What [`Gateway::admit`] decided.
pub(crate) enum Admission {
    /// The reserved slot, carrying the permit that keeps the count honest.
    Accepted(Admitted),
    Rejected(Reject),
}

/// Which limit refused, with the two numbers C prints beside it.
pub(crate) enum Reject {
    /// `(SYS_child_process_count - 1 >= BBS_max_client)`.
    Total(usize, usize),
    /// `(j >= BBS_max_client_per_ip)`.
    PerIp(usize, usize),
}

/// A connection that holds a slot: the permit keeps the total reserved and the `Drop` gives
/// both counts back, whether the session ran, failed to start, or was never polled.
pub(crate) struct Admitted {
    _permit: OwnedSemaphorePermit,
    pub(crate) gateway: Arc<Gateway>,
    pub(crate) addr: SocketAddr,
    /// `j`, the number from the same address before this one.
    same_ip: usize,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        self.gateway.release(self.addr.ip());
    }
}

/// The `net_server.c` accept loop serves both sockets from one `epoll`, and the only thing that
/// differs between them is what happens to the accepted socket.
pub enum Front {
    /// The telnet arm: the session reads and writes the socket directly.
    Telnet,
    /// The SSH arm: russh runs the protocol over the socket and [`crate::ssh`] moves the
    /// terminal's bytes through the channel. Carries the host keys and the password-only
    /// method list that `ssh_set_auth_methods()` installed.
    Ssh(Arc<russh::server::Config>),
}

impl Front {
    /// `NET_SERVER_TYPE_*` in C's log messages; the reject lines quote the same word.
    fn kind(&self) -> &'static str {
        match self {
            Front::Telnet => "telnet",
            Front::Ssh(_) => "SSH",
        }
    }
}

/// `net_server.c:996-1010`: the second connection from one address says how many were already
/// counted, the first says nothing of the kind.
fn log_accept(admitted: &Admitted, kind: &str) {
    if admitted.same_ip == 0 {
        tracing::info!("Accept {kind} connection from {}", admitted.addr);
    } else {
        tracing::info!(
            "Accept {kind} connection from {}, already have {} connections",
            admitted.addr,
            admitted.same_ip
        );
    }
}

/// One accepted telnet connection: `bbs_main` on the socket.
async fn serve_one(admitted: Admitted, stream: TcpStream) {
    let gateway = Arc::clone(&admitted.gateway);
    let addr = admitted.addr;
    let mut terminal = Terminal::new(ByteIo::Tcp(stream));
    terminal.set_exit_watch(gateway.state.exit_watch());
    let mut session = crate::session::Session::new(
        Arc::clone(&gateway.state),
        gateway.root.clone(),
        gateway.next_sid(),
        addr.ip().to_string(),
        addr.port(),
        false,
    );

    bbs::bbs_main(&mut terminal, &mut session).await;
    // `bbs_session_cleanup()`, which C ran on the `cleanup:` label of every exit path.
    session.cleanup().await;
}

/// `net_server(hostaddr, port)` for one socket, until the daemon asks to stop.
///
/// The listener is nonblocking in the C sense too: `accept` is called in a loop until it
/// reports `EAGAIN`, which `accept()` on a Tokio listener does by awaiting. The difference
/// shows up only under a burst, where the port hands each connection its own task instead of
/// its own process.
pub async fn listen(gateway: Arc<Gateway>, listener: TcpListener, front: Front) {
    let kind = front.kind();
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    tracing::info!("Listening at {addr} ({kind})");

    loop {
        tokio::select! {
            _ = gateway.state.shutdown() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(err) => {
                        // C logs the errno and keeps the listener; a persistent failure shows
                        // up as a stream of these lines, as it did in the log of the original.
                        tracing::error!("accept(socket_server) error: {err}");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                };

                match gateway.admit(peer).await {
                    Admission::Accepted(admitted) => {
                        log_accept(&admitted, kind);
                        match &front {
                            Front::Telnet => {
                                tokio::spawn(serve_one(admitted, stream));
                            }
                            Front::Ssh(config) => {
                                tokio::spawn(crate::ssh::serve(
                                    admitted,
                                    stream,
                                    Arc::clone(config),
                                ));
                            }
                        }
                    }
                    Admission::Rejected(Reject::Total(live, max)) => {
                        tracing::error!("Rejected {kind} connection from {peer} over limit ({live} >= {max})")
                    }
                    Admission::Rejected(Reject::PerIp(count, max)) => {
                        tracing::error!("Rejected {kind} connection from {peer} over limit per IP ({count} >= {max})")
                    }
                }
            }
        }
    }
}

/// Bind one address, which is `net_server.c:600-660`'s `socket()`+`bind()`+`listen()` and the
/// reason the daemon's exit code is `-1` when it fails.
pub async fn bind(addr: &str) -> Result<TcpListener, BindError> {
    let listener = TcpListener::bind(addr).await.map_err(|err| BindError {
        addr: addr.to_string(),
        cause: err.to_string(),
    })?;
    Ok(listener)
}

/// `SocketError` in `net_server.c`: the address and the reason, which `main.c` reports and
/// exits on.
#[derive(Debug)]
pub struct BindError {
    pub addr: String,
    pub cause: String,
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to listen on {}: {}", self.addr, self.cause)
    }
}

impl std::error::Error for BindError {}
