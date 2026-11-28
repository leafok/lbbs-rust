use sqlx::Row;

use super::*;

fn viewed(aids: &[i32]) -> ViewLog {
    ViewLog::from_base(42, aids.to_vec())
}

#[test]
fn a_fresh_log_holds_nothing_and_reports_it() {
    let log = ViewLog::anonymous();

    assert!(log.is_empty());
    assert_eq!(log.len(), 0);
    assert_eq!(log.pending(), 0);
    assert!(!log.is_viewed(7));
}

#[test]
fn the_saved_set_answers_for_its_first_and_last_aid() {
    let log = viewed(&[3, 8, 15]);

    assert!(log.is_viewed(3));
    assert!(log.is_viewed(15));
    assert!(!log.is_viewed(4));
    assert!(!log.is_viewed(16));
}

#[test]
fn rows_arriving_out_of_order_stay_sorted_in_the_increment() {
    let mut log = viewed(&[10]);

    // The order a reader hits threads in: newest first, then backwards through the board.
    assert!(log.set_viewed(30));
    assert!(log.set_viewed(4));
    assert!(log.set_viewed(22));

    assert_eq!(log.pending_aids(), &[4, 22, 30]);
    assert_eq!(log.pending(), 3);
    assert!(log.is_viewed(4));
    assert_eq!(log.len(), 4);
}

#[test]
fn marking_a_seen_article_again_adds_no_record() {
    let mut log = viewed(&[10, 20]);

    assert!(!log.set_viewed(20));
    assert_eq!(log.pending(), 0);

    assert!(log.set_viewed(21));
    assert!(!log.set_viewed(21));
    assert_eq!(log.pending(), 1);
}

#[test]
fn merging_moves_the_increment_into_the_saved_set_in_order() {
    let mut log = viewed(&[1, 5, 9]);

    log.set_viewed(7);
    log.set_viewed(3);
    log.merge_inc();

    assert_eq!(log.base, vec![1, 3, 5, 7, 9]);
    assert_eq!(log.pending(), 0);
    assert!(log.is_viewed(3));
    assert_eq!(log.len(), 5);
}

#[test]
fn merging_an_aid_both_lists_hold_leaves_one_copy() {
    // The case C logs as a duplicate: the increment was flushed and then marked again.
    let mut log = viewed(&[1, 2, 3]);

    log.inc.push(2);
    log.merge_inc();

    assert_eq!(log.base, vec![1, 2, 3]);
}

#[test]
fn a_full_increment_reports_itself_before_the_next_mark() {
    let mut log = viewed(&[]);
    let last = MAX_VIEWED_AID_INC_CNT as i32;
    log.inc.extend(1..=last);

    assert!(log.is_full());
    assert_eq!(log.pending(), MAX_VIEWED_AID_INC_CNT);
    // What mark_viewed would flush, in ascending order.
    assert_eq!(log.pending_aids().first(), Some(&1));
    assert_eq!(log.pending_aids().last(), Some(&last));
}

#[test]
fn a_guest_has_nothing_to_write() {
    let mut log = ViewLog::anonymous();

    assert!(!log.flushable());
    log.set_viewed(9);
    // The uid guard, not the empty-buffer guard, is what holds a guest out of the table.
    assert!(!log.flushable());
    assert_eq!(log.pending(), 1);
}

#[test]
fn a_logged_in_session_with_no_new_reads_has_nothing_to_write() {
    assert!(!viewed(&[1, 2]).flushable());
}

/// Opt-in check of the two statements this module issues, against the live database:
/// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
///
/// This one writes, so it uses an uid above every account the site has and deletes those
/// rows again at the end. `view_article_log` has no foreign key on either column, and no
/// reader's own history is touched.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn live_flush_round_trips_the_increment() {
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

    let mut log = ViewLog::from_base(uid, Vec::new());
    // More than one batch, because the statement splitting is the part a unit test cannot
    // see — and aids this low all exist as articles.
    for aid in 1..=150 {
        assert!(log.set_viewed(aid));
    }
    assert_eq!(log.pending(), 150);

    assert_eq!(log.save_inc(&db).await.unwrap(), 150);

    let loaded = ViewLog::load(&db, uid).await.unwrap();
    assert_eq!(loaded.len(), 150);
    assert_eq!(loaded.pending(), 0);
    assert!(loaded.is_viewed(1));
    assert!(loaded.is_viewed(150));
    assert!(!loaded.is_viewed(151));

    // Flushing the same increment twice inserts nothing the second time, which is why
    // C's log line counts affected rows rather than marks.
    assert_eq!(log.save_inc(&db).await.unwrap(), 0);

    sqlx::query("DELETE FROM view_article_log WHERE UID = ?")
        .bind(uid)
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(ViewLog::load(&db, uid).await.unwrap().len(), 0);
}
