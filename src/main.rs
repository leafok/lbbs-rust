/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `bbsd` — the entry point, ported from `src/main.c` and the parent half of
//! `src/net_server.c`.
//!
//! The order is `main()`'s: arguments, daemonize, server root, log, configuration, badwords,
//! the EULA timestamp, the runtime directories, the menus and text files, the first load from
//! the database, then the listeners — and on `SIGTERM` the drain, then the exit. What the two
//! `fork()`-based halves of the original were for becomes task and state work:
//!
//! * `net_server()` accepted on both sockets from one `epoll` loop and `fork_server()`d per
//!   connection. [`net::Gateway`] holds the two limits and [`net::listen`] runs the loop; each
//!   accepted connection is a task ([`net`], [`ssh`]).
//! * `section_list_loader_launch()` started a second process that polled the database once a
//!   second for the whole life of the daemon. [`Loader::tick`] is that body, driven by a task
//!   here that stops when the daemon does — and by the *same* loader the startup pass used,
//!   because the forked child inherited the op-log cursor it had just read, and a periodic pass
//!   without that cursor appends new articles but never replays a change.
//! * The `SYS_conf_reload` branch of `net_server.c:809-885` — `log_restart`, `load_conf`,
//!   `bwf_load`, the EULA `stat`, both menu trees, `load_file`, `load_section_config_from_db`,
//!   then `kill(0, SIGUSR1)` so the children picked up the same — is [`reload_configuration`],
//!   run by `SIGHUP`. The last step has no equivalent: there are no children to tell, and the
//!   sessions read the published [`Arc`] snapshots the next time they draw a screen.
//!
//! Deviations worth naming up front:
//!
//! * The runtime is built here by hand rather than with `#[tokio::main]`, because `init_daemon()`
//!   is still a `fork()`: a runtime built before it would leave its worker threads nonexistent
//!   in the process that goes on serving.
//! * Every startup failure leaves with a non-zero status. C's `goto cleanup` path unloads the
//!   shared-memory pools and returns `0` (`main.c:460-533`), which tells a supervisor that a
//!   daemon which never bound a socket exited happily; there is nothing to unload here, so the
//!   failure is what the status reports. The codes still match the original where it had a
//!   distinct one: `1` for a bad argument, `255` for `-1`, `254` for `-2`.
//! * The four `var/*.shm` files `main.c:294-317` truncates are the SysV segment backing files
//!   and are not created at all. `trie_dict_init`, `article_block_init`, `section_list_init` and
//!   `lml_init` need no startup step either, since the pools are ordinary memory.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use lbbs::bwf::Badwords;
use lbbs::config::{self, Config};
use lbbs::daemon::{self, Args};
use lbbs::db::Db;
use lbbs::loader::Loader;
use lbbs::log::{self, LogSink};
use lbbs::menu::MenuSet;
use lbbs::net::{self, Front, Gateway};
use lbbs::paths::{
    CONF_BBSD, CONF_BWF, CONF_MENU, CONF_TOP10_MENU, DATA_EULA, VAR_ARTICLE_CACHE_DIR,
    VAR_CHICKEN_DIR, VAR_SECTION_AID_LOC_DIR,
};
use lbbs::ssh;
use lbbs::state::AppState;

/// `main()`'s successful return.
const EXIT_OK: u8 = 0;
/// `arg_error()`'s `exit(1)`.
const EXIT_USAGE: u8 = 1;
/// The `-1` returns of `main.c:223`, `:228`, `:243` and `net_server()`'s own failures.
const EXIT_STARTUP: u8 = 255;
/// `load_conf()` or `bwf_load()` failing, which `main.c:260`/`:266` answered with `-2`.
const EXIT_CONFIG: u8 = 254;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let argv0 = argv
        .first()
        .cloned()
        .unwrap_or_else(|| daemon::PROG_NAME.to_string());

    // main.c:188-208, handlers applied in argument order, so the first printing option wins.
    let flags = match daemon::parse(&argv) {
        Args::Help => {
            daemon::print_stderr(&daemon::usage(daemon::PROG_NAME));
            return code(EXIT_OK);
        }
        Args::Version => {
            daemon::print_stderr(&format!(
                "{}\n{}\n",
                daemon::app_info(),
                daemon::COPYRIGHT_INFO
            ));
            return code(EXIT_OK);
        }
        Args::CompileConfig => {
            daemon::print_stderr(&daemon::compile_config());
            return code(EXIT_OK);
        }
        Args::Invalid => {
            daemon::print_stderr("Invalid arguments\n");
            daemon::print_stderr(&daemon::usage(daemon::PROG_NAME));
            return code(EXIT_USAGE);
        }
        Args::Run(flags) => flags,
    };

    // main.c:211-214. C ignored the code of init_daemon() and carried on as a foreground
    // process; detaching is the point of the option, so the port stops instead.
    if !flags.foreground {
        if let Err(err) = daemon::init_daemon() {
            daemon::print_stderr(&format!("{err}\n"));
            return code(EXIT_STARTUP);
        }
    }

    // main.c:217-229: chdir(dirname(argv[0])) and then chdir("..").
    let root = match daemon::enter_server_root(&argv0) {
        Ok(root) => root,
        Err(err) => {
            daemon::print_stderr(&format!("{err}\n"));
            return code(EXIT_STARTUP);
        }
    };

    // main.c:231-238: the locale is part of the article cache header's format, so a server
    // without it writes different bytes than the C one. APP_INFO is printed where the C one was.
    if let Err(err) = daemon::apply_locale() {
        daemon::print_stderr(&format!("{err}\n"));
        return code(EXIT_STARTUP);
    }
    daemon::print_stderr(&format!("{}\n", daemon::app_info()));

    // main.c:241-253.
    let sink = match log::begin(&root) {
        Ok(sink) => sink,
        Err(err) => {
            daemon::print_stderr(&format!("{err}\n"));
            return code(EXIT_STARTUP);
        }
    };
    if flags.foreground {
        if flags.display_log {
            sink.redirect_common_to_console();
        }
        if flags.display_error_log {
            sink.redirect_error_to_console();
        }
    }
    if let Err(err) = sink.install() {
        daemon::print_stderr(&format!("{err}\n"));
        return code(EXIT_STARTUP);
    }
    tracing::info!("Starting {}", daemon::app_info());

    // main.c:258-267, both of which answer -2.
    let config = match config::load(CONF_BBSD) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("{err}");
            return code(EXIT_CONFIG);
        }
    };

    let badwords = match Badwords::load(root.join(CONF_BWF)) {
        Ok(badwords) => badwords,
        Err(err) => {
            tracing::error!("bwf_load({CONF_BWF}) error: {err}");
            return code(EXIT_CONFIG);
        }
    };

    // main.c:270-291: the EULA timestamp and the two directories the article cache uses.
    let eula_tm = match file_mtime(&root.join(DATA_EULA)) {
        Some(tm) => tm,
        None => {
            tracing::error!("stat({DATA_EULA}) error");
            return code(EXIT_STARTUP);
        }
    };
    if let Err(err) = create_dir(&root.join(VAR_ARTICLE_CACHE_DIR)) {
        tracing::error!("mkdir({VAR_ARTICLE_CACHE_DIR}) error: {err}");
        return code(EXIT_STARTUP);
    }
    if let Err(err) = create_dir(&root.join(VAR_SECTION_AID_LOC_DIR)) {
        tracing::error!("mkdir({VAR_SECTION_AID_LOC_DIR}) error: {err}");
        return code(EXIT_STARTUP);
    }
    // The chicken records and the one log they share (`chicken.c:31-32`); without the directory
    // `create_a_egg` cannot write its egg and `chicken_main` answers its `-2`.
    if let Err(err) = create_dir(&root.join(VAR_CHICKEN_DIR)) {
        tracing::error!("mkdir({VAR_CHICKEN_DIR}) error: {err}");
        return code(EXIT_STARTUP);
    }

    // main.c:350-360 and :363-369 need no database, so they run before the runtime is built;
    // the menus are read from disk by read_menu_file below.

    // The tokio runtime, built after the daemonize so no worker thread is ever forked.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("runtime construction error: {err}");
            return code(EXIT_STARTUP);
        }
    };

    let startup = Startup {
        root,
        config,
        badwords,
        eula_tm,
        sink,
    };
    match runtime.block_on(serve(startup)) {
        Ok(()) => code(EXIT_OK),
        Err(err) => {
            tracing::error!("{err}");
            // C returned -1 from the `cleanup:` label after logging; the port says so too.
            code(EXIT_STARTUP)
        }
    }
}

/// Everything the synchronous half of `main()` worked out for the async half.
struct Startup {
    root: PathBuf,
    config: Config,
    badwords: Badwords,
    eula_tm: i64,
    sink: LogSink,
}

/// The database, the in-memory state, both listeners and the loader task, then the drain.
async fn serve(startup: Startup) -> Result<(), StartupError> {
    let Startup {
        root,
        config,
        badwords,
        eula_tm,
        sink,
    } = startup;

    // ssh_finalize() had no equivalent until the listeners existed: a server that cannot sign a
    // handshake cannot answer one connection, which is why net_server.c returned -1 here.
    let host_keys = ssh::load_host_keys(&root).map_err(|err| StartupError {
        message: err.to_string(),
    })?;
    let ssh_config = ssh::config(host_keys);

    let db = Db::connect(&config.db).await.map_err(|err| StartupError {
        message: format!("Db::connect() error: {err}"),
    })?;

    let state = AppState::new(config, db);
    state.publish_badwords(badwords);
    state.set_eula_tm(eula_tm);

    // main.c:350-360, then the reload path's copy of the same two loads.
    let menus = read_menu_file(&root, CONF_MENU, "bbs_menu")?;
    state.publish_menus(menus);
    let top10 = read_menu_file(&root, CONF_TOP10_MENU, "top10_menu")?;
    state.publish_top10(top10);

    // main.c:363-369: every missing text file is a log line and nothing more.
    state.load_data_files(&root);

    // main.c:372-416: the two user tables, the boards, the op watermark, all articles and the
    // per-user counters, before the first client can be accepted. The `Arc` comes first because
    // the loader that does this pass is the one that carries on doing them: C's
    // `section_list_loader_launch` forked, so the child kept the op cursor `main.c:395` had read
    // — and a loader that starts a second one from scratch never replays an article change.
    let state = std::sync::Arc::new(state);
    let mut loader = Loader::new(std::sync::Arc::clone(&state), &root);
    loader.initial_load().await.map_err(|err| StartupError {
        message: format!("initial load error: {err}"),
    })?;

    // main.c:418-448: SIGHUP reloads, SIGTERM exits, SIGUSR1 reopens the logs, SIGPIPE is
    // ignored. The port's `SIGPIPE` equivalent is that a failed write is an `io::Error` the
    // session loops already turn into an end of session.
    spawn_signal_handlers(std::sync::Arc::clone(&state), sink.clone());

    // net_server.c:604-660: bind both sockets, or the daemon does not start.
    let ports = state.config();
    let telnet = net::bind(&listener_addr(&ports, ports.telnet_port))
        .await
        .map_err(|err| StartupError {
            message: err.to_string(),
        })?;
    let ssh_listener = net::bind(&listener_addr(&ports, ports.ssh_port))
        .await
        .map_err(|err| StartupError {
            message: err.to_string(),
        })?;
    drop(ports);

    let gateway = Gateway::new(std::sync::Arc::clone(&state), root.clone());

    // section_list_loader_launch(), as a task that stops when the daemon does — the same loader,
    // so the pass order starts from the cursor the startup pass left behind. Its own `select!`
    // waits on the shutdown the accept loop below breaks on, so nothing has to abort it.
    tokio::spawn(loader.run());

    // The two accept loops, which in C were the same epoll over two sockets.
    let telnet_task = tokio::spawn(net::listen(
        std::sync::Arc::clone(&gateway),
        telnet,
        Front::Telnet,
    ));
    let ssh_task = tokio::spawn(net::listen(
        std::sync::Arc::clone(&gateway),
        ssh_listener,
        Front::Ssh(ssh_config),
    ));

    // The parent loop of net_server.c:809-: every 100 ms either a reload pass or the exit flag.
    loop {
        tokio::select! {
            _ = state.shutdown() => break,
            _ = state.wait_reload() => {
                reload_configuration(&state, &root, &sink).await;
            }
        }
    }

    tracing::info!("Main process exiting");
    // net_server.c:771-807: notify, wait 15 s, then stop waiting 1 s later.
    gateway.drain().await;
    telnet_task.abort();
    ssh_task.abort();

    tracing::info!("Main process exit normally");
    Ok(())
}

/// The reload branch of the C accept loop, minus the `kill(0, SIGUSR1)` that told the children.
///
/// A load that fails leaves the previous snapshot published where C would have dropped the
/// half-loaded one: the menus are swapped only once the whole file parses, because a session
/// that reaches a menu in the meantime would otherwise see an empty tree.
async fn reload_configuration(state: &std::sync::Arc<AppState>, root: &Path, sink: &LogSink) {
    tracing::info!("Reload configuration");

    if sink.restart() < 0 {
        tracing::error!("Restart logging failed");
    }

    match config::load(CONF_BBSD) {
        // The listeners already bound their addresses, so a changed `bbs_port` takes effect on
        // the next start — as it did in C, where the accept loop never re-bound.
        Ok(config) => state.publish_config(config),
        Err(err) => tracing::error!("Reload conf failed: {err}"),
    }

    match Badwords::load(root.join(CONF_BWF)) {
        Ok(badwords) => state.publish_badwords(badwords),
        Err(err) => tracing::error!("Reload BWF conf failed: {err}"),
    }

    match file_mtime(&root.join(DATA_EULA)) {
        Some(tm) => state.set_eula_tm(tm),
        None => tracing::error!("stat({DATA_EULA}) error"),
    }

    match read_menu_file(root, CONF_MENU, "bbs_menu") {
        Ok(menus) => state.publish_menus(menus),
        Err(err) => tracing::error!("{}", err.message),
    }
    match read_menu_file(root, CONF_TOP10_MENU, "top10_menu") {
        Ok(menus) => state.publish_top10(menus),
        Err(err) => tracing::error!("{}", err.message),
    }

    state.load_data_files(root);

    let loader = Loader::new(std::sync::Arc::clone(state), root);
    if let Err(err) = loader.load_section_config().await {
        tracing::error!("load_section_config_from_db(1) error: {err}");
    }
}

/// `load_menu()`: read the file and hand the bytes to the parser, with C's error word.
fn read_menu_file(root: &Path, path: &str, name: &str) -> Result<MenuSet, StartupError> {
    let data = std::fs::read(root.join(path)).map_err(|err| StartupError {
        message: format!("load_menu({name}) error: {err}"),
    })?;
    MenuSet::parse(&data).map_err(|err| StartupError {
        message: format!("load_menu({name}) error: {err}"),
    })
}

/// `sin_addr = (hostaddr[0] != '\0' ? inet_addr(hostaddr) : INADDR_ANY)` (`net_server.c:620`):
/// an empty `bbs_ip`/`bbs_address` means every v4 address, not the local host.
fn listener_addr(config: &Config, port: u16) -> String {
    if config.bbs_address.is_empty() {
        format!("0.0.0.0:{port}")
    } else {
        format!("{}:{port}", config.bbs_address)
    }
}

/// `stat(path).st_mtim.tv_sec`, which `main.c:270` and the reload branch both read.
fn file_mtime(path: &Path) -> Option<i64> {
    let tm = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some(tm.as_secs() as i64)
}

/// `mkdir(path, 0750)`, which the original tolerated `EEXIST` and nothing else.
fn create_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::DirBuilderExt;

    // `mode` is the only reason this needs to be mut, and it only exists on Unix.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    // No mode to ask for on Windows, where the directory inherits its ACL.
    #[cfg(unix)]
    builder.mode(0o750);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// The `sigaction` table of `main.c:418-448`, as tasks.
fn spawn_signal_handlers(state: std::sync::Arc<AppState>, sink: LogSink) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let mut hangup = signal(SignalKind::hangup()).expect("SIGHUP");
        let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM");
        let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT");
        let mut reopen = signal(SignalKind::user_defined1()).expect("SIGUSR1");

        loop {
            tokio::select! {
                // `sig_hup_handler`: the flag the accept loop polls.
                _ = hangup.recv() => state.request_reload(),
                // `sig_term_handler`, and the same thing `shutdown_bbs` asks for.
                _ = terminate.recv() => state.exit(),
                // C installed no SIGINT handler, so the default disposition killed the daemon
                // at once; asking every session to close first is the gentler equivalent of a
                // ^C on a foreground `-f` run.
                _ = interrupt.recv() => state.exit(),
                // `sig_usr1_handler`, which the children ran and the parent ignored: the
                // daemon has the only copy of the two streams now.
                _ = reopen.recv() => {
                    if sink.restart() < 0 {
                        tracing::error!("Restart logging failed");
                    }
                }
            }
        }
    });

    // Windows delivers none of those: there is no SIGHUP to reload on and no SIGUSR1 to reopen the
    // logs with, which leaves the sink above with nothing to do. What it does deliver, the console
    // control events, all arrive through `ctrl_c`, and C had no handler for the ^C they stand for
    // beyond the default kill — the orderly exit above is the port's gentler version of it.
    #[cfg(not(unix))]
    {
        let _ = &sink;
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                state.exit();
            }
        });
    }
}

/// A startup failure, in the words `main.c` wrote to the log before its `return`.
#[derive(Debug)]
struct StartupError {
    message: String,
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StartupError {}

fn code(value: u8) -> ExitCode {
    ExitCode::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    use lbbs::session::now;

    #[test]
    fn an_empty_bind_address_is_every_interface() {
        let mut cfg = Config {
            bbs_id: "t".into(),
            bbs_name: "t".into(),
            bbs_server: String::new(),
            bbs_address: String::new(),
            telnet_port: 2323,
            ssh_port: 2222,
            max_client: 10,
            max_client_per_ip: 2,
            start_dt: String::new(),
            sys_id: 0,
            db: lbbs::config::DbConfig {
                host: String::new(),
                username: String::new(),
                password: String::new(),
                database: String::new(),
                timezone: String::new(),
            },
        };
        assert_eq!(listener_addr(&cfg, cfg.telnet_port), "0.0.0.0:2323");
        cfg.bbs_address = "127.0.0.1".into();
        assert_eq!(listener_addr(&cfg, cfg.ssh_port), "127.0.0.1:2222");
    }

    #[test]
    fn a_missing_file_has_no_mtime() {
        assert_eq!(file_mtime(Path::new("/nonexistent/eula.txt")), None);
    }

    #[test]
    fn eula_timestamp_is_the_c_one() {
        let dir = std::env::temp_dir().join(format!("lbbs-mtime-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("eula.txt");
        std::fs::write(&file, b"agreement").unwrap();
        let tm = file_mtime(&file).unwrap();
        assert!((tm - now()).abs() < 60, "{tm} vs {}", now());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_directory_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("lbbs-mkdir-{}", now()));
        assert!(create_dir(&dir).is_ok());
        // The second call is the EEXIST the original forgave.
        assert!(create_dir(&dir).is_ok());
        assert!(std::fs::metadata(&dir).unwrap().is_dir());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn a_menu_file_reports_c_read_failure() {
        let err = read_menu_file(
            Path::new("/nonexistent"),
            "var/menu_merged.conf",
            "bbs_menu",
        )
        .unwrap_err();
        assert!(
            err.message.starts_with("load_menu(bbs_menu) error: "),
            "{}",
            err.message
        );
    }

    #[test]
    fn the_exit_codes_are_the_ones_main_c_returned() {
        // -1 and -2 as the shell sees them, plus arg_error()'s 1.
        assert_eq!(
            (EXIT_OK, EXIT_USAGE, EXIT_STARTUP, EXIT_CONFIG),
            (0, 1, 255, 254)
        );
        assert_eq!(bbsd_code(-1), EXIT_STARTUP);
        assert_eq!(bbsd_code(-2), EXIT_CONFIG);
        assert_eq!(bbsd_code(0), EXIT_OK);
    }

    /// `return -1` from `main()` is status 255; the port keeps the C numbers in one place.
    fn bbsd_code(c_code: i32) -> u8 {
        c_code as u8
    }
}
