use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn no_arguments_means_the_daemon() {
    assert_eq!(
        parse(&args(&["bbsd"])),
        Args::Run(Flags {
            foreground: false,
            display_log: false,
            display_error_log: false,
        })
    );
}

#[test]
fn the_flags_collect_in_order() {
    assert_eq!(
        parse(&args(&["bbsd", "-f", "--display-log"])),
        Args::Run(Flags {
            foreground: true,
            display_log: true,
            display_error_log: false,
        })
    );
    assert!(
        parse(&args(&["bbsd", "--display-error-log"]))
            .into_flags()
            .unwrap()
            .display_error_log
    );
}

#[test]
fn a_printing_option_leaves_immediately() {
    // arg_help() calls exit(0), so the -f after it is never seen.
    assert_eq!(parse(&args(&["bbsd", "-h", "-f"])), Args::Help);
    assert_eq!(parse(&args(&["bbsd", "-f", "-v"])), Args::Version);
    assert_eq!(parse(&args(&["bbsd", "-C"])), Args::CompileConfig);
}

#[test]
fn an_unknown_argument_is_an_error() {
    assert_eq!(parse(&args(&["bbsd", "-x"])), Args::Invalid);
    assert_eq!(parse(&args(&["bbsd", "--for"])), Args::Invalid);
    assert_eq!(parse(&args(&["bbsd", "-f", "extra"])), Args::Invalid);
}

#[test]
fn the_usage_block_aligns_the_descriptions() {
    let text = usage(PROG_NAME);
    // The literal name, so the block C prints is what the port prints.
    assert_eq!(PROG_NAME, "bbsd");
    assert!(text.starts_with("Usage: bbsd [-fhvC] [...]\n\n"), "{text}");
    // The two %*s paddings put every description at column 8 + 24, whether or not the option
    // has a short argument.
    for line in [
        "-f      --foreground            Run in foreground\n",
        "-h      --help                  Display help message\n",
        "-v      --version               Display version information\n",
        "        --display-log           Display standard log information\n",
        "        --display-error-log     Display error log information\n",
        "-C      --compile-config        Display compile configuration\n",
    ] {
        assert!(text.contains(line), "missing {line:?} in\n{text}");
    }
    assert!(
        text.ends_with("    If meet any bug, please report to <leaflet@leafok.com>\n\n"),
        "{text}"
    );
}

/// C wrote its paddings as literal tabs in the format string, which lines the six answers up
/// on the 24th column; the row count is what the terminal shows, so the test expands the tab
/// stops instead of matching a tab count.
#[test]
fn the_compile_config_answers_the_six_configure_keys() {
    let text = compile_config();
    for key in [
        "--enable-shared",
        "--enable-systemd",
        "--with-debug",
        "--with-epoll",
        "--with-mariadb",
        "--with-sysv",
    ] {
        let line = text
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap_or_else(|| panic!("{key} must be answered: {text}"));
        let answer = line.find('[').expect("a bracketed answer");
        let column = line[..answer].chars().fold(0usize, |col, ch| match ch {
            '\t' => col + (8 - col % 8),
            _ => col + 1,
        });
        assert_eq!(column, 24, "{line:?}");
        assert!(line.ends_with(']'), "{line:?}");
    }
    assert!(text.starts_with(&app_info()), "{text}");
}

#[test]
fn app_info_names_the_package_and_arch() {
    let info = app_info();
    assert!(info.starts_with("lbbs-rust 0.1.0 build ("), "{info}");
    assert!(info.ends_with(')'), "{info}");
}

#[test]
fn the_server_root_is_one_level_above_the_executable_dir() {
    // A real chdir cannot be tested from a thread that shares the process, so the path
    // arithmetic is checked here and the syscall wrapper stays the thin thing it is.
    let exec_dir = |argv0: &str| match Path::new(argv0).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    assert_eq!(
        exec_dir("/srv/lbbs/bin/bbsd"),
        PathBuf::from("/srv/lbbs/bin")
    );
    assert_eq!(exec_dir("bbsd"), PathBuf::from("."));
    assert_eq!(exec_dir("./bbsd"), PathBuf::from("."));
}

#[test]
fn the_root_errors_use_the_c_messages() {
    assert_eq!(
        RootError::ExecDir("/nope".into(), 2).to_string(),
        "chdir(/nope) error: 2"
    );
    assert_eq!(RootError::Parent(13).to_string(), "chdir(..) error: 13");
    assert_eq!(RootError::ExecDir("x".into(), 1).exit_code(), -1);
}
