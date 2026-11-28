use super::*;

use lbbs::session::now;

#[test]
fn an_empty_bind_address_is_every_interface() {
    let mut cfg = Config {
        bbs_id: "t".into(),
        bbs_name: "t".into(),
        bbs_server: String::new(),
        bbs_address: String::new(),
        telnet_port: 2323,
        ssh_port: 2222,
        max_client: 10,
        max_client_per_ip: 2,
        start_dt: String::new(),
        sys_id: 0,
        db: lbbs::config::DbConfig {
            host: String::new(),
            username: String::new(),
            password: String::new(),
            database: String::new(),
            timezone: String::new(),
        },
    };
    assert_eq!(listener_addr(&cfg, cfg.telnet_port), "0.0.0.0:2323");
    cfg.bbs_address = "127.0.0.1".into();
    assert_eq!(listener_addr(&cfg, cfg.ssh_port), "127.0.0.1:2222");
}

#[test]
fn a_missing_file_has_no_mtime() {
    assert_eq!(file_mtime(Path::new("/nonexistent/eula.txt")), None);
}

#[test]
fn eula_timestamp_is_the_c_one() {
    let dir = std::env::temp_dir().join(format!("lbbs-mtime-{}", now()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("eula.txt");
    std::fs::write(&file, b"agreement").unwrap();
    let tm = file_mtime(&file).unwrap();
    assert!((tm - now()).abs() < 60, "{tm} vs {}", now());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_existing_directory_is_not_an_error() {
    let dir = std::env::temp_dir().join(format!("lbbs-mkdir-{}", now()));
    assert!(create_dir(&dir).is_ok());
    // The second call is the EEXIST the original forgave.
    assert!(create_dir(&dir).is_ok());
    assert!(std::fs::metadata(&dir).unwrap().is_dir());
    std::fs::remove_dir(&dir).unwrap();
}

#[test]
fn a_menu_file_reports_c_read_failure() {
    let err = read_menu_file(
        Path::new("/nonexistent"),
        "var/menu_merged.conf",
        "bbs_menu",
    )
    .unwrap_err();
    assert!(
        err.message.starts_with("load_menu(bbs_menu) error: "),
        "{}",
        err.message
    );
}

#[test]
fn the_exit_codes_are_the_ones_main_c_returned() {
    // -1 and -2 as the shell sees them, plus arg_error()'s 1.
    assert_eq!(
        (EXIT_OK, EXIT_USAGE, EXIT_STARTUP, EXIT_CONFIG),
        (0, 1, 255, 254)
    );
    assert_eq!(bbsd_code(-1), EXIT_STARTUP);
    assert_eq!(bbsd_code(-2), EXIT_CONFIG);
    assert_eq!(bbsd_code(0), EXIT_OK);
}

/// `return -1` from `main()` is status 255; the port keeps the C numbers in one place.
fn bbsd_code(c_code: i32) -> u8 {
    c_code as u8
}
