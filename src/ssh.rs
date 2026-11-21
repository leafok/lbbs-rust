/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The SSH front end, ported from the libssh callbacks in `src/net_server.c`.
//!
//! C `fork_server()`s as soon as an SSH connection is accepted, then runs the whole protocol in
//! the child: `ssh_bind_accept_fd()`, the key exchange, the password callback, waiting up to
//! `SSH_AUTH_MAX_DURATION` (60 s) for the client's channel, and finally `bbs_main()` with the
//! libssh session in nonblocking mode and `ssh_channel_read_nonblocking()` standing in for
//! `read(1)`. One process cannot fork, so the port keeps C's observable contract and moves the
//! plumbing around it:
//!
//! * [`crate::net`] still accepts and counts connections, and hands the connected socket to
//!   [`russh::server::run_stream`] with a ready [`SshHandler`]. The `Server` trait, which would
//!   own an accept loop of its own, stays unimplemented so the two limits live in one place.
//! * Password authentication only, the way `ssh_set_auth_methods(…, SSH_AUTH_METHOD_PASSWORD)`
//!   asked for it, and with the same three-try budget: `auth_password` (`net_server.c:119-153`)
//!   counts a failed, an erroring, or a format-rejected attempt, and after `BBS_login_retry_times`
//!   refuses every further attempt. C's `2` from `check_user` becomes [`Session::update_eula`]
//!   instead of a prompt.
//! * One session channel per connection. C's `channel_open` handed back the single
//!   `SSH_channel` it kept in file scope; a second request here is rejected.
//! * `exec` and `subsystem` are refused in the way C's `exec_request` refused them: log
//!   `Forbid exec /bin/sh -c <command>)` and let the request succeed without running anything
//!   (`net_server.c:207`, `219`).
//!
//! The two differences worth knowing about:
//!
//! * The window size is not honoured. `pty_request` stored rows and columns in `cdata->winsize`
//!   only to pass them to `openpty()`/`TIOCSWINSZ` for a process that never called
//!   `TIOCGWINSZ`, and the screen code here is built on [`crate::paths::SCREEN_ROWS`] — the
//!   same 24×80 `igetch`-era constants the telnet path uses.
//! * There is no 60-second auth clock. russh's `inactivity_timeout` counts packets, so reusing
//!   C's `SSH_AUTH_MAX_DURATION` for it would drop an idle *session* after a minute; the port
//!   leaves it unset, keeps `max_auth_attempts` at C's retry count, and lets
//!   [`crate::bbs::bbs_center`] enforce `BBS_max_user_idle_time` on the key reads as it always
//!   did.
//!
//! Between the two halves sits a [`crate::term::Pipe`], which is what `dup2(socket_client,
//! STDIN_FILENO)` was for C. A pump task copies the channel's bytes into the pipe and the
//! terminal's bytes into `Handle::data`; when either side goes away [`Pipe::hang_up`] makes the
//! terminal's next read report end of stream, which is the same `KEY_NULL` a telnet client
//! hanging up produces, so `bbs_main` needs no SSH-specific exit path.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use russh::server::{self, Auth, Msg, Session as SshSession};
use russh::{Channel, ChannelId, Pty, SshId};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::bbs;
use crate::login::{self, LOGIN_RETRY_TIMES};
use crate::net::Admitted;
use crate::paths::{SSH_HOST_ECDSA_KEY, SSH_HOST_ED25519_KEY, SSH_HOST_RSA_KEY};
use crate::session::Session;
use crate::term::{ByteIo, Pipe, Terminal};

/// The buffer the pump copies out of the terminal with, in bytes.
const PUMP_BUF_LEN: usize = 16 * 1024;

/// What [`load_host_keys`] could not do.
#[derive(Debug)]
pub struct HostKeyError {
    /// The reason, in C's words: it gave up on the whole SSH listener rather than one key.
    pub cause: String,
}

impl std::fmt::Display for HostKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cause)
    }
}

impl std::error::Error for HostKeyError {}

/// `net_server.c:560-592`: the three packaged key types, in the order libssh was offered them.
///
/// A key that is not there is logged and skipped, exactly as a failed
/// `ssh_bind_options_set()` was; only an empty set is fatal, because a server that cannot sign
/// the handshake cannot finish one.
pub fn load_host_keys(root: &Path) -> Result<Vec<russh::keys::PrivateKey>, HostKeyError> {
    let mut keys = Vec::new();
    for path in [SSH_HOST_RSA_KEY, SSH_HOST_ED25519_KEY, SSH_HOST_ECDSA_KEY] {
        let full = root.join(path);
        match russh::keys::load_secret_key(&full, None) {
            Ok(key) => keys.push(key),
            Err(err) => tracing::error!("Error loading SSH host key {}: {err}", full.display()),
        }
    }

    if keys.is_empty() {
        return Err(HostKeyError {
            cause: "Error: no valid SSH host key".to_string(),
        });
    }
    Ok(keys)
}

/// The `sshbind` options of `net_server.c:560-602` plus the child's session options.
///
/// The host key algorithms are the same list C appended with `+ssh-ed25519,ecdsa-sha2-nistp256,
/// ssh-rsa`: `Config::preferred` takes `Cow`ed algorithm lists, and the default already offers
/// all three of the keys [`load_host_keys`] reads, so only the banner is set here.
pub fn config(keys: Vec<russh::keys::PrivateKey>) -> Arc<server::Config> {
    Arc::new(server::Config {
        // libssh answered `SSH-2.0-libssh_<version>`; name the port instead of that.
        server_id: SshId::Standard(Cow::Owned(format!(
            "SSH-2.0-lbbs-rust_{}",
            env!("CARGO_PKG_VERSION")
        ))),
        methods: (&[russh::MethodKind::Password][..]).into(),
        keys,
        max_auth_attempts: LOGIN_RETRY_TIMES as usize,
        inactivity_timeout: None,
        // A terminal session dies on buffered output, and C's child never buffered either:
        // `ssh_set_blocking(SSH_session, 0)` was the last option it set.
        nodelay: true,
        ..server::Config::default()
    })
}

/// One SSH connection, shared by the handler russh drives and the two tasks it starts.
///
/// Everything is in a `Mutex<Option<_>>` because each piece is handed over exactly once: the
/// terminal and the session go to the session task, the far half of the pipe and the input
/// queue go to the pump, and the input sender is the only thing both sides touch.
struct Connection {
    session: Mutex<Option<Session>>,
    terminal: Mutex<Option<Terminal>>,
    /// The network half of [`Pipe::hang_up`]'s pair, taken by the pump.
    net: Mutex<Option<Pipe>>,
    input_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    input_rx: Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>,
    /// `bbs_main`, awaited by [`serve`] so the online row is deleted before the slot is given
    /// back and the daemon's drain sees the session as finished.
    session_task: Mutex<Option<JoinHandle<()>>>,
}

impl Connection {
    fn new(admitted: &Admitted) -> Arc<Self> {
        let gateway = Arc::clone(&admitted.gateway);
        let (session_pipe, net) = Pipe::new();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let addr = admitted.addr;

        let session = Session::new(
            Arc::clone(&gateway.state),
            gateway.root.clone(),
            gateway.next_sid(),
            addr.ip().to_string(),
            addr.port(),
            true,
        );

        let mut terminal = Terminal::new(ByteIo::Pipe(session_pipe));
        terminal.set_exit_watch(gateway.state.exit_watch());

        Arc::new(Connection {
            session: Mutex::new(Some(session)),
            terminal: Mutex::new(Some(terminal)),
            net: Mutex::new(Some(net)),
            input_tx: Mutex::new(Some(input_tx)),
            input_rx: Mutex::new(Some(input_rx)),
            session_task: Mutex::new(None),
        })
    }

    /// Client keystrokes, or `false` once the connection is over.
    async fn send_input(&self, data: &[u8]) -> bool {
        let input = self.input_tx.lock().await;
        match &*input {
            Some(sender) => sender.send(data.to_vec()).is_ok(),
            None => false,
        }
    }

    /// `sdata->error`, and the child's exit when the socket went away: no more input, so the
    /// pump hangs up the pipe and the session's next key read ends it.
    async fn hang_up(&self) {
        *self.input_tx.lock().await = None;
    }
}

/// The handler russh runs for one connection. `Handler` has no lifetime parameters of its own,
/// so the connection state is an [`Arc`] that outlives it.
pub struct SshHandler {
    conn: Arc<Connection>,
    /// `SSH_channel`: the one session channel this connection gets.
    chan: Option<ChannelId>,
    /// `sdata->tries`.
    tries: u32,
    /// `sdata->error`.
    refused: bool,
}

impl SshHandler {
    fn new(conn: Arc<Connection>) -> Self {
        SshHandler {
            conn,
            chan: None,
            tries: 0,
            refused: false,
        }
    }

    /// C started `bbs_main()` once the channel existed; here that is also where the pump starts,
    /// because until the channel id is known there is nowhere to send output.
    async fn start(&mut self, handle: server::Handle, chan: ChannelId) {
        let Some(net) = self.conn.net.lock().await.take() else {
            return;
        };
        let Some(input_rx) = self.conn.input_rx.lock().await.take() else {
            return;
        };
        let (Some(terminal), Some(session)) = (
            self.conn.terminal.lock().await.take(),
            self.conn.session.lock().await.take(),
        ) else {
            return;
        };

        let conn = Arc::clone(&self.conn);
        let task = tokio::spawn(async move {
            let (mut terminal, mut session) = (terminal, session);
            bbs::bbs_main(&mut terminal, &mut session).await;
            // `bbs_session_cleanup()`, on the `cleanup:` label C reached from every exit.
            session.cleanup().await;
            // Dropping the terminal drops its end of the pipe, which is how the pump learns the
            // session is over and closes the channel.
        });
        *conn.session_task.lock().await = Some(task);

        tokio::spawn(pump(net, input_rx, handle, chan));
    }
}

impl server::Handler for SshHandler {
    type Error = russh::Error;

    /// `auth_password` (`net_server.c:119-153`).
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.refused {
            return Ok(Auth::reject());
        }

        let accepted = {
            // The session is taken by the channel once it opens, and an account may not be
            // authenticated into a session that has already started.
            let mut session = self.conn.session.lock().await;
            let Some(session) = session.as_mut() else {
                return Ok(Auth::reject());
            };

            let outcome = if user == "guest" {
                login::load_guest_info(session).await;
                Ok(login::Auth::Success)
            } else {
                // C printed `check_user`'s messages to the child's stdout, which was not yet the
                // channel's: nothing the client could see, so the writes go to a closed pipe.
                login::check_user(&mut mute_terminal(), session, user, password).await
            };

            match outcome {
                Ok(login::Auth::Success) => true,
                Ok(login::Auth::Agreement) => {
                    session.update_eula = true;
                    true
                }
                // C counted `1` and `-1` the same way, and a database error is `-1`.
                Ok(login::Auth::Failed) | Ok(login::Auth::Error) | Err(_) => {
                    self.tries += 1;
                    tracing::info!(
                        "User [{user}] authentication failed ({}/{LOGIN_RETRY_TIMES})",
                        self.tries
                    );
                    if self.tries >= LOGIN_RETRY_TIMES {
                        tracing::error!("SSH auth error, tried {} times", self.tries);
                        self.refused = true;
                    }
                    false
                }
            }
        };

        if !accepted {
            return Ok(Auth::reject());
        }
        tracing::info!("User [{user}] authenticated successfully");
        Ok(Auth::Accept)
    }

    /// `channel_open` (`net_server.c:279-291`), which handed back its one channel.
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        if self.chan.is_some() {
            reply
                .reject(russh::ChannelOpenFailure::ResourceShortage)
                .await;
            return Ok(());
        }

        let handle = session.handle();
        reply.accept().await;
        self.chan = Some(channel.id());
        self.start(handle, channel.id()).await;
        Ok(())
    }

    /// `pty_request` (`net_server.c:155-180`), acknowledged; the size is dropped for the reason
    /// given in the module comment.
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _cols: u32,
        _rows: u32,
        _px: u32,
        _py: u32,
        _modes: &[(Pty, u32)],
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        Ok(())
    }

    /// `shell_request` (`net_server.c:244-262`), which C answered by `exec_pty("-l", NULL)` —
    /// nothing spawned, the request simply granted.
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_success(channel);
        Ok(())
    }

    /// `exec_request` (`net_server.c:225-242`) and the `subsystem_request` arm that fed
    /// `/usr/lib/sftp-server` into it (`:272-275`), which this file does not.
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        tracing::error!("Forbid exec /bin/sh -c {})", String::from_utf8_lossy(data));
        let _ = session.channel_success(channel);
        Ok(())
    }

    /// `subsystem_request` (`net_server.c:264-277`).
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        tracing::error!("subsystem_request(subsystem={name})");
        if name != "sftp" {
            let _ = session.channel_failure(channel);
        }
        Ok(())
    }

    /// `channel_data_function` (`net_server.c:230-262`): the bytes went to the child's stdin,
    /// and here they go to the terminal's input queue.
    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        self.conn.send_input(data).await;
        Ok(())
    }

    /// The half-close in C's read loop: `ssh_channel_read_nonblocking()` returning `0` made the
    /// child break out of the poll loop and finish `bbs_main()` on a closed input.
    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        self.conn.hang_up().await;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        session: &mut SshSession,
    ) -> Result<(), Self::Error> {
        self.conn.hang_up().await;
        self.chan = None;
        let _ = session.close(channel);
        Ok(())
    }
}

/// `ssh_channel_read_nonblocking`/`ssh_channel_write` in one loop: channel → pipe and
/// pipe → channel, ending when either side is gone.
async fn pump(
    mut net: Pipe,
    mut input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    handle: server::Handle,
    chan: ChannelId,
) {
    let mut buf = vec![0u8; PUMP_BUF_LEN];
    // Once the sender is gone the receive branch would be ready forever, so it is switched off
    // the first time it reports `None` — the pipe's hang-up then ends the session for us.
    let mut input_open = true;

    loop {
        tokio::select! {
            biased;

            written = net.read(&mut buf) => match written {
                // The terminal is gone, so the session has run to its end.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if handle.data(chan, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            },
            keystroke = input_rx.recv(), if input_open => {
                match keystroke {
                    Some(data) => net.push(&data),
                    None => {
                        input_open = false;
                        net.hang_up();
                    }
                }
            }
        }
    }

    // C's child called `ssh_channel_close(SSH_channel)` on the same paths.
    let _ = handle.eof(chan).await;
    let _ = handle.close(chan).await;
}

/// A terminal whose writes all fail, for messages the original had no channel to show.
fn mute_terminal() -> Terminal {
    let (terminal, client) = Pipe::new();
    drop(client);
    Terminal::new(ByteIo::Pipe(terminal))
}

/// One accepted SSH connection, from [`crate::net`]'s accept loop.
///
/// The slot in [`crate::net::Gateway`] is released when this function returns, which is after
/// `bbs_main` has deleted the `user_online` row — the counting difference a forked child did not
/// have, since its exit was the parent's `waitpid`.
pub(crate) async fn serve(admitted: Admitted, stream: TcpStream, config: Arc<server::Config>) {
    let addr: SocketAddr = admitted.addr;
    let conn = Connection::new(&admitted);

    if config.nodelay && stream.set_nodelay(true).is_err() {
        // libssh set the same option on the same socket and only logged the failure.
        tracing::error!("set_nodelay() failed");
    }

    let handler = SshHandler::new(Arc::clone(&conn));
    let running = match russh::server::run_stream(config, stream, handler).await {
        Ok(running) => running,
        Err(err) => {
            // `ssh_handle_key_exchange() error: %s` in the child, followed by its `cleanup:`.
            tracing::error!("ssh_handle_key_exchange() error: {err}");
            conn.hang_up().await;
            return;
        }
    };

    // The future ends with russh's own loop for this connection: the client disconnected, a
    // protocol error happened, or the channel closed.
    if let Err(err) = running.await {
        tracing::error!("ssh event loop error: {err}");
    }

    conn.hang_up().await;
    let task = conn.session_task.lock().await.take();
    if let Some(task) = task {
        // A session task that panicked has already lost its half of the pipe, so the pump is
        // done too; waiting is what gives the online row a chance to go.
        let _ = task.await;
    }
    drop(admitted);
    tracing::debug!("SSH connection from {addr} closed");
}
