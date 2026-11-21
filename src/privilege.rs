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
//! * `s_priv_list` is a `Vec` capped at [`paths::MAX_SECTION`] instead of a fixed array.
//!   The array in C is a zero-initialized global, so a search in an empty list compares
//!   against `0`; [`UserPriv::search`] reproduces that by treating a missing entry as
//!   `sid == 0`.
//! * `load` takes the pool, not a live `MYSQL*`, and each statement autocommits. C's
//!   `load_priv` returns `1` when the connection is `NULL` and `-1` on a query error; the
//!   port returns the [`sqlx::Error`] instead, since every caller only tested `!= 0`.
//! * `load` builds a fresh record instead of resetting the fields of a reused global; C now
//!   zeroes `s_priv_list` at the top of `load_priv`, so both start from the same place.
//!
//! # Outstanding divergence
//!
//! [`UserPriv::search`] is still the search C had before the fix (`while (left < right)`,
//! no `found` flag), so the port keeps the defects C has since removed: the list goes out
//! of order as soon as boards arrive out of order, an entry can then stop being findable,
//! and the slot the search lands in is read even when nothing matched. C's fixed search
//! inserts in place and reports a real hit, so a section master or a board mask reaches the
//! board-specific word where the port still reads the global one. Porting it would change
//! which privileges are reachable, so it is left open deliberately — see
//! `tests::an_insert_that_lands_wrong_hides_a_board`, which pins the pre-fix result.
//! [`Full`] and the dropping it counts are ported from the current C.

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
    /// `s_priv_list`, kept in the order C's insert left it in.
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
    /// The starting point of `load_priv` (`:121-124`), before any query has run.
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

    /// `search_priv` (`:30-58`): the binary search, left exactly as written, including the
    /// way it can land on the wrong slot. The list is therefore not always sorted after an
    /// insert, and a later lookup can miss an entry the same way it does in C.
    fn search(&self, sid: i16) -> (usize, bool) {
        // `left` and `right` are signed because C's are, and an empty list starts with
        // `right == -1`, which is what keeps its loop from running.
        let mut left: i64 = 0;
        let mut right = self.sections.len() as i64 - 1;

        while left < right {
            let mid = (left + right) / 2;
            let entry = self.sections[mid as usize].sid;
            if sid < entry {
                right = mid - 1;
            } else if sid > entry {
                left = mid + 1;
            } else {
                left = mid;
                break;
            }
        }

        let offset = left.max(0) as usize;
        // C tests `sid == s_priv_list[left].sid` even when the list is empty, reading the
        // zeroed global, where only `sid == 0` can match.
        let found = self.sections.get(offset).map(|entry| entry.sid).unwrap_or(0) == sid;

        (offset, found)
    }

    /// `setpriv` (`:60-97`). `sid == 0` is the global word, not a list entry.
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

    /// `getpriv` (`:99-111`): the stored word, or the global one for an unknown positive
    /// `sid`, or `S_NONE` for a negative one.
    pub fn getpriv(&self, sid: i16) -> (i32, bool) {
        let (offset, found) = self.search(sid);
        if found {
            // An empty list reports the zeroed slot C would have read.
            return match self.sections.get(offset) {
                Some(entry) => (entry.s_priv, entry.is_favor),
                None => (S_NONE, false),
            };
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

    /// The board privileges a session may exercise, or `None` when the search did not land
    /// on the board — which, given the search above, is not the same as "not in the list".
    pub fn section(&self, sid: i16) -> Option<SectionPriv> {
        let (offset, found) = self.search(sid);
        if found { self.sections.get(offset).copied() } else { None }
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

/// `setpriv` had nowhere left to put the entry (`:80-83`); [`Dropped`] is what reports it
/// instead of leaving the refusal silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// `load_priv` (`user_priv.c:113-280`), one query per step in the original order — the
/// order matters, because the insertion points depend on it.
pub async fn load(db: &Db, uid: i32) -> Result<UserPriv, sqlx::Error> {
    let mut privs = UserPriv::new(uid);

    // Permission (`:130`): the two global bits come only from a verified account.
    if let Some(flags) = db.user_flags(uid).await? {
        if flags.p_post {
            privs.g_priv |= S_POST;
        }
        if flags.p_msg {
            privs.g_priv |= S_MSG;
        }
    }

    // Admin (`:151`).
    if let Some(major) = db.admin_major(uid).await? {
        privs.level |= if major { P_ADMIN_M } else { P_ADMIN_S };
        privs.g_priv |= if major { S_ALL } else { S_ADMIN };
    }

    // Section master (`:173`).
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

    // Section status (`:198`), a per-board pass over every enabled board.
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

    // Section ban (`:233`).
    let mut dropped = Dropped::default();
    for sid in db.banned_sids(uid).await? {
        let (s_priv, is_favor) = privs.getpriv(sid);
        if privs.setpriv(sid, s_priv & !S_POST, is_favor).is_err() {
            dropped.add(sid);
        }
    }
    dropped.report(uid, "section ban");

    // Favourites (`:255`): the flag lives on the list entry, and reaching a board through
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
    }

    #[test]
    fn a_full_list_drops_the_insert_instead_of_failing() {
        let mut privs = UserPriv::new(7);
        for sid in 1..=paths::MAX_SECTION as i16 {
            privs.setpriv(sid, S_LIST, false).unwrap();
        }
        assert_eq!(privs.sections.len(), paths::MAX_SECTION);
        assert!(privs.setpriv(9999, S_LIST, false).is_err());
        assert_eq!(privs.sections.len(), paths::MAX_SECTION);
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
        assert!(!UserPriv::new(0).checklevel2(P_USER), "a guest is below P_USER");
    }

    #[test]
    fn an_insert_that_lands_wrong_hides_a_board() {
        // The C search assumes a sorted list and inserts where it last looked, so the list
        // goes out of order as soon as boards arrive out of order — and an entry can then
        // stop being findable. Here 10 is stored but never found again. A port that
        // "fixed" the search would report different privileges.
        let mut privs = UserPriv::new(7);
        for (sid, word) in [(10, S_POST), (20, S_GETEXP), (7, S_MSG)] {
            privs.setpriv(sid, word, false).unwrap();
        }
        privs.setpriv(5, S_LIST, false).unwrap();
        let sids: Vec<i16> = privs.section_privs().iter().map(|e| e.sid).collect();
        assert_eq!(sids, vec![5, 7, 20, 10]);
        assert_eq!(privs.getpriv(10), (privs.g_priv, false));
        assert_eq!(privs.section(10), None);
        assert_eq!(privs.getpriv(5), (S_LIST, false));
        assert_eq!(privs.getpriv(7), (S_MSG, false));
        assert_eq!(privs.getpriv(20), (S_GETEXP, false));
    }

    #[test]
    fn favourites_live_on_the_board_entry() {
        let mut privs = UserPriv::new(7);
        assert!(!privs.is_favor(4));
        let (s_priv, _) = privs.getpriv(4);
        privs.setpriv(4, s_priv, true).unwrap();
        assert!(privs.is_favor(4));
        assert!(privs.checkpriv(4, S_DEFAULT));
    }
}
