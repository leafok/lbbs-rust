use super::*;

fn dt(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()
}

#[test]
fn str_length_charges_two_cells_per_wide_character() {
    // The values the PHP gave for the strings the generators pad.
    assert_eq!(str_length("ab"), 2);
    assert_eq!(str_length("a b"), 3);
    assert_eq!(str_length("老站务"), 6);
    assert_eq!(str_length("水区"), 4);
    assert_eq!(str_length("测试标题"), 8);
    assert_eq!(str_length("精华区/老站务/"), 14);
    assert_eq!(str_length(""), 0);
    assert_eq!(str_length("a\tb"), 3); // a tab is one cell, as in PHP
}

#[test]
fn first_line_stops_at_the_newline_and_drops_control_characters() {
    // `split_line($s, "", $width, 1, "")` with a one-line limit.
    assert_eq!(first_line("abc\ndef", 55), "abc");
    assert_eq!(first_line("a\rb\u{7}c", 55), "abc");
    assert_eq!(first_line("", 55), "");
    assert_eq!(first_line("a\tb", 55), "a\tb");
}

#[test]
fn first_line_never_splits_a_wide_character() {
    // Five cells take two characters and leave the third out, six take all three.
    assert_eq!(first_line("测试测试测试", 5), "测试");
    assert_eq!(first_line("测试测试测试", 6), "测试测");
    assert_eq!(
        str_length(&first_line(
            "第一行很长很长很长的标题内容需要截断掉一部分才行呢",
            12
        )),
        12
    );
}

#[test]
fn addslashes_backslashes_the_four_php_escapes() {
    // The byte-for-byte result `addslashes("a'b\"c\\d\0e")` gives.
    assert_eq!(addslashes("a'b\"c\\d\0e"), "a\\'b\\\"c\\\\d\\0e");
    assert_eq!(addslashes("水区"), "水区");
}

#[test]
fn a_negative_pad_is_the_php_value_error() {
    assert_eq!(pad(55, 9).unwrap(), " ".repeat(46));
    assert_eq!(pad(55, 55).unwrap(), "");
    let error = pad(20, 24).unwrap_err();
    assert_eq!(
        error.message,
        "str_repeat(): Argument #2 ($times) must be greater than or equal to 0\n"
    );
    assert_eq!(error.code, EXIT_FATAL);
}

#[test]
fn a_null_column_is_the_empty_string_php_saw() {
    assert_eq!(text(&None), "");
    assert_eq!(text(&Some(String::new())), "");
    assert_eq!(text(&Some("水区".to_string())), "水区");
}

#[test]
fn the_two_php_date_formats_render_as_php_did() {
    // `Nov` is PHP's English month abbreviation, whatever the locale of the cron run.
    assert_eq!(
        format_datetime(&dt("2026-11-20 08:09:10"), "M d H:i:s"),
        "Nov 20 08:09:10"
    );
    assert_eq!(
        format_datetime(&dt("2026-11-20 08:09:10"), "Y.m.d"),
        "2026.11.20"
    );
}

#[test]
fn a_null_datetime_becomes_the_current_time() {
    // PHP's deprecated `new DateTimeImmutable(null)`: the notice is dropped, the time kept.
    assert_eq!(
        format_datetime(&None, "Y.m.d"),
        Local::now().naive_local().format("%Y.%m.%d").to_string()
    );
}

#[test]
fn a_query_failure_carries_the_php_prefix_and_status() {
    let failure = Failure::query("Query data error: ", &sqlx::Error::RowNotFound);
    assert!(failure.message.starts_with("Query data error: "));
    assert_eq!(failure.code, EXIT_QUERY);
    assert_eq!(EXIT_FATAL as i32, -1);
    assert_eq!(-1i8 as u8, 255);
    assert_eq!(-2i8 as u8, 254);
    assert_eq!(-3i8 as u8, 253);
}

#[test]
fn a_failed_write_is_the_reason_for_leaving() {
    let dir = std::env::temp_dir().join(format!("lbbs-batch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("menu");
    write_file(&path, "%menu M_TEST\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "%menu M_TEST\n");

    // The directory the C daemon assumes already exists: `var/gen_ex/`.
    let missing = dir.join("no-such-dir").join("18");
    let error = write_file(&missing, "").unwrap_err();
    // `write_file` prints the path as the platform spells it — backslashes on Windows — so the
    // prefix is read off the same `Path` instead of hand-joining it with a `/`.
    assert!(error.message.starts_with(&format!(
        "file_put_contents({}): Failed to open stream: ",
        missing.display()
    )));
    assert_eq!(error.code, EXIT_FATAL);
    std::fs::remove_dir_all(&dir).unwrap();
}
