//! Which threads this reader starred, including the stars not yet written.
//!
//! Mirrors `src/article_favor.c`. Like [`crate::viewlog`] this is a sorted saved set plus a
//! sorted per-session increment, but here the increment means "toggled since login" rather
//! than "added": starring a thread the account already owns puts its aid in the increment so
//! the flush can `DELETE` it. Membership is therefore the exclusive or of the two arrays
//! (`article_favor_check`, `article_favor.c:293-342`), and folding the increment in is a
//! symmetric difference (`article_favor_merge_inc`, `:225-291`).
//!
//! Only a topic root can be starred: every caller passes `aid` when `tid == 0` and `tid`
//! otherwise (`section_list_display.c:1084`, `:1095`), and the table's own key is the root
//! article's id.
//!
//! # Deviations from C
//!
//! * `article_favor.c:397-409` cancels a pending toggle by removing `aid_inc[left]`, where
//!   `left` is whatever the membership search left behind — it happens to be the increment's
//!   index only when the aid was found there. The port searches the increment for the aid
//!   outright, which is the branch's stated intent.
//! * The two arrays are fixed in C: `aid_base[MAX_FAVOR_AID_BASE_CNT]` and
//!   `aid_inc[MAX_FAVOR_AID_INC_CNT]`, so loading a 10 001st star logs `Too many
//!   article_favorite records for uid=%d` and drops it, and a merge logs `Too many base aids,
//!   %d will be discarded`. Both grow here, so neither limit is reproduced; only the
//!   increment's [`MAX_FAVOR_AID_INC_CNT`] flush threshold, which decides how often the
//!   database is touched, carries over.
//! * The overflow path in C returns -2 and keeps appending past the end of the array when
//!   the flush fails. The port propagates the error and leaves the arrays alone.

use crate::db::Db;

/// `MAX_FAVOR_AID_INC_CNT` (`include/article_favor.h:18`): toggles buffered before a flush.
pub const MAX_FAVOR_AID_INC_CNT: usize = 1000;

/// `MAX_FAVOR_AID_BASE_CNT` (`include/article_favor.h:17`), recorded for the comment in
/// [`FavorLog::merge_inc`] about why C's merge could discard aids.
pub const MAX_FAVOR_AID_BASE_CNT: usize = 10000;

/// What asking to star or unstar a thread did. C's `article_favor_set` returns 0, 1 or 2;
/// 1 and 2 are distinguished here because they mean opposite things for the increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marked {
    /// `return 0` (`article_favor.c:392-395`): the thread already looked like `state`.
    Unchanged,
    /// `return 1` (`:447`): the toggle now sits in the increment, waiting for a flush.
    Pending,
    /// `return 2` (`:408`): the increment held the toggle, so dropping it restores the saved
    /// answer and nothing needs writing.
    Cancelled,
}

/// The star state of one account: the saved aids, then this session's toggles.
#[derive(Debug, Clone, Default)]
pub struct FavorLog {
    /// `ARTICLE_FAVOR::uid`, the account the increment is written under.
    uid: i32,
    /// Topic-root aids from `article_favorite`, ascending.
    base: Vec<i32>,
    /// Roots toggled since the load, ascending.
    inc: Vec<i32>,
}

impl FavorLog {
    /// `ARTICLE_FAVOR BBS_article_favor;` — what a guest session keeps for its whole life,
    /// since `article_favor_load` returns early for uid 0 (`article_favor.c:38-48`).
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

    /// `aid_inc_cnt`: how many toggles the next flush would write.
    pub fn pending(&self) -> usize {
        self.inc.len()
    }

    /// Whether the increment has room for another toggle.
    pub fn is_full(&self) -> bool {
        self.inc.len() >= MAX_FAVOR_AID_INC_CNT
    }

    /// Whether [`save_inc`](Self::save_inc) has anything to write — C's guard at
    /// `article_favor.c:132-135`, which keeps a guest out of the table entirely.
    pub fn flushable(&self) -> bool {
        self.uid > 0 && !self.inc.is_empty()
    }

    /// `article_favor_check` (`article_favor.c:293-342`): held by exactly one of the arrays.
    pub fn is_favor(&self, aid: i32) -> bool {
        self.base.binary_search(&aid).is_ok() ^ self.inc.binary_search(&aid).is_ok()
    }

    /// The aids [`save_inc`](Self::save_inc) would insert and delete, which is the two-pointer
    /// walk at `article_favor.c:149-173` made a pair of vectors.
    ///
    /// An aid in the increment alone is a new star; an aid the saved set also holds is one
    /// the session just unstarred.
    pub fn pending_changes(&self) -> (Vec<i32>, Vec<i32>) {
        let mut add = Vec::new();
        let mut remove = Vec::new();

        for aid in &self.inc {
            if self.base.binary_search(aid).is_ok() {
                remove.push(*aid);
            } else {
                add.push(*aid);
            }
        }

        (add, remove)
    }

    /// The merged, ascending star list — what `query_favor_articles` walks after flushing
    /// (`article_favor.c:463-494`).
    pub fn favor_aids(&self) -> Vec<i32> {
        let mut aids: Vec<i32> = self
            .inc
            .iter()
            .copied()
            .filter(|aid| !self.base.binary_search(aid).is_ok())
            .collect();
        aids.extend(
            self.base
                .iter()
                .copied()
                .filter(|aid| !self.inc.binary_search(aid).is_ok()),
        );
        aids.sort_unstable();
        aids
    }

    /// `article_favor_set` (`article_favor.c:344-448`) minus the flush.
    ///
    /// Callers that want C's whole sequence use [`mark`](Self::mark).
    pub fn set(&mut self, aid: i32, state: bool) -> Marked {
        if self.is_favor(aid) == state {
            return Marked::Unchanged;
        }

        if let Some(at) = self.inc.binary_search(&aid).ok() {
            // The toggle cancels itself: the saved set already says what state asks for.
            self.inc.remove(at);
            return Marked::Cancelled;
        }

        let at = self.inc.partition_point(|held| *held < aid);
        self.inc.insert(at, aid);
        Marked::Pending
    }

    /// `article_favor_save_inc` (`article_favor.c:113-223`): insert the new stars, delete the
    /// removed ones, both batched inside [`Db::favor_update`]. The increment stays in place,
    /// as in C — the session-end call at `bbs_main.c:447` never merges, because the process
    /// is about to exit.
    pub async fn save_inc(&self, db: &Db) -> Result<(u64, u64), sqlx::Error> {
        if !self.flushable() {
            return Ok((0, 0));
        }

        let (add, remove) = self.pending_changes();
        let (inserted, deleted) = db.favor_update(self.uid, &add, &remove).await?;

        tracing::info!(
            "Saved {inserted} and deleted {deleted} article_favorite records for uid={}",
            self.uid
        );

        Ok((inserted, deleted))
    }

    /// `article_favor.c:411-426`: make room if the increment is full, then toggle.
    pub async fn mark(&mut self, db: &Db, aid: i32, state: bool) -> Result<Marked, sqlx::Error> {
        if self.is_favor(aid) == state {
            return Ok(Marked::Unchanged);
        }

        if self.is_full() {
            self.save_inc(db).await?;
            self.merge_inc();
            // C appends at `aid_inc_cnt`, which the merge just reset to zero; the aid cannot
            // already be in an empty increment, so this is set()'s insert in disguise.
            self.inc.push(aid);
            return Ok(Marked::Pending);
        }

        Ok(self.set(aid, state))
    }

    /// `article_favor_merge_inc` (`article_favor.c:225-291`): fold the increment into the
    /// saved set with the same exclusive or the readers use, so a star and an unstar of the
    /// same thread both cancel out and the list stays ascending.
    pub fn merge_inc(&mut self) {
        if self.inc.is_empty() {
            return;
        }

        let mut merged = Vec::with_capacity(self.base.len() + self.inc.len());
        let (mut i, mut j) = (0, 0);
        while i < self.base.len() || j < self.inc.len() {
            // The sentinel stands for "this list is exhausted", which no real aid reaches.
            let base = *self.base.get(i).unwrap_or(&i32::MAX);
            let fresh = *self.inc.get(j).unwrap_or(&i32::MAX);

            if base == fresh {
                // Both lists hold the aid: the session toggled a saved star, so the merged
                // answer is the opposite of the saved one and the aid goes into neither list
                // (`article_favor.c:244-248`).
                i += 1;
                j += 1;
            } else if base < fresh {
                merged.push(base);
                i += 1;
            } else {
                merged.push(fresh);
                j += 1;
            }
        }

        self.base = merged;
        self.inc.clear();
    }

    /// `article_favor_load` (`article_favor.c:23-98`): the saved stars for a logged-in uid,
    /// with the increment reset. A guest id has no rows and gets no query.
    pub async fn load(db: &Db, uid: i32) -> Result<Self, sqlx::Error> {
        if uid == 0 {
            return Ok(Self::anonymous());
        }

        let base = db.favor_aids(uid).await?;
        let len = base.len();

        tracing::info!("Loaded {len} article_favorite records for uid={uid}");

        Ok(Self {
            uid,
            base: sorted(base),
            inc: Vec::new(),
        })
    }
}

/// `ORDER BY AID` sorts the rows already, but both arrays are documented as ascending.
fn sorted(mut aids: Vec<i32>) -> Vec<i32> {
    aids.sort_unstable();
    aids
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;

    fn starred(aids: &[i32]) -> FavorLog {
        FavorLog::from_base(42, aids.to_vec())
    }

    #[test]
    fn the_saved_set_answers_for_its_first_and_last_aid() {
        let favor = starred(&[3, 8, 15]);

        assert!(favor.is_favor(3));
        assert!(favor.is_favor(15));
        assert!(!favor.is_favor(4));
        assert!(!favor.is_favor(16));
    }

    #[test]
    fn starring_a_fresh_thread_holds_the_toggle_until_a_flush() {
        let mut favor = starred(&[10]);

        assert_eq!(favor.set(20, true), Marked::Pending);
        assert!(favor.is_favor(20));
        assert_eq!(favor.pending(), 1);
        // Nothing written yet, and the saved list is untouched.
        assert_eq!(favor.pending_changes(), (vec![20], vec![]));
        assert_eq!(favor.favor_aids(), vec![10, 20]);
    }

    #[test]
    fn unstarring_a_saved_thread_is_a_pending_removal_not_an_addition() {
        // The exclusive or: the increment holds the aid, so the reader reports it gone while
        // the row stays in the table until the session flushes it.
        let mut favor = starred(&[10, 20]);

        assert_eq!(favor.set(20, false), Marked::Pending);
        assert!(!favor.is_favor(20));
        assert_eq!(favor.pending_changes(), (vec![], vec![20]));
        assert_eq!(favor.favor_aids(), vec![10]);
    }

    #[test]
    fn asking_for_the_state_it_already_has_changes_nothing() {
        let mut favor = starred(&[10]);

        assert_eq!(favor.set(10, true), Marked::Unchanged);
        assert_eq!(favor.set(11, false), Marked::Unchanged);
        assert_eq!(favor.pending(), 0);
    }

    #[test]
    fn toggling_twice_cancels_the_pending_write() {
        let mut favor = starred(&[10]);

        assert_eq!(favor.set(20, true), Marked::Pending);
        assert_eq!(favor.set(20, false), Marked::Cancelled);
        assert_eq!(favor.pending(), 0);
        assert!(!favor.is_favor(20));

        // And the other way round, on a thread that really is saved.
        assert_eq!(favor.set(10, false), Marked::Pending);
        assert_eq!(favor.set(10, true), Marked::Cancelled);
        assert_eq!(favor.pending(), 0);
        assert!(favor.is_favor(10));
    }

    #[test]
    fn toggles_arriving_out_of_order_stay_sorted_in_the_increment() {
        let mut favor = starred(&[]);

        favor.set(30, true);
        favor.set(4, true);
        favor.set(22, true);

        assert_eq!(favor.pending_changes(), (vec![4, 22, 30], vec![]));
    }

    #[test]
    fn merging_a_star_and_an_unstar_leaves_the_saved_list_as_the_table_will_be() {
        let mut favor = starred(&[1, 5, 9]);

        favor.set(9, false);
        favor.set(7, true);
        favor.set(3, true);
        favor.merge_inc();

        assert_eq!(favor.base, vec![1, 3, 5, 7]);
        assert_eq!(favor.pending(), 0);
        assert!(!favor.is_favor(9));
        assert!(favor.is_favor(7));
        assert_eq!(favor.len(), 4);
    }

    #[test]
    fn a_full_increment_reports_itself_before_the_next_toggle() {
        let mut favor = starred(&[]);
        favor.inc.extend(1..=MAX_FAVOR_AID_INC_CNT as i32);

        assert!(favor.is_full());
        assert_eq!(
            favor.pending_changes(),
            ((1..=1000).collect::<Vec<_>>(), vec![])
        );
    }

    #[test]
    fn a_guest_has_nothing_to_write() {
        let mut favor = FavorLog::anonymous();

        assert!(!favor.flushable());
        favor.set(9, true);
        assert!(!favor.flushable());
        assert_eq!(favor.pending(), 1);
    }

    #[test]
    fn a_logged_in_session_with_no_toggles_has_nothing_to_write() {
        assert!(!starred(&[1, 2]).flushable());
    }

    /// Opt-in check of the two statements this module issues, against the live database:
    /// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
    ///
    /// As in [`crate::viewlog`]'s equivalent, the uid is above every account the site has and
    /// the rows are deleted again before the test returns; `article_favorite` has no foreign
    /// key, so an id this low is simply a star nobody owns until the cleanup runs.
    #[tokio::test]
    #[ignore = "needs the live board database and LBBS_REF_CONF"]
    async fn live_flush_writes_both_directions() {
        let path = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
        let text = std::fs::read_to_string(path).unwrap();
        let (config, _warnings) = crate::config::parse(&text);
        let db = Db::connect(&config.db).await.unwrap();

        let max_uid: i32 = sqlx::query("SELECT COALESCE(MAX(UID), 0) FROM user_pubinfo")
            .fetch_one(db.pool())
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        let uid = max_uid + 1;

        let mut favor = FavorLog::from_base(uid, Vec::new());
        // More than one batch of stars, which is the only way to see the INSERT split.
        for aid in 1..=120 {
            assert_eq!(favor.set(aid, true), Marked::Pending);
        }
        assert_eq!(favor.save_inc(&db).await.unwrap(), (120, 0));

        let loaded = FavorLog::load(&db, uid).await.unwrap();
        assert_eq!(loaded.len(), 120);
        assert!(loaded.is_favor(1));
        assert!(loaded.is_favor(120));

        // The same flush with the increment left in place inserts nothing twice.
        assert_eq!(favor.save_inc(&db).await.unwrap(), (0, 0));

        // Unstarring saved threads is the other half of the walk: aids the two lists share
        // go to the DELETE, and the log line reports the split.
        favor = loaded;
        assert_eq!(favor.set(7, false), Marked::Pending);
        assert_eq!(favor.set(11, false), Marked::Pending);
        assert_eq!(favor.save_inc(&db).await.unwrap(), (0, 2));

        let reloaded = FavorLog::load(&db, uid).await.unwrap();
        assert_eq!(reloaded.len(), 118);
        assert!(!reloaded.is_favor(7));
        assert!(reloaded.is_favor(8));

        // A merge then folds a toggle into the saved set without a write, which is what the
        // overflow path in `mark` relies on.
        favor = reloaded;
        assert_eq!(favor.set(9, false), Marked::Pending);
        favor.merge_inc();
        assert_eq!(favor.pending(), 0);
        assert!(!favor.is_favor(9));
        assert_eq!(favor.len(), 117);

        sqlx::query("DELETE FROM article_favorite WHERE UID = ?")
            .bind(uid)
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(FavorLog::load(&db, uid).await.unwrap().len(), 0);
    }
}
