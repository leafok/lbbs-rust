//! User privileges, ported from `src/user_priv.c` and `include/user_priv.h`.
//!
//! One record per session answers two questions: *what may this user do at all*
//! ([`UserPriv::g_priv`], the global word) and *what may they do in board N*
//! ([`UserPriv::getpriv`]). A third number, [`UserPriv::level`], is a rank used by the
//! menu layer and by the SSH-only staff rule in [`crate::login`].
//!
//! The two level predicates are deliberately different, as in C: [`UserPriv::checklevel`]
//! is a mask test and [`UserPriv::checklevel2`] is a numeric `>=` test on the same value.
//! Because the ranks are powers of two that get OR-ed together, `>=` behaves like "at
//! least this senior", which is what the staff gate relies on.
//!
//! # Deviations from C
//!
//! * `s_priv_list` is a `Vec` capped at [`paths::MAX_SECTION`] instead of a fixed array;
//!   C's array is a zero-initialized global whose unused slots still hold `0`, where the
//!   port has no entry at all. `UserPriv::search` answers with a hit flag, as C does, so
//!   neither one reads a slot that holds nothing.
//! * `load` takes the pool, not a live `MYSQL*`, and each statement autocommits. C's
//!   `load_priv` returns `1` when the connection is `NULL` and `-1` on a query error; the
//!   port returns the [`sqlx::Error`] instead, since every caller only tested `!= 0`.
//! * `load` builds a fresh record instead of resetting the fields of a reused global; C
//!   zeroes `s_priv_list` at the top of `load_priv` (`:157`), so both start from the same
//!   place.

use crate::db::Db;
use crate::paths;

/// `enum user_priv_t` — the exact values, including the unused `0x10` gap and the `0x80`
/// bit that `S_ADMIN` and `S_ALL` assume but no query ever sets.
pub const S_NONE: i32 = 0x0;
pub const S_LIST: i32 = 0x1;
pub const S_GETEXP: i32 = 0x2;
pub const S_POST: i32 = 0x4;
pub const S_MSG: i32 = 0x8;
pub const S_MAN_S: i32 = 0x20;
/// `0x40 | 0x20`
pub const S_MAN_M: i32 = 0x60;
/// `0x80 | 0x40 | 0x20`
pub const S_ADMIN: i32 = 0xe0;
pub const S_ALL: i32 = 0xff;
/// `S_LIST | S_GETEXP`, the baseline every logged-in user gets.
pub const S_DEFAULT: i32 = 0x3;

/// `enum user_level_t` — `P_AUTH_USER` and `P_MAN_C` are reserved and unused.
pub const P_GUEST: i32 = 0x0;
pub const P_USER: i32 = 0x1;
pub const P_AUTH_USER: i32 = 0x2;
pub const P_MAN_S: i32 = 0x4;
pub const P_MAN_M: i32 = 0x8;
pub const P_MAN_C: i32 = 0x10;
pub const P_ADMIN_S: i32 = 0x20;
pub const P_ADMIN_M: i32 = 0x40;

/// One row of `s_priv_list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionPriv {
    pub sid: i16,
    pub s_priv: i32,
    pub is_favor: bool,
}

/// `BBS_user_priv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPriv {
    pub uid: i32,
    pub level: i32,
    pub g_priv: i32,
    /// `s_priv_list`, in `sid` order — the order the fixed insert maintains, not the order
    /// the boards arrived in.
    sections: Vec<SectionPriv>,
}

impl Default for UserPriv {
    /// The zeroed global, which is what a session sees before a login has filled it in.
    fn default() -> Self {
        Self {
            uid: 0,
            level: 0,
            g_priv: 0,
            sections: Vec::new(),
        }
    }
}

impl UserPriv {
    /// The starting point of `load_priv` (`:152-155`), before any query has run.
    pub fn new(uid: i32) -> Self {
        Self {
            uid,
            level: if uid == 0 { P_GUEST } else { P_USER },
            g_priv: S_DEFAULT,
            sections: Vec::new(),
        }
    }

    pub fn section_privs(&self) -> &[SectionPriv] {
        &self.sections
    }

    /// `search_priv` (`:31-61`): the binary search, with C's `found` flag. An insert at the
    /// offset it returns therefore keeps the list sorted, which is what makes every stored
    /// board findable again — the flag also means a miss is a miss, not the slot the search
    /// stopped beside.
    fn search(&self, sid: i16) -> (usize, bool) {
        // `left` and `right` are signed because C's are, and an empty list starts with
        // `right == -1`, which is what keeps its loop from running.
        let mut left: i64 = 0;
        let mut right = self.sections.len() as i64 - 1;
        let mut found = false;

        while left <= right {
            let mid = (left + right) / 2;
            let entry = self.sections[mid as usize].sid;
            if sid < entry {
                right = mid - 1;
            } else if sid > entry {
                left = mid + 1;
            } else {
                found = true;
                left = mid;
                break;
            }
        }

        // `left` is the insertion point when nothing matched, and the match when something
        // did; either way it stays inside `0..=len`.
        (left as usize, found)
    }

    /// `setpriv` (`:63-101`). `sid == 0` is the global word, not a list entry.
    pub fn setpriv(&mut self, sid: i16, s_priv: i32, is_favor: bool) -> Result<(), Full> {
        if sid == 0 {
            self.g_priv = s_priv;
            return Ok(());
        }

        let (offset, found) = self.search(sid);
        if let Some(entry) = self.sections.get_mut(offset).filter(|_| found) {
            entry.s_priv = s_priv;
            entry.is_favor = is_favor;
            return Ok(());
        }

        if self.sections.len() >= paths::MAX_SECTION {
            return Err(Full);
        }

        self.sections.insert(
            offset,
            SectionPriv {
                sid,
                s_priv,
                is_favor,
            },
        );

        Ok(())
    }

    /// `getpriv` (`:103-115`): the stored word, or the global one for an unknown board, or
    /// `S_NONE` for a negative `sid`. `sid == 0` is never a list entry, so it always reads
    /// the global word — which is how `checkpriv(&BBS_priv, 0, priv)` works in the menu.
    pub fn getpriv(&self, sid: i16) -> (i32, bool) {
        let (offset, found) = self.search(sid);
        if let Some(entry) = self.sections.get(offset).filter(|_| found) {
            return (entry.s_priv, entry.is_favor);
        }

        (if sid >= 0 { self.g_priv } else { S_NONE }, false)
    }

    /// `checkpriv` (`user_priv.h:81-85`): every bit asked for must be present.
    pub fn checkpriv(&self, sid: i16, priv_bits: i32) -> bool {
        (self.getpriv(sid).0 & priv_bits) == priv_bits
    }

    /// The global word, the form the menu layer uses (`checkpriv(&BBS_priv, 0, priv)`).
    pub fn check_global(&self, priv_bits: i32) -> bool {
        (self.g_priv & priv_bits) == priv_bits
    }

    /// `checklevel` (`user_priv.h:61-69`): a mask test, with `P_GUEST` meaning "anyone".
    pub fn checklevel(&self, level: i32) -> bool {
        if level == P_GUEST {
            return true;
        }
        self.level & level != 0
    }

    /// `checklevel2` (`user_priv.h:72-75`): a numeric comparison, which is how the
    /// per-board `read_user_level` / `write_user_level` gates and the staff-only SSH rule
    /// are evaluated.
    pub fn checklevel2(&self, level: i32) -> bool {
        self.level >= level
    }

    /// `is_favor` (`user_priv.h:87-92`).
    pub fn is_favor(&self, sid: i16) -> bool {
        self.getpriv(sid).1
    }

    /// The board privileges a session may exercise, or `None` when the board is not in the
    /// list, in which case [`UserPriv::getpriv`] reports the global word instead.
    pub fn section(&self, sid: i16) -> Option<SectionPriv> {
        let (offset, found) = self.search(sid);
        if found {
            self.sections.get(offset).copied()
        } else {
            None
        }
    }
}

/// Bookkeeping for `setpriv` refusals: the list holds [`paths::MAX_SECTION`] boards, and
/// each pass of [`load`] keeps going with the privileges it already gathered. A refused
/// entry reads back as the global word, so a dropped board ban or read restriction quietly
/// loosens access — hence the first refusal and the total are logged per pass.
#[derive(Default)]
struct Dropped {
    count: usize,
    first_sid: i16,
}

impl Dropped {
    fn add(&mut self, sid: i16) {
        if self.count == 0 {
            self.first_sid = sid;
        }
        self.count += 1;
    }

    fn report(&self, uid: i32, stage: &str) {
        if self.count > 0 {
            tracing::error!(
                "Drop privilege of {} section(s) of user {} from SID {} at {} stage: \
                 s_priv_list limit {} reached",
                self.count,
                uid,
                self.first_sid,
                stage,
                paths::MAX_SECTION
            );
        }
    }
}

/// `setpriv` had nowhere left to put the entry (`:81-84`); `Dropped` is what reports it
/// instead of leaving the refusal silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// `load_priv` (`user_priv.c:141-341`), one query per step in the original order — the
/// order matters because each pass reads the words the previous one left behind.
pub async fn load(db: &Db, uid: i32) -> Result<UserPriv, sqlx::Error> {
    let mut privs = UserPriv::new(uid);

    // Permission (`:162`): the two global bits come only from a verified account.
    if let Some(flags) = db.user_flags(uid).await? {
        if flags.p_post {
            privs.g_priv |= S_POST;
        }
        if flags.p_msg {
            privs.g_priv |= S_MSG;
        }
    }

    // Admin (`:183`).
    if let Some(major) = db.admin_major(uid).await? {
        privs.level |= if major { P_ADMIN_M } else { P_ADMIN_S };
        privs.g_priv |= if major { S_ALL } else { S_ADMIN };
    }

    // Section master (`:205`).
    let mut dropped = Dropped::default();
    for row in db.masterships(uid).await? {
        let sid = row.sid;
        let major = row.major;
        privs.level |= if major { P_MAN_M } else { P_MAN_S };
        let (current, is_favor) = privs.getpriv(sid);
        let s_priv = current | if major { S_MAN_M } else { S_MAN_S };
        if privs.setpriv(sid, s_priv, is_favor).is_err() {
            dropped.add(sid);
        }
    }
    dropped.report(uid, "section master");

    // Section status (`:237`), a per-board pass over every enabled board.
    let mut dropped = Dropped::default();
    for row in db.section_levels().await? {
        let sid = row.sid;
        let (mut s_priv, is_favor) = privs.getpriv(sid);
        if privs.level < row.read_user_level as i32 {
            s_priv &= !S_LIST;
        }
        if privs.level < row.write_user_level as i32 {
            s_priv &= !S_POST;
        }
        if !row.exp_get {
            s_priv &= !S_GETEXP;
        }
        if privs.setpriv(sid, s_priv, is_favor).is_err() {
            dropped.add(sid);
        }
    }
    dropped.report(uid, "section status");

    // Section ban (`:279`).
    let mut dropped = Dropped::default();
    for sid in db.banned_sids(uid).await? {
        let (s_priv, is_favor) = privs.getpriv(sid);
        if privs.setpriv(sid, s_priv & !S_POST, is_favor).is_err() {
            dropped.add(sid);
        }
    }
    dropped.report(uid, "section ban");

    // Favourites (`:308`): the flag lives on the list entry, and reaching a board through
    // the global word first is what materialises the entry.
    let mut dropped = Dropped::default();
    for sid in db.favorite_sids(uid).await? {
        let (s_priv, is_favor) = privs.getpriv(sid);
        if !is_favor && privs.setpriv(sid, s_priv, true).is_err() {
            dropped.add(sid);
        }
    }
    dropped.report(uid, "section favorite");

    Ok(privs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_record_knows_only_its_rank() {
        assert_eq!(UserPriv::new(0).level, P_GUEST);
        assert_eq!(UserPriv::new(7).level, P_USER);
        assert_eq!(UserPriv::new(7).g_priv, S_DEFAULT);
        // The zeroed global of a session that has not logged in yet.
        assert_eq!(UserPriv::default().g_priv, 0);
    }

    #[test]
    fn an_unknown_board_falls_back_to_the_global_word() {
        let privs = UserPriv::new(7);
        assert_eq!(privs.getpriv(42), (S_DEFAULT, false));
        // A negative board is not the global one.
        assert_eq!(privs.getpriv(-1), (S_NONE, false));
        // `sid == 0` never enters the list, so `checkpriv(&BBS_priv, 0, priv)` in the menu
        // reads the global word even for a session with boards recorded.
        let mut privs = UserPriv::new(7);
        privs.setpriv(4, S_POST, false).unwrap();
        assert_eq!(privs.getpriv(0), (S_DEFAULT, false));
        // A miss reports the global word, not the word of the board beside it.
        assert_eq!(privs.getpriv(3), (S_DEFAULT, false));
        assert_eq!(privs.getpriv(5), (S_DEFAULT, false));
    }

    #[test]
    fn board_privileges_replace_the_inherited_ones() {
        let mut privs = UserPriv::new(7);
        privs.setpriv(4, S_LIST, false).unwrap();
        assert!(privs.checkpriv(4, S_LIST));
        // The board entry replaces the global word instead of adding to it.
        assert!(!privs.checkpriv(4, S_GETEXP));
        // Elsewhere the global word still applies, and `S_DEFAULT` never allowed posting.
        assert!(privs.checkpriv(5, S_LIST));
        assert!(!privs.checkpriv(5, S_POST));
        privs.g_priv |= S_POST;
        assert!(privs.checkpriv(5, S_POST));
        assert!(!privs.checkpriv(4, S_POST), "board 4 keeps its own word");
    }

    #[test]
    fn the_zero_board_is_the_global_word() {
        let mut privs = UserPriv::new(7);
        privs.setpriv(0, S_ALL, false).unwrap();
        assert_eq!(privs.g_priv, S_ALL);
        assert_eq!(privs.section_privs().len(), 0);
        // What a rewritten global word means for the boards that were never recorded: the new
        // word, and everything inside it (`test_user_priv.c:118-123`).
        assert_eq!(privs.getpriv(7), (S_ALL, false));
        assert!(privs.checkpriv(7, S_ADMIN));
    }

    #[test]
    fn a_full_list_drops_the_insert_instead_of_failing() {
        // Filled from the top down, so every placement is the search's to find
        // (`test_user_priv.c:284-291`).
        let mut privs = UserPriv::new(7);
        for sid in (1..=paths::MAX_SECTION as i16).rev() {
            privs.setpriv(sid, S_LIST, false).unwrap();
        }
        assert_eq!(privs.sections.len(), paths::MAX_SECTION);
        assert!(privs.setpriv(9999, S_LIST, false).is_err());
        assert_eq!(privs.sections.len(), paths::MAX_SECTION);
        // A refusal disturbs nothing already stored, and the refused board reads as the global
        // word (`test_user_priv.c:293-299`).
        assert_eq!(privs.getpriv(1), (S_LIST, false));
        assert_eq!(privs.getpriv(paths::MAX_SECTION as i16), (S_LIST, false));
        assert_eq!(privs.getpriv(9999), (S_DEFAULT, false));
        // The global word is never capacity-limited (`test_user_priv.c:301-304`).
        privs.setpriv(0, S_ALL, false).unwrap();
        assert_eq!(privs.getpriv(9999), (S_ALL, false));
    }

    #[test]
    fn a_second_entry_for_the_same_board_replaces_the_first() {
        // A repeated `setpriv` must not grow the list: two slots for one board would put a
        // duplicate in the middle of the range the binary search walks
        // (`test_user_priv.c:157-162`).
        let mut privs = UserPriv::new(7);
        privs.setpriv(5, S_LIST, false).unwrap();
        privs.setpriv(5, S_POST, false).unwrap();
        assert_eq!(privs.section_privs().len(), 1);
        assert_eq!(privs.getpriv(5), (S_POST, false));
    }

    #[test]
    fn the_two_level_predicates_disagree_on_purpose() {
        let mut privs = UserPriv::new(7);
        privs.level = P_USER | P_MAN_S; // 0x5
        // A mask test sees the deputy-master bit; a numeric test sees the rank.
        assert!(privs.checklevel(P_MAN_S));
        assert!(privs.checklevel2(P_MAN_S));
        assert!(!privs.checklevel(P_MAN_M));
        assert!(!privs.checklevel2(P_MAN_M));
        // They disagree above the mask's own bit: `P_ADMIN_S` (0x20) holds none of
        // `P_MAN_M` (0x8) and still counts as greater than it.
        privs.level = P_ADMIN_S;
        assert!(!privs.checklevel(P_MAN_M));
        assert!(privs.checklevel2(P_MAN_M));
        // `P_GUEST` is "no requirement", not "bit 0".
        assert!(privs.checklevel(P_GUEST));
        assert!(UserPriv::new(0).checklevel(P_GUEST));
        assert!(
            !UserPriv::new(0).checklevel2(P_USER),
            "a guest is below P_USER"
        );
    }

    #[test]
    fn boards_that_arrive_out_of_order_stay_reachable() {
        // This is what the fixed search is for: an insert lands in sort order no matter when
        // the board turns up, so the word written for board 10 survives the boards that come
        // after it. Before the fix the list ended up `[5, 7, 20, 10]` and board 10 read back
        // as the global word.
        let mut privs = UserPriv::new(7);
        for (sid, word) in [(10, S_POST), (20, S_GETEXP), (7, S_MSG)] {
            privs.setpriv(sid, word, false).unwrap();
        }
        privs.setpriv(5, S_LIST, false).unwrap();
        let sids: Vec<i16> = privs.section_privs().iter().map(|e| e.sid).collect();
        assert_eq!(sids, vec![5, 7, 10, 20]);
        assert_eq!(privs.getpriv(10), (S_POST, false));
        assert_eq!(
            privs.section(10),
            Some(SectionPriv {
                sid: 10,
                s_priv: S_POST,
                is_favor: false
            })
        );
        assert_eq!(privs.getpriv(5), (S_LIST, false));
        assert_eq!(privs.getpriv(7), (S_MSG, false));
        assert_eq!(privs.getpriv(20), (S_GETEXP, false));
    }

    #[test]
    fn every_insertion_order_leaves_the_list_sorted_and_findable() {
        // The section-master query has no `ORDER BY`, so a session can gain boards in any
        // order; the status pass then rewrites each of them. Sweep several orders over the
        // whole list, forwards and backwards as C's `test_insert_orders` does
        // (`test_user_priv.c:180-216`), and check the invariant both passes lean on afterwards.
        for step in [1i32, 3, 7, 11, 13, 37, 61, 99] {
            for backwards in [false, true] {
                let mut privs = UserPriv::new(7);
                for i in 0..paths::MAX_SECTION as i32 {
                    let i = if backwards {
                        paths::MAX_SECTION as i32 - 1 - i
                    } else {
                        i
                    };
                    let sid = (i * step % paths::MAX_SECTION as i32 + 1) as i16;
                    privs
                        .setpriv(sid, S_LIST | (i % 8) as i32, false)
                        .expect("one board per sid, so nothing is refused");
                }

                let mut last = 0;
                for entry in privs.section_privs() {
                    assert!(
                        entry.sid > last,
                        "step {step} backwards={backwards}: list not sorted at {}",
                        entry.sid
                    );
                    last = entry.sid;
                }

                for i in 0..paths::MAX_SECTION as i32 {
                    let sid = (i * step % paths::MAX_SECTION as i32 + 1) as i16;
                    assert_eq!(
                        privs.getpriv(sid).0 & S_LIST,
                        S_LIST,
                        "step {step} backwards={backwards}: board {sid} became unreachable"
                    );
                }
            }
        }
    }

    #[test]
    fn the_master_and_status_passes_compose_in_the_load_order() {
        // The two passes `load` runs over a record, replayed without a database: the unordered
        // masterships first — where the words a board never had are OR-ed onto whatever
        // `getpriv` reported for it — then the status pass over every board in `ORDER BY SID`
        // (`user_priv.c:205-277`, the shape C pins at `test_user_priv.c:219-254`).
        let mut privs = UserPriv::new(7);
        for sid in [20i16, 10] {
            let (s_priv, is_favor) = privs.getpriv(sid);
            privs.setpriv(sid, s_priv | S_MAN_M, is_favor).unwrap();
        }
        assert_eq!(privs.getpriv(20), (S_MAN_M | S_DEFAULT, false));
        assert_eq!(privs.getpriv(10), (S_MAN_M | S_DEFAULT, false));

        for sid in 1i16..=30 {
            let (s_priv, is_favor) = privs.getpriv(sid);
            privs.setpriv(sid, s_priv & !S_LIST, is_favor).unwrap();
        }
        assert_eq!(privs.section_privs().len(), 30);
        // The masters keep their word, a plain board keeps only what survives the mask, and
        // the boards that were never recorded still answer with the global word.
        let masked = (S_MAN_M | S_DEFAULT) & !S_LIST;
        assert_eq!(privs.getpriv(20), (masked, false));
        assert_eq!(privs.getpriv(10), (masked, false));
        assert_eq!(privs.getpriv(25), (S_GETEXP, false));
        assert_eq!(privs.getpriv(31), (S_DEFAULT, false));
        assert!(privs.checkpriv(20, S_MAN_M));
        assert!(!privs.checkpriv(25, S_LIST));
        assert!(privs.checkpriv(25, S_GETEXP));
    }

    #[test]
    fn favourites_live_on_the_board_entry() {
        let mut privs = UserPriv::new(7);
        assert!(!privs.is_favor(4));
        let (s_priv, _) = privs.getpriv(4);
        privs.setpriv(4, s_priv, true).unwrap();
        assert!(privs.is_favor(4));
        assert!(privs.checkpriv(4, S_DEFAULT));
        // The flag is carried by the entry, so a later word for the same board keeps it until
        // something writes the flag off again (`test_user_priv.c:267-272`).
        privs.setpriv(4, S_LIST | S_POST, true).unwrap();
        assert!(privs.is_favor(4));
        assert_eq!(privs.getpriv(4), (S_LIST | S_POST, true));
        privs.setpriv(4, S_LIST | S_POST, false).unwrap();
        assert!(!privs.is_favor(4));
    }
}
