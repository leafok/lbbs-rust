/* SPDX-License-Identifier: GPL-3.0-or-later */
//! 糖糖: the game money the chicken game buys food with, kept in `user_pubinfo.game_money`.
//!
//! Port of `utils/game/money.c` — `money_balance` (`:23-26`), `money_deposit` (`:28-116`),
//! `money_withdraw` (`:118-201`) and `money_refresh` (`:203-239`) — with `USER_MONEY_MAX`
//! from `utils/game/money.h:14`. The only caller in the reference tree is
//! `utils/game/chicken.c`, which refreshes at entry (`:62-65`), reads the balance onto its
//! status line (`:220`) and spends it on feed (`:534`), snacks (`:548`) and a rest (`:595`),
//! and sells a chicken into it (`:834`).
//!
//! # Deviations from C
//!
//! * `BBS_user_money` (`:21`) is a process global. The port hands the number to the caller as a
//!   [`Wallet`], which one game holds between its refresh and its last purchase: the C server is
//!   one process per session, so a global there is a per-session field here, and the two servers
//!   share the column, not the cache.
//! * C opens its own connection (`db_open`) for every one of the four statements pairs and
//!   `mysql_close`s it at the end of the success path only — the error returns of `:48-51`,
//!   `:54-58`, `:64-68`, `:69-73`, `:100-104` and `:107-111` hand the handle back to the caller
//!   leaked, and the ones after `BEGIN` leave the transaction open on it. The pooled connection
//!   has no handle to leak, and a [`sqlx::Transaction`] rolled back on drop is the implicit
//!   rollback `mysql_close` performs.
//! * `SET autocommit = 0` (`:48`) is what `sqlx`'s `BEGIN` does for the session it opens, so the
//!   port has no statement for it; the `BEGIN`/`COMMIT` pair of `:54` and `:107` is
//!   [`crate::db::Db::begin`] and `tx.commit()`.
//! * C cannot tell a result set it failed to store from a query that returned no row: both
//!   `Get user money failed` (`:69-73`, `:159-163`, `:225-229`) and the missing-row case of
//!   `:74`, `:164` and `:230` are one `Option` here. A query error is the `-1` of both, and an
//!   absent row leaves the cached balance alone exactly as C's `atoi(row[0])` left the global.
//! * `atoi(row[0])` reads a NULL column as a crash. [`crate::db::lock_game_money`] reports it as
//!   the query error it would have been, since `game_money` is `NOT NULL` in the schema.

use crate::db;
use crate::session::Session;

/// `USER_MONEY_MAX` (`money.h:14`), one billion.
pub const USER_MONEY_MAX: i32 = 1_000_000_000;

/// `BBS_user_money` (`:21`), the balance as this game last read it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wallet {
    balance: i32,
}

impl Wallet {
    /// [`money_balance`] (`:23-26`): the number the status line prints, not the number in the
    /// column.
    pub fn balance(&self) -> i32 {
        self.balance
    }

    /// A wallet holding `balance`, for the pages that print one with no database to fill it from.
    #[cfg(test)]
    pub(crate) fn with_balance(balance: i32) -> Self {
        Self { balance }
    }
}

/// `money_refresh` (`:203-239`): the one money call that runs outside a transaction, and the one
/// `chicken_main` (`:62-65`) refuses to start the game without.
pub async fn money_refresh(session: &Session, wallet: &mut Wallet) -> i32 {
    match session.state.db.game_money(session.privs.uid).await {
        Ok(Some(money)) => wallet.balance = money,
        // `:230-233`: no row is not an error, and the global keeps whatever it was.
        Ok(None) => {}
        Err(err) => {
            tracing::error!("Query user_pubinfo error: {err}");
            return -1;
        }
    }
    0
}

/// `money_deposit` (`:28-116`). Returns the amount actually put in the account, which is less
/// than `money` once the balance sits under `USER_MONEY_MAX`, `0` for an amount that is not
/// positive, `-1` for a database that could not be reached or asked, and `-2` for an account
/// already at or over the billion.
pub async fn money_deposit(session: &Session, wallet: &mut Wallet, money: i32) -> i32 {
    if money <= 0 {
        // `:36-39`: before `db_open`, so before any SQL.
        return 0;
    }
    let uid = session.privs.uid;
    let mut tx = match session.state.db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!("Begin transaction error: {err}");
            return -1;
        }
    };

    let read = match db::lock_game_money(&mut tx, uid).await {
        Ok(Some(money)) => Some(money),
        Ok(None) => None,
        Err(err) => {
            tracing::error!("Query user_pubinfo error: {err}");
            return -1;
        }
    };
    // `:74-77`: `BBS_user_money` is only overwritten when a row came back.
    let balance = read.unwrap_or(wallet.balance);

    let Some((next, credited)) = credit(balance, money) else {
        // `:90-94`: nothing is written, and the transaction C never started is never committed.
        return -2;
    };

    if let Err(err) = db::update_game_money(&mut tx, uid, next).await {
        tracing::error!("Update user_pubinfo error: {err}");
        return -1;
    }
    if let Err(err) = tx.commit().await {
        tracing::error!("Commit transaction error: {err}");
        return -1;
    }

    wallet.balance = next;
    credited
}

/// `money_withdraw` (`:118-201`). Returns the amount taken out — the whole sum or nothing —
/// `0` for an amount that is not positive, `-1` for a database error, and `-2` for a balance
/// that will not cover it.
pub async fn money_withdraw(session: &Session, wallet: &mut Wallet, money: i32) -> i32 {
    if money <= 0 {
        return 0;
    }
    let uid = session.privs.uid;
    let mut tx = match session.state.db.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!("Begin transaction error: {err}");
            return -1;
        }
    };

    let read = match db::lock_game_money(&mut tx, uid).await {
        Ok(Some(money)) => Some(money),
        Ok(None) => None,
        Err(err) => {
            tracing::error!("Query user_pubinfo error: {err}");
            return -1;
        }
    };
    let balance = read.unwrap_or(wallet.balance);

    let Some((next, taken)) = debit(balance, money) else {
        // `:175-179`: `No enough balance`, no `UPDATE`.
        return -2;
    };

    if let Err(err) = db::update_game_money(&mut tx, uid, next).await {
        tracing::error!("Update user_pubinfo error: {err}");
        return -1;
    }
    if let Err(err) = tx.commit().await {
        tracing::error!("Commit transaction error: {err}");
        return -1;
    }

    wallet.balance = next;
    taken
}

/// The three branches of `:80-94`. `None` is the `-2` of `:90-94`: an account already at the
/// cap, which C reports before it writes anything.
fn credit(balance: i32, money: i32) -> Option<(i32, i32)> {
    if balance + money < USER_MONEY_MAX {
        Some((balance + money, money))
    } else if balance < USER_MONEY_MAX {
        Some((USER_MONEY_MAX, USER_MONEY_MAX - balance))
    } else {
        None
    }
}

/// The test of `:170-179`. `None` is the `-2` of `:175-179`.
fn debit(balance: i32, money: i32) -> Option<(i32, i32)> {
    if balance >= money {
        Some((balance - money, money))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
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
}
