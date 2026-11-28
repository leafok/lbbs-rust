use super::*;
use crate::section_list::Section;

fn counts(article_count: i64, section_master: &str) -> Counts {
    Counts {
        article_count,
        section_master: section_master.to_string(),
    }
}

fn section(sid: i16, name: &str, title: &str) -> Section {
    Section {
        sid,
        name: name.to_string(),
        title: title.to_string(),
        comment: String::new(),
        read_user_level: 0,
        write_user_level: 0,
    }
}

fn group(cid: i16, cname: &str, c_title: &str, rows: Vec<(Section, Counts)>) -> Group<Counts> {
    Group {
        cid,
        name: cname.to_string(),
        title: c_title.to_string(),
        sections: rows,
    }
}

#[test]
fn class_field_is_bracketed_and_padded_to_ten_cells() {
    assert_eq!(class_field("Water").unwrap(), "[Water]     ");
    assert_eq!(class_field("水区").unwrap(), "[水区]      ");
}

#[test]
fn class_title_lays_count_name_and_bracket_out() {
    // `gen_section_menu.php:98-100`: `5 - intval(log10(12))` is four spaces, and the class
    // name is padded on its raw byte length, so `Test` leaves twenty-eight spaces.
    assert_eq!(
        class_title(12, "Test", "水区").unwrap(),
        format!("    12 ＋  Test{}[水区]", " ".repeat(28))
    );
}

#[test]
fn zero_count_is_padded_as_one_and_printed_as_zero() {
    // A count of 0 is measured as if it were 1, which is `5 - intval(log10(1))` spaces.
    assert_eq!(count_pad(0).unwrap(), "     ");
    assert_eq!(
        section_title(0, "Test", &class_field("C").unwrap(), "T", "").unwrap(),
        format!(
            "     0 ＋  Test{}{}T{}诚征版主中",
            " ".repeat(16),
            class_field("C").unwrap(),
            " ".repeat(21)
        )
    );
}

#[test]
fn a_master_replaces_the_wanted_text() {
    assert_eq!(
        section_title(3, "Test", &class_field("C").unwrap(), "T", "boss").unwrap(),
        format!(
            "     3 ＋  Test{}{}T{}boss",
            " ".repeat(16),
            class_field("C").unwrap(),
            " ".repeat(21)
        )
    );
}

#[test]
fn ex_rows_put_the_generation_date_where_the_master_was() {
    let tm = NaiveDateTime::parse_from_str("2026-10-15 16:57:05", "%Y-%m-%d %H:%M:%S").ok();
    let groups = vec![Group {
        cid: 1,
        name: "Test".to_string(),
        title: "水区".to_string(),
        sections: vec![(
            section(7, "Board", "板块"),
            ExCounts {
                ex_menu_tm: tm,
                ex_article_count: 12,
            },
        )],
    }];
    let out = ex_class_menus(&groups).unwrap();
    // `:283-295`: the block opens with the divider and the class's own `%menu` header.
    assert!(out.starts_with("#---------------------------------------------------------------------\n%menu M_EX_Test\ntitle       0, 0, \"精华区 > 水区\"\n"));
    assert!(out.contains(&format!(
            "@LIST_EX_SECTION   4, 4, 1, 0,   \"Board\",    \"    12 ＋  Board{}[水区]{}板块{}2026.10.15\"\n",
            " ".repeat(15),
            " ".repeat(6),
            " ".repeat(18)
        )));
}

#[test]
fn an_empty_digest_board_is_dropped_and_the_next_one_takes_row_four() {
    let groups = vec![Group {
        cid: 1,
        name: "Test".to_string(),
        title: "C".to_string(),
        sections: vec![
            (
                section(1, "A", "a"),
                ExCounts {
                    ex_menu_tm: None,
                    ex_article_count: 0,
                },
            ),
            (
                section(2, "B", "b"),
                ExCounts {
                    ex_menu_tm: None,
                    ex_article_count: 1,
                },
            ),
            (
                section(3, "C", "c"),
                ExCounts {
                    ex_menu_tm: None,
                    ex_article_count: 2,
                },
            ),
        ],
    }];
    let out = ex_class_menus(&groups).unwrap();
    assert!(!out.contains("\"A\""));
    assert!(out.contains("@LIST_EX_SECTION   4, 4, 1, 0,   \"B\""));
    assert!(out.contains("@LIST_EX_SECTION   0, 4, 1, 0,   \"C\""));
}

#[test]
fn only_the_first_board_of_the_flat_list_sits_on_row_four() {
    let groups = vec![
        group(
            1,
            "One",
            "a",
            vec![
                (section(1, "A", "a"), counts(1, "m")),
                (section(2, "B", "b"), counts(2, "m")),
            ],
        ),
        group(2, "Two", "b", vec![(section(3, "C", "c"), counts(3, "m"))]),
    ];
    let out = flat_menus(&groups, false).unwrap();
    assert_eq!(out.matches("@LIST_SECTION   4, 4, 1, 0,").count(), 1);
    assert_eq!(out.matches("@LIST_SECTION   0, 4, 1, 0,").count(), 2);
}

#[test]
fn the_favourite_list_carries_the_sid_in_field_three() {
    let groups = vec![group(
        1,
        "One",
        "a",
        vec![(section(42, "A", "a"), counts(1, "m"))],
    )];
    let out = flat_menus(&groups, true).unwrap();
    assert!(out.starts_with("@LIST_SECTION   4, 4, 42, 0,   \"A\",    \""));
}

#[test]
fn the_six_blocks_appear_in_the_php_order() {
    let groups = vec![group(
        1,
        "One",
        "a",
        vec![(section(1, "A", "a"), counts(1, "m"))],
    )];
    let out = assemble(&groups, &[]).unwrap();
    for marker in [
        "%menu M_EGROUP",
        "!M__One",
        "%menu M__One",
        "%menu M_ANNOUNCE",
        "%menu M_BOARD",
        "use_filter",
        "%menu M_FAVOR_SECTION",
    ] {
        assert!(out.contains(marker), "{marker} missing");
    }
    assert!(out.find("%menu M__One").unwrap() < out.find("%menu M_ANNOUNCE").unwrap());
    assert!(out.find("%menu M_BOARD").unwrap() < out.find("%menu M_FAVOR_SECTION").unwrap());
    // An empty digest pass contributes its headers but no class items.
    assert!(!out.contains("!M_EX_One"));
    assert!(!out.contains("%menu M_EX_One"));
}

#[test]
fn quotes_in_a_title_get_the_backslash_the_php_added() {
    assert!(class_field("他\"说").unwrap().starts_with("[他\\\"说]"));
}

#[test]
fn a_class_wider_than_its_field_fails_like_str_repeat() {
    let failure = class_field(&"超".repeat(6)).unwrap_err();
    assert_eq!(failure.code, batch::EXIT_FATAL);
}

#[test]
fn the_two_list_errors_keep_their_step_and_status() {
    let failure = list_error("Query section list error", sqlx::Error::RowNotFound);
    assert_eq!(failure.code, EXIT_DEEP);
    assert!(failure.message.starts_with("Query section list error: "));
}
