use super::*;

fn dt(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()
}

#[test]
fn a_directory_block_names_itself_by_sid_and_fid() {
    assert_eq!(
        menu_header(18, 0, "水区"),
        "#---------------------------------------------------------------------\n\
             %menu M_EX_18_0\n\
             title       0, 0, \"精华区 > 水区\"\n\
             screen      2, 0, S_EX_DIR\n\
             page        4, 1, 20\n"
    );
}

#[test]
fn a_directory_line_keeps_the_raw_path_and_pads_the_name() {
    // `gen_ex_list.php:37-44`: six cells of `老站务` leave forty-nine of padding.
    assert_eq!(
        dir_item(
            18,
            23,
            "精华区/老站务/",
            "老站务",
            &dt("2026-01-02 03:04:05"),
            4
        )
        .unwrap(),
        format!(
            "!M_EX_18_23   4, 4, 1, 0, \"精华区/老站务/\", \" [目录]  老站务{} 2026.01.02\"\n",
            " ".repeat(49)
        )
    );
}

#[test]
fn an_article_line_right_aligns_the_number_over_seven_cells() {
    // `gen_ex_list.php:66-74`: six digits take one space, four cells of title take fifty-one.
    assert_eq!(
        article_item(472407, "标题", &dt("2026-11-20 08:09:10"), 0).unwrap(),
        format!(
            "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"472407\", \" 472407  标题{} 2026.11.20\"\n",
            " ".repeat(51)
        )
    );
}

#[test]
fn a_small_aid_is_padded_more_than_a_large_one() {
    // Six spaces in front of a one-digit number, none in front of a seven-digit one.
    assert!(
        article_item(1, "t", &None, 0)
            .unwrap()
            .starts_with("@VIEW_EX_ARTICLE   0, 4, 1, 0, \"1\", \"      1  t")
    );
    assert!(
        article_item(1234567, "t", &None, 0)
            .unwrap()
            .starts_with("@VIEW_EX_ARTICLE   0, 4, 1, 0, \"1234567\", \"1234567  t")
    );
}

#[test]
fn escaping_a_quote_does_not_recount_the_field() {
    // PHP measures the fifty-five cells before the backslash appears, so the field grows.
    assert_eq!(
        article_item(9, "\"x", &dt("2026-11-20 08:09:10"), 0).unwrap(),
        format!(
            "@VIEW_EX_ARTICLE   0, 4, 1, 0, \"9\", \"      9  \\\"x{} 2026.11.20\"\n",
            " ".repeat(53)
        )
    );
}

#[test]
fn the_screen_block_is_the_php_bytes() {
    assert!(SCREEN.starts_with(
        "#---------------------------------------------------------------------\n%S_EX_DIR\n"
    ));
    assert!(SCREEN.contains("\x1b[44;37m    \x1b[1;37m编  号"));
    assert!(SCREEN.ends_with(&format!("{}%\n", "\n".repeat(20))));
    assert_eq!(SCREEN.len(), 318);
}

#[test]
fn the_board_file_is_named_by_its_sid() {
    assert_eq!(output_path(18), "var/gen_ex/18");
}

/// `SELECT title FROM section_config WHERE SID = ?`: the board's name, which is the title of
/// the root menu block `:8-15` prints.
const BOARD_TITLE_SQL: &str = "SELECT title FROM section_config WHERE SID = ?";

/// The tree of two real boards against the reference tool's own files. The live run is
/// read-only, so the daemon's `ex_menu_update` flag is never set for it to find: the sandbox
/// copy of `gen_ex_list.php` selects these two boards by `SID`, skips its `UPDATE`, and its
/// output is what this compares against —
/// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored gen_ex`.
/// Each tree is also written to `/tmp/lbbs-gen_ex/` so the two files can be diffed by
/// hand when the oracle directory is not there.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn a_real_digest_tree_is_the_php_bytes() {
    let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let text = std::fs::read_to_string(&conf).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = crate::db::Db::connect(&config.db).await.unwrap();

    let out_dir = std::path::Path::new("/tmp/lbbs-gen_ex");
    std::fs::create_dir_all(out_dir).unwrap();

    for sid in [13i16, 1] {
        let title: Option<String> = sqlx::query_scalar(BOARD_TITLE_SQL)
            .bind(sid)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let buffer = gen_ex_dir(&db, sid, 0, "", batch::text(&title))
            .await
            .expect("the tree generates")
            .expect("the board's queries succeeded");
        let file = format!("{SCREEN}{buffer}");
        std::fs::write(out_dir.join(sid.to_string()), &file).unwrap();

        let oracle = std::path::Path::new("/tmp/phpgen/oracle_live2/gen_ex").join(sid.to_string());
        if oracle.exists() {
            assert_eq!(
                std::fs::read_to_string(&oracle).unwrap(),
                file,
                "board {sid} differs from the PHP"
            );
        }
    }
}
