use super::*;

fn user_row(uid: i32, username: &str) -> UserRow {
    UserRow {
        uid,
        username: username.to_owned(),
        nickname: None,
        gender: "M".to_owned(),
        gender_pub: true,
        life: 100,
        exp: None,
        visit_count: 0,
        signup_dt: None,
        last_login_dt: None,
        last_logout_dt: None,
        birthday: None,
        introduction: Some("简介".to_owned()),
    }
}

fn online_row(sid: &str, uid: i32, action: &str) -> OnlineRow {
    OnlineRow {
        sid: sid.to_owned(),
        uid,
        ip: "10.0.0.1".to_owned(),
        current_action: action.to_owned(),
        login_tm: 1000,
        last_tm: Some(2000),
    }
}

fn roster() -> Roster {
    // The load query is `ORDER BY username`, and the collation is case-insensitive, so
    // this is the order the pool holds them in.
    Roster::load(&[
        user_row(30, "amy"),
        user_row(10, "Ann"),
        user_row(11, "anna"),
        user_row(12, "annabelle"),
        user_row(20, "bob"),
    ])
}

/// `user_list.c:245` and `:250`: the load reports what it sorted and how many it kept. Both
/// are `log_debug()`, which is the error stream here.
#[test]
fn the_load_reports_the_user_count_and_the_last_uid() {
    let (common, error) = crate::log::captured(|| {
        let roster = roster();
        assert_eq!(roster.count(), 5);
    });

    assert!(
        error.contains("Rebuild index of 5 users, last_uid=20\n"),
        "{error}"
    );
    assert!(error.contains("Loaded 5 users\n"), "{error}");
    assert_eq!(common, "", "{common}");
}

#[test]
fn the_roster_is_ordered_by_username_and_indexed_by_uid() {
    let roster = roster();
    assert_eq!(roster.count(), 5);
    assert_eq!(roster.page_count(), 1);
    assert_eq!(
        roster
            .page(0)
            .unwrap()
            .iter()
            .map(|u| u.username.as_str())
            .collect::<Vec<_>>(),
        vec!["amy", "Ann", "anna", "annabelle", "bob"]
    );
    // `id` is the position in that order, `uid` the database key.
    assert_eq!(roster.by_id(0).unwrap().uid, 30);
    assert_eq!(roster.by_uid(20).unwrap().username, "bob");
    assert_eq!(roster.by_uid(11).unwrap().id, 2);
    assert_eq!(roster.by_uid(999), None);
    assert_eq!(roster.by_id(999), None);
    assert_eq!(roster.by_id(-1), None);
    assert_eq!(roster.by_uid(11).unwrap().intro, "简介");
}

#[test]
fn a_username_prefix_search_ignores_case_and_respects_the_limit() {
    let roster = roster();
    let found = roster.by_username_prefix("ANN", 10);
    assert_eq!(
        found
            .iter()
            .map(|u| u.username.as_str())
            .collect::<Vec<_>>(),
        vec!["Ann", "anna", "annabelle"]
    );
    assert_eq!(roster.by_username_prefix("ann", 2).len(), 2);
    assert!(roster.by_username_prefix("zoe", 10).is_empty());
    assert!(roster.by_username_prefix("", 10).is_empty());
    // A prefix longer than a name cannot match it, which is what the NUL that
    // `strncasecmp` compares gives the C original.
    assert!(roster.by_username_prefix("anna-b", 10).is_empty());
}

#[test]
fn paging_stops_past_the_last_page() {
    let rows: Vec<UserRow> = (0..21)
        .map(|i| user_row(i, &format!("user{i:03}")))
        .collect();
    let roster = Roster::load(&rows);
    assert_eq!(roster.page_count(), 2);
    assert_eq!(roster.page(0).unwrap().len(), 20);
    assert_eq!(roster.page(1).unwrap().len(), 1);
    assert_eq!(roster.page(2), None);
    assert_eq!(roster.page(-1), None);
}

#[test]
fn the_online_list_counts_guests_and_drops_sessions_it_cannot_label() {
    let roster = roster();
    let rows = vec![
        online_row("Telnet_Process_1", 0, "MENU"),
        online_row("Telnet_Process_2", 10, "VIEW_ARTICLE"),
        online_row("Telnet_Process_3", 11, "NOT_AN_ACTION"),
        online_row("Telnet_Process_4", 12, ""),
        online_row("Telnet_Process_5", 777, "MENU"),
    ];
    let online = OnlineList::load(&rows, &roster);

    let (users, guests) = online.counts();
    assert_eq!((users, guests), (2, 1));
    assert_eq!(
        online
            .users
            .iter()
            .map(|u| u.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["Telnet_Process_2", "Telnet_Process_4"]
    );
    assert_eq!(online.users[0].user.username, "Ann");
    assert_eq!(online.users[0].current_action_title, Some("阅读文章"));
    // A session that has not chosen an action yet is listed with an empty label, which is
    // what `current_action_title = ""` means in the pool.
    assert_eq!(online.users[1].current_action_title, Some(""));
    assert_eq!(online.users[1].last_tm, 2000);
}

#[test]
fn online_sessions_of_a_user_are_paged_by_index() {
    let roster = roster();
    let rows = vec![
        online_row("Telnet_Process_a", 10, "MENU"),
        online_row("Telnet_Process_b", 20, "MENU"),
        online_row("Telnet_Process_c", 10, "USER_ONLINE"),
    ];
    let online = OnlineList::load(&rows, &roster);

    let both = online.sessions_of(10, 0, 10);
    assert_eq!(both.len(), 2);
    assert_eq!(both[1].session_id, "Telnet_Process_c");
    // `start_id` is an index into the uid-ordered sessions, not a skip count: index 1 is
    // the second session of uid 10, index 2 belongs to uid 20 and so finds nothing.
    assert_eq!(online.sessions_of(10, 1, 10).len(), 1);
    assert!(online.sessions_of(10, 2, 10).is_empty());
    assert_eq!(online.sessions_of(10, 0, 1).len(), 1);
    assert!(online.sessions_of(30, 0, 10).is_empty());
    assert!(online.sessions_of(10, 9, 10).is_empty());
    assert_eq!(online.page_count(), 1);
    assert_eq!(online.page(0).unwrap().len(), 3);
}

#[test]
fn action_title_maps_the_codes_the_web_side_writes() {
    assert_eq!(action_title("VIEW_ARTICLE"), Some("阅读文章"));
    assert_eq!(action_title("LOGIN"), Some("进入大厅"));
    assert_eq!(action_title(""), None);
    assert_eq!(action_title("view_article"), None);
}

#[test]
fn the_stat_map_counts_visible_articles_of_users_it_loaded() {
    let articles = vec![
        Article {
            aid: 100,
            sid: 1,
            uid: 1,
            visible: true,
            ..Article::default()
        },
        Article {
            aid: 101,
            sid: 1,
            uid: 1,
            visible: false,
            ..Article::default()
        },
        Article {
            aid: 102,
            sid: 1,
            uid: 5,
            visible: true,
            ..Article::default()
        },
    ];
    let first = Roster::load(&[user_row(1, "ann"), user_row(2, "bob"), user_row(3, "cy")]);
    let extended_roster = Roster::load(&[
        user_row(1, "ann"),
        user_row(2, "bob"),
        user_row(3, "cy"),
        user_row(5, "dee"),
    ]);

    let mut stats = UserStatMap::new();
    // `uid = 5` is not in the map yet, which is the C `last_uid` guard.
    stats.update(&first, &articles);
    assert_eq!(stats.article_count(1), Some(1));
    assert_eq!(stats.article_count(2), Some(0));
    assert_eq!(stats.article_count(5), None);

    // The watermark means a second pass only sees the users and articles added since.
    stats.update(&extended_roster, &articles);
    assert_eq!(stats.article_count(5), Some(0));
    let mut extended = articles.clone();
    extended.push(Article {
        aid: 103,
        sid: 1,
        uid: 5,
        visible: true,
        ..Article::default()
    });
    stats.update(&extended_roster, &extended);
    assert_eq!(stats.article_count(5), Some(1));
    assert_eq!(stats.covered_users(), 4);

    stats.article_cnt_inc(1, -1);
    assert_eq!(stats.article_count(1), Some(0));
    assert!(!stats.article_cnt_inc(99, 1));
}

#[test]
fn a_reload_does_not_recount_the_users_already_covered() {
    // The regression: `update_stats` used to hand the whole roster over every tick, which
    // appended ids below `last_uid` and broke the sorted table the lookups assume.
    let all = roster();
    let articles = vec![Article {
        aid: 7,
        sid: 1,
        uid: 30,
        visible: true,
        ..Article::default()
    }];

    let mut stats = UserStatMap::new();
    for _ in 0..3 {
        stats.update(&all, &articles);
    }
    assert_eq!(stats.covered_users(), 5);
    assert_eq!(stats.article_count(30), Some(1));
}

#[test]
fn uid_list_resumes_where_the_last_pass_stopped() {
    let roster = roster();
    assert_eq!(roster.uid_list_from(1, 10), vec![10, 11, 12, 20, 30]);
    assert_eq!(roster.uid_list_from(12, 2), vec![12, 20]);
    assert!(roster.uid_list_from(31, 10).is_empty());
}

/// `user_stat.c:54-64`: the watermark moves with the users a pass actually loaded, and with
/// none when it loaded none. The line `8e02db3` replaced ran even for `i == 0`, reading
/// `uid_list[-1]` — four bytes below the array — and publishing whatever was there.
#[test]
fn an_empty_window_leaves_the_watermark_where_it_was() {
    let nobody = Roster::load(&[]);
    let mut stats = UserStatMap::new();
    stats.update(&nobody, &[]);
    assert_eq!(
        stats.last_uid, -1,
        "a pass that loaded no user publishes no uid"
    );
    assert_eq!(stats.next_uid(), 0, "and asks for the same window again");

    stats.update(&roster(), &[]);
    let seen = stats.last_uid;
    assert_eq!(seen, 30);

    stats.update(&nobody, &[]);
    assert_eq!(stats.last_uid, seen, "and an empty pass cannot move it");
    assert_eq!(stats.next_uid(), seen + 1);
}

/// The map `build_map` (`tests/test_user_stat.c:65-79`) seeds: uids 1, 2, 3 and 5
/// with counts 0, 5, 0 and 7, so uid 4 is the gap, uid 5 the last entry and two stored users
/// read as zero.
fn build_map(last_uid: i32, last_article_index: i32) -> UserStatMap {
    UserStatMap {
        stats: vec![(1, 0), (2, 5), (3, 0), (5, 7)],
        last_uid,
        last_article_index,
    }
}

/// `test_lookup` (`test_user_stat.c:112-158`), whose stated point is that reading a count
/// must not depend on where the search settles: the two zero counts are found, and every
/// neighbour of a stored uid — below the first, in the gap, after the last, far above, and
/// the negative — reports no record rather than the slot next to it.
#[test]
fn the_reference_lookup_table() {
    let stats = build_map(100, -1);
    assert_eq!(
        UserStatMap::new().covered_users(),
        0,
        "user_stat_map_init: user_count"
    );
    assert_eq!(UserStatMap::new().last_uid, -1, "map_init: last_uid");
    assert_eq!(
        UserStatMap::new().last_article_index,
        -1,
        "map_init: last_article_index"
    );

    for (uid, want) in [(1, Some(0)), (2, Some(5)), (3, Some(0)), (5, Some(7))] {
        assert_eq!(stats.article_count(uid), want, "the count of uid {uid}");
    }
    for uid in [0, 4, 6, 1_000_000, -1] {
        assert_eq!(stats.article_count(uid), None, "no record for uid {uid}");
    }

    // A single entry is both the first and the last of the list.
    let only_one = UserStatMap {
        stats: vec![(1, 0)],
        ..build_map(100, -1)
    };
    assert_eq!(only_one.article_count(1), Some(0), "get of a single entry");
    assert_eq!(only_one.article_count(2), None, "get past a single entry");
    assert_eq!(
        only_one.article_count(3),
        None,
        "get far past a single entry"
    );

    // Counting up follows the same search and reports a miss instead of writing one.
    let mut stats = build_map(100, -1);
    assert!(stats.article_cnt_inc(3, 1), "inc uid 3");
    assert!(stats.article_cnt_inc(3, 4), "inc uid 3 again");
    assert_eq!(stats.article_count(3), Some(5), "increments accumulate");
    assert!(stats.article_cnt_inc(1, -2), "inc uid 1 by a negative");
    assert_eq!(stats.article_count(1), Some(-2), "the count went negative");

    for uid in [4, 6, 0] {
        assert!(
            !stats.article_cnt_inc(uid, 1),
            "inc of the absent uid {uid}"
        );
    }
    assert_eq!(stats.covered_users(), 4, "an absent uid adds no entry");
    assert_eq!(
        stats.article_count(2),
        Some(5),
        "an absent uid left uid 2 alone"
    );
    assert_eq!(
        stats.article_count(5),
        Some(7),
        "an absent uid left uid 5 alone"
    );
}

/// `test_update_without_users` and `test_update_at_the_end`
/// (`test_user_stat.c:162-222`), over the same five articles the C file builds: uids
/// 1, 1, 2, 3, 4 of which the third is hidden. No user is ever loaded in a test run, so the
/// scan runs against a map the caller seeded and the watermarks have to stay where they are.
#[test]
fn the_reference_scan_table() {
    let article = |uid: i32, visible: bool| Article {
        aid: 0,
        sid: 1,
        uid,
        visible,
        ..Article::default()
    };
    let articles = vec![
        article(1, true),
        article(1, true),
        article(2, false),
        article(3, true),
        article(4, true),
    ];
    let nobody = Roster::load(&[]);

    let mut stats = build_map(100, -1);
    stats.update(&nobody, &articles);
    assert_eq!(stats.covered_users(), 4, "an empty pool keeps user_count");
    assert_eq!(stats.last_uid, 100, "an empty pool keeps last_uid");
    assert_eq!(
        stats.last_article_index, 4,
        "the cursor is the last article scanned"
    );
    assert_eq!(
        stats.article_count(1),
        Some(2),
        "both of uid 1's articles counted"
    );
    assert_eq!(
        stats.article_count(2),
        Some(5),
        "uid 2's only article is hidden"
    );
    assert_eq!(stats.article_count(3), Some(1), "uid 3 counted once");
    assert_eq!(
        stats.article_count(5),
        Some(7),
        "uid 5 has no article at all"
    );

    // A pass over no new article changes nothing.
    stats.update(&nobody, &articles);
    assert_eq!(stats.last_article_index, 4, "nothing new: cursor");
    assert_eq!(stats.article_count(1), Some(2), "nothing new: uid 1");
    assert_eq!(stats.article_count(3), Some(1), "nothing new: uid 3");

    // An author above `last_uid` waits for the pass that loads it.
    let mut stats = build_map(2, -1);
    stats.update(&nobody, &articles);
    assert_eq!(stats.last_uid, 2, "low last_uid: still nothing loaded");
    assert_eq!(
        stats.article_count(1),
        Some(2),
        "low last_uid: uid 1 is covered"
    );
    assert_eq!(
        stats.article_count(2),
        Some(5),
        "low last_uid: its only article is hidden"
    );
    assert_eq!(
        stats.article_count(3),
        Some(0),
        "low last_uid: uid 3 is above it, so its article is skipped"
    );
    assert_eq!(
        stats.article_count(5),
        Some(7),
        "low last_uid: uid 5 has no article"
    );

    // A map that already reached the end of the block recounts nothing.
    let mut stats = build_map(100, 4);
    stats.article_cnt_inc(1, 41);
    stats.update(&nobody, &articles);
    assert_eq!(
        stats.article_count(1),
        Some(41),
        "update at the end recounts nothing"
    );
    assert_eq!(
        stats.article_count(5),
        Some(7),
        "update at the end leaves the last entry"
    );
    assert_eq!(stats.last_uid, 100, "update at the end: last_uid");
    assert_eq!(stats.last_article_index, 4, "update at the end: cursor");
}
