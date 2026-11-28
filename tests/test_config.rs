use super::*;

#[test]
fn parses_live_style_config_with_crlf_and_comments() {
    let text = "bbs_id LBBS-devel\r\nbbs_name MyBBS\r\nbbs_port 2323\r\n\
                    bbs_max_client 1000 # Keep it as low as possible\r\n\
                    bbs_start_dt 2000-10-30\r\nbbs_sys_id 600\r\n\r\n";
    let (cfg, warns) = parse(text);
    assert_eq!(cfg.bbs_id, "LBBS-devel");
    assert_eq!(cfg.bbs_name, "MyBBS");
    assert_eq!(cfg.telnet_port, 2323);
    assert_eq!(cfg.max_client, 1000);
    assert_eq!(cfg.start_dt, "2000年10月30日");
    assert_eq!(cfg.sys_id, 600);
    assert!(warns.0.is_empty(), "{warns:?}");
}

#[test]
fn keys_are_case_insensitive() {
    let (cfg, _) = parse("DB_HOST db.example.com\nBBS_Name Whatever\n");
    assert_eq!(cfg.db.host, "db.example.com");
    assert_eq!(cfg.bbs_name, "Whatever");
}

#[test]
fn start_dt_pads_to_minimum_width() {
    let (cfg, _) = parse("bbs_start_dt 2000-1-5\n");
    assert_eq!(cfg.start_dt, "2000年 1月 5日");
}

#[test]
fn out_of_range_client_limits_fall_back_to_the_hard_limit() {
    let (cfg, warns) = parse("bbs_max_client 0\nbbs_max_client_per_ip 99999\n");
    assert_eq!(cfg.max_client, MAX_CLIENT_LIMIT);
    assert_eq!(cfg.max_client_per_ip, MAX_CLIENT_PER_IP_LIMIT);
    assert_eq!(warns.0.len(), 2);
}

#[test]
fn above_cap_client_limit_resets_to_the_cap() {
    let (cfg, warns) = parse("bbs_max_client 20000\n");
    assert_eq!(cfg.max_client, MAX_CLIENT_LIMIT);
    assert_eq!(warns.0.len(), 1);
}

#[test]
fn client_limit_caps_match_the_reference() {
    assert_eq!(MAX_CLIENT_LIMIT, 10000);
    assert_eq!(MAX_CLIENT_PER_IP_LIMIT, 10000);
    assert!(MAX_CLIENT_PER_IP_LIMIT <= MAX_CLIENT_LIMIT);
}

#[test]
fn extra_uncommented_token_rejects_the_line() {
    let (cfg, warns) = parse("bbs_port 2323 2324\nbbs_sys_id 7\n");
    assert_eq!(
        cfg.telnet_port, 23,
        "line with an extra token must be skipped"
    );
    assert_eq!(cfg.sys_id, 7);
    assert_eq!(warns.0.len(), 1);
}

#[test]
fn unknown_keys_are_logged_and_ignored() {
    let (cfg, warns) = parse("bbs_nonsense hi\n");
    assert_eq!(cfg.bbs_id, "Example BBS");
    assert_eq!(
        warns.0,
        vec!["Unknown config bbs_nonsense = hi".to_string()]
    );
}

#[test]
fn atoi_stops_at_the_first_non_digit() {
    assert_eq!(c_atoi("2323abc"), 2323);
    assert_eq!(c_atoi("  -12x"), -12);
    assert_eq!(c_atoi("abc"), 0);
    assert_eq!(c_atoi("+7"), 7);
}

#[test]
fn truncation_does_not_split_a_utf8_char() {
    assert_eq!(c_trunc("abcd", 2), "ab");
    assert_eq!(c_trunc("中文", 1), "");
    assert_eq!(c_trunc("中文", 4), "中");
}
