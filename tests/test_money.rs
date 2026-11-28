use super::*;

#[tokio::test]
async fn an_amount_that_is_not_positive_never_reaches_the_database() {
    // `:36-39` and `:126-129`: the return is before `db_open()`, so even the detached
    // database of `offline_session` is never asked, and the cached balance stays put.
    let session = crate::session::offline_session();
    let mut wallet = Wallet { balance: 40 };
    assert_eq!(money_deposit(&session, &mut wallet, 0).await, 0);
    assert_eq!(money_deposit(&session, &mut wallet, -5).await, 0);
    assert_eq!(money_withdraw(&session, &mut wallet, 0).await, 0);
    assert_eq!(money_withdraw(&session, &mut wallet, -5).await, 0);
    assert_eq!(wallet.balance(), 40);
}

#[test]
fn depositing_toward_the_cap_credits_only_what_is_left() {
    assert_eq!(credit(10, 5), Some((15, 5)));
    assert_eq!(
        credit(USER_MONEY_MAX - 3, 10),
        Some((USER_MONEY_MAX, 3)),
        "the clamp of `:85-89`"
    );
    assert_eq!(credit(USER_MONEY_MAX, 10), None, "the -2 of `:90-94`");
    assert_eq!(credit(USER_MONEY_MAX + 1, 10), None, "the same -2");
}

#[test]
fn withdrawing_takes_the_whole_sum_or_nothing() {
    assert_eq!(debit(50, 20), Some((30, 20)));
    assert_eq!(debit(19, 20), None, "the -2 of `:175-179`");
    assert_eq!(debit(0, 0), Some((0, 0)), "the `>=` of `:170` is not a `>`");
}

#[tokio::test]
async fn money_refresh_reports_a_database_it_cannot_reach() {
    // `Db::detached` fails the query instead of waiting for a server, which is C's `-1` of
    // `:210-214` and `:220-224` with nothing in the cache to keep.
    let session = crate::session::offline_session();
    let mut wallet = Wallet::default();
    assert_eq!(money_refresh(&session, &mut wallet).await, -1);
    assert_eq!(wallet.balance(), 0);
}

#[tokio::test]
async fn a_purchase_stops_at_the_transaction_when_the_database_is_gone() {
    let session = crate::session::offline_session();
    let mut wallet = Wallet { balance: 40 };
    // The `BEGIN` of `:54` is the first thing the two writers do, so `-1` arrives with the
    // cached balance still 40 and no `UPDATE` behind it.
    assert_eq!(money_deposit(&session, &mut wallet, 10).await, -1);
    assert_eq!(wallet.balance(), 40);
    assert_eq!(money_withdraw(&session, &mut wallet, 10).await, -1);
    assert_eq!(wallet.balance(), 40);
}
