use super::*;
use crate::privilege::{S_ADMIN, S_DEFAULT, S_POST};

fn item(priv_mask: i32, level: i32) -> MenuItem {
    MenuItem {
        row: 0,
        col: 0,
        action: String::new(),
        action_menu_id: None,
        action_cmd: None,
        submenu: false,
        priv_mask,
        level,
        name: String::new(),
        text: String::new(),
    }
}

#[test]
fn strings_are_clamped_to_the_column_widths() {
    assert_eq!(
        clamp_bytes("toomuchtoolongname", USERNAME_MAX_LEN),
        "toomuchtoolo"
    );
    // A cut that lands inside a multi-byte character drops the whole character.
    assert_eq!(clamp_bytes("视图", 3), "视");
    assert_eq!(clamp_bytes("ok", 40), "ok");
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn a_session_starts_anonymous() {
    let session = offline_session();
    assert!(session.is_anonymous());
    assert!(!session.is_guest());
    assert!(!session.is_staff());
    assert_eq!(session.privs, UserPriv::default());
    assert_eq!(session.sid, "Telnet_Process_7");
    assert_eq!(session.bbs_name(), "www.bbs.test");
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn identity_is_stored_at_the_widths_the_columns_have() {
    let mut session = offline_session();
    session.set_identity("leaflet_longer_than_this", "昵称", 1234, "Asia/Shanghai");
    assert_eq!(session.username, "leaflet_long");
    assert_eq!(session.exp, 1234);
    assert!(!session.is_anonymous());
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn staff_are_the_accounts_telnet_has_to_refuse() {
    let mut session = offline_session();
    session.privs.level = P_MAN_S;
    assert!(session.is_staff());
    session.privs.level = P_MAN_S - 1;
    assert!(!session.is_staff());
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn the_global_word_and_level_decide_what_a_menu_shows() {
    let post = item(S_POST, 1);
    let public = item(0, 0);

    let mut session = offline_session();
    // The zeroed globals of a session that has not logged in hide even a public item,
    // whose level requirement is 0 and is therefore always met.
    assert!(!session.permits(&post));
    assert!(session.permits(&public));

    session.privs = UserPriv::new(7);
    assert!(
        !session.permits(&post),
        "an unverified account has no post bit"
    );
    session.privs.g_priv |= S_POST;
    assert!(session.permits(&post));
    assert_eq!(session.privs.g_priv, S_DEFAULT | S_POST);
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn the_favourites_filter_reads_the_board_number_out_of_the_priv_field() {
    let favorite = item(27, 0);

    let mut session = offline_session();
    session.privs = UserPriv::new(7);
    assert!(!session.filter_allows(Action::FavorSectionFilter, &favorite));

    let (s_priv, _) = session.privs.getpriv(27);
    session.privs.setpriv(27, s_priv, true).unwrap();
    assert!(session.filter_allows(Action::FavorSectionFilter, &favorite));

    // The filter still honours the level column.
    session.privs.level = P_MAN_S - 1;
    let staff_only = item(27, P_MAN_S);
    assert!(session.privs.checklevel2(P_MAN_S - 1));
    assert!(!session.filter_allows(Action::FavorSectionFilter, &staff_only));
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn an_unimplemented_filter_falls_back_to_the_privilege_test() {
    let admin_only = item(S_ADMIN, 0);
    let session = offline_session();
    assert_eq!(
        session.filter_allows(Action::ListSection, &admin_only),
        session.permits(&admin_only)
    );
}

// `Db::detached` needs a runtime to be present, so these are tokio tests.
#[tokio::test]
async fn help_is_enabled_until_a_nested_pager_turns_it_off() {
    let mut session = offline_session();
    assert!(session.help_enabled());
    session.set_help_enabled(false);
    assert!(!session.help_enabled());
}
