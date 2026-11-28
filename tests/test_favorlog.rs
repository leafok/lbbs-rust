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
