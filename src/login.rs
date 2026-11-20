//! Login and authentication, ported from `src/login.c`.
//!
//! The shape of the C module is kept: three prompts, a credential check in one
//! transaction, a user-data load that decides whether the account may be here at all, and
//! the online bookkeeping that follows. Everything the C code writes to the database is
//! written here too, in the same order, because the web application reads those tables.
//!
//! # Deviations from C
//!
//! * The transaction is sqlx's, so `SET autocommit=0; BEGIN; … COMMIT;` becomes
//!   [`crate::db::Db::begin`] plus an explicit `commit` or `rollback` on every path. C left
//!   the connection in whatever state it had reached and relied on `mysql_close` to undo
//!   it; a pooled connection must not be handed back with an open transaction.
//! * `SET autocommit=0` is issued per transaction by sqlx rather than on the connection, so
//!   the statements after `COMMIT` (`mark_login`, the online rows) are individually
//!   committed exactly as in the original.
//! * `NULL` columns crash the original: `load_user_info` (`login.c:439-448`) runs
//!   `atoi`/`strncpy` on `life`, `last_login_dt`, `user_timezone`, `exp` and `nickname`
//!   without testing them. The port reads them as zero or empty, which is what the web
//!   application's schema defaults produce.
//! * The user's timezone is stored on the session instead of being pushed into the process
//!   with `setenv("TZ", …)` (`login.c:389-403`), which a shared process cannot do. The
//!   original also applied it only after the dead-account check, so no ordering is lost.
//! * The EULA exchange returns its verdict to the caller. C logged it and discarded it
//!   (`bbs_main.c:391`), ending the session either way.

use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::db::{self, Db};
use crate::ipmask;
use crate::paths::{
    DATA_EULA, DATA_LOGIN_ERROR, DATA_REGISTER, MAX_USER_IDLE_TIME, USERNAME_MAX_LEN,
};
use crate::privilege::{self, UserPriv};
use crate::screen::{self, Echo};
use crate::session::{Session, clamp_bytes, now};
use crate::term::Terminal;
use crate::term::key::{CR, KEY_NULL, KEY_TIMEOUT};

/// `BBS_login_retry_times` (`login.c:38`).
pub const LOGIN_RETRY_TIMES: u32 = 3;
/// `BBS_username_min_len` (`login.c:31`); the maximum is [`USERNAME_MAX_LEN`].
pub const USERNAME_MIN_LEN: usize = 3;
/// `BBS_password_min_len` (`login.c:32`) and `BBS_password_max_len` (`bbs.h:21`).
pub const PASSWORD_MIN_LEN: usize = 5;
pub const PASSWORD_MAX_LEN: usize = 12;
/// `BBS_allowed_login_failures_within_interval` (`login.c:34`).
pub const ALLOWED_FAILURES_WITHIN_INTERVAL: i64 = 10;
/// `BBS_login_failures_count_interval`, in minutes (`login.c:35`).
pub const FAILURES_COUNT_INTERVAL_MINUTES: i64 = 10;
/// `BBS_allowed_login_failures_per_account` (`login.c:36`).
pub const ALLOWED_FAILURES_PER_ACCOUNT: i64 = 3;
/// The `life` values that never expire (`login.c:458`). 365 is in the list, so a "one year"
/// lifetime means immortal.
pub const IMMORTAL_LIFE: [i16; 4] = [333, 365, 666, 999];

/// The account prompt (`login.c:50`), two adjacent literals joined with one space.
const PROMPT_USERNAME: &str = "\x1b[1;33m请输入帐号\x1b[m(试用请输入`\x1b[1;36mguest\x1b[m', \
                              注册请输入`\x1b[1;31mnew\x1b[m'): ";
/// The password prompt (`login.c:75`).
const PROMPT_PASSWORD: &str = "\x1b[1;37m请输入密码\x1b[m: ";

/// `b'x' as u32` is not a pattern, so the agreement prompt's letters get names.
const KEY_A: u32 = b'A' as u32;
const KEY_D: u32 = b'D' as u32;
const KEY_R: u32 = b'R' as u32;
const KEY_LOWER_A: u32 = b'a' as u32;
const KEY_LOWER_D: u32 = b'd' as u32;
const KEY_LOWER_R: u32 = b'r' as u32;

/// What [`login`] decided, mirroring `bbs_login`'s `0` and `-1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// A person is now on the site, as `guest`, as themselves, or pending the agreement.
    In,
    /// The session is over: `new`, three failures, or the client went away.
    Out,
}

/// `check_user`'s return (`login.c:106`), where C's `1` and `-1` both mean "try again".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// Credentials accepted and the account may stay.
    Success,
    /// Credentials accepted, but the agreement has to be re-accepted first: C's `2`.
    Agreement,
    /// Refused or wrong (C's `1`).
    Failed,
    /// The database or the user data could not be read (C's `-1`).
    Error,
}

/// `load_user_info`'s return (`login.c:412`): `0`, `-1`, `-2`, `-3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    Active,
    /// No `user_pubinfo` row, or the privilege queries failed.
    Unreadable,
    /// The agreement changed since this account last signed in.
    OutdatedAgreement,
    /// Absent longer than the account's own `life` allowed.
    Dead,
}

/// `user_update_agreement`'s return (`login.c:633`): `-1`, `0`, `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agreement {
    Cancelled,
    Refused,
    Accepted,
}

/// `login.c:129-140`: a letter, then letters, digits and underscores, 3 to 12 bytes.
///
/// The original ran `isalpha` over signed chars, so a byte above `0x7f` was undefined
/// behaviour that glibc reported as "not a letter"; the port rejects non-ASCII directly.
pub fn username_ok(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.len() < USERNAME_MIN_LEN || bytes.len() > USERNAME_MAX_LEN {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| {
        b.is_ascii_alphabetic() || (i > 0 && (b.is_ascii_digit() || *b == b'_'))
    })
}

/// `login.c:140-150`: alphanumeric only, 5 to 12 bytes — so a modern password with a
/// punctuation mark cannot be typed in here at all.
pub fn password_ok(password: &str) -> bool {
    let bytes = password.as_bytes();
    bytes.len() >= PASSWORD_MIN_LEN
        && bytes.len() <= PASSWORD_MAX_LEN
        && bytes.iter().all(u8::is_ascii_alphanumeric)
}

/// `BBS_eula_tm` (`main.c:275`): the agreement counts as updated when `data/eula.txt` is
/// newer than the account's last login.
pub fn eula_mtime(root: &Path) -> i64 {
    std::fs::metadata(root.join(DATA_EULA))
        .and_then(|stat| stat.modified())
        .map(|when| match when.duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_secs() as i64,
            // A file dated before 1970 predates any account's last login anyway.
            Err(_) => 0,
        })
        .unwrap_or(0)
}

/// `prints` followed by `iflush`, which is how the original puts a message out.
async fn say(term: &mut Terminal, text: &str) {
    term.print(text);
    let _ = term.flush().await;
}

/// `bbs_login` (`login.c:40-104`).
///
/// The three attempts are counted the way C counts them: a timeout, an empty line and a
/// refused password each consume one, and only the third path shows the failure poem.
pub async fn login(term: &mut Terminal, session: &mut Session) -> Login {
    for attempt in 1..=LOGIN_RETRY_TIMES {
        term.clrtoeol();
        say(term, PROMPT_USERNAME).await;

        let Some(line) = screen::str_input(
            term,
            USERNAME_MAX_LEN + 1,
            Echo::On,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            continue;
        };
        let username = String::from_utf8_lossy(&line).into_owned();

        if username == "guest" {
            load_guest_info(session).await;
            tracing::info!("User [{username}] authenticated successfully");
            return Login::In;
        }

        if username == "new" {
            session.display_file(term, DATA_REGISTER, 1).await;
            return Login::Out;
        }

        if username.is_empty() {
            continue;
        }

        term.clrtoeol();
        say(term, PROMPT_PASSWORD).await;

        let Some(line) = screen::str_input(
            term,
            PASSWORD_MAX_LEN + 1,
            Echo::Off,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            continue;
        };
        let password = String::from_utf8_lossy(&line).into_owned();

        let outcome = match check_user(term, session, &username, &password).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::error!("check_user db error: {e}");
                Auth::Error
            }
        };

        // A rejection is one of the three used-up attempts: the loop asks again. `Agreement`
        // means the credentials are good and the session is on the site, pending the text.
        if outcome == Auth::Agreement {
            session.update_eula = true;
        }
        if outcome == Auth::Success || outcome == Auth::Agreement {
            tracing::info!("User [{username}] authenticated successfully");
            return Login::In;
        }
        tracing::info!(
            "User [{username}] authentication failed ({attempt}/{LOGIN_RETRY_TIMES})"
        );
        let _ = term.flush().await;
    }

    session.display_file(term, DATA_LOGIN_ERROR, 1).await;
    Login::Out
}

/// `check_user` (`login.c:106-410`).
pub async fn check_user(
    term: &mut Terminal,
    session: &mut Session,
    username: &str,
    password: &str,
) -> Result<Auth, sqlx::Error> {
    if !username_ok(username) || !password_ok(password) {
        // The original rejects the format before touching the database, so nothing here is
        // logged and the lockout counters never see the attempt.
        say(term, "\x1b[1;31m用户名或密码格式错误...\x1b[m\r\n").await;
        return Ok(Auth::Failed);
    }

    let db = session.state.db.clone();
    // `ip_mask(client_addr, 1, '%')` on a copy of the address: masking the original would
    // put `a.b.c.%` into the login log instead of the real address.
    let subnet = ipmask::subnet_pattern(&session.hostaddr);

    let mut tx = db.begin().await?;

    let from_subnet = db::count_errors_by_ip(&mut tx, FAILURES_COUNT_INTERVAL_MINUTES, &subnet)
        .await?;
    if from_subnet >= ALLOWED_FAILURES_WITHIN_INTERVAL {
        tx.rollback().await?;
        say(
            term,
            "\x1b[1;31m来源存在多次失败登陆尝试，请稍后再试，或使用Web方式访问\x1b[m\r\n",
        )
        .await;
        return Ok(Auth::Failed);
    }

    let from_account = db::count_errors_by_user(&mut tx, username).await?;
    if from_account >= ALLOWED_FAILURES_PER_ACCOUNT {
        tx.rollback().await?;
        say(
            term,
            "\x1b[1;31m账户存在多次失败登陆尝试，请使用Web方式登录解锁\x1b[m\r\n",
        )
        .await;
        return Ok(Auth::Failed);
    }

    let Some(account) = db::find_account(&mut tx, username, password).await? else {
        db::record_login_error(&mut tx, username, password, &session.hostaddr).await?;
        // The failure row is committed before the message, which is why the plaintext
        // password in `user_err_login_log` survives even a client that disconnects here.
        tx.commit().await?;
        say(term, "\x1b[1;31m错误的用户名或密码...\x1b[m\r\n").await;
        return Ok(Auth::Failed);
    };

    db::record_login(&mut tx, account.uid, &session.hostaddr).await?;
    // `login.c:284` commits before it tests `p_login`, so a refused account still counts
    // as a visit on the front page — and still writes no failure row.
    tx.commit().await?;

    // `BBS_username` takes the row's spelling, not the typed one, which is how the case is
    // normalised for the rest of the session.
    session.username = clamp_bytes(&account.username, USERNAME_MAX_LEN);
    session.uid = account.uid;
    session.privs = UserPriv::default();

    if !account.p_login {
        say(term, "\x1b[1;31m您目前无权登陆...\x1b[m\r\n").await;
        return Ok(Auth::Failed);
    }

    let loaded = load_user_info(&db, session, account.uid).await;
    match loaded {
        Account::Active => {
            if staff_must_use_ssh(term, session).await {
                return Ok(Auth::Failed);
            }
        }
        Account::Unreadable => {
            say(term, "\x1b[1;31m读取用户数据错误...\x1b[m\r\n").await;
            return Ok(Auth::Error);
        }
        Account::OutdatedAgreement => {
            if staff_must_use_ssh(term, session).await {
                return Ok(Auth::Failed);
            }
            return Ok(Auth::Agreement);
        }
        Account::Dead => {
            say(
                term,
                "\x1b[1;31m很遗憾，您已经永远离开了我们的世界！\x1b[m\r\n",
            )
            .await;
            return Ok(Auth::Failed);
        }
    }

    if loaded == Account::Active {
        db.mark_login(account.uid).await?;
        if session.online_add().await.is_err() {
            // `login.c:380` turns any online-row failure into `ret = -1` after the
            // credentials have already been accepted.
            return Ok(Auth::Error);
        }
        session.login_tm = now();
        session.last_access_tm = session.login_tm;
    }

    Ok(Auth::Success)
}

/// The rule shared by `login.c:340` and `:352`: a board master or an administrator has to
/// arrive over SSH. Returns `true` when the session was refused.
async fn staff_must_use_ssh(term: &mut Terminal, session: &Session) -> bool {
    if session.is_ssh || !session.is_staff() {
        return false;
    }
    say(term, "\x1b[1;31m非普通账户必须使用SSH方式登录\x1b[m\r\n").await;
    true
}

/// `login.c:458-461`: an account is dead once it has been away longer than its own `life`
/// (in days), unless `life` is one of the immortal sentinels in [`IMMORTAL_LIFE`].
pub fn is_dead(life: i16, last_login: i64, at: i64) -> bool {
    !IMMORTAL_LIFE.contains(&life) && at - last_login > 60 * 60 * 24 * life as i64
}

/// `load_user_info` (`login.c:412-481`).
pub async fn load_user_info(db: &Db, session: &mut Session, uid: i32) -> Account {
    let Ok(Some(row)) = db.pub_info(uid).await else {
        return Account::Unreadable;
    };

    let last_login = row.last_login_dt.unwrap_or(0);
    let username = session.username.clone();
    session.set_identity(
        &username,
        row.nickname.as_deref().unwrap_or(""),
        row.exp.unwrap_or(0),
        &row.user_timezone,
    );

    if is_dead(row.life, last_login, now()) {
        return Account::Dead;
    }

    // `load_priv` is the only thing that fills `BBS_priv`, and it comes before the
    // agreement test: a session with an outdated agreement still has its privileges.
    let Ok(privs) = privilege::load(db, uid).await else {
        return Account::Unreadable;
    };
    session.privs = privs;

    if last_login < eula_mtime(&session.root) {
        return Account::OutdatedAgreement;
    }

    Account::Active
}

/// `load_guest_info` (`login.c:483-521`). Its C return value is ignored by the caller, so a
/// database that cannot be reached still admits the guest — with the zeroed privileges the
/// original would have run on.
pub async fn load_guest_info(session: &mut Session) {
    session.uid = 0;
    session.set_identity("guest", "Guest", 0, "");

    match privilege::load(&session.state.db, 0).await {
        Ok(privs) => session.privs = privs,
        Err(e) => {
            tracing::error!("load_priv(guest) error: {e}");
            return;
        }
    }

    if let Err(e) = session.online_add().await {
        tracing::error!("user_online_add error: {e}");
        return;
    }

    session.login_tm = now();
    session.last_access_tm = session.login_tm;
}

/// `user_update_agreement` (`login.c:633-708`).
pub async fn update_agreement(term: &mut Terminal, session: &mut Session) -> Agreement {
    screen::clearscr(term);
    term.moveto(2, 0);
    say(term, "尊敬的用户：").await;
    term.moveto(3, 0);
    say(
        term,
        "    本站的《用户许可协议》已更新，您必须在仔细阅读并接受后，才能继续使用本站提供的服务。",
    )
    .await;
    screen::press_any_key(term).await;

    session.display_file(term, DATA_EULA, 1).await;

    loop {
        screen::clearscr(term);
        term.moveto(2, 0);

        let ch = screen::press_any_key_ex(
            term,
            "您是否接受本站的《用户许可协议》? (A)接受, (D)拒绝, (R)再看看协议 [D]",
            60,
        )
        .await;

        // C ran `toupper` over the key code, which leaves anything outside ASCII letters
        // alone; the port widens only the two letters it cares about.
        let ch = match ch {
            KEY_LOWER_A => KEY_A,
            KEY_LOWER_D => KEY_D,
            KEY_LOWER_R => KEY_R,
            other => other,
        };

        match ch {
            KEY_NULL => return Agreement::Cancelled,
            KEY_TIMEOUT | CR | KEY_D => {
                term.moveto(3, 0);
                say(term, "您已拒绝《用户许可协议》，再见！").await;
                screen::press_any_key(term).await;
                return Agreement::Refused;
            }
            KEY_R => {
                session.display_file(term, DATA_EULA, 1).await;
                continue;
            }
            KEY_A => {
                // The same statement as `mark_login`, and the reason a pending agreement
                // stops being pending: `last_login_dt` moves past the file's mtime.
                if let Err(e) = session.state.db.mark_login(session.privs.uid).await {
                    tracing::error!("Update user_pubinfo error: {e}");
                    return Agreement::Cancelled;
                }
                term.moveto(3, 0);
                say(term, "您已接受《用户许可协议》，请重新登陆。").await;
                screen::press_any_key(term).await;
                return Agreement::Accepted;
            }
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_letters_first_then_letters_digits_underscore() {
        assert!(username_ok("abc"));
        assert!(username_ok("Leaflet"));
        assert!(username_ok("a_b_1"));
        assert!(username_ok("leaflet12345")); // exactly USERNAME_MAX_LEN
        assert!(!username_ok("ab"), "too short");
        assert!(!username_ok("leaflet123456"), "one past the maximum");
        assert!(!username_ok("1abc"), "cannot start with a digit");
        assert!(!username_ok("_abc"), "cannot start with an underscore");
        assert!(!username_ok("ab-c"), "no punctuation");
        assert!(!username_ok(""), "empty");
    }

    #[test]
    fn non_ascii_is_rejected_rather_than_passed_to_isalpha() {
        // The original fed a negative `int` to `isalpha`; the port calls it not-a-letter.
        assert!(!username_ok("枫林用"));
        assert!(!password_ok("枫林用户密码"));
    }

    #[test]
    fn passwords_are_alphanumeric_within_twelve_bytes() {
        assert!(password_ok("abcde"));
        assert!(password_ok("a1b2c3d4e5f6"));
        assert!(!password_ok("abcd"), "too short");
        assert!(!password_ok("abcdefghijk12"), "too long");
        assert!(!password_ok("abc!def"), "no punctuation");
        assert!(!password_ok(""), "empty");
    }

    #[test]
    fn immortal_accounts_have_no_deadline() {
        assert_eq!(IMMORTAL_LIFE, [333, 365, 666, 999]);
        // 365 is in the list, so a configured "one year" lifetime never expires.
        assert!(!is_dead(365, 0, i64::from(u32::MAX)));
    }

    #[test]
    fn the_deadline_is_the_accounts_own_lifetime_in_days() {
        let day = 60 * 60 * 24;
        assert!(is_dead(30, 0, 31 * day));
        // Exactly at the limit is still alive: the original compares with `>`.
        assert!(!is_dead(30, 0, 30 * day));
        assert!(!is_dead(365, 0, 10_000 * day), "365 is an immortal life");
        // A NULL `last_login_dt` arrives as 0, which is older than any real lifetime.
        assert!(is_dead(30, 0, now()));
    }

    #[test]
    fn the_prompts_are_the_ones_the_original_prints() {
        assert_eq!(
            PROMPT_USERNAME,
            "\x1b[1;33m请输入帐号\x1b[m(试用请输入`\x1b[1;36mguest\x1b[m', \
             注册请输入`\x1b[1;31mnew\x1b[m'): "
        );
        assert_eq!(PROMPT_PASSWORD, "\x1b[1;37m请输入密码\x1b[m: ");
    }

    #[test]
    fn a_missing_agreement_file_acts_like_an_old_epoch() {
        assert_eq!(eula_mtime(Path::new("/nonexistent-lbbs-root")), 0);
    }

    #[tokio::test]
    async fn a_malformed_pair_is_refused_before_any_query() {
        // The format test (`login.c:129-152`) comes first, so this path never reaches the
        // database — the unreachable handle in `offline_session` stays untouched.
        let (mut client, server) = crate::term::Pipe::new();
        let mut term = Terminal::new(crate::term::ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        let outcome = check_user(&mut term, &mut session, "ab", "abcdef")
            .await
            .unwrap();
        assert_eq!(outcome, Auth::Failed);
        assert_eq!(session.uid, 0);

        let wire = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(wire.contains("用户名或密码格式错误"), "{wire}");
    }

    #[tokio::test]
    async fn three_silent_attempts_end_the_session() {
        // Each closed read is one of the three, and the failure file is what follows:
        // `bbs_login` returns -1 without ever touching an account.
        let (client, server) = crate::term::Pipe::new();
        let mut term = Terminal::new(crate::term::ByteIo::Pipe(server));
        drop(client);
        let mut session = crate::session::offline_session();

        assert_eq!(login(&mut term, &mut session).await, Login::Out);
        assert!(session.is_anonymous());
        assert!(!session.update_eula);
    }
}
