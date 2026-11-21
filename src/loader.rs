/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The background loader, ported from `src/section_list_loader.c`.
//!
//! The C original keeps the pools current in a dedicated child process: `main.c:389-416`
//! fills them once at startup, then `section_list_loader_launch` (`:683`) forks a loop that
//! re-reads the board config, appends new articles, replays `bbs_article_op`, and reloads the
//! two user tables on their own timers, while every client process reaches a board through
//! `section_list_rw_lock`. The port keeps that pass order and those timers, and takes the
//! board write lock once per batch instead of once per board.
//!
//! The C loop re-`SELECT`s an article row while holding the lock, for the `'M'` and `'T'`
//! letters (`:519`, `:562`), and writes the render cache from the same rows. No lock guard may
//! be held across an `.await`, so [`fetch_op_extras`] runs those queries up front and
//! [`apply_ops`] works on the results. The cost is that a failing per-op `SELECT` now aborts a
//! batch the original would have applied in part, which is harmless: every letter the replay
//! reaches assigns a value read from the database (`visible = 0`, `lock = 1`, `CID = …`), and a
//! batch that is not counted leaves the watermark where it was, so its ops are simply applied
//! again.
//!
//! The two cache renders (`:331`, `:546`) are ported by [`generate_caches_from_rows`] and
//! [`generate_caches_from_modifies`], which the [`Loader`] calls after the pool step rather
//! than inside it — the same rows, but with the boards behind a read lock instead of the one
//! board's write lock that C holds while it writes the file. C answers a failed render by
//! breaking out of the batch (`ret = -4`); a cache file here is derived from rows the pool
//! already has, so one is logged and the rest of the batch is still rendered.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::future::join_all;

use crate::board::{Article, Board, Section};
use crate::cache;
use crate::db::{ArticleRow, ContentRow, Db, OpRow, SectionRow};
use crate::ipmask;
use crate::paths::{
    USER_OFF_LINE_SECS, VAR_ARTICLE_CACHE_DIR, VAR_GEN_EX_MENU_DIR,
};
use crate::roster::{OnlineList, Roster};
use crate::state::AppState;

/// `LOAD_ARTICLE_COUNT_LIMIT` (`include/section_list_loader.h:19`): the rows one query
/// brings back, and the count the C loop compares against to decide whether to run again.
pub const LOAD_BATCH: usize = 1000;
/// `BBS_section_list_load_interval` (`bbs.c:38`), the figure the post and delete screens
/// quote as "a new article is visible in N seconds".
pub const SECTION_LIST_INTERVAL: Duration = Duration::from_secs(1);
/// `BBS_user_list_load_interval` (`bbs.c:41`), which `user_info_update` also quotes.
pub const USER_LIST_INTERVAL: Duration = Duration::from_secs(60);
/// `BBS_user_online_list_load_interval` (`bbs.c:42`).
pub const USER_ONLINE_INTERVAL: Duration = Duration::from_secs(5);

/// Why a pass stopped before the end of its rows. Each variant is a `break` out of the C
/// `while`, so none of them means the loader is broken — they say how soon a retry can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `article_block_find_by_aid` found nothing (`:447`): the article this op belongs to has
    /// not been appended yet, so the batch ends and the watermark stays on this op until the
    /// article arrives through the AID scan.
    NotLoaded,
    /// `ERR_UNKNOWN_SECTION` (`:467`, `:592`): an op names a board that the last config load
    /// did not include, which the C original answers by setting `SYS_conf_reload`.
    UnknownSection,
    /// `section_list_move_topic` refused the move. The original logs "retry in the next loop"
    /// and leaves the watermark on the op (`:602`), so a topic whose head is not loaded yet
    /// moves as soon as it is.
    MoveLater,
    /// The `bbs_content` row of a modify is gone. The original zeroes the article's CID and
    /// ends the batch without counting the op (`:548`), so this repeats until the row appears.
    ContentMissing,
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoaded => write!(f, "article not loaded yet"),
            Self::UnknownSection => write!(f, "unknown section"),
            Self::MoveLater => write!(f, "topic move deferred"),
            Self::ContentMissing => write!(f, "article content row missing"),
        }
    }
}

/// What one replay pass got through.
#[derive(Debug)]
pub struct Replay {
    /// Ops applied in `MID` order, which is how far the watermark may move. Whatever the pass
    /// did not reach is replayed by the next one.
    pub applied: usize,
    /// `Some` when the pass stopped early.
    pub stop: Option<Stop>,
}

impl Replay {
    fn complete(count: usize) -> Self {
        Self {
            applied: count,
            stop: None,
        }
    }

    fn stopped(applied: usize, stop: Stop) -> Self {
        Self {
            applied,
            stop: Some(stop),
        }
    }
}

#[derive(Debug)]
pub enum LoaderError {
    /// Any statement the loader runs. The C original closes its connection and returns a
    /// negative code; the port keeps the pool and lets the caller log and retry.
    Sql(sqlx::Error),
    /// An article or an op named a board that is not in the pool, which the caller answers by
    /// reloading the section config (`Stop::UnknownSection`).
    UnknownSection,
    /// `section_list_create` refused a board: the pool holds
    /// [`crate::paths::MAX_SECTION`] of them, or the `sname` is already taken (`:122`).
    SectionPoolFull,
}

impl fmt::Display for LoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "loader query failed: {e}"),
            Self::UnknownSection => write!(f, "unknown section"),
            Self::SectionPoolFull => write!(f, "section list is full or a board name is duplicated"),
        }
    }
}

impl Error for LoaderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sql(e) => Some(e),
            Self::UnknownSection | Self::SectionPoolFull => None,
        }
    }
}

impl From<sqlx::Error> for LoaderError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

/// One row of the bulk article load (`:244-302`) as an arena article. The row's content
/// column belongs to the render cache rather than the arena, and `sub_ip` only ever reaches
/// the cache masked, so neither is copied here.
pub fn article_from_row(row: &ArticleRow, board: &Board) -> Article {
    let mut article = Article {
        aid: row.aid,
        tid: row.tid,
        sid: row.sid,
        cid: row.cid,
        uid: row.uid,
        visible: row.visible,
        excerption: row.excerption,
        ontop: row.ontop,
        lock: row.lock,
        transship: row.transship,
        sub_dt: row.sub_dt.unwrap_or_default(),
        ..Default::default()
    };
    article.set_labels(
        row.username.as_deref(),
        row.nickname.as_deref(),
        row.title.as_deref(),
    );

    if article.visible && article.tid != 0 {
        // A reply to a head that is absent or invisible is detached from the topic and hidden
        // with it (`:292-302`).
        let head_visible = board
            .article_by_aid(article.tid)
            .is_some_and(|head| head.visible);
        if !head_visible {
            article.tid = 0;
            article.visible = false;
        }
    }

    article
}

/// `load_section_config_from_db` (`:42`) over rows already read, with the master names the
/// per-board query (`:88`) produced for each of them.
///
/// A board missing from the pool is created with its counters at zero; a board that is
/// already there keeps its articles, because `section_list_update` (`section_list.c:712`)
/// rewrites only `sname`, `stitle` and `master_list`. Renaming a board therefore leaves the
/// arena links alone, and the articles loaded under the old name stay where they are.
pub fn load_section_config_into(
    board: &mut Board, rows: &[SectionRow], master_lists: &[String],
) -> Result<(), LoaderError> {
    for (row, master_list) in rows.iter().zip(master_lists) {
        // `atoi(row[0])` of a `mediumint`. A NULL `sname` or `title` reaches `strncpy` in C
        // and becomes an empty string here.
        let sname = row.sname.as_deref().unwrap_or_default();
        let stitle = row.title.as_deref().unwrap_or_default();

        if board.section(row.sid).is_none() {
            if board
                .create_section(row.sid, sname, stitle, master_list)
                .is_none()
            {
                return Err(LoaderError::SectionPoolFull);
            }
        } else {
            board.update_section(row.sid, sname, stitle, master_list);
        }

        let section = board
            .section_mut(row.sid)
            .expect("the board was just created or found");
        // `:153-156`. The gen_ex menu of `:158-175` is the other half of the pass, in
        // [`load_gen_ex_menus_into`], so `ex_menu_tm` here stays at zero until that lands.
        section.class_id = i32::from(row.cid);
        section.read_user_level = i32::from(row.read_user_level);
        section.write_user_level = i32::from(row.write_user_level);
        section.enable = row.enable;
    }

    Ok(())
}

/// The `update_gen_ex` half of `load_section_config_from_db` (`:162-179`): the digest menu of
/// every enabled board whose `section_config.ex_menu_tm` moved past the one the pool holds.
///
/// C runs it only from `main.c:389` and the reload `net_server.c:871`, never from the article
/// loop's `section_list_loader.c:751`, which in the port is [`Loader::load_section_config`] — the
/// only caller, for all three of those passes go through it.
///
/// A menu that will not read or will not parse is unloaded and the timestamp is left where it
/// was (`:170-178`), so the board keeps its stale menu until the next pass retries — which is how
/// C reaches `section_list_ex_dir_display` with `ex_menu_tm != 0` and an empty tree.
pub fn load_gen_ex_menus_into(board: &mut Board, rows: &[SectionRow], dir: &Path) {
    for row in rows {
        let Some(section) = board.section_mut(row.sid) else {
            continue;
        };

        // `atol(row[7] == NULL ? "0" : row[7]) > p_section->ex_menu_tm`, after the `enable` test.
        let tm = row.ex_menu_tm.unwrap_or(0);
        if !row.enable || tm <= section.ex_menu_tm {
            continue;
        }

        let path = dir.join(row.sid.to_string());
        let loaded = std::fs::read(&path)
            .map_err(|err| err.to_string())
            .and_then(|data| crate::menu::MenuSet::parse(&data).map_err(|err| err.to_string()));

        match loaded {
            Ok(set) => {
                section.ex_menu_set = Some(std::sync::Arc::new(set));
                section.ex_menu_tm = tm;
                tracing::debug!(
                    "Loaded gen_ex_menu of section {} [{}]",
                    row.sid,
                    section.sname
                );
            }
            Err(error) => {
                section.ex_menu_set = None;
                tracing::error!("load_menu({}) error: {error}", path.display());
            }
        }
    }
}

/// `append_articles_from_db` (`:197`) over rows already read. The arena links, the board
/// counters and the statistics map live behind one lock, so a batch has to go in as one step.
///
/// The count can be lower than the rows given: a board at
/// [`crate::paths::ARTICLE_LIMIT_PER_SECTION`] or an `AID` that is no newer than the arena's
/// last makes `section_list_append_article` fail, and the C original breaks out and reports
/// the count it reached (`:317-323`). The articles past that point are picked up by the next
/// pass, which starts at the same AID.
pub fn append_articles_into(board: &mut Board, rows: &[ArticleRow]) -> Result<usize, LoaderError> {
    let mut appended = 0;

    for row in rows {
        // `section_list_find_by_sid` (`:285`).
        if board.section(row.sid).is_none() {
            return Err(LoaderError::UnknownSection);
        }

        let article = article_from_row(row, board);
        if board.append_article(&article).is_err() {
            break;
        }
        appended += 1;
    }

    Ok(appended)
}

/// `UTF8_fixed_width` as the C loader child sees it (`str_process.c:22`). A client's
/// `(V)变宽/(F)定宽` answer only changes that global in the client's own process, so every
/// cache file the loader writes is measured at the fixed-width default.
pub const CACHE_FIXED_WIDTH: bool = true;

/// `article_cache_generate` for the posts a load pass appended (`section_list_loader.c:331`).
///
/// `rows` is the batch trimmed to what the pool took, in the `AID` order the query returns. The
/// article is rebuilt from its row rather than read back out of the arena: the render wants the
/// same `username` / `nickname` / `title` / `sub_dt` the row carries, and the body column the
/// arena has no place for is the one this function is for.
///
/// `overwrite` is C's `0`, so a post that was cached at an earlier startup keeps its file: the
/// name is the `CID`, and only a modify can change what those bytes should be — which is the
/// `'M'` letter's job below. Returns the number of files written, so a caller can tell a batch
/// that rendered from a batch that found everything already there.
pub fn generate_caches_from_rows(
    cache_dir: &str,
    bbs_name: &str,
    bbs_server: &str,
    board: &Board,
    rows: &[ArticleRow],
) -> usize {
    let mut rendered = 0;

    for row in rows {
        // The board this pass could not find (`:285`) has no names to put in a header, and the
        // caller is about to reload the section config anyway.
        let Some(section) = board.section(row.sid) else {
            continue;
        };
        let article = article_from_row(row, board);
        // `content == NULL` is the `NULL pointer error` of `article_cache.c:78`, which in C
        // breaks the batch with `ret = -4` before the row is even reached.
        let Some(content) = row.content.as_deref() else {
            tracing::error!(
                "article_cache_generate(aid={}, cid={}) error: no content row",
                article.aid,
                article.cid
            );
            continue;
        };

        if render_cache(cache_dir, bbs_name, bbs_server, &article, section, content, row.sub_ip.as_deref()) {
            rendered += 1;
        }
    }

    rendered
}

/// `article_cache_generate` for the `'M'` ops a replay got through (`section_list_loader.c:546`).
///
/// C renders from `p_article` after it has assigned the new `CID`, and the port reads the same
/// article back out of the arena, so the file lands under the new name and the one the modify
/// left behind is simply never opened again — the same orphan C's `article_cache_cleanup` does
/// not remove either.
pub fn generate_caches_from_modifies(
    cache_dir: &str,
    bbs_name: &str,
    bbs_server: &str,
    board: &Board,
    ops: &[OpRow],
    extras: &[OpExtra],
    applied: usize,
) -> usize {
    let mut rendered = 0;

    for (op, extra) in ops.iter().take(applied).zip(extras) {
        if op.kind.as_bytes().first() != Some(&b'M') {
            continue;
        }
        let Some(art) = board.article_index(op.aid) else {
            continue;
        };
        let article = &board.articles[art];
        let Some(section) = board.section(article.sid) else {
            continue;
        };
        let Some(row) = extra.content.as_ref() else {
            continue;
        };
        let Some(content) = row.content.as_deref() else {
            tracing::error!(
                "article_cache_generate(aid={}, cid={}) error: no content row",
                article.aid,
                article.cid
            );
            continue;
        };

        if render_cache(cache_dir, bbs_name, bbs_server, article, section, content, row.sub_ip.as_deref()) {
            rendered += 1;
        }
    }

    rendered
}

/// The one call shape both renders share: mask the source address the way the loader does
/// (`:273`, `:540`), hand the five arguments to [`cache::generate`], and report whether a file
/// came out of it.
fn render_cache(
    cache_dir: &str,
    bbs_name: &str,
    bbs_server: &str,
    article: &Article,
    section: &Section,
    content: &str,
    sub_ip: Option<&str>,
) -> bool {
    let sub_ip = ipmask::mask(sub_ip.unwrap_or_default(), 1, b'*');

    match cache::generate(
        cache_dir,
        bbs_name,
        bbs_server,
        article,
        section,
        content.as_bytes(),
        &sub_ip,
        false,
        CACHE_FIXED_WIDTH,
    ) {
        Ok(true) => true,
        // The file was already there, which is the C early return.
        Ok(false) => false,
        Err(e) => {
            tracing::error!(
                "article_cache_generate(aid={}, cid={}) error: {e}",
                article.aid,
                article.cid
            );
            false
        }
    }
}

/// The rows the `'M'` and `'T'` letters re-`SELECT`, read before the board lock is taken.
/// There is one entry per op, because a batch can hold two ops on the same article and each
/// has to see the row as it was when its own turn came.
#[derive(Debug, Clone, Default)]
pub struct OpExtra {
    /// `SELECT bbs.CID, sub_ip, bbs_content.content … WHERE AID = ?` for `'M'` (`:516`).
    pub content: Option<ContentRow>,
    /// `SELECT SID FROM bbs WHERE AID = ?`, the destination of a `'T'` (`:559`).
    pub dest_sid: Option<i16>,
}

pub async fn fetch_op_extras(db: &Db, ops: &[OpRow]) -> Result<Vec<OpExtra>, sqlx::Error> {
    join_all(ops.iter().map(|op| async {
        match op.kind.as_str() {
            "M" => db.article_content(op.aid).await.map(|row| OpExtra {
                content: row,
                ..Default::default()
            }),
            "T" => db.article_sid(op.aid).await.map(|sid| OpExtra {
                dest_sid: sid,
                ..Default::default()
            }),
            _ => Ok(OpExtra::default()),
        }
    }))
    .await
    .into_iter()
    .collect()
}

/// `apply_article_op_log_from_db` (`:407`) over rows already read, as one step over one board
/// pool rather than a lock handed from board to board.
///
/// The board an op applies to is the one its article is loaded in now, not a `SID` in the log
/// row: `article_block_find_by_aid` searches the arena globally (`:447`), so the ops around a
/// move land on the topic's new board.
pub fn apply_ops(board: &mut Board, ops: &[OpRow], extras: &[OpExtra]) -> Replay {
    for (index, op) in ops.iter().enumerate() {
        let Some(art) = board.article_index(op.aid) else {
            return Replay::stopped(index, Stop::NotLoaded);
        };
        let sid = board.articles[art].sid;
        if board.section(sid).is_none() {
            return Replay::stopped(index, Stop::UnknownSection);
        }
        let extra = extras.get(index).cloned().unwrap_or_default();

        // Only the first byte of `type`, which is how `row[2][0]` reads it.
        let letter = op.kind.as_bytes().first().copied().unwrap_or(b'?');

        match letter {
            b'D' | b'X' | b'S' => {
                let visible = letter == b'S';
                // Both failures are logged by the original, which then carries on with the
                // batch (`:490`).
                let _ = board.set_article_visible(sid, op.aid, visible);
                let _ = board.calculate_page(sid, op.aid);
            }
            b'L' | b'U' => board.articles[art].lock = letter == b'L',
            b'M' => match &extra.content {
                Some(row) => {
                    board.articles[art].cid = row.cid;
                    // "Set excerption = 0 implicitly in case of rare condition" (`:556`).
                    board.articles[art].excerption = false;
                }
                None => {
                    board.articles[art].cid = 0;
                    return Replay::stopped(index, Stop::ContentMissing);
                }
            },
            b'T' => {
                // `sid_dest > 0 && sid_dest != p_article->sid` (`:587`): a destination that is
                // gone, or is the board the article already sits on, means the topic is already
                // where the log says it should be.
                if let Some(dest_sid) = extra.dest_sid.filter(|sid| *sid > 0) {
                    if dest_sid != sid {
                        if board.section(dest_sid).is_none() {
                            return Replay::stopped(index, Stop::UnknownSection);
                        }
                        if board.move_topic(sid, dest_sid, op.aid).is_err() {
                            return Replay::stopped(index, Stop::MoveLater);
                        }
                    }
                }
            }
            b'E' | b'O' => {
                let _ = board.set_article_excerption(sid, op.aid, letter == b'E');
            }
            b'F' | b'V' => {
                // The flag goes onto the arena first, which is how `update_article_ontop`
                // learns whether to add the article or evict it (`:623`).
                board.articles[art].ontop = letter == b'F';
                let _ = board.update_article_ontop(sid, art);
            }
            b'Z' => board.articles[art].transship = true,
            // An unknown letter is skipped and the op still counts as processed (`:633`).
            // `'A'` lands here too; the query already filters it out.
            _ => {}
        }
    }

    Replay::complete(ops.len())
}

/// The loader's cursor into the op log, plus the timers of the C loop.
///
/// It owns its [`AppState`] rather than borrowing it because the daemon hands one loader from
/// the startup pass straight to the periodic task: in C that was a `fork()`, and the child
/// inherited `last_article_op_log_mid` from the parent that had just read it. A second loader
/// built from the same state would start with `watermark` at `None`, and a loader without a
/// cursor never reaches the op log — new articles would still arrive, changes would not.
pub struct Loader {
    state: std::sync::Arc<AppState>,
    /// `last_article_op_log_mid`. `None` until [`Loader::initial_load`] reads it, which also
    /// keeps a loader that never finished its startup pass from replaying ops at all.
    pub watermark: Option<i32>,
    /// `SYS_conf_reload`, which the C original sets when a load runs into a board it does not
    /// know (`:770`, `:797`). Nothing else raises it: the loader's `SIGUSR1` handler only
    /// reopens the log files.
    reload_config: bool,
    /// `VAR_ARTICLE_CACHE_DIR` under the server root: where [`cache::generate`] writes the file
    /// that [`crate::browse`] reads back.
    cache_dir: PathBuf,
    /// `VAR_GEN_EX_MENU_DIR` under the same root: the one file per board that
    /// [`load_gen_ex_menus_into`] reads.
    gen_ex_dir: PathBuf,
    /// `tm_user_list_reload` and `tm_user_online_list_reload`, both started a full interval
    /// out because the startup pass has already loaded the two tables.
    next_users: Instant,
    next_online: Instant,
}

impl Loader {
    /// `root` is the server root [`crate::daemon::enter_server_root`] settled on, the same one
    /// a session keeps as [`crate::session::Session::root`].
    pub fn new(state: std::sync::Arc<AppState>, root: &Path) -> Self {
        let now = Instant::now();
        Self {
            state,
            watermark: None,
            reload_config: false,
            cache_dir: root.join(VAR_ARTICLE_CACHE_DIR),
            gen_ex_dir: root.join(VAR_GEN_EX_MENU_DIR),
            next_users: now + USER_LIST_INTERVAL,
            next_online: now + USER_ONLINE_INTERVAL,
        }
    }

    /// `main.c:372-416`: both user tables, the board config, the op watermark, every article,
    /// then the per-user counters.
    pub async fn initial_load(&mut self) -> Result<(), LoaderError> {
        self.reload_roster().await?;
        self.reload_online().await?;
        self.load_section_config().await?;

        // `set_last_article_op_log_from_db` (`:363`). Everything below that watermark is
        // covered by the article scan, which reads every column the ops would have touched.
        self.watermark = self.state.db.last_op_id().await?;

        let count = self.append_new_articles().await?;
        let last_aid = self.state.board().last_aid();
        tracing::info!(count, last_aid, "initially loaded articles");

        self.update_stats().await
    }

    /// The body of the C loop: config when a load asked for it, then new articles, then the
    /// op log, then whichever user table is due. Public so a caller with a shutdown signal can
    /// drive the passes itself instead of waiting for [`Loader::run`] to notice.
    pub async fn tick(&mut self) {
        if self.reload_config {
            self.reload_config = false;
            if let Err(e) = self.load_section_config().await {
                tracing::warn!("section config reload failed: {e}");
            }
        }

        match self.append_new_articles().await {
            Ok(0) => {}
            Ok(count) => {
                let last_aid = self.state.board().last_aid();
                tracing::info!(count, last_aid, "incrementally loaded articles");
            }
            Err(e) => {
                tracing::warn!("article load failed: {e}");
                if matches!(e, LoaderError::UnknownSection) {
                    self.reload_config = true;
                }
                return;
            }
        }

        match self.replay_ops().await {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, last_mid = ?self.watermark, "proceeded article logs"),
            Err(e) => {
                tracing::warn!("article op replay failed: {e}");
                if matches!(e, LoaderError::UnknownSection) {
                    self.reload_config = true;
                }
            }
        }

        let now = Instant::now();
        if now >= self.next_users {
            self.next_users = now + USER_LIST_INTERVAL;
            if let Err(e) = self.reload_roster().await {
                tracing::warn!("user list reload failed: {e}");
            }
            if let Err(e) = self.update_stats().await {
                tracing::warn!("user statistics update failed: {e}");
            }
        }
        if now >= self.next_online {
            self.next_online = now + USER_ONLINE_INTERVAL;
            if let Err(e) = self.reload_online().await {
                tracing::warn!("online list reload failed: {e}");
            }
        }
    }

    /// `section_list_loader_launch` (`:683`) minus the `fork()`, which is also why the daemon
    /// gives this task the loader that ran [`Loader::initial_load`]: the C child inherited the
    /// parent's `last_article_op_log_mid` along with the rest of its memory, and the cursor is
    /// what lets a pass reach the op log at all.
    ///
    /// The C child wakes once a second until the interval has passed since the top of its loop;
    /// the task sleeps for whatever is left of [`SECTION_LIST_INTERVAL`], which gives the same
    /// period, and stops when the daemon does.
    pub async fn run(mut self) {
        loop {
            let started = Instant::now();
            self.tick().await;
            let rest = SECTION_LIST_INTERVAL.saturating_sub(started.elapsed());
            tokio::select! {
                _ = tokio::time::sleep(rest) => {}
                _ = self.state.shutdown() => break,
            }
        }
    }

    /// `user_list_pool_reload(0)`: rebuild the roster and publish it, which is the buffer
    /// index swap of `:1044` expressed as an [`std::sync::Arc`].
    pub async fn reload_roster(&self) -> Result<(), LoaderError> {
        let rows = self.state.db.roster().await?;
        self.state.publish_roster(Roster::load(&rows));
        Ok(())
    }

    /// `user_list_pool_reload(1)`, whose query filters on `BBS_user_off_line` (`:331`).
    pub async fn reload_online(&self) -> Result<(), LoaderError> {
        let rows = self.state.db.online(USER_OFF_LINE_SECS).await?;
        let roster = self.state.roster();
        self.state.publish_online(OnlineList::load(&rows, &roster));
        Ok(())
    }

    /// `user_stat_update`, which the C loader calls right after the roster because the
    /// counters only cover the uids the roster knows.
    pub async fn update_stats(&self) -> Result<(), LoaderError> {
        let roster = self.state.roster();
        self.state.board_mut().update_stats(&roster);
        Ok(())
    }

    /// `load_section_config_from_db(1)` (`:42`): the config rows and the digest menus together, as
    /// `main.c:389` and `net_server.c:871` ask for it. The loader child's own refresh (`:751`)
    /// passes `0`, and the port has no second process to keep the halves apart, so one pass does
    /// what C's two did. The master names are read before the board lock; the C original runs the
    /// same query per board while holding that board's lock and breaks out with the boards it had
    /// reached already updated — a `break` that did not release that lock until d7f2f5d. Here a
    /// failed query leaves the previous config in place, which the next pass tries again.
    pub async fn load_section_config(&self) -> Result<(), LoaderError> {
        let rows = self.state.db.sections().await?;
        let master_lists = join_all(rows.iter().map(|row| self.state.db.section_masters(row.sid)))
            .await
            .into_iter()
            .collect::<Result<Vec<String>, sqlx::Error>>()?;

        let mut board = self.state.board_mut();
        load_section_config_into(&mut board, &rows, &master_lists)?;
        load_gen_ex_menus_into(&mut board, &rows, &self.gen_ex_dir);
        Ok(())
    }

    /// `append_articles_from_db(last_aid + 1, 0, LOAD_ARTICLE_COUNT_LIMIT)`, run until a batch
    /// comes back short (`:760-773`). Returns the articles loaded.
    pub async fn append_new_articles(&self) -> Result<usize, LoaderError> {
        let mut total = 0;

        loop {
            let start_aid = self.state.board().last_aid() + 1;
            let rows = self.state.db.articles_from(start_aid, LOAD_BATCH as i32).await?;
            let fetched = rows.len();
            let outcome = {
                let mut board = self.state.board_mut();
                append_articles_into(&mut board, &rows)
            };

            // C renders the cache of each post as it appends it (`:327`), so the rows that
            // reached the pool are rendered even when the batch then failed. A failed batch
            // renders all of them: the rows whose board the pool does not have are exactly the
            // ones [`generate_caches_from_rows`] skips, and a file for a post the pool refused
            // for now only waits there for the pass that takes it.
            let to_render = match &outcome {
                Ok(appended) => &rows[..*appended],
                Err(_) => rows.as_slice(),
            };
            self.render_row_caches(to_render);

            let appended = outcome?;
            total += appended;

            // Only a batch that both filled the query and landed in full is followed up: a
            // board at the per-section limit would otherwise restart from the same AID
            // forever.
            if appended == fetched && fetched == LOAD_BATCH {
                continue;
            }
            return Ok(total);
        }
    }

    /// `article_cache_generate` for one batch of appended rows, under the boards' *read* lock.
    /// The two site strings come from the live config, so `SIGHUP` changes the header and
    /// footer of whatever is rendered after it.
    fn render_row_caches(&self, rows: &[ArticleRow]) {
        if rows.is_empty() {
            return;
        }

        let config = self.state.config();
        let board = self.state.board();
        let dir = self.cache_dir.to_string_lossy();
        let rendered =
            generate_caches_from_rows(&dir, &config.bbs_name, &config.bbs_server, &board, rows);
        if rendered > 0 {
            tracing::debug!(rendered, "article cache files generated");
        }
    }

    /// The same for the `'M'` ops a replay applied.
    fn render_modify_caches(&self, ops: &[OpRow], extras: &[OpExtra], applied: usize) {
        if applied == 0 {
            return;
        }

        let config = self.state.config();
        let board = self.state.board();
        let dir = self.cache_dir.to_string_lossy();
        let rendered = generate_caches_from_modifies(
            &dir,
            &config.bbs_name,
            &config.bbs_server,
            &board,
            ops,
            extras,
            applied,
        );
        if rendered > 0 {
            tracing::debug!(rendered, "article cache files generated");
        }
    }

    /// `apply_article_op_log_from_db` in its batched form (`:789-800`). Returns the number of
    /// ops applied, and [`LoaderError::UnknownSection`] when the board config should be read
    /// again before the next attempt.
    pub async fn replay_ops(&mut self) -> Result<usize, LoaderError> {
        let Some(mut mid) = self.watermark else {
            return Ok(0);
        };
        let mut total = 0;

        loop {
            let ops = self.state.db.ops_after(mid, LOAD_BATCH as i32).await?;
            if ops.is_empty() {
                return Ok(total);
            }

            let extras = fetch_op_extras(&self.state.db, &ops).await?;
            let replay = {
                let mut board = self.state.board_mut();
                apply_ops(&mut board, &ops, &extras)
            };
            // `:542`, after the pool step: an op the replay did not reach has not changed the
            // article, so it needs no file yet.
            self.render_modify_caches(&ops, &extras, replay.applied);

            if replay.applied > 0 {
                mid = ops[replay.applied - 1].mid;
                self.watermark = Some(mid);
            }
            total += replay.applied;

            match replay.stop {
                None if ops.len() == LOAD_BATCH => {}
                None => return Ok(total),
                Some(Stop::UnknownSection) => return Err(LoaderError::UnknownSection),
                Some(stop) => {
                    tracing::debug!("article op replay stopped at mid {mid}: {stop}");
                    return Ok(total);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{ARTICLE_TITLE_MAX_LEN, MASTER_LIST_BYTES, NICKNAME_MAX_LEN, USERNAME_MAX_LEN};

    fn board() -> Board {
        let mut board = Board::default();
        board.create_section(1, "test", "Test", "m ");
        board.create_section(2, "other", "Other", "");
        board
    }

    fn row(aid: i32, sid: i16) -> ArticleRow {
        ArticleRow {
            aid,
            tid: 0,
            sid,
            cid: aid,
            uid: 1,
            visible: true,
            excerption: false,
            ontop: false,
            lock: false,
            transship: false,
            username: Some("tester".into()),
            nickname: Some(" Tester ".into()),
            title: Some(format!("t{aid}")),
            sub_dt: Some(1000 + i64::from(aid)),
            sub_ip: None,
            content: None,
        }
    }

    fn reply_to(aid: i32, sid: i16, tid: i32) -> ArticleRow {
        ArticleRow { tid, ..row(aid, sid) }
    }

    fn op(mid: i32, aid: i32, kind: &str) -> OpRow {
        OpRow {
            mid,
            aid,
            kind: kind.to_owned(),
        }
    }

    fn section_row(sid: i16, sname: &str, title: &str) -> SectionRow {
        SectionRow {
            sid,
            sname: Some(sname.to_owned()),
            title: Some(title.to_owned()),
            cid: 7,
            read_user_level: 2,
            write_user_level: 3,
            enable: true,
            ex_menu_tm: None,
        }
    }

    #[test]
    fn a_loaded_row_keeps_its_columns_apart() {
        let board = board();
        let article = article_from_row(&row(5, 1), &board);
        assert_eq!(article.sub_dt, 1005);
        assert_eq!(article.title, "t5");
        assert_eq!(article.username, "tester");
        assert_eq!(article.nickname, " Tester ");
        assert_eq!(article.cid, 5);
        assert!(article.visible);
        // The links stay the arena's to fill, which is what `memset` used to zero.
        assert_eq!(article.prior, 0);
        assert_eq!(article.topic_next, 0);
    }

    #[test]
    fn labels_are_cut_to_the_c_buffer_sizes() {
        let board = board();
        let mut long = row(6, 1);
        long.username = Some("abcdefghijklm".into());
        long.nickname = Some("昵".repeat(20));
        long.title = Some("标".repeat(60));

        let article = article_from_row(&long, &board);
        assert_eq!(article.username, "abcdefghijkl");
        assert_eq!(article.username.len(), USERNAME_MAX_LEN);
        // A multi-byte label loses the character that would not fit whole.
        assert_eq!(article.nickname, "昵".repeat(13));
        assert_eq!(article.nickname.len(), NICKNAME_MAX_LEN - 1);
        assert_eq!(article.title, "标".repeat(53));
        assert_eq!(article.title.len(), ARTICLE_TITLE_MAX_LEN - 1);
    }

    #[test]
    fn a_reply_to_an_absent_or_hidden_topic_is_loaded_invisible() {
        let mut board = board();
        let mut head = row(10, 1);
        head.visible = false;
        append_articles_into(&mut board, &[head]).unwrap();

        let detached = article_from_row(&reply_to(11, 1, 999), &board);
        assert_eq!(detached.tid, 0);
        assert!(!detached.visible);

        let hidden_head = article_from_row(&reply_to(11, 1, 10), &board);
        assert_eq!(hidden_head.tid, 0);
        assert!(!hidden_head.visible);

        // The same row against a visible head keeps its topic.
        board.set_article_visible(1, 10, true).unwrap();
        let attached = article_from_row(&reply_to(11, 1, 10), &board);
        assert_eq!(attached.tid, 10);
        assert!(attached.visible);
    }

    #[test]
    fn an_unknown_board_stops_the_article_load() {
        let mut board = board();
        let err = append_articles_into(&mut board, &[row(1, 1), row(2, 7)]).unwrap_err();
        assert!(matches!(err, LoaderError::UnknownSection));
        // The article that did fit is still there, which is what the C loop breaks on.
        assert_eq!(board.article_count(), 1);
    }

    #[test]
    fn a_row_the_arena_refuses_ends_the_batch_without_an_error() {
        let mut board = board();
        // Two rows with the same `AID` cannot happen in the database, but they are the
        // cheapest way to reach the failure the original breaks on: the arena only takes a
        // strictly newer aid.
        let appended =
            append_articles_into(&mut board, &[row(1, 1), row(1, 1), row(2, 1)]).unwrap();
        assert_eq!(appended, 1);
        assert_eq!(board.article_count(), 1);
    }

    #[test]
    fn deleting_a_topic_hides_its_replies_and_re_indexes_the_pages() {
        let mut board = board();
        append_articles_into(
            &mut board,
            &[row(1, 1), reply_to(2, 1, 1), row(3, 1)],
        )
        .unwrap();

        let replay = apply_ops(&mut board, &[op(7, 1, "D")], &[]);
        assert_eq!(replay.applied, 1);
        assert!(replay.stop.is_none());

        let section = board.section(1).unwrap();
        assert!(!board.article_by_aid(1).unwrap().visible);
        assert!(!board.article_by_aid(2).unwrap().visible);
        assert!(board.article_by_aid(3).unwrap().visible);
        assert_eq!(section.visible_article_count, 1);
        assert_eq!(section.visible_topic_count, 1);
        assert_eq!(section.last_page_visible_article_count, 1);
        // `calculate_page` starts the re-index at the op's own article, which is invisible
        // now, so page 0 is opened by it even though nothing visible is there.
        assert_eq!(section.page_first, vec![0]);
    }

    #[test]
    fn restoring_a_topic_does_not_restore_its_replies() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

        apply_ops(&mut board, &[op(7, 1, "X")], &[]);
        let replay = apply_ops(&mut board, &[op(8, 1, "S")], &[]);
        assert_eq!(replay.applied, 1);

        assert!(board.article_by_aid(1).unwrap().visible);
        // Hiding cascades, showing does not, so the reply stays out until its own op arrives.
        assert!(!board.article_by_aid(2).unwrap().visible);
    }

    #[test]
    fn an_op_for_an_article_that_is_not_loaded_holds_the_watermark() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();

        let ops = [op(5, 1, "L"), op(6, 9, "X"), op(7, 1, "U")];
        let replay = apply_ops(&mut board, &ops, &[]);
        assert_eq!(replay.applied, 1);
        assert_eq!(replay.stop, Some(Stop::NotLoaded));
        // Only the first op went through, so the lock is on and the delete never was.
        assert!(board.article_by_aid(1).unwrap().lock);
        assert!(board.article_by_aid(1).unwrap().visible);
    }

    /// Only a cursor makes a pass reach the op log: `watermark` is `None` until the startup pass
    /// reads it, and a loader that never read it returns before its first query, so new articles
    /// still arrive through the AID scan while no `'X'`, `'M'` or `'E'` ever does. The C child
    /// got that value from the `fork()`; the daemon gets it by giving [`Loader::run`] the loader
    /// that ran [`Loader::initial_load`], which is what this test pins the cost of losing.
    #[tokio::test]
    async fn a_loader_without_a_cursor_never_reaches_the_op_log() {
        let session = crate::session::offline_session();
        let root = std::path::Path::new("/srv/lbbs");

        let mut fresh = Loader::new(session.state.clone(), root);
        assert_eq!(
            fresh.replay_ops().await.unwrap(),
            0,
            "no cursor means the pass ends before the query"
        );

        let mut seeded = Loader::new(session.state, root);
        seeded.watermark = Some(1);
        assert!(
            seeded.replay_ops().await.is_err(),
            "with one, the same call reaches an unreachable database"
        );
    }

    #[test]
    fn flag_letters_apply_to_the_board_the_article_is_loaded_in() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();

        let ops = [op(3, 1, "E"), op(4, 1, "Z"), op(5, 1, "F"), op(6, 1, "U")];
        let replay = apply_ops(&mut board, &ops, &[]);
        assert!(replay.stop.is_none());
        assert_eq!(replay.applied, 4);

        let article = board.article_by_aid(1).unwrap();
        assert!(article.excerption);
        assert!(article.transship);
        assert!(article.ontop);
        assert!(!article.lock);
        assert_eq!(board.section(1).unwrap().ontop_article_count(), 1);
        assert_eq!(board.section(2).unwrap().ontop_article_count(), 0);
    }

    #[test]
    fn unsetting_a_sticky_article_takes_it_out_of_the_list() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1), row(2, 1)]).unwrap();

        apply_ops(&mut board, &[op(1, 1, "F"), op(2, 2, "F")], &[]);
        assert_eq!(board.section(1).unwrap().ontop, vec![0, 1]);

        apply_ops(&mut board, &[op(3, 1, "V")], &[]);
        assert_eq!(board.section(1).unwrap().ontop, vec![1]);
        assert!(!board.article_by_aid(1).unwrap().ontop);
    }

    #[test]
    fn a_modify_brings_the_new_cid_and_clears_the_digest_flag() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();
        board.set_article_excerption(1, 1, true).unwrap();

        let content = ContentRow {
            cid: 77,
            sub_ip: None,
            content: None,
        };
        let extras = [OpExtra {
            content: Some(content),
            dest_sid: None,
        }];
        let replay = apply_ops(&mut board, &[op(4, 1, "M")], &extras);
        assert!(replay.stop.is_none());

        let article = board.article_by_aid(1).unwrap();
        assert_eq!(article.cid, 77);
        assert!(!article.excerption);
    }

    #[test]
    fn a_modify_without_its_content_row_ends_the_batch() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1), row(2, 1)]).unwrap();

        let ops = [op(4, 1, "M"), op(5, 1, "L")];
        let replay = apply_ops(&mut board, &ops, &[]);
        assert_eq!(replay.stop, Some(Stop::ContentMissing));
        // The CID is zeroed but the op is not counted, so the next pass starts from it again.
        assert_eq!(replay.applied, 0);
        assert_eq!(board.article_by_aid(1).unwrap().cid, 0);
        assert!(!board.article_by_aid(2).unwrap().lock);
    }

    #[test]
    fn a_move_follows_the_sid_stored_in_bbs() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

        let ops = [op(3, 1, "T")];
        let to_other = [OpExtra {
            dest_sid: Some(2),
            ..Default::default()
        }];
        assert!(apply_ops(&mut board, &ops, &to_other).stop.is_none());
        // The head and its reply move together, and the arena keeps the new board.
        assert_eq!(board.article_by_aid(1).unwrap().sid, 2);
        assert_eq!(board.article_by_aid(2).unwrap().sid, 2);
        assert_eq!(board.section(1).unwrap().article_count, 0);
        assert_eq!(board.section(2).unwrap().article_count, 2);

        // Replaying the op finds the topic already where the log says, so it is skipped
        // rather than reported as a failure.
        assert!(apply_ops(&mut board, &ops, &to_other).stop.is_none());
        assert_eq!(board.section(2).unwrap().article_count, 2);

        // No destination row means the article was deleted under the op.
        let replay = apply_ops(&mut board, &ops, &[OpExtra::default()]);
        assert_eq!(replay.applied, 1);
        assert!(replay.stop.is_none());
    }

    #[test]
    fn a_move_to_an_unknown_board_asks_for_a_config_reload() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();

        let ops = [op(3, 1, "T")];
        let extras = [OpExtra {
            dest_sid: Some(9),
            ..Default::default()
        }];
        let replay = apply_ops(&mut board, &ops, &extras);
        assert_eq!(replay.stop, Some(Stop::UnknownSection));
        assert_eq!(board.article_by_aid(1).unwrap().sid, 1);
    }

    #[test]
    fn a_move_the_board_refuses_is_retried_from_the_same_op() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

        // `section_list_move_topic` works on a topic head, and aid 2 is a reply.
        let ops = [op(3, 2, "T")];
        let extras = [OpExtra {
            dest_sid: Some(2),
            ..Default::default()
        }];
        let replay = apply_ops(&mut board, &ops, &extras);
        assert_eq!(replay.applied, 0);
        assert_eq!(replay.stop, Some(Stop::MoveLater));
        assert_eq!(board.article_by_aid(2).unwrap().sid, 1);
    }

    #[test]
    fn an_unknown_letter_is_processed_and_counted() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();

        let replay = apply_ops(&mut board, &[op(2, 1, "Q"), op(3, 1, "A")], &[]);
        assert_eq!(replay.applied, 2);
        assert!(replay.stop.is_none());
    }

    #[test]
    fn a_config_load_renames_a_board_without_touching_its_articles() {
        let mut board = board();
        append_articles_into(&mut board, &[row(1, 1)]).unwrap();

        let rows = vec![
            section_row(1, "renamed", "Renamed"),
            section_row(3, "new", "New"),
        ];
        let masters = vec!["alice bob ".to_owned(), String::new()];
        load_section_config_into(&mut board, &rows, &masters).unwrap();

        let section = board.section_by_name("renamed").unwrap();
        assert_eq!(section.sid, 1);
        assert_eq!(section.stitle, "Renamed");
        assert_eq!(section.master_list, "alice bob ");
        assert_eq!(section.article_count, 1);
        assert_eq!(section.class_id, 7);
        assert!(section.enable);
        assert_eq!(section.read_user_level, 2);
        assert_eq!(section.write_user_level, 3);

        // A board the config gained starts empty.
        let fresh = board.section(3).unwrap();
        assert_eq!(fresh.article_count, 0);
        assert!(fresh.page_first.is_empty());
        assert!(fresh.head.is_none());
    }

    #[test]
    fn a_config_load_cuts_the_master_list_to_its_buffer() {
        let mut board = Board::default();
        let masters = vec!["user_one user_two user_three ".repeat(4)];
        load_section_config_into(&mut board, &[section_row(1, "test", "Test")], &masters).unwrap();

        // `char master_list[(BBS_username_max_len + 1) * 3 + 1]`, the NUL excluded.
        assert_eq!(board.section(1).unwrap().master_list.len(), MASTER_LIST_BYTES);
    }

    #[test]
    fn a_second_load_of_the_same_config_does_not_duplicate_boards() {
        let mut board = Board::default();
        let rows = vec![
            section_row(1, "test", "Test"),
            section_row(2, "other", "Other"),
        ];
        let masters = vec![String::new(), String::new()];
        load_section_config_into(&mut board, &rows, &masters).unwrap();
        load_section_config_into(&mut board, &rows, &masters).unwrap();

        assert_eq!(board.sections.len(), 2);
    }

    /// `section_list_loader.c:162-179`: the row's stamp decides, `enable` gates, and a menu that
    /// will not read leaves the board with no tree and its stamp behind, so the next pass retries.
    #[test]
    fn a_digest_menu_loads_when_its_stamp_moves_and_not_otherwise() {
        let dir = std::env::temp_dir().join(format!("lbbs-gen-ex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let menu = concat!(
            "%S_EX_DIR\n",
            "%\n",
            "%menu M_EX_1_0\n",
            "screen 2, 0, S_EX_DIR\n",
            "@VIEW_EX_ARTICLE 4, 4, 1, 0, \"100\", \"(100)\"\n",
            "%\n",
        );
        std::fs::write(dir.join("1"), menu).unwrap();
        // Board 2 has a file that is not a menu, and board 3 has none at all.
        std::fs::write(dir.join("2"), "%menu M\nscreen 2, 0, S_NONE\n%\n").unwrap();

        let mut board = Board::default();
        let mut rows = vec![
            section_row(1, "test", "Test"),
            section_row(2, "broken", "Broken"),
            section_row(3, "gone", "Gone"),
            section_row(4, "closed", "Closed"),
            section_row(5, "unstamped", "Unstamped"),
        ];
        for row in &mut rows[..4] {
            row.ex_menu_tm = Some(1000);
        }
        rows[3].enable = false;
        let masters = vec![String::new(); rows.len()];
        load_section_config_into(&mut board, &rows, &masters).unwrap();
        load_gen_ex_menus_into(&mut board, &rows, &dir);

        let set = board.section(1).unwrap().ex_menu_set.clone().expect("loaded");
        assert_eq!(board.section(1).unwrap().ex_menu_tm, 1000, "the stamp moves on");
        assert_eq!(set.items[0].name, "100", "the name is the AID the item passes on");

        for sid in [2, 3] {
            let section = board.section(sid).unwrap();
            assert!(section.ex_menu_set.is_none(), "{sid} kept a tree it never read");
            assert_eq!(section.ex_menu_tm, 0, "{sid} was stamped by a failed load");
        }
        // A disabled board is skipped before the stamp is looked at (`:164`), and a NULL
        // `ex_menu_tm` is `atol` of nothing: neither is due for anything.
        assert_eq!(board.section(4).unwrap().ex_menu_tm, 0);
        assert_eq!(board.section(5).unwrap().ex_menu_tm, 0);

        // The same stamp again: the file behind the board moved on, and the tree did not, because
        // C only reloads while the row is *newer* than what the board holds.
        std::fs::write(dir.join("1"), menu.replace("(100)", "(rewritten)")).unwrap();
        load_gen_ex_menus_into(&mut board, &rows, &dir);
        let set = board.section(1).unwrap().ex_menu_set.clone().unwrap();
        assert_eq!(set.items[0].text, "(100)", "an unchanged stamp does not reload");

        // A newer one does. Board 2 is retried on every pass all along: its stamp never advanced,
        // so the row stays newer than what the board holds.
        rows[0].ex_menu_tm = Some(2000);
        load_gen_ex_menus_into(&mut board, &rows, &dir);
        let set = board.section(1).unwrap().ex_menu_set.clone().unwrap();
        assert_eq!(set.items[0].text, "(rewritten)");
        assert_eq!(board.section(1).unwrap().ex_menu_tm, 2000);
        assert!(board.section(2).unwrap().ex_menu_set.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_cache_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("lbbs-loader-cache-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        format!("{}/", dir.to_str().unwrap())
    }

    fn dir_entries(dir: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// A row as the bulk query returns it: the joined `bbs_content` body and the address the
    /// loader masks on the way to the footer.
    fn cached_row(aid: i32, sid: i16) -> ArticleRow {
        ArticleRow {
            sub_ip: Some("203.0.113.9".into()),
            content: Some(format!("line {aid}\n")),
            ..row(aid, sid)
        }
    }

    const SITE: &str = "枫林在线";
    const SERVER: &str = "www.bbs.test";

    #[test]
    fn the_rows_a_pass_appended_get_a_cache_file() {
        let dir = temp_cache_dir("rows");
        let mut board = board();
        let rows = [cached_row(31, 1), cached_row(32, 1)];
        assert_eq!(append_articles_into(&mut board, &rows).unwrap(), 2);

        assert_eq!(generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows), 2);
        assert_eq!(dir_entries(&dir), ["31", "32"]);

        let cached = cache::load(&dir, 31).unwrap().unwrap();
        let text = String::from_utf8_lossy(&cached.data).into_owned();
        assert!(
            text.starts_with("发布者: tester ( Tester ), 版块: test (Test)\n标  题: t31\n"),
            "{text}"
        );
        assert!(text.contains("line 31\n"), "{text}");
        // One octet masked, which is the `ip_mask(sub_ip, 1, '*')` the C loader runs first.
        assert!(text.contains("[FROM: 203.0.113.*]"), "{text}");
    }

    #[test]
    fn an_existing_cache_file_is_kept() {
        let dir = temp_cache_dir("existing");
        let mut board = board();
        let rows = [cached_row(33, 1)];
        append_articles_into(&mut board, &rows).unwrap();
        assert_eq!(generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows), 1);

        // C opens the file read-only and returns without writing; a second pass that found a
        // body already rendered must not touch it, only report nothing as new.
        std::fs::write(format!("{dir}33"), b"sentinel").unwrap();
        assert_eq!(generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows), 0);
        assert_eq!(std::fs::read(format!("{dir}33")).unwrap(), b"sentinel");
    }

    #[test]
    fn a_row_without_a_content_column_renders_nothing() {
        let dir = temp_cache_dir("nocontent");
        let mut board = board();
        let rows = [row(34, 1)];
        append_articles_into(&mut board, &rows).unwrap();

        assert_eq!(generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows), 0);
        assert!(dir_entries(&dir).is_empty());
    }

    #[test]
    fn a_row_of_a_board_the_pool_lacks_renders_nothing() {
        let dir = temp_cache_dir("noboards");
        let board = board();
        let rows = [cached_row(36, 9)];

        // The same row is what makes `append_articles_into` return `UnknownSection`; the render
        // pass skips it instead of writing a header with no board name in it.
        assert_eq!(generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows), 0);
        assert!(dir_entries(&dir).is_empty());
    }

    #[test]
    fn a_replayed_modify_renders_the_new_cid() {
        let dir = temp_cache_dir("modify");
        let mut board = board();
        let rows = [cached_row(35, 1)];
        append_articles_into(&mut board, &rows).unwrap();
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows);

        let ops = [op(9, 35, "M")];
        let extras = [OpExtra {
            content: Some(ContentRow {
                cid: 700,
                sub_ip: Some("198.51.100.7".into()),
                content: Some("改过的正文\n".into()),
            }),
            ..Default::default()
        }];
        let replay = apply_ops(&mut board, &ops, &extras);
        assert_eq!(replay.applied, 1);

        let rendered = generate_caches_from_modifies(&dir, SITE, SERVER, &board, &ops, &extras, replay.applied);
        assert_eq!(rendered, 1);
        assert_eq!(dir_entries(&dir), ["35", "700"]);

        let cached = cache::load(&dir, 700).unwrap().unwrap();
        let text = String::from_utf8_lossy(&cached.data).into_owned();
        assert!(text.contains("改过的正文"), "{text}");
        assert!(text.contains("标  题: t35"), "{text}");
        assert!(text.contains("[FROM: 198.51.100.*]"), "{text}");

        // A letter that is not a modify changed no body, and the ops past the one the replay
        // stopped on are not rendered at all.
        let ops = [op(10, 35, "L"), op(11, 35, "M")];
        let extras = [OpExtra::default(), extras[0].clone()];
        let replay = apply_ops(&mut board, &ops, &extras);
        assert_eq!(replay.applied, 2);
        assert_eq!(dir_entries(&dir), ["35", "700"]);
        assert_eq!(
            generate_caches_from_modifies(&dir, SITE, SERVER, &board, &ops, &extras, 1),
            0,
            "only the `'L'` op was in the reported range"
        );
    }

    /// Opt-in check against the real database, which is the only way to exercise the async
    /// half of this module: `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
    /// Every statement the loader runs is a `SELECT`, so this stays read-only.
    #[tokio::test]
    #[ignore = "needs the live board database and LBBS_REF_CONF"]
    async fn live_load_fills_the_pools() {
        let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
        let root = Path::new(&conf)
            .ancestors()
            .nth(2)
            .expect("LBBS_REF_CONF must be <root>/conf/bbsd.conf")
            .to_path_buf();
        let text = std::fs::read_to_string(&conf).unwrap();
        let (config, _warnings) = crate::config::parse(&text);
        let db = Db::connect(&config.db).await.unwrap();
        let state = std::sync::Arc::new(AppState::new(config, db));

        let mut loader = Loader::new(state.clone(), &root);
        loader.initial_load().await.unwrap();
        assert!(loader.watermark.is_some());
        assert!(!loader.reload_config);

        let board = state.board();
        // Floors rather than exact counts, which is what makes this survive a live site: the
        // batch loop has to have run more than once for these to hold.
        assert!(board.sections.len() > 10);
        assert!(board.article_count() > 1000);
        // Every article the scan accepted is in exactly one board, which is what makes the
        // counters below add up.
        let in_boards: i32 = board.sections.iter().map(|s| s.article_count).sum();
        assert_eq!(i32::try_from(board.article_count()).unwrap(), in_boards);
        for section in board.sections.iter() {
            assert!(section.visible_article_count <= section.article_count);
            assert!(section.visible_topic_count <= section.topic_count);
            assert!(section.topic_count <= section.article_count);
            assert!(section.ontop.len() <= section.article_count as usize);
            // The first article of a board always opens page 0, even an invisible one, so a
            // board with articles has at least one page head and an empty board has none.
            assert_eq!(!section.page_first.is_empty(), section.article_count > 0);
            let mut aids = Vec::new();
            for art in &section.page_first {
                assert_eq!(board.articles[*art].sid, section.sid);
                aids.push(board.articles[*art].aid);
            }
            assert!(aids.windows(2).all(|w| w[0] < w[1]));
        }

        // The statistics map covers the uids the roster loaded, so at least one of them has a
        // counter to report.
        let mut posted = 0;
        for user in state.roster().users.iter() {
            posted += i32::from(board.user_article_count(user.uid).unwrap_or_default() > 0);
        }
        assert!(posted > 0);
        let newest_cid = board.article_by_aid(board.last_aid()).map(|article| article.cid);
        drop(board);

        // Every post the scan took needs a file for the reader to open, which is the render of
        // `:327`. The newest post is the one a tree that never ran the C loader would lack.
        let cache_dir = root.join(VAR_ARTICLE_CACHE_DIR);
        let cache_dir = cache_dir.to_string_lossy().into_owned();
        let cid = newest_cid.expect("the scan loaded at least one article");
        assert!(
            cache::load(&cache_dir, cid)
                .expect("read cache file")
                .is_some(),
            "cid {cid} has no cache file in {cache_dir}"
        );

        // One more pass with the watermark in place, which is the steady state: nothing new to
        // load, and the two user tables published again.
        let before = loader.watermark;
        let users = state.roster().count();
        loader.tick().await;
        assert!(loader.watermark >= before);
        assert_eq!(state.roster().count(), users);
        assert!(!state.board().sections.is_empty());
    }
}
