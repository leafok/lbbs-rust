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
