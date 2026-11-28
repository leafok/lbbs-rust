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
//!
//! The C batch carries the board it is holding in `last_sid` and releases it twice: after each
//! hand-over (`:287`, `:468`) and once more for the last board of the loop (`:342`, `:658`). Both
//! of those pairs were wrong until `27ffdde` cleared the id when the unlock left nothing held and
//! added the `p_section != NULL` the release was missing. The port has no handle to go stale:
//! [`append_articles_into`] and [`apply_ops`] are handed a `&mut Board` by a caller that locked
//! that one board for the whole batch, so the id, the hand-over and the final release are all
//! gone rather than ported.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::future::join_all;

use crate::board::{Article, Board, Section};
use crate::cache;
use crate::db::{ArticleRow, ContentRow, Db, OpRow, SectionRow};
use crate::ipmask;
use crate::paths::{USER_OFF_LINE_SECS, VAR_ARTICLE_CACHE_DIR, VAR_GEN_EX_MENU_DIR};
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
            Self::SectionPoolFull => {
                write!(f, "section list is full or a board name is duplicated")
            }
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
/// already there keeps its articles, because `section_list_update` (`section_list.c:709`)
/// rewrites only `sname`, `stitle` and `master_list`. Renaming a board therefore leaves the
/// arena links alone, and the articles loaded under the old name stay where they are.
pub fn load_section_config_into(
    board: &mut Board,
    rows: &[SectionRow],
    master_lists: &[String],
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
/// loop's `section_list_loader.c:755`, which in the port is [`Loader::load_section_config`] — the
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

/// `article_cache_generate` for the posts a load pass appended (`section_list_loader.c:333`).
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

        if render_cache(
            cache_dir,
            bbs_name,
            bbs_server,
            &article,
            section,
            content,
            row.sub_ip.as_deref(),
        ) {
            rendered += 1;
        }
    }

    rendered
}

/// `article_cache_generate` for the `'M'` ops a replay got through (`section_list_loader.c:550`).
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

        if render_cache(
            cache_dir,
            bbs_name,
            bbs_server,
            article,
            section,
            content,
            row.sub_ip.as_deref(),
        ) {
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
        tracing::info!("initially loaded {count} articles (last_aid={last_aid})");

        self.update_stats().await
    }

    /// The body of the C loop: config when a load asked for it, then new articles, then the
    /// op log, then whichever user table is due. Public so a caller with a shutdown signal can
    /// drive the passes itself instead of waiting for [`Loader::run`] to notice.
    pub async fn tick(&mut self) {
        if self.reload_config {
            self.reload_config = false;
            Self::report_config_pass(self.load_section_config().await);
        }

        match self.append_new_articles().await {
            Ok(0) => {}
            Ok(count) => {
                let last_aid = self.state.board().last_aid();
                tracing::info!("incrementally loaded {count} articles (last_aid={last_aid})");
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
            Ok(count) => {
                let last_mid = self.watermark;
                tracing::info!("proceeded {count} article logs (last_mid={last_mid:?})")
            }
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
    /// index swap of `:661-667` expressed as an [`std::sync::Arc`].
    pub async fn reload_roster(&self) -> Result<(), LoaderError> {
        let rows = self.state.db.roster().await?;
        self.state.publish_roster(Roster::load(&rows));
        Ok(())
    }

    /// `user_list_pool_reload(1)`, whose query filters on `BBS_user_off_line` (`:280`). The pass
    /// is both queries and one publish: C loads the list, then `user_login_count_load` (`:372`),
    /// and reaches its index swap (`:654-660`) only if neither failed — so the online roster a
    /// user sees and the 累计访问人次 beside it always come from the same pass.
    pub async fn reload_online(&self) -> Result<(), LoaderError> {
        let rows = self.state.db.online(USER_OFF_LINE_SECS).await?;
        let online = {
            let roster = self.state.roster();
            OnlineList::load(&rows, &roster)
        };
        let count = self.state.db.login_count().await?;
        self.publish_online_pass(online, count);
        Ok(())
    }

    /// The publish half of [`Loader::reload_online`]: one swap carrying both results, so the
    /// pair a user sees is always a pair from the same pass. Split out because the queries
    /// cannot run without the board's database.
    fn publish_online_pass(&self, online: OnlineList, count: Option<i32>) {
        self.state.publish_online(online);
        // `:396-403` assigns in both branches, so a `user_login_log` with no rows resets the
        // total to 0 instead of leaving the previous pass standing.
        self.state.set_login_count(count.unwrap_or(0));
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
        let master_lists = join_all(
            rows.iter()
                .map(|row| self.state.db.section_masters(row.sid)),
        )
        .await
        .into_iter()
        .collect::<Result<Vec<String>, sqlx::Error>>()?;

        let mut board = self.state.board_mut();
        load_section_config_into(&mut board, &rows, &master_lists)?;
        load_gen_ex_menus_into(&mut board, &rows, &self.gen_ex_dir);
        Ok(())
    }

    /// `section_list_loader.c:755-762`: a config pass answers with exactly one line, the success
    /// one on the common stream (`log_common("Reload section config successfully")`) and the
    /// failure one on the error stream.
    fn report_config_pass(result: Result<(), LoaderError>) {
        match result {
            Ok(()) => tracing::info!("Reload section config successfully"),
            Err(e) => tracing::warn!("section config reload failed: {e}"),
        }
    }

    /// `append_articles_from_db(last_aid + 1, 0, LOAD_ARTICLE_COUNT_LIMIT)`, run until a batch
    /// comes back short (`:760-773`). Returns the articles loaded.
    pub async fn append_new_articles(&self) -> Result<usize, LoaderError> {
        let mut total = 0;

        loop {
            let start_aid = self.state.board().last_aid() + 1;
            let rows = self
                .state
                .db
                .articles_from(start_aid, LOAD_BATCH as i32)
                .await?;
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
            tracing::debug!("generated {rendered} article cache files");
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
            tracing::debug!("generated {rendered} article cache files");
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
#[path = "../tests/test_loader.rs"]
mod tests;
