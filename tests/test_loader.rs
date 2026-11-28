use super::*;
use crate::paths::{ARTICLE_TITLE_MAX_LEN, MASTER_LIST_BYTES, NICKNAME_MAX_LEN, USERNAME_MAX_LEN};

fn board() -> Board {
    let mut board = Board::default();
    board.create_section(1, "test", "Test", "m ");
    board.create_section(2, "other", "Other", "");
    board
}

fn row(aid: i32, sid: i16) -> ArticleRow {
    ArticleRow {
        aid,
        tid: 0,
        sid,
        cid: aid,
        uid: 1,
        visible: true,
        excerption: false,
        ontop: false,
        lock: false,
        transship: false,
        username: Some("tester".into()),
        nickname: Some(" Tester ".into()),
        title: Some(format!("t{aid}")),
        sub_dt: Some(1000 + i64::from(aid)),
        sub_ip: None,
        content: None,
    }
}

fn reply_to(aid: i32, sid: i16, tid: i32) -> ArticleRow {
    ArticleRow {
        tid,
        ..row(aid, sid)
    }
}

fn op(mid: i32, aid: i32, kind: &str) -> OpRow {
    OpRow {
        mid,
        aid,
        kind: kind.to_owned(),
    }
}

fn section_row(sid: i16, sname: &str, title: &str) -> SectionRow {
    SectionRow {
        sid,
        sname: Some(sname.to_owned()),
        title: Some(title.to_owned()),
        cid: 7,
        read_user_level: 2,
        write_user_level: 3,
        enable: true,
        ex_menu_tm: None,
    }
}

#[test]
fn a_loaded_row_keeps_its_columns_apart() {
    let board = board();
    let article = article_from_row(&row(5, 1), &board);
    assert_eq!(article.sub_dt, 1005);
    assert_eq!(article.title, "t5");
    assert_eq!(article.username, "tester");
    assert_eq!(article.nickname, " Tester ");
    assert_eq!(article.cid, 5);
    assert!(article.visible);
    // The links stay the arena's to fill, which is what `memset` used to zero.
    assert_eq!(article.prior, 0);
    assert_eq!(article.topic_next, 0);
}

#[test]
fn labels_are_cut_to_the_c_buffer_sizes() {
    let board = board();
    let mut long = row(6, 1);
    long.username = Some("abcdefghijklm".into());
    long.nickname = Some("昵".repeat(20));
    long.title = Some("标".repeat(60));

    let article = article_from_row(&long, &board);
    assert_eq!(article.username, "abcdefghijkl");
    assert_eq!(article.username.len(), USERNAME_MAX_LEN);
    // A multi-byte label loses the character that would not fit whole.
    assert_eq!(article.nickname, "昵".repeat(13));
    assert_eq!(article.nickname.len(), NICKNAME_MAX_LEN - 1);
    assert_eq!(article.title, "标".repeat(53));
    assert_eq!(article.title.len(), ARTICLE_TITLE_MAX_LEN - 1);
}

#[test]
fn a_reply_to_an_absent_or_hidden_topic_is_loaded_invisible() {
    let mut board = board();
    let mut head = row(10, 1);
    head.visible = false;
    append_articles_into(&mut board, &[head]).unwrap();

    let detached = article_from_row(&reply_to(11, 1, 999), &board);
    assert_eq!(detached.tid, 0);
    assert!(!detached.visible);

    let hidden_head = article_from_row(&reply_to(11, 1, 10), &board);
    assert_eq!(hidden_head.tid, 0);
    assert!(!hidden_head.visible);

    // The same row against a visible head keeps its topic.
    board.set_article_visible(1, 10, true).unwrap();
    let attached = article_from_row(&reply_to(11, 1, 10), &board);
    assert_eq!(attached.tid, 10);
    assert!(attached.visible);
}

#[test]
fn an_unknown_board_stops_the_article_load() {
    let mut board = board();
    let err = append_articles_into(&mut board, &[row(1, 1), row(2, 7)]).unwrap_err();
    assert!(matches!(err, LoaderError::UnknownSection));
    // The article that did fit is still there, which is what the C loop breaks on.
    assert_eq!(board.article_count(), 1);
}

#[test]
fn a_row_the_arena_refuses_ends_the_batch_without_an_error() {
    let mut board = board();
    // Two rows with the same `AID` cannot happen in the database, but they are the
    // cheapest way to reach the failure the original breaks on: the arena only takes a
    // strictly newer aid.
    let appended = append_articles_into(&mut board, &[row(1, 1), row(1, 1), row(2, 1)]).unwrap();
    assert_eq!(appended, 1);
    assert_eq!(board.article_count(), 1);
}

#[test]
fn deleting_a_topic_hides_its_replies_and_re_indexes_the_pages() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1), row(3, 1)]).unwrap();

    let replay = apply_ops(&mut board, &[op(7, 1, "D")], &[]);
    assert_eq!(replay.applied, 1);
    assert!(replay.stop.is_none());

    let section = board.section(1).unwrap();
    assert!(!board.article_by_aid(1).unwrap().visible);
    assert!(!board.article_by_aid(2).unwrap().visible);
    assert!(board.article_by_aid(3).unwrap().visible);
    assert_eq!(section.visible_article_count, 1);
    assert_eq!(section.visible_topic_count, 1);
    assert_eq!(section.last_page_visible_article_count, 1);
    // `calculate_page` starts the re-index at the op's own article, which is invisible
    // now, so page 0 is opened by it even though nothing visible is there.
    assert_eq!(section.page_first, vec![0]);
}

#[test]
fn restoring_a_topic_does_not_restore_its_replies() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

    apply_ops(&mut board, &[op(7, 1, "X")], &[]);
    let replay = apply_ops(&mut board, &[op(8, 1, "S")], &[]);
    assert_eq!(replay.applied, 1);

    assert!(board.article_by_aid(1).unwrap().visible);
    // Hiding cascades, showing does not, so the reply stays out until its own op arrives.
    assert!(!board.article_by_aid(2).unwrap().visible);
}

#[test]
fn an_op_for_an_article_that_is_not_loaded_holds_the_watermark() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();

    let ops = [op(5, 1, "L"), op(6, 9, "X"), op(7, 1, "U")];
    let replay = apply_ops(&mut board, &ops, &[]);
    assert_eq!(replay.applied, 1);
    assert_eq!(replay.stop, Some(Stop::NotLoaded));
    // Only the first op went through, so the lock is on and the delete never was.
    assert!(board.article_by_aid(1).unwrap().lock);
    assert!(board.article_by_aid(1).unwrap().visible);
}

/// `section_list_loader.c:755-762`: the loader child prints one line per config pass, and the
/// success line is `log_common()` — the only C message the port routes to `bbs.log`.
#[test]
fn a_config_pass_announces_itself_once() {
    let (ok_common, ok_error) = crate::log::captured(|| {
        Loader::report_config_pass(Ok(()));
    });
    assert!(
        ok_common.contains("Reload section config successfully\n"),
        "{ok_common}"
    );
    assert_eq!(ok_error, "", "{ok_error}");

    // The failure line names the reason, which is the message a live board's log is read
    // for when a rename does not show up.
    let (bad_common, bad_error) = crate::log::captured(|| {
        Loader::report_config_pass(Err(LoaderError::UnknownSection));
    });
    assert_eq!(bad_common, "", "{bad_common}");
    assert!(bad_error.contains("[WARN] "), "{bad_error}");
    assert!(
        bad_error.contains("section config reload failed: "),
        "{bad_error}"
    );
}

/// `user_list_pool_reload(1)` publishes the list and the visit total from one swap
/// (`user_list.c:623-631` then `:654-660`), and the total always comes from that same pass:
/// `:396-403` writes `atoi(row[0])` when a row came back and `0` when none did, so an empty
/// `user_login_log` is published as an empty total rather than left at the previous value.
#[tokio::test]
async fn the_online_pass_publishes_the_visit_total_with_the_list() {
    let session = crate::session::offline_session();
    let state = &session.state;
    let loader = Loader::new(state.clone(), std::path::Path::new("/srv/lbbs"));

    loader.publish_online_pass(OnlineList::default(), Some(123_456));
    assert_eq!(state.login_count(), 123_456);

    loader.publish_online_pass(OnlineList::default(), None);
    assert_eq!(state.login_count(), 0, "no row means the total is 0");
}

/// Only a cursor makes a pass reach the op log: `watermark` is `None` until the startup pass
/// reads it, and a loader that never read it returns before its first query, so new articles
/// still arrive through the AID scan while no `'X'`, `'M'` or `'E'` ever does. The C child
/// got that value from the `fork()`; the daemon gets it by giving [`Loader::run`] the loader
/// that ran [`Loader::initial_load`], which is what this test pins the cost of losing.
#[tokio::test]
async fn a_loader_without_a_cursor_never_reaches_the_op_log() {
    let session = crate::session::offline_session();
    let root = std::path::Path::new("/srv/lbbs");

    let mut fresh = Loader::new(session.state.clone(), root);
    assert_eq!(
        fresh.replay_ops().await.unwrap(),
        0,
        "no cursor means the pass ends before the query"
    );

    let mut seeded = Loader::new(session.state, root);
    seeded.watermark = Some(1);
    assert!(
        seeded.replay_ops().await.is_err(),
        "with one, the same call reaches an unreachable database"
    );
}

#[test]
fn flag_letters_apply_to_the_board_the_article_is_loaded_in() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();

    let ops = [op(3, 1, "E"), op(4, 1, "Z"), op(5, 1, "F"), op(6, 1, "U")];
    let replay = apply_ops(&mut board, &ops, &[]);
    assert!(replay.stop.is_none());
    assert_eq!(replay.applied, 4);

    let article = board.article_by_aid(1).unwrap();
    assert!(article.excerption);
    assert!(article.transship);
    assert!(article.ontop);
    assert!(!article.lock);
    assert_eq!(board.section(1).unwrap().ontop_article_count(), 1);
    assert_eq!(board.section(2).unwrap().ontop_article_count(), 0);
}

#[test]
fn unsetting_a_sticky_article_takes_it_out_of_the_list() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), row(2, 1)]).unwrap();

    apply_ops(&mut board, &[op(1, 1, "F"), op(2, 2, "F")], &[]);
    assert_eq!(board.section(1).unwrap().ontop, vec![0, 1]);

    apply_ops(&mut board, &[op(3, 1, "V")], &[]);
    assert_eq!(board.section(1).unwrap().ontop, vec![1]);
    assert!(!board.article_by_aid(1).unwrap().ontop);
}

#[test]
fn a_modify_brings_the_new_cid_and_clears_the_digest_flag() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();
    board.set_article_excerption(1, 1, true).unwrap();

    let content = ContentRow {
        cid: 77,
        sub_ip: None,
        content: None,
    };
    let extras = [OpExtra {
        content: Some(content),
        dest_sid: None,
    }];
    let replay = apply_ops(&mut board, &[op(4, 1, "M")], &extras);
    assert!(replay.stop.is_none());

    let article = board.article_by_aid(1).unwrap();
    assert_eq!(article.cid, 77);
    assert!(!article.excerption);
}

#[test]
fn a_modify_without_its_content_row_ends_the_batch() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), row(2, 1)]).unwrap();

    let ops = [op(4, 1, "M"), op(5, 1, "L")];
    let replay = apply_ops(&mut board, &ops, &[]);
    assert_eq!(replay.stop, Some(Stop::ContentMissing));
    // The CID is zeroed but the op is not counted, so the next pass starts from it again.
    assert_eq!(replay.applied, 0);
    assert_eq!(board.article_by_aid(1).unwrap().cid, 0);
    assert!(!board.article_by_aid(2).unwrap().lock);
}

#[test]
fn a_move_follows_the_sid_stored_in_bbs() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

    let ops = [op(3, 1, "T")];
    let to_other = [OpExtra {
        dest_sid: Some(2),
        ..Default::default()
    }];
    assert!(apply_ops(&mut board, &ops, &to_other).stop.is_none());
    // The head and its reply move together, and the arena keeps the new board.
    assert_eq!(board.article_by_aid(1).unwrap().sid, 2);
    assert_eq!(board.article_by_aid(2).unwrap().sid, 2);
    assert_eq!(board.section(1).unwrap().article_count, 0);
    assert_eq!(board.section(2).unwrap().article_count, 2);

    // Replaying the op finds the topic already where the log says, so it is skipped
    // rather than reported as a failure.
    assert!(apply_ops(&mut board, &ops, &to_other).stop.is_none());
    assert_eq!(board.section(2).unwrap().article_count, 2);

    // No destination row means the article was deleted under the op.
    let replay = apply_ops(&mut board, &ops, &[OpExtra::default()]);
    assert_eq!(replay.applied, 1);
    assert!(replay.stop.is_none());
}

#[test]
fn a_move_to_an_unknown_board_asks_for_a_config_reload() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();

    let ops = [op(3, 1, "T")];
    let extras = [OpExtra {
        dest_sid: Some(9),
        ..Default::default()
    }];
    let replay = apply_ops(&mut board, &ops, &extras);
    assert_eq!(replay.stop, Some(Stop::UnknownSection));
    assert_eq!(board.article_by_aid(1).unwrap().sid, 1);
}

#[test]
fn a_move_the_board_refuses_is_retried_from_the_same_op() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1), reply_to(2, 1, 1)]).unwrap();

    // `section_list_move_topic` works on a topic head, and aid 2 is a reply.
    let ops = [op(3, 2, "T")];
    let extras = [OpExtra {
        dest_sid: Some(2),
        ..Default::default()
    }];
    let replay = apply_ops(&mut board, &ops, &extras);
    assert_eq!(replay.applied, 0);
    assert_eq!(replay.stop, Some(Stop::MoveLater));
    assert_eq!(board.article_by_aid(2).unwrap().sid, 1);
}

#[test]
fn an_unknown_letter_is_processed_and_counted() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();

    let replay = apply_ops(&mut board, &[op(2, 1, "Q"), op(3, 1, "A")], &[]);
    assert_eq!(replay.applied, 2);
    assert!(replay.stop.is_none());
}

#[test]
fn a_config_load_renames_a_board_without_touching_its_articles() {
    let mut board = board();
    append_articles_into(&mut board, &[row(1, 1)]).unwrap();

    let rows = vec![
        section_row(1, "renamed", "Renamed"),
        section_row(3, "new", "New"),
    ];
    let masters = vec!["alice bob ".to_owned(), String::new()];
    load_section_config_into(&mut board, &rows, &masters).unwrap();

    let section = board.section_by_name("renamed").unwrap();
    assert_eq!(section.sid, 1);
    assert_eq!(section.stitle, "Renamed");
    assert_eq!(section.master_list, "alice bob ");
    assert_eq!(section.article_count, 1);
    assert_eq!(section.class_id, 7);
    assert!(section.enable);
    assert_eq!(section.read_user_level, 2);
    assert_eq!(section.write_user_level, 3);

    // A board the config gained starts empty.
    let fresh = board.section(3).unwrap();
    assert_eq!(fresh.article_count, 0);
    assert!(fresh.page_first.is_empty());
    assert!(fresh.head.is_none());
}

#[test]
fn a_config_load_cuts_the_master_list_to_its_buffer() {
    let mut board = Board::default();
    let masters = vec!["user_one user_two user_three ".repeat(4)];
    load_section_config_into(&mut board, &[section_row(1, "test", "Test")], &masters).unwrap();

    // `char master_list[(BBS_username_max_len + 1) * 3 + 1]`, the NUL excluded.
    assert_eq!(
        board.section(1).unwrap().master_list.len(),
        MASTER_LIST_BYTES
    );
}

#[test]
fn a_second_load_of_the_same_config_does_not_duplicate_boards() {
    let mut board = Board::default();
    let rows = vec![
        section_row(1, "test", "Test"),
        section_row(2, "other", "Other"),
    ];
    let masters = vec![String::new(), String::new()];
    load_section_config_into(&mut board, &rows, &masters).unwrap();
    load_section_config_into(&mut board, &rows, &masters).unwrap();

    assert_eq!(board.sections.len(), 2);
}

/// `section_list_loader.c:162-179`: the row's stamp decides, `enable` gates, and a menu that
/// will not read leaves the board with no tree and its stamp behind, so the next pass retries.
#[test]
fn a_digest_menu_loads_when_its_stamp_moves_and_not_otherwise() {
    let dir = std::env::temp_dir().join(format!("lbbs-gen-ex-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let menu = concat!(
        "%S_EX_DIR\n",
        "%\n",
        "%menu M_EX_1_0\n",
        "screen 2, 0, S_EX_DIR\n",
        "@VIEW_EX_ARTICLE 4, 4, 1, 0, \"100\", \"(100)\"\n",
        "%\n",
    );
    std::fs::write(dir.join("1"), menu).unwrap();
    // Board 2 has a file that is not a menu, and board 3 has none at all.
    std::fs::write(dir.join("2"), "%menu M\nscreen 2, 0, S_NONE\n%\n").unwrap();

    let mut board = Board::default();
    let mut rows = vec![
        section_row(1, "test", "Test"),
        section_row(2, "broken", "Broken"),
        section_row(3, "gone", "Gone"),
        section_row(4, "closed", "Closed"),
        section_row(5, "unstamped", "Unstamped"),
    ];
    for row in &mut rows[..4] {
        row.ex_menu_tm = Some(1000);
    }
    rows[3].enable = false;
    let masters = vec![String::new(); rows.len()];
    load_section_config_into(&mut board, &rows, &masters).unwrap();
    load_gen_ex_menus_into(&mut board, &rows, &dir);

    let set = board
        .section(1)
        .unwrap()
        .ex_menu_set
        .clone()
        .expect("loaded");
    assert_eq!(
        board.section(1).unwrap().ex_menu_tm,
        1000,
        "the stamp moves on"
    );
    assert_eq!(
        set.items[0].name, "100",
        "the name is the AID the item passes on"
    );

    for sid in [2, 3] {
        let section = board.section(sid).unwrap();
        assert!(
            section.ex_menu_set.is_none(),
            "{sid} kept a tree it never read"
        );
        assert_eq!(section.ex_menu_tm, 0, "{sid} was stamped by a failed load");
    }
    // A disabled board is skipped before the stamp is looked at (`:164`), and a NULL
    // `ex_menu_tm` is `atol` of nothing: neither is due for anything.
    assert_eq!(board.section(4).unwrap().ex_menu_tm, 0);
    assert_eq!(board.section(5).unwrap().ex_menu_tm, 0);

    // The same stamp again: the file behind the board moved on, and the tree did not, because
    // C only reloads while the row is *newer* than what the board holds.
    std::fs::write(dir.join("1"), menu.replace("(100)", "(rewritten)")).unwrap();
    load_gen_ex_menus_into(&mut board, &rows, &dir);
    let set = board.section(1).unwrap().ex_menu_set.clone().unwrap();
    assert_eq!(
        set.items[0].text, "(100)",
        "an unchanged stamp does not reload"
    );

    // A newer one does. Board 2 is retried on every pass all along: its stamp never advanced,
    // so the row stays newer than what the board holds.
    rows[0].ex_menu_tm = Some(2000);
    load_gen_ex_menus_into(&mut board, &rows, &dir);
    let set = board.section(1).unwrap().ex_menu_set.clone().unwrap();
    assert_eq!(set.items[0].text, "(rewritten)");
    assert_eq!(board.section(1).unwrap().ex_menu_tm, 2000);
    assert!(board.section(2).unwrap().ex_menu_set.is_none());

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The shape the daemon uses: `root.join(VAR_ARTICLE_CACHE_DIR)` as a string, whose trailing
/// separator a Windows `PathBuf` does not keep. Appending one here made every `dir_entries`
/// assertion below true whether or not the file landed inside the directory.
fn temp_cache_dir(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("lbbs-loader-cache-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

fn dir_entries(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

/// A row as the bulk query returns it: the joined `bbs_content` body and the address the
/// loader masks on the way to the footer.
fn cached_row(aid: i32, sid: i16) -> ArticleRow {
    ArticleRow {
        sub_ip: Some("203.0.113.9".into()),
        content: Some(format!("line {aid}\n")),
        ..row(aid, sid)
    }
}

const SITE: &str = "MyBBS";
const SERVER: &str = "www.bbs.test";

#[test]
fn the_rows_a_pass_appended_get_a_cache_file() {
    let dir = temp_cache_dir("rows");
    let mut board = board();
    let rows = [cached_row(31, 1), cached_row(32, 1)];
    assert_eq!(append_articles_into(&mut board, &rows).unwrap(), 2);

    assert_eq!(
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows),
        2
    );
    assert_eq!(dir_entries(&dir), ["31", "32"]);

    let cached = cache::load(&dir, 31).unwrap().unwrap();
    let text = String::from_utf8_lossy(&cached.data).into_owned();
    assert!(
        text.starts_with("发布者: tester ( Tester ), 版块: test (Test)\n标  题: t31\n"),
        "{text}"
    );
    assert!(text.contains("line 31\n"), "{text}");
    // One octet masked, which is the `ip_mask(sub_ip, 1, '*')` the C loader runs first.
    assert!(text.contains("[FROM: 203.0.113.*]"), "{text}");
}

#[test]
fn an_existing_cache_file_is_kept() {
    let dir = temp_cache_dir("existing");
    let mut board = board();
    let rows = [cached_row(33, 1)];
    append_articles_into(&mut board, &rows).unwrap();
    assert_eq!(
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows),
        1
    );

    // C opens the file read-only and returns without writing; a second pass that found a
    // body already rendered must not touch it, only report nothing as new.
    std::fs::write(cache::path(&dir, 33), b"sentinel").unwrap();
    assert_eq!(
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows),
        0
    );
    assert_eq!(std::fs::read(cache::path(&dir, 33)).unwrap(), b"sentinel");
}

#[test]
fn a_row_without_a_content_column_renders_nothing() {
    let dir = temp_cache_dir("nocontent");
    let mut board = board();
    let rows = [row(34, 1)];
    append_articles_into(&mut board, &rows).unwrap();

    assert_eq!(
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows),
        0
    );
    assert!(dir_entries(&dir).is_empty());
}

#[test]
fn a_row_of_a_board_the_pool_lacks_renders_nothing() {
    let dir = temp_cache_dir("noboards");
    let board = board();
    let rows = [cached_row(36, 9)];

    // The same row is what makes `append_articles_into` return `UnknownSection`; the render
    // pass skips it instead of writing a header with no board name in it.
    assert_eq!(
        generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows),
        0
    );
    assert!(dir_entries(&dir).is_empty());
}

#[test]
fn a_replayed_modify_renders_the_new_cid() {
    let dir = temp_cache_dir("modify");
    let mut board = board();
    let rows = [cached_row(35, 1)];
    append_articles_into(&mut board, &rows).unwrap();
    generate_caches_from_rows(&dir, SITE, SERVER, &board, &rows);

    let ops = [op(9, 35, "M")];
    let extras = [OpExtra {
        content: Some(ContentRow {
            cid: 700,
            sub_ip: Some("198.51.100.7".into()),
            content: Some("改过的正文\n".into()),
        }),
        ..Default::default()
    }];
    let replay = apply_ops(&mut board, &ops, &extras);
    assert_eq!(replay.applied, 1);

    let rendered =
        generate_caches_from_modifies(&dir, SITE, SERVER, &board, &ops, &extras, replay.applied);
    assert_eq!(rendered, 1);
    assert_eq!(dir_entries(&dir), ["35", "700"]);

    let cached = cache::load(&dir, 700).unwrap().unwrap();
    let text = String::from_utf8_lossy(&cached.data).into_owned();
    assert!(text.contains("改过的正文"), "{text}");
    assert!(text.contains("标  题: t35"), "{text}");
    assert!(text.contains("[FROM: 198.51.100.*]"), "{text}");

    // A letter that is not a modify changed no body, and the ops past the one the replay
    // stopped on are not rendered at all.
    let ops = [op(10, 35, "L"), op(11, 35, "M")];
    let extras = [OpExtra::default(), extras[0].clone()];
    let replay = apply_ops(&mut board, &ops, &extras);
    assert_eq!(replay.applied, 2);
    assert_eq!(dir_entries(&dir), ["35", "700"]);
    assert_eq!(
        generate_caches_from_modifies(&dir, SITE, SERVER, &board, &ops, &extras, 1),
        0,
        "only the `'L'` op was in the reported range"
    );
}

/// Opt-in check against the real database, which is the only way to exercise the async
/// half of this module: `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
/// Every statement the loader runs is a `SELECT`, so this stays read-only.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn live_load_fills_the_pools() {
    let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let root = Path::new(&conf)
        .ancestors()
        .nth(2)
        .expect("LBBS_REF_CONF must be <root>/conf/bbsd.conf")
        .to_path_buf();
    let text = std::fs::read_to_string(&conf).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = Db::connect(&config.db).await.unwrap();
    let state = std::sync::Arc::new(AppState::new(config, db));

    let mut loader = Loader::new(state.clone(), &root);
    loader.initial_load().await.unwrap();
    assert!(loader.watermark.is_some());
    assert!(!loader.reload_config);

    let board = state.board();
    // Floors rather than exact counts, which is what makes this survive a live site: the
    // batch loop has to have run more than once for these to hold.
    assert!(board.sections.len() > 10);
    assert!(board.article_count() > 1000);
    // Every article the scan accepted is in exactly one board, which is what makes the
    // counters below add up.
    let in_boards: i32 = board.sections.iter().map(|s| s.article_count).sum();
    assert_eq!(i32::try_from(board.article_count()).unwrap(), in_boards);
    for section in board.sections.iter() {
        assert!(section.visible_article_count <= section.article_count);
        assert!(section.visible_topic_count <= section.topic_count);
        assert!(section.topic_count <= section.article_count);
        assert!(section.ontop.len() <= section.article_count as usize);
        // The first article of a board always opens page 0, even an invisible one, so a
        // board with articles has at least one page head and an empty board has none.
        assert_eq!(!section.page_first.is_empty(), section.article_count > 0);
        let mut aids = Vec::new();
        for art in &section.page_first {
            assert_eq!(board.articles[*art].sid, section.sid);
            aids.push(board.articles[*art].aid);
        }
        assert!(aids.windows(2).all(|w| w[0] < w[1]));
    }

    // The statistics map covers the uids the roster loaded, so at least one of them has a
    // counter to report.
    let mut posted = 0;
    for user in state.roster().users.iter() {
        posted += i32::from(board.user_article_count(user.uid).unwrap_or_default() > 0);
    }
    assert!(posted > 0);
    // The visit total the top page prints, from `user_login_count_load` (`:372`) — a board
    // that has logins recorded has a highest `ID`, and `initial_load` runs the online pass.
    assert!(
        state.login_count() > 0,
        "the online pass publishes 累计访问人次"
    );
    let newest_cid = board
        .article_by_aid(board.last_aid())
        .map(|article| article.cid);
    drop(board);

    // Every post the scan took needs a file for the reader to open, which is the render of
    // `:327`. The newest post is the one a tree that never ran the C loader would lack.
    let cache_dir = root.join(VAR_ARTICLE_CACHE_DIR);
    let cache_dir = cache_dir.to_string_lossy().into_owned();
    let cid = newest_cid.expect("the scan loaded at least one article");
    assert!(
        cache::load(&cache_dir, cid)
            .expect("read cache file")
            .is_some(),
        "cid {cid} has no cache file in {cache_dir}"
    );

    // One more pass with the watermark in place, which is the steady state: nothing new to
    // load, and the two user tables published again.
    let before = loader.watermark;
    let users = state.roster().count();
    loader.tick().await;
    assert!(loader.watermark >= before);
    assert_eq!(state.roster().count(), users);
    assert!(!state.board().sections.is_empty());
}
