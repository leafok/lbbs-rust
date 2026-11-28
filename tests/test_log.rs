use super::*;

#[test]
fn info_goes_to_the_common_stream_only() {
    let (common, error) = captured(|| {
        tracing::info!("欢迎光临 {}", "Example BBS");
    });
    assert!(common.contains("[INFO] 欢迎光临 Example BBS\n"), "{common}");
    assert!(error.is_empty(), "{error}");
    // C flushed every line and wrote exactly one newline per call.
    assert_eq!(common.matches('\n').count(), 1, "{common}");
}

#[test]
fn the_error_stream_names_the_call_site() {
    let (common, error) = captured(|| {
        tracing::error!("open({}) failed", "var/menu.conf");
        tracing::debug!("idle");
        tracing::warn!("slow");
    });
    assert!(common.is_empty(), "{common}");
    assert!(error.contains("[ERROR] [lbbs::log::tests"), "{error}");
    assert!(error.contains("open(var/menu.conf) failed"), "{error}");
    assert!(error.contains("[DEBUG] "), "{error}");
    // WARN has no C counterpart and rides the error stream.
    assert!(error.contains("[WARN] "), "{error}");
    assert_eq!(error.matches('\n').count(), 3, "{error}");
}

/// The visitor takes the message and nothing else, which is only sound while every value
/// sits inside that message the way C's format string held it. A count passed as a field
/// would vanish from the line without failing anything, so the counts the loader reports are
/// formatted in — and this is what would silently disappear if they were not.
#[test]
fn a_value_carried_as_a_field_never_reaches_the_line() {
    let count = 4_096;
    let (inline, _) = captured(|| {
        tracing::info!("initially loaded {count} articles (last_aid=101)");
    });
    assert!(
        inline.contains("[INFO] initially loaded 4096 articles (last_aid=101)\n"),
        "{inline}"
    );

    let (dropped, _) = captured(|| {
        tracing::info!(count, "initially loaded articles");
    });
    assert!(
        !dropped.contains("4096"),
        "the field form is what the call sites must not use: {dropped}"
    );
}

#[test]
fn a_line_is_cut_at_the_buffer_limit() {
    let message = "x".repeat(LINE_BUFFER_LEN * 2);
    let line = format_line(&Level::INFO, "net_server.c", 410, &message);
    assert_eq!(line.len(), LINE_BUFFER_LEN - 1, "{}", line.len());
    assert_eq!(line.last(), Some(&b'\n'));
    assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);

    // A short line keeps its head and gains exactly one newline.
    let text = String::from_utf8(format_line(&Level::ERROR, "io.c", 88, "bye")).unwrap();
    assert!(text.ends_with("[ERROR] [io.c:88] bye\n"), "{text}");
    assert_eq!(text.matches('[').count(), 4, "{text}");
}

#[test]
fn the_head_carries_utc_time_and_the_pid() {
    let line = String::from_utf8(format_line(&Level::INFO, "main.c", 1, "hello")).unwrap();
    let mut fields = line.split("] [");
    let stamp = fields.next().unwrap().strip_prefix('[').unwrap();
    assert_eq!(stamp.len(), 19, "{stamp}");
    assert_eq!(&stamp[4..5], "-", "{stamp}");
    assert_eq!(fields.next().unwrap(), std::process::id().to_string());
    assert!(fields.next().unwrap().starts_with("INFO] hello\n"));
}

#[test]
fn a_redirected_stream_survives_a_restart() {
    let dir = temp_server_root("redirect", true);
    let sink = begin(&dir).unwrap();
    sink.redirect_to_console();

    // The C flags make log_restart() a no-op once redirected, so the files stay untouched.
    assert_eq!(sink.restart(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join(LOG_FILE_INFO)).unwrap(),
        ""
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn restart_picks_up_a_rotated_file() {
    let dir = temp_server_root("rotate", true);
    let sink = begin(&dir).unwrap();
    let common = dir.join(LOG_FILE_INFO);

    sink.common.write_line(b"first\n");
    assert_eq!(std::fs::read_to_string(&common).unwrap(), "first\n");

    // What logrotate with copytruncate leaves behind: the open descriptor still points at
    // the truncated file, so the path has to be reopened.
    std::fs::write(&common, "external\n").unwrap();
    assert_eq!(sink.restart(), 0);
    sink.common.write_line(b"second\n");
    assert_eq!(
        std::fs::read_to_string(&common).unwrap(),
        "external\nsecond\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_log_directory_is_an_error() {
    // C relies on the packaged tree, so a missing log/ directory is an error, not a mkdir.
    let dir = temp_server_root("missing-log-dir", false);
    let err = begin(&dir).unwrap_err();
    assert_eq!(err.exit_code(), -1);
    assert!(
        err.to_string()
            .starts_with(&format!("fopen({LOG_FILE_INFO}) error: ")),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A directory that looks like a server root, with or without its log directory.
fn temp_server_root(name: &str, with_log_dir: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lbbs-log-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if with_log_dir {
        std::fs::create_dir_all(dir.join("log")).unwrap();
    }
    dir
}
