use super::*;

fn entry() -> Entry {
    Entry {
        aid: 472407,
        title: Some("测试标题".to_string()),
        sname: Some("Test".to_string()),
        s_title: Some("水区".to_string()),
        username: Some("leaflet".to_string()),
        sub_dt: NaiveDateTime::parse_from_str("2026-11-20 08:09:10", "%Y-%m-%d %H:%M:%S").ok(),
    }
}

#[test]
fn board_row_lays_out_rank_board_date_and_author() {
    // `gen_top.php:53-62`, measured against the PHP with these inputs.
    let (section, _) = render(&entry(), 1).unwrap();
    assert_eq!(
        section,
        format!(
            " \x1b[1;37m第 \x1b[31m 1 \x1b[37m名 版块 : \x1b[33m水区 [Test]{} \
                 \x1b[37m【 \x1b[32mNov 20 08:09:10 \x1b[37m】\x1b[35m{}leaflet ",
            " ".repeat(12), // 20 - str_length("水区") - strlen("Test")
            " ".repeat(9)   // 16 - strlen("leaflet")
        )
    );
}

#[test]
fn article_row_pads_the_title_to_sixty_cells() {
    // `gen_top.php:64-67`: eight cells of title leave fifty-two of padding, then two spaces.
    let (_, article) = render(&entry(), 1).unwrap();
    assert_eq!(
        article,
        format!(
            " \x1b[1;37m       标题 : \x1b[44;37m测试标题{}  \x1b[0;40;37m",
            " ".repeat(52)
        )
    );
}

#[test]
fn long_title_is_cut_at_sixty_cells() {
    let title = "中".repeat(50); // 100 cells
    let row = Entry {
        title: Some(title),
        ..entry()
    };
    let (_, article) = render(&row, 1).unwrap();
    assert!(article.contains(&"中".repeat(30))); // 60 cells, the rest gone
    assert!(!article.contains(&"中".repeat(31)));
}

#[test]
fn rank_is_the_third_menu_row_of_its_pair() {
    let (_, menu) = assemble(&[entry(), entry()]).unwrap();
    assert!(menu.contains("@LOCATE_ARTICLE   4, 3, 1, 0,   \"Test|472407\""));
    assert!(menu.contains("@LOCATE_ARTICLE   6, 3, 1, 0,   \"Test|472407\""));
    assert!(menu.ends_with("%\n"));
}

#[test]
fn empty_result_is_the_two_headers() {
    let (text, menu) = assemble(&[]).unwrap();
    assert_eq!(text, TEXT_HEADER);
    assert_eq!(menu, format!("{}%\n{}%\n", MENU_HEADER, SCREEN_HEADER));
}

#[test]
fn a_wide_board_title_fails_the_way_str_repeat_did() {
    let row = Entry {
        s_title: Some("超".repeat(12)), // 24 cells, over the 20 minus "Test"
        ..entry()
    };
    let failure = render(&row, 1).unwrap_err();
    assert_eq!(failure.code, batch::EXIT_FATAL);
    assert!(failure.message.starts_with("str_repeat(): Argument #2"));
}

#[test]
fn a_null_datetime_prints_a_blank_stamp() {
    // The PHP prints the current time here; the column is NOT NULL in practice, and the
    // port's `Option` keeps the query honest about what the schema allows.
    let row = Entry {
        sub_dt: None,
        ..entry()
    };
    let (section, _) = render(&row, 1).unwrap();
    assert!(section.contains("【 \x1b[32m"));
}

#[test]
fn query_failure_keeps_the_php_step_and_status() {
    let failure = Failure::query("Query data error: ", &sqlx::Error::RowNotFound);
    assert_eq!(failure.code, batch::EXIT_QUERY);
    assert!(failure.message.starts_with("Query data error: "));
}

/// [`SQL`] with the seven-day window widened to ten years: the live board has nothing inside a
/// week, and the ten rows this returns are what the PHP's own widened copy was given.
const TEN_YEARS_SQL: &str = "SELECT AID, bbs.title AS title, sname,
			section_config.title AS s_title, username, sub_dt
			FROM bbs INNER JOIN section_config ON bbs.SID = section_config.SID
			WHERE section_config.recommend AND TID = 0 AND visible AND view_count >= 10
			AND (sub_dt >= SUBDATE(NOW(), INTERVAL 3650 DAY))
			ORDER BY excerption DESC, (view_count + reply_count) DESC, transship
			LIMIT 10";

/// The ranked rows against the reference tool's files, which is the only way the populated
/// path is seen: the sandbox copy of `gen_top.php` runs the same widened query and writes
/// `/tmp/phpgen/relaxed_top/` —
/// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored gen_top`.
/// Both files are also written to `/tmp/lbbs-top/` for a hand diff when the oracle is
/// not there.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn the_ranked_rows_are_the_php_bytes() {
    let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let text = std::fs::read_to_string(&conf).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = crate::db::Db::connect(&config.db).await.unwrap();

    let rows: Vec<Entry> = sqlx::query_as(TEN_YEARS_SQL)
        .fetch_all(db.pool())
        .await
        .expect("the ranking query runs");
    assert!(!rows.is_empty(), "the widened window should find rows");

    let (text_file, menu_file) = assemble(&rows).unwrap();
    let out_dir = std::path::Path::new("/tmp/lbbs-top");
    std::fs::create_dir_all(out_dir).unwrap();
    std::fs::write(out_dir.join("bbs_top.txt"), &text_file).unwrap();
    std::fs::write(out_dir.join("bbs_top_menu.conf"), &menu_file).unwrap();

    for (name, generated) in [
        ("bbs_top.txt", &text_file),
        ("bbs_top_menu.conf", &menu_file),
    ] {
        let oracle = std::path::Path::new("/tmp/phpgen/relaxed_top").join(name);
        if oracle.exists() {
            assert_eq!(
                std::fs::read_to_string(&oracle).unwrap(),
                *generated,
                "{name} differs from the PHP"
            );
        }
    }
}
