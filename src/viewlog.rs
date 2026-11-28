//! What this reader has already seen.
//!
//! Mirrors `src/article_view_log.c`: the board listing marks a thread `n` when it has new
//! posts, which needs to know every article id the account has read. That list is thousands
//! of rows, so the C front end never rewrites it. It loads the saved set once after login as
//! a sorted array (`aid_base`), appends what the session reads to a second sorted array
//! (`aid_inc`) of at most [`MAX_VIEWED_AID_INC_CNT`] entries, and when that overflows — or
//! when the session ends — inserts only the increment (`article_view_log.c:144-172`).
//!
//! # Deviations from C
//!
//! * `article_view_log_load()` is called with `keep_inc = 0` everywhere (`bbs_main.c:403`),
//!   so the port has no `keep_inc` argument; loading always starts a fresh increment.
//! * `article_view_log.c:363-379` picks the insertion point out of the `left` and `right`
//!   indices the membership search was left holding. That search always ends with
//!   `left == right`, so the two branches it feeds are exactly the first element not less
//!   than the aid; the port says that directly and keeps the same array order.
//! * On a failed flush C returned -2 with the increment still full and went on appending.
//!   The port propagates the error and leaves the arrays alone, so the caller decides
//!   whether to keep marking articles in memory for the rest of the session.
//! * The uid 0 short-circuit (`article_view_log.c:37-48`) is [`ViewLog::anonymous`] plus
//!   [`ViewLog::flushable`], rather than a half-initialised struct every caller has to
//!   guard before calling [`ViewLog::save_inc`].

use crate::db::Db;

/// `MAX_VIEWED_AID_INC_CNT` (`include/article_view_log.h:16`): rows buffered before a flush.
pub const MAX_VIEWED_AID_INC_CNT: usize = 1000;

/// What asking to mark an article did, so a caller can tell a fresh view from a repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marked {
    /// Both arrays already held the aid (`article_view_log.c:336`, `:342`).
    Already,
    /// The aid went into the increment; no database write was needed.
    Added,
    /// The increment was full, so it was flushed and folded into the saved set first
    /// (`article_view_log.c:347-361`) before the aid went in.
    Flushed,
}

/// The two sorted aid arrays of one session, [`ViewLog::base`] then [`ViewLog::inc`].
#[derive(Debug, Clone, Default)]
pub struct ViewLog {
    /// `ARTICLE_VIEW_LOG::uid`, the account the increment is written under.
    uid: i32,
    /// Aids from `view_article_log`, ascending, as the `ORDER BY AID` returns them.
    base: Vec<i32>,
    /// Aids read since the load, kept ascending so [`ViewLog::is_viewed`] can search them.
    inc: Vec<i32>,
}

impl ViewLog {
    /// `ARTICLE_VIEW_LOG BBS_article_view_log;` — the zeroed global a forked child starts
    /// with, which is what a guest session keeps for its whole life.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A loaded log whose saved set is already in hand, which is also what the tests use.
    pub fn from_base(uid: i32, base: Vec<i32>) -> Self {
        Self {
            uid,
            base: sorted(base),
            inc: Vec::new(),
        }
    }

    /// `aid_base_cnt` plus `aid_inc_cnt`.
    pub fn len(&self) -> usize {
        self.base.len() + self.inc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `aid_inc_cnt`: how many rows the next flush would insert.
    pub fn pending(&self) -> usize {
        self.inc.len()
    }

    /// Whether the increment has room for another aid.
    pub fn is_full(&self) -> bool {
        self.inc.len() >= MAX_VIEWED_AID_INC_CNT
    }

    /// Whether [`save_inc`](Self::save_inc) has anything to write, which is C's guard at
    /// `article_view_log.c:133` — an unauthenticated session must not put uid 0 rows in
    /// `view_article_log`, where they would make every article read by every guest "viewed".
    pub fn flushable(&self) -> bool {
        self.uid > 0 && !self.inc.is_empty()
    }

    /// `article_view_log_is_viewed` (`article_view_log.c:251-298`): the saved set, then the
    /// increment.
    pub fn is_viewed(&self, aid: i32) -> bool {
        self.base.binary_search(&aid).is_ok() || self.inc.binary_search(&aid).is_ok()
    }

    /// `article_view_log_set_viewed` (`article_view_log.c:300-383`) minus the flush: put the
    /// aid in the increment and report whether that was new.
    ///
    /// Callers that want C's whole sequence, flush included, use
    /// [`mark_viewed`](Self::mark_viewed).
    pub fn set_viewed(&mut self, aid: i32) -> bool {
        if self.is_viewed(aid) {
            return false;
        }

        // The first element not less than the aid, which is where the aid has to go for the
        // increment to stay ascending.
        let at = self.inc.partition_point(|held| *held < aid);
        self.inc.insert(at, aid);
        true
    }

    /// The increment, in the order a flush would insert it.
    pub fn pending_aids(&self) -> &[i32] {
        &self.inc
    }

    /// `article_view_log_save_inc` (`article_view_log.c:119-179`), batched inside
    /// [`Db::view_log_insert`].
    ///
    /// The increment stays in place, as in C: the session-end call at `bbs_main.c:441` never
    /// merges afterwards, because the process is on its way out.
    pub async fn save_inc(&self, db: &Db) -> Result<u64, sqlx::Error> {
        if !self.flushable() {
            return Ok(0);
        }

        let affected = db.view_log_insert(self.uid, &self.inc).await?;

        tracing::info!(
            "Saved {affected} view_article_log records for uid={}",
            self.uid
        );

        Ok(affected)
    }

    /// `article_view_log.c:347-361`: make room if the increment is full, then mark.
    pub async fn mark_viewed(&mut self, db: &Db, aid: i32) -> Result<Marked, sqlx::Error> {
        if self.is_viewed(aid) {
            return Ok(Marked::Already);
        }

        if self.is_full() {
            self.save_inc(db).await?;
            self.merge_inc();
            // C appends at `aid_inc_cnt`, which the merge just reset to zero.
            self.inc.push(aid);
            return Ok(Marked::Flushed);
        }

        self.set_viewed(aid);
        Ok(Marked::Added)
    }

    /// `article_view_log_merge_inc` (`article_view_log.c:181-249`): fold the increment into
    /// the saved set and drop it.
    ///
    /// C logs `Duplicate aid = %d found in both Base (offset = %d) and Inc (offset = %d)`
    /// (`article_view_log.c:214`) when the two arrays meet the same aid, which means the
    /// increment was flushed twice; either way one copy survives, so the port dedupes in
    /// silence and the test pins the resulting length.
    pub fn merge_inc(&mut self) {
        if self.inc.is_empty() {
            return;
        }

        let mut merged = std::mem::take(&mut self.base);
        merged.extend(self.inc.iter().copied());
        merged.sort_unstable();
        merged.dedup();
        self.base = merged;
        self.inc.clear();
    }

    /// `article_view_log_load` (`article_view_log.c:22-99`): the saved set for a logged-in
    /// uid, with the increment reset. A guest id has no rows and gets no query.
    pub async fn load(db: &Db, uid: i32) -> Result<Self, sqlx::Error> {
        if uid == 0 {
            return Ok(Self::anonymous());
        }

        let base = db.view_log_aids(uid).await?;
        let len = base.len();

        tracing::info!("Loaded {len} view_article_log records for uid={uid}");

        Ok(Self {
            uid,
            base: sorted(base),
            inc: Vec::new(),
        })
    }
}

/// `ORDER BY AID` sorts the rows already, but the arrays are documented as ascending and a
/// caller building one by hand should not have to know that.
fn sorted(mut aids: Vec<i32>) -> Vec<i32> {
    aids.sort_unstable();
    aids
}

#[cfg(test)]
#[path = "../tests/test_viewlog.rs"]
mod tests;
