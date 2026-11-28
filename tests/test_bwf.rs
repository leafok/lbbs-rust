use super::*;

fn write_tmp(name: &str, body: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("lbbs_bwf_{name}_{}", std::process::id()));
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn masks_the_whole_line_for_open_ended_patterns() {
    // The shipped config wraps entries in [^\n]*...[^\n]*, which erases the line.
    let path = write_tmp("line", b"[^\\n]*foo[^\\n]*\n");
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut buf = b"xx foo yy".to_vec();
    assert_eq!(bwf.mask(&mut buf, b'*'), 1);
    assert_eq!(buf, b"*********");
}

#[test]
fn masks_only_the_span_of_bare_patterns() {
    let path = write_tmp("bare", b"secret\n");
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut buf = b"a secret b secret".to_vec();
    assert_eq!(bwf.mask(&mut buf, b'*'), 2);
    assert_eq!(buf, b"a ****** b ******");
}

#[test]
fn case_is_folded_ascii_style_like_pcre2_without_utf() {
    let path = write_tmp("case", b"FUCK\n");
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut buf = b"how fuck you".to_vec();
    assert_eq!(bwf.mask(&mut buf, b'*'), 1);
}

#[test]
fn posix_space_class_matches_cjk_across_whitespace() {
    // Mirrors `法[[:space:]]*轮` from conf/badwords.conf.
    let path = write_tmp("posix", "[^\\n]*法[[:space:]]*轮[^\\n]*\n".as_bytes());
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut buf = "标题 法 轮 内容".as_bytes().to_vec();
    assert_eq!(bwf.mask(&mut buf, b'*'), 1);
    assert!(buf.iter().all(|&b| b == b'*'));
}

#[test]
fn crlf_and_blank_lines_are_tolerated() {
    let path = write_tmp("crlf", b"alpha\r\n\r\nbeta\r\n");
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(bwf.patterns, 2);
}

#[test]
fn overlong_pattern_is_reported_as_truncated() {
    let long = format!("{}x", "a".repeat(70));
    let path = write_tmp("long", long.as_bytes());
    assert!(matches!(
        Badwords::load(&path),
        Err(BwfError::Truncated { len: 71, .. })
    ));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn empty_config_never_loops() {
    let path = write_tmp("empty", b"");
    let bwf = Badwords::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let mut buf = b"hello".to_vec();
    assert_eq!(bwf.mask(&mut buf, b'*'), 0);
    assert_eq!(buf, b"hello");
}
