/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The state one daemon process serves its clients from.
//!
//! The C original splits this across four shared-memory segments (`section_list`,
//! `article_block`, `user_list`, and the per-child `hash_dict`s) plus SysV semaphores, because
//! a new fork()ed child attached to all of them read-only and the loader was a separate
//! process. One async process needs none of that: the [`Board`] sits behind one
//! [`RwLock`], and the double-buffered user tables become an [`Arc`] swap, which is the same
//! trick with a cheaper publish.
//!
//! Lock discipline, which is what keeps the async tasks free of deadlocks:
//!
//! * A read guard is taken, a small snapshot is cloned out of it, and the guard is dropped
//!   before the next `.await`. The accessors below return owned values for exactly that
//!   reason; nothing hands out a guard into an `async fn`.
//! * [`AppState::board`] / [`AppState::board_mut`] cover the article arena, the boards and
//!   the per-user counters at once, so an operation that moves articles and updates the
//!   counters is atomic. The loader takes the write lock per batch, not per article.
//!
//! Reload is the other half of what `net_server()` did in the parent: `conf/bbsd.conf`,
//! `conf/badwords.conf`, `data/eula.txt` and both menu trees are re-read and published here, so
//! a client that enters a menu again sees the new tree while a screen already drawn keeps the
//! snapshot it was built from.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{RwLockReadGuard, RwLockWriteGuard};

use tokio::sync::Notify;

use crate::board::Board;
use crate::bwf::Badwords;
use crate::config::Config;
use crate::db::Db;
use crate::menu::MenuSet;
use crate::paths::{DATA_FILES_STARTUP, MAX_CLIENT_LIMIT};
use crate::roster::{OnlineList, Roster};

/// Everything a client task needs besides its own session.
pub struct AppState {
    /// `BBS_*` of `init.c:64-211`. `RELOADCONF` swaps the whole snapshot; a session keeps the
    /// `Arc` it was built with until it next reaches the menu loop.
    config: RwLock<Arc<Config>>,
    pub db: Db,
    /// `data_files[]`: the `data/` texts and `var/bbs_top.txt` the pagers serve from RAM.
    data_files: RwLock<Arc<HashMap<String, Vec<u8>>>>,
    board: RwLock<Board>,
    /// `p_user_list_pool->user_list[index_current]`.
    roster: RwLock<Arc<Roster>>,
    /// `p_user_list_pool->user_online_list[index_current]`.
    online: RwLock<Arc<OnlineList>>,
    /// `p_user_list_pool->user_login_count`, the highest `user_login_log.ID` at the last load.
    login_count: AtomicI32,
    /// `bbs_menu`.
    menus: RwLock<Arc<MenuSet>>,
    /// `top10_menu`, which the C loader forced `allow_exit` on.
    top10: RwLock<Arc<MenuSet>>,
    /// The compiled `conf/badwords.conf`.
    badwords: RwLock<Arc<Badwords>>,
    /// `BBS_eula_tm`: the mtime of `data/eula.txt` captured at startup and on reload.
    eula_tm: AtomicI64,
    /// `BBS_max_client`, which the accept loop compares the live session count against.
    max_client: AtomicUsize,
    /// `BBS_max_client_per_ip`.
    max_client_per_ip: AtomicUsize,
    /// `SYS_server_exit` and the wake-up that ends every task: `SIGTERM`/`SIGINT` in C,
    /// [`AppState::exit`] here. Transport hands a copy to each terminal, because a C child read
    /// the same flag out of shared memory inside its key waits.
    exit: crate::term::ExitWatch,
    /// `SYS_conf_reload`, the flag a session raises through [`AppState::request_reload`].
    reloading: AtomicBool,
    /// The wake-up behind that flag.
    reload: Notify,
}

/// A lock guard is only poisoned by a task that panicked while holding it. The C daemon would
/// have left the same torn segment mapped into every child, so the port keeps serving from
/// the state it has rather than failing every client that arrives afterwards.
fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AppState {
    pub fn new(config: Config, db: Db) -> Self {
        let limits = (config.max_client, config.max_client_per_ip);
        Self {
            max_client: AtomicUsize::new(limits.0),
            max_client_per_ip: AtomicUsize::new(limits.1),
            config: RwLock::new(Arc::new(config)),
            data_files: RwLock::new(Arc::new(HashMap::new())),
            db,
            board: RwLock::new(Board::default()),
            roster: RwLock::new(Arc::new(Roster::default())),
            online: RwLock::new(Arc::new(OnlineList::default())),
            login_count: AtomicI32::new(0),
            menus: RwLock::new(Arc::new(MenuSet::default())),
            top10: RwLock::new(Arc::new(MenuSet::default())),
            badwords: RwLock::new(Arc::new(Badwords::empty())),
            eula_tm: AtomicI64::new(0),
            exit: crate::term::ExitWatch::default(),
            reloading: AtomicBool::new(false),
            reload: Notify::const_new(),
        }
    }

    /// The current configuration snapshot.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&read(&self.config))
    }

    /// `load_conf()` on reload: the new values, with the accept limits published alongside them
    /// because `BBS_max_client*` are read out of the same file.
    pub fn publish_config(&self, config: Config) {
        self.publish_limits(config.max_client, config.max_client_per_ip);
        *write(&self.config) = Arc::new(config);
    }

    /// `get_data_file()`: the preloaded copy of one `data/` file.
    pub fn data_file(&self, name: &str) -> Option<Vec<u8>> {
        read(&self.data_files).get(name).cloned()
    }

    pub fn publish_data_files(&self, files: HashMap<String, Vec<u8>>) {
        *write(&self.data_files) = Arc::new(files);
    }

    /// `load_file()`: read every startup file, keeping the ones already loaded when one is
    /// missing. The C loader `mmap`ed each into `data_files[i].p_shm`; a `Vec<u8>` per name is
    /// the same thing without the segment.
    pub fn load_data_files(&self, root: &Path) -> Vec<String> {
        let mut files: HashMap<String, Vec<u8>> = HashMap::with_capacity(DATA_FILES_STARTUP.len());
        let mut failed = Vec::new();

        for name in DATA_FILES_STARTUP {
            match std::fs::read(root.join(name)) {
                Ok(data) => {
                    files.insert((*name).to_string(), data);
                }
                Err(err) => {
                    failed.push(format!("mmap({name}) error: {err}"));
                    // C leaves the previous mapping in place; the published table keeps it too.
                    if let Some(previous) = self.data_file(name) {
                        files.insert((*name).to_string(), previous);
                    }
                }
            }
        }

        self.publish_data_files(files);
        failed
    }

    pub fn board(&self) -> RwLockReadGuard<'_, Board> {
        read(&self.board)
    }

    pub fn board_mut(&self) -> RwLockWriteGuard<'_, Board> {
        write(&self.board)
    }

    /// The current roster. Cloning the `Arc` is the swap the C code performs on its buffer
    /// indices, and a held `Arc` is unaffected by the next reload — which is how a screen
    /// iterating users stays consistent while the loader works.
    pub fn roster(&self) -> Arc<Roster> {
        Arc::clone(&read(&self.roster))
    }

    pub fn publish_roster(&self, roster: Roster) {
        *write(&self.roster) = Arc::new(roster);
    }

    pub fn online(&self) -> Arc<OnlineList> {
        Arc::clone(&read(&self.online))
    }

    pub fn publish_online(&self, online: OnlineList) {
        *write(&self.online) = Arc::new(online);
    }

    /// `get_user_login_count`, which the top page shows as the site's login total.
    pub fn login_count(&self) -> i32 {
        self.login_count.load(Ordering::Relaxed)
    }

    pub fn set_login_count(&self, count: i32) {
        self.login_count.store(count, Ordering::Relaxed);
    }

    /// `bbs_menu`. A session holds the returned `Arc` for as long as it walks the tree, so a
    /// reload only reaches the next client that opens a menu.
    pub fn menus(&self) -> Arc<MenuSet> {
        Arc::clone(&read(&self.menus))
    }

    pub fn publish_menus(&self, menus: MenuSet) {
        *write(&self.menus) = Arc::new(menus);
    }

    /// `top10_menu`.
    pub fn top10(&self) -> Arc<MenuSet> {
        Arc::clone(&read(&self.top10))
    }

    pub fn publish_top10(&self, menus: MenuSet) {
        *write(&self.top10) = Arc::new(menus);
    }

    pub fn badwords(&self) -> Arc<Badwords> {
        Arc::clone(&read(&self.badwords))
    }

    /// `bwf_load()` replacing the compiled patterns.
    pub fn publish_badwords(&self, badwords: Badwords) {
        *write(&self.badwords) = Arc::new(badwords);
    }

    /// `BBS_eula_tm`, the reference a user's `AgreementTime` is compared against.
    pub fn eula_tm(&self) -> i64 {
        self.eula_tm.load(Ordering::Relaxed)
    }

    pub fn set_eula_tm(&self, tm: i64) {
        self.eula_tm.store(tm, Ordering::Relaxed);
    }

    /// The two accept limits, as the reload path publishes them.
    pub fn limits(&self) -> (usize, usize) {
        (
            self.max_client.load(Ordering::Relaxed),
            self.max_client_per_ip.load(Ordering::Relaxed),
        )
    }

    /// `load_conf()`'s clamping, applied again on reload.
    pub fn publish_limits(&self, max_client: usize, max_client_per_ip: usize) {
        self.max_client
            .store(max_client.clamp(1, MAX_CLIENT_LIMIT), Ordering::Relaxed);
        self.max_client_per_ip.store(
            max_client_per_ip.clamp(1, crate::paths::MAX_CLIENT_PER_IP_LIMIT),
            Ordering::Relaxed,
        );
    }

    /// `SYS_server_exit`, which the menu loop and the key readers poll.
    pub fn is_exiting(&self) -> bool {
        self.exit.is_exiting()
    }

    /// Set the flag and wake everyone waiting on [`AppState::shutdown`]. C's
    /// `net_server()` set the flag and then `kill(0, SIGTERM)`; one notify does both.
    pub fn exit(&self) {
        self.exit.exit();
    }

    /// The flag a transport watches so its waits end with the daemon's, rather than running out
    /// their own idle deadlines first.
    pub fn exit_watch(&self) -> crate::term::ExitWatch {
        self.exit.clone()
    }

    /// `reload_bbs_conf`'s `kill(getppid(), SIGHUP)`: asks the daemon to run its reload pass.
    pub fn request_reload(&self) {
        self.reloading.store(true, Ordering::Release);
        self.reload.notify_waiters();
    }

    /// Whether a reload was asked for, clearing the request.
    pub fn take_reload_request(&self) -> bool {
        self.reloading.swap(false, Ordering::AcqRel)
    }

    /// Resolves on the next [`AppState::request_reload`]. The wait is registered before the
    /// flag is read so a request landing in between cannot be missed.
    pub async fn wait_reload(&self) {
        loop {
            let notified = self.reload.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.take_reload_request() {
                return;
            }
            notified.await;
        }
    }

    /// Resolves when the server is shutting down. A task that is already inside a blocking wait
    /// still has to notice [`AppState::is_exiting`] the way the C loops checked the flag —
    /// [`AppState::exit_watch`] is how the transport gets that.
    pub async fn shutdown(&self) {
        self.exit.wait().await;
    }
}

#[cfg(test)]
#[path = "../tests/test_state.rs"]
mod tests;
