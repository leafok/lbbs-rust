//! One client session's user environment.
//!
//! The C daemon keeps this in process globals — `BBS_username`, `BBS_nickname`,
//! `BBS_user_exp`, `BBS_user_tz`, `BBS_priv`, `BBS_current_action`, `BBS_login_tm`,
//! `BBS_last_access_tm`, `hostaddr_client`, `port_client`, `SSH_v2`, `BBS_update_eula` —
//! which is safe only because `net_server.c:293` forks a process per connection. Here each
//! connection task owns one [`Session`], so the same fields are struct members and nothing
//! has to be reset between clients.
//!
//! # Deviations from C
//!
//! * `user_online.SID` is `Telnet_Process_<n>` with a per-connection number instead of
//!   `getpid()` (`login.c:544`), because one process serves every session. The prefix stays
//!   so the web application's online list keeps recognising the row.
//! * `check_user` pushes the user's timezone into the process with `setenv("TZ", …)` plus
//!   `tzset()` (`login.c:389-403`). That is a process-wide mutation, so the port records
//!   [`Session::user_tz`] and leaves the environment alone; timestamps keep coming out in
//!   the server's zone.
//! * `BBS_update_eula` was a global set in two places and read in one. It is a field here,
//!   so one session's pending agreement cannot leak into another.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use chrono::Local;

use crate::db::SID_PREFIX;
use crate::favorlog::FavorLog;
use crate::menu::{Action, MenuItem, MenuView, is_permitted};
use crate::paths::{
    CURRENT_ACTION_MAX_LEN, DATA_GOODBYE, NICKNAME_MAX_LEN, USER_TZ_MAX_LEN, USERNAME_MAX_LEN,
    VAR_SECTION_AID_LOC_DIR,
};
use crate::privilege::{P_MAN_S, UserPriv};
use crate::screen::{self, DisplayCtx, PagerHooks};
use crate::state::AppState;
use crate::term::Terminal;
use crate::term::key::KEY_NULL;
use crate::viewlog::ViewLog;

/// `BBS_current_action_refresh_interval` (`bbs.c:59`): how long an unchanged action may go
/// without another `UPDATE user_online`.
pub const CURRENT_ACTION_REFRESH_INTERVAL: i64 = 60;

/// The session's `user_online.SID`, in the width the column allows.
pub fn sid_for(id: u64) -> String {
    format!("{SID_PREFIX}{id}")
}

/// Wall-clock seconds, the unit every timestamp in the original is kept in.
pub fn now() -> i64 {
    Local::now().timestamp()
}

/// `strncpy(dst, src, sizeof(dst) - 1)` onto a `String`. C kept the raw bytes even when the
/// copy ended mid-character; the port drops the incomplete one.
pub fn clamp_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Everything one connection knows about who is typing.
pub struct Session {
    pub state: Arc<AppState>,
    /// The `BBS_*` snapshot this session prints from, refreshed at [`Session::refresh_config`].
    pub config: Arc<crate::config::Config>,
    /// The directory `data/`, `var/` and `conf/` hang off, as in `main.c:217-229`.
    pub root: PathBuf,
    pub sid: String,
    pub hostaddr: String,
    pub port: u16,
    /// `SSH_v2`, which the transport sets when the connection is accepted.
    pub is_ssh: bool,

    pub uid: i32,
    pub username: String,
    pub nickname: String,
    pub exp: i32,
    pub user_tz: String,
    pub privs: UserPriv,

    /// Set by the login path when the agreement was updated after this account last signed
    /// in; the session then runs the agreement exchange and ends.
    pub update_eula: bool,

    pub login_tm: i64,
    pub last_access_tm: i64,
    pub current_action: String,
    pub current_action_tm: i64,

    /// `BBS_article_view_log` (`article_view_log.c:20`): what this reader has seen, loaded
    /// after login and flushed when the menu ends.
    pub view_log: ViewLog,
    /// `BBS_article_favor` (`article_favor.c:21`): the same shape for the starred threads.
    pub favor: FavorLog,
    /// `section_aid_locations` (`section_list_display.c:43`): where this reader was last left
    /// in each board, keyed by `sid` instead of by the board's slot in the shared-memory pool.
    /// Loaded before the two tables above and written just after the goodbye file.
    pub aid_locations: HashMap<i16, i32>,
    /// The `static int display_nickname` of `section_list_display`, which is why the `n` key
    /// outlives the board it was pressed in.
    pub display_nickname: bool,
    /// The `static int display_sname` of `article_favor_display`, the other `n` key of the same
    /// shape: the starred-thread list keeps its column choice across openings.
    pub display_sname: bool,

    /// The pager's `show_help` static, which nested pagers shared.
    help_enabled: bool,
}

impl Session {
    /// A session that has not authenticated yet, holding the zeroed globals a forked child
    /// would have started from.
    pub fn new(
        state: Arc<AppState>,
        root: impl Into<PathBuf>,
        sid: String,
        hostaddr: String,
        port: u16,
        is_ssh: bool,
    ) -> Self {
        let config = state.config();
        Self {
            state,
            config,
            root: root.into(),
            sid,
            hostaddr,
            port,
            is_ssh,
            uid: 0,
            username: String::new(),
            nickname: String::new(),
            exp: 0,
            user_tz: String::new(),
            privs: UserPriv::default(),
            update_eula: false,
            login_tm: 0,
            last_access_tm: now(),
            current_action: String::new(),
            current_action_tm: 0,
            view_log: ViewLog::anonymous(),
            favor: FavorLog::anonymous(),
            aid_locations: HashMap::new(),
            display_nickname: false,
            display_sname: false,
            help_enabled: true,
        }
    }

    /// `RELOADCONF` reaching this session: the next menu draw shows the reloaded site name and
    /// limits. C read the same globals out of the (write-protected) segment the parent updated.
    pub fn refresh_config(&mut self) {
        self.config = self.state.config();
    }

    /// `BBS_priv.uid == 0` with no name attached, which is what the pre-login screens and
    /// the anonymous count on the welcome page key off (`bbs_main.c:72-75`).
    pub fn is_anonymous(&self) -> bool {
        self.uid == 0 && self.username.is_empty()
    }

    pub fn is_guest(&self) -> bool {
        self.username == "guest"
    }

    /// The SSH-only staff rule (`login.c:340`): at or above [`P_MAN_S`], telnet is refused.
    pub fn is_staff(&self) -> bool {
        self.privs.checklevel2(P_MAN_S)
    }

    /// The username, nickname, experience and timezone `check_user` copies out of the row
    /// (`login.c:442-448`), each clamped to its column width.
    pub fn set_identity(&mut self, username: &str, nickname: &str, exp: i32, user_tz: &str) {
        self.username = clamp_bytes(username, USERNAME_MAX_LEN);
        self.nickname = clamp_bytes(nickname, NICKNAME_MAX_LEN);
        self.exp = exp;
        self.user_tz = clamp_bytes(user_tz, USER_TZ_MAX_LEN);
    }

    /// `user_online_add` (`login.c:524-553`): the visit row, then this session's online row
    /// replacing whatever an earlier connection of the same id left behind.
    pub async fn online_add(&mut self) -> Result<(), sqlx::Error> {
        self.state.db.visit_log(&self.hostaddr).await?;
        self.online_delete().await?;
        self.state
            .db
            .online_login(&self.sid, self.privs.uid, &self.hostaddr)
            .await
    }

    /// `user_online_del` (`login.c:555-569`).
    pub async fn online_delete(&mut self) -> Result<(), sqlx::Error> {
        self.state.db.online_delete(&self.sid).await
    }

    /// `user_online_exp` (`login.c:571-590`), the time-online allowance paid at logout.
    pub async fn online_exp(&mut self) -> Result<(), sqlx::Error> {
        self.state.db.online_logout(self.privs.uid).await
    }

    /// `user_online_update` (`login.c:592-632`). `None` is C's `NULL`: keep the current
    /// action and only refresh the heartbeat. The return is C's `1` for "the row changed".
    pub async fn online_update(&mut self, action: Option<&str>) -> Result<bool, sqlx::Error> {
        let unchanged = match action {
            None => true,
            Some(action) => self.current_action == action,
        };
        if unchanged && now() - self.current_action_tm < CURRENT_ACTION_REFRESH_INTERVAL {
            return Ok(false);
        }

        if let Some(action) = action {
            self.current_action = clamp_bytes(action, CURRENT_ACTION_MAX_LEN);
        }
        self.current_action_tm = now();

        self.state
            .db
            .online_action(&self.sid, &self.current_action)
            .await?;

        Ok(true)
    }

    /// `user_online_update(NULL)` and a fresh last-access time, which is what the pager
    /// calls on every key it accepts.
    pub async fn record_activity(&mut self) {
        let _ = self.online_update(None).await;
        self.last_access_tm = now();
    }

    /// `display_file` (`screen.c:889-911`) of a file under the server root. A read failure
    /// is logged and answered with `KEY_NULL`, the value C returns for it.
    ///
    /// The future is boxed: the pager calls back into [`PagerHooks::show_help`], which
    /// displays a file, so the recursive type cannot be sized.
    pub fn display_file<'a>(
        &'a mut self,
        term: &'a mut Terminal,
        filename: &'a str,
        eof_exit: i32,
    ) -> Pin<Box<dyn Future<Output = u32> + Send + 'a>> {
        Box::pin(async move {
            if let Err(e) = self.online_update(Some("VIEW_FILE")).await {
                tracing::error!("user_online_update(VIEW_FILE) error: {e}");
            }

            let path = self.root.join(filename);
            let data = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(e) => {
                    tracing::error!("display_file({filename}) error: {e}");
                    return KEY_NULL;
                }
            };

            screen::display_file(term, self, &data, eof_exit).await
        })
    }

    /// `snprintf(filename, sizeof(filename), "%s/%d", VAR_SECTION_AID_LOC_DIR, uid)`
    /// (`section_list_display.c:1480`, `:1524`): one file per account, guests (uid 0) included.
    fn aid_locations_path(&self) -> PathBuf {
        self.root
            .join(VAR_SECTION_AID_LOC_DIR)
            .join(self.privs.uid.to_string())
    }

    /// `section_aid_locations_load` (`:1514`). A missing file is C's `return 0`, and a board
    /// that is no longer in the pool is skipped rather than imported.
    fn load_aid_locations(&mut self) {
        let path = self.aid_locations_path();

        let pairs = match crate::browse::aid_locations_read(&path) {
            Ok(pairs) => pairs,
            Err(e) => {
                tracing::error!("fopen({}, rb) error: {e}", path.display());
                return;
            }
        };

        let board = self.state.board();
        for (sid, aid) in pairs {
            let Ok(sid) = i16::try_from(sid) else {
                continue; // a board number the pool cannot hold never existed here
            };
            if board.section(sid).is_some() {
                self.aid_locations.insert(sid, aid);
            }
        }
    }

    /// `section_aid_locations_save` (`:1473`): one `sid`/`aid` pair per board of the pool, in
    /// pool order, with `0` for the boards this session never opened.
    fn save_aid_locations(&self) {
        let path = self.aid_locations_path();

        let pairs: Vec<(i16, i32)> = {
            let board = self.state.board();
            board
                .sections
                .iter()
                .map(|section| {
                    (
                        section.sid,
                        self.aid_locations.get(&section.sid).copied().unwrap_or(0),
                    )
                })
                .collect()
        };

        if let Err(e) = crate::browse::aid_locations_write(&path, &pairs) {
            tracing::error!("fopen({}, wb) error: {e}", path.display());
        }
    }

    /// `bbs_main.c:403` and `:410`: the two per-user tables the reading and starring screens
    /// key off. C's `article_view_log_load` / `article_favor_load` failures only aborted the
    /// child at the `goto cleanup`, so here a database that is briefly unreachable costs the
    /// reader its `n` marks and `@` stars for this session, not the session itself.
    ///
    /// The board reading positions come first, at `bbs_main.c:396`, because they are what the
    /// first board list of the session opens at.
    pub async fn load_user_logs(&mut self) {
        self.load_aid_locations();

        let db = &self.state.db;

        self.view_log = match ViewLog::load(db, self.uid).await {
            Ok(view_log) => view_log,
            Err(e) => {
                tracing::error!("Query view_article_log error: {e}");
                ViewLog::anonymous()
            }
        };

        self.favor = match FavorLog::load(db, self.uid).await {
            Ok(favor) => favor,
            Err(e) => {
                tracing::error!("Query article_favorite error: {e}");
                FavorLog::anonymous()
            }
        };
    }

    /// `bbs_logout` (`bbs_main.c:106-130`): the time-online allowance, then the goodbye
    /// file. Reached only from the session loop, as in C — a login that never got past the
    /// prompt skips both.
    pub async fn logout(&mut self, term: &mut Terminal) {
        if let Err(e) = self.online_exp().await {
            tracing::error!("user_online_exp error: {e}");
        }

        self.display_file(term, DATA_GOODBYE, 1).await;

        tracing::info!(
            "User [{}] (uid={}) logout, idle for {} seconds since last input",
            self.username,
            self.privs.uid,
            now() - self.last_access_tm
        );
    }

    /// `bbs_main.c:441` and `:447`: the reads and stars this session accumulated, written
    /// just after the goodbye file and before the online row is dropped. The board positions
    /// go first, at `:435`.
    pub async fn save_user_logs(&self) {
        self.save_aid_locations();

        if let Err(e) = self.view_log.save_inc(&self.state.db).await {
            tracing::error!("article_view_log_save_inc() error: {e}");
        }

        if let Err(e) = self.favor.save_inc(&self.state.db).await {
            tracing::error!("article_favor_save_inc() error: {e}");
        }
    }

    /// `bbs_session_cleanup` (`bbs_main.c:132-151`), which every session end runs.
    pub async fn cleanup(&mut self) {
        if let Err(e) = self.online_delete().await {
            tracing::error!("user_online_del error: {e}");
        }
    }

    /// The line `bbs_main.c:385` writes for every accepted session.
    pub fn log_login(&self) {
        tracing::info!(
            "User [{}] (uid={}) login from {}:{}",
            self.username,
            self.privs.uid,
            self.hostaddr,
            self.port
        );
    }
}

impl MenuView for Session {
    fn permits(&self, item: &MenuItem) -> bool {
        // `checkpriv(&BBS_priv, 0, priv)`: a menu item is tested against the global word,
        // never against a per-board entry.
        is_permitted(self.privs.g_priv, self.privs.level, item)
    }

    fn filter_allows(&self, filter: Action, item: &MenuItem) -> bool {
        match filter {
            // `favor_section_filter` (`menu_proc.c:196-201`), where the item's "priv"
            // field carries the board number to look up.
            Action::FavorSectionFilter => {
                self.privs.is_favor(item.priv_mask as i16) && self.privs.checklevel2(item.level)
            }
            // The parser rejects a `use_filter` menu whose name is not a known action, and
            // the shipped `menu_merged.conf` has only the favourites menu, so today this
            // arm cannot be reached.
            _ => self.permits(item),
        }
    }

    fn bbs_name(&self) -> &str {
        &self.config.bbs_name
    }
}

impl PagerHooks for Session {
    async fn on_activity(&mut self) {
        Session::record_activity(self).await;
    }

    /// `display_file_key_handler` (`screen.c:874-887`), the help row of a file pager. A
    /// feature with its own footer replaces this by wrapping the session in its own hooks.
    async fn key_handler(&mut self, ch: &mut u32, ctx: &mut DisplayCtx) -> i32 {
        screen::file_key_handler(*ch, ctx);
        0
    }

    fn help_enabled(&mut self) -> bool {
        self.help_enabled
    }

    fn set_help_enabled(&mut self, enabled: bool) {
        self.help_enabled = enabled;
    }

    async fn show_help(&mut self, term: &mut Terminal, help_filename: &str) -> u32 {
        self.display_file(term, help_filename, 1).await
    }
}

/// A session wired to an unreachable database, for the tests that only touch the parts of
/// a session that are pure state. Nothing here issues a query.
#[cfg(test)]
pub(crate) fn offline_session() -> Session {
    let db = crate::config::DbConfig {
        host: String::new(),
        username: String::new(),
        password: String::new(),
        database: String::new(),
        timezone: String::new(),
    };
    let config = crate::config::Config {
        bbs_id: String::new(),
        bbs_name: "www.bbs.test".to_string(),
        bbs_server: String::new(),
        bbs_address: String::new(),
        telnet_port: 2323,
        ssh_port: 2222,
        max_client: 1,
        max_client_per_ip: 1,
        start_dt: String::new(),
        sys_id: 0,
        db: db.clone(),
    };
    let state = AppState::new(config, crate::db::Db::detached(&db));

    Session::new(
        Arc::new(state),
        "/srv/lbbs",
        sid_for(7),
        "202.38.64.1".to_string(),
        2323,
        false,
    )
}

#[cfg(test)]
#[path = "../tests/test_session.rs"]
mod tests;
