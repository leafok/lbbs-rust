use super::*;

fn db_config() -> DbConfig {
    DbConfig {
        host: "db.example".into(),
        username: "boarduser".into(),
        password: "not-a-real-secret".into(),
        database: "boarddb".into(),
        timezone: "+08:00".into(),
    }
}

#[test]
fn the_session_reproduces_the_c_client_settings() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let options = Db::options(&db_config(), &manifest.join("Cargo.toml"));
    assert_eq!(options.get_host(), "db.example");
    assert_eq!(options.get_username(), "boarduser");
    assert_eq!(options.get_database(), Some("boarddb"));
    // `mysql_set_character_set(db, "utf8")` names no collation, so the server's default
    // for the charset applies; sqlx has to be kept off its utf8mb4 default to match.
    assert_eq!(options.get_charset(), WIRE_CHARSET);
    assert_eq!(options.get_collation(), None);
    assert!(matches!(options.get_ssl_mode(), MySqlSslMode::VerifyCa));

    // No `conf/ca_cert.pem` is `SSL_MODE_PREFERRED`, which the C client falls back to.
    let options = Db::options(&db_config(), &manifest.join("no-such-ca.pem"));
    assert!(matches!(options.get_ssl_mode(), MySqlSslMode::Preferred));
}

#[test]
fn sign_slot_statements_name_their_own_column() {
    for slot in [SignSlot::One, SignSlot::Two, SignSlot::Three] {
        let expected = format!(
            "SELECT {} AS sign FROM user_pubinfo WHERE UID = ?",
            slot.column()
        );
        assert_eq!(slot.query(), expected);
        let expected = format!(
            "UPDATE user_pubinfo SET {} = ? WHERE UID = ?",
            slot.column()
        );
        assert_eq!(slot.update_query(), expected);
    }

    // `sign_id` of `user_sign_edit:215-220`: the three slots, and nothing else.
    assert_eq!(SignSlot::from_index(0), None);
    assert_eq!(SignSlot::from_index(-1), None);
    assert_eq!(SignSlot::from_index(4), None);
    assert_eq!(SignSlot::from_index(1), Some(SignSlot::One));
    assert_eq!(SignSlot::from_index(3), Some(SignSlot::Three));
}

#[test]
fn the_money_statements_are_the_same_three_times_and_twice() {
    // `money.c` writes the read out at `:60-62`, `:150-152` and `:216-218` and the write at
    // `:96-98` and `:181-183`, with the same two columns and the same lock every time. The
    // third one is what `money_refresh` asks on a connection with no transaction around it.
    assert_eq!(
        GAME_MONEY_FOR_UPDATE,
        "SELECT game_money FROM user_pubinfo WHERE UID = ? FOR UPDATE"
    );
    assert_eq!(
        UPDATE_GAME_MONEY,
        "UPDATE user_pubinfo SET game_money = ? WHERE UID = ?"
    );
}

#[test]
fn master_run_fills_the_buffer_then_cuts_the_next_name() {
    let mut list = String::new();
    for _ in 0..3 {
        append_bounded(&mut list, "abcdefghijkl", MASTER_LIST_BYTES);
        append_bounded(&mut list, " ", MASTER_LIST_BYTES);
    }
    // Three names of exactly `BBS_username_max_len` fit, trailing space included.
    assert_eq!(list, "abcdefghijkl abcdefghijkl abcdefghijkl ");
    assert_eq!(list.len(), MASTER_LIST_BYTES);

    // A wider name than the buffer budgets for, as `varchar(20)` allows, and a last one
    // that runs out of room mid-word: `strncat` stops at the byte, and the space that
    // would follow it has no room left either.
    let mut list = String::new();
    for name in ["abcdefghijklmnopqrst", "second", "third", "fourth", "fifth"] {
        append_bounded(&mut list, name, MASTER_LIST_BYTES);
        append_bounded(&mut list, " ", MASTER_LIST_BYTES);
    }
    assert_eq!(list, "abcdefghijklmnopqrst second third fourt");
    assert_eq!(list.len(), MASTER_LIST_BYTES);
}

#[test]
fn a_cut_in_a_multi_byte_name_keeps_whole_characters() {
    let mut list = String::new();
    // `中` is three bytes, so `room = 4` would cut the second one in half.
    append_bounded(&mut list, "中文", MASTER_LIST_BYTES);
    assert_eq!(list, "中文");
    let mut list = String::new();
    append_bounded(&mut list, "中文", 4);
    assert_eq!(list, "中");
}

/// Opt-in check against the real database, which is the only way to prove the type
/// decisions above and the session bootstrap at the same time:
/// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
///
/// Read-only. The login path is exercised with a wrong password so its `SELECT` runs and
/// no `INSERT` does, and the transaction is dropped instead of committed.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn live_database_decodes_as_the_reference_types() {
    use sqlx::Row;

    let path = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let text = std::fs::read_to_string(path).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = Db::connect(&config.db).await.unwrap();

    let row = sqlx::query(
            "SELECT @@session.character_set_client AS client, @@session.character_set_results AS results,
                    @@session.collation_connection AS collation, @@session.time_zone AS tz,
                    @@session.sql_mode AS session_mode, @@global.sql_mode AS global_mode",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
    // What `mysql_set_character_set(db, "utf8")` leaves behind, collation included.
    assert_eq!(row.try_get::<String, _>("client").unwrap(), "utf8mb3");
    assert_eq!(row.try_get::<String, _>("results").unwrap(), "utf8mb3");
    assert_eq!(
        row.try_get::<String, _>("collation").unwrap(),
        "utf8mb3_general_ci"
    );
    assert_eq!(row.try_get::<String, _>("tz").unwrap(), config.db.timezone);

    // sqlx advertises `CLIENT_IGNORE_SPACE` in the handshake and libmysqlclient does not,
    // which makes the server add `IGNORE_SPACE` to the session. It only changes how a
    // space between a built-in function name and its parenthesis parses, and no statement
    // in this layer has one. Everything else must stay the server's own mode: sqlx's
    // default `PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION` append is off in `Db::options`.
    let session_mode = row.try_get::<String, _>("session_mode").unwrap();
    assert_eq!(
        session_mode
            .strip_prefix("IGNORE_SPACE,")
            .unwrap_or(&session_mode),
        row.try_get::<String, _>("global_mode").unwrap()
    );

    let sections = db.sections().await.unwrap();
    assert!(!sections.is_empty());
    db.section_masters(sections[0].sid).await.unwrap();
    assert!(!db.section_levels().await.unwrap().is_empty());

    let articles = db.articles_from(0, 3).await.unwrap();
    assert!(!articles.is_empty());
    let aid = articles[0].aid;
    db.article_content(aid).await.unwrap();
    db.article_sid(aid).await.unwrap();
    let watermark = db.last_op_id().await.unwrap().unwrap();
    assert!(watermark > 0);
    db.ops_after(0, 3).await.unwrap();

    assert!(!db.roster().await.unwrap().is_empty());
    db.online(900).await.unwrap();
    assert!(db.login_count().await.unwrap().unwrap() > 0);

    let (uid, username): (i32, String) =
        sqlx::query_as("SELECT UID, username FROM user_list WHERE enable ORDER BY UID LIMIT 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    db.pub_info(uid).await.unwrap();
    db.user_flags(uid).await.unwrap();
    db.admin_major(uid).await.unwrap();
    db.masterships(uid).await.unwrap();
    db.banned_sids(uid).await.unwrap();
    db.favorite_sids(uid).await.unwrap();
    for slot in [SignSlot::One, SignSlot::Two, SignSlot::Three] {
        db.signature(uid, slot).await.unwrap();
        db.signature_row(uid, slot).await.unwrap();
    }
    db.introduction(uid).await.unwrap();
    // `money_refresh` reads the column the same way, on autocommit, which is why the
    // `FOR UPDATE` here takes and drops its row lock inside one statement.
    db.game_money(uid).await.unwrap();

    let mut tx = db.begin().await.unwrap();
    count_errors_by_ip(&mut tx, 10, "127.0.0%").await.unwrap();
    count_errors_by_user(&mut tx, &username).await.unwrap();
    assert!(
        find_account(&mut tx, &username, "wrong-on-purpose")
            .await
            .unwrap()
            .is_none()
    );
    drop(tx);
}
