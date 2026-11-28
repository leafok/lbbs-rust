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
    assert!(!is_dead(365, 1, i64::from(u32::MAX)));
}

#[test]
fn the_deadline_is_the_accounts_own_lifetime_in_days() {
    let day = 60 * 60 * 24;
    // One second shy of thirty-one days away from a one-day-old login.
    assert!(is_dead(30, day, 31 * day + day - 1));
    assert!(
        !is_dead(30, day, 31 * day),
        "exactly the limit is still alive"
    );
    assert!(!is_dead(365, day, 10_000 * day), "365 is an immortal life");
}

/// `login.c:458`, since `d373719`: a `NULL last_login_dt` arrives as `0`, and `0` is the
/// account that has never signed in rather than one that signed in at the epoch. It is asked
/// to accept the agreement instead of being buried.
#[test]
fn an_account_that_never_signed_in_has_no_deadline() {
    let day = 60 * 60 * 24;
    assert!(!is_dead(30, 0, now()));
    assert!(!is_dead(30, 0, 10_000 * day));
    // The same account a day after it did sign in is judged by `life` again.
    assert!(is_dead(30, day, 32 * day));
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
