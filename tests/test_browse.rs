use super::*;
use crate::roster::Roster;
use crate::term::Pipe;

/// 2023-11-14, a date whose month abbreviation is three cells in any zone.
const NOW: i64 = 1_700_000_000;
const DAY: i64 = 86_400;

fn post(aid: i32) -> Article {
    Article {
        aid,
        sid: 1,
        uid: 1,
        username: format!("user{aid}"),
        nickname: format!("nick{aid}"),
        title: format!("title {aid}"),
        sub_dt: NOW - 60,
        visible: true,
        ..Article::default()
    }
}

fn reply(aid: i32, tid: i32) -> Article {
    let mut article = post(aid);
    article.tid = tid;
    article
}

fn marked(aid: i32, ontop: bool) -> Article {
    let mut article = post(aid);
    article.ontop = ontop;
    article
}

fn board_with(articles: &[Article]) -> Board {
    let mut board = Board::default();
    assert!(
        board
            .create_section(1, "test", "测试板", "master")
            .is_some()
    );
    for article in articles {
        board.append_article(article).expect("append");
    }
    board
}

/// The page's rows as aids, which is what the assertions are about.
fn aids(board: &Board, page: &Page) -> Vec<i32> {
    page.rows
        .iter()
        .map(|&art| board.article(art).expect("row").aid)
        .collect()
}

fn page_of(board: &Board, page_id: i32) -> Page {
    let section = board.section(1).expect("section");
    query_section(board, section, page_id).expect("page")
}

/// The rows a page answers with, and where its sticky tail starts.
#[test]
fn the_first_page_holds_twenty_rows_oldest_first() {
    let articles: Vec<Article> = (100..125).map(post).collect();
    let board = board_with(&articles);

    let page = page_of(&board, 0);
    assert_eq!(aids(&board, &page), (100..120).collect::<Vec<i32>>());
    assert_eq!(page.count(), 20);
    assert_eq!(page.ontop_start_offset, 20);

    let page = page_of(&board, 1);
    assert_eq!(aids(&board, &page), (120..125).collect::<Vec<i32>>());
    // Two indexed pages, no stickies, so the with-sticky count is the index count.
    assert_eq!(page.page_count, 2);
}

#[test]
fn the_stickies_follow_the_rows_of_the_last_page() {
    let articles: Vec<Article> = (100..121).map(|aid| marked(aid, aid < 102)).collect();
    let board = board_with(&articles);

    // The indexed pages still show every article, stickies included …
    let page = page_of(&board, 0);
    assert_eq!(page.count(), 20);

    // … and the last one repeats them at the end, in the sticky list's own order.
    let page = page_of(&board, 1);
    assert_eq!(aids(&board, &page), vec![120, 100, 101]);
    assert_eq!(page.ontop_start_offset, 1);
    assert_eq!(page.page_count, 2);
}

#[test]
fn a_full_page_leaves_the_stickies_to_the_page_after_it() {
    let articles: Vec<Article> = (100..140).map(|aid| marked(aid, aid >= 130)).collect();
    let board = board_with(&articles);

    let page = page_of(&board, 1);
    assert_eq!(page.count(), 20);
    assert_eq!(
        page.ontop_start_offset, 20,
        "no room for a sticky on a full page"
    );

    let page = page_of(&board, 2);
    assert_eq!(aids(&board, &page), (130..140).collect::<Vec<i32>>());
    assert_eq!(page.ontop_start_offset, 0, "the whole page is stickies");
}

#[test]
fn a_page_number_outside_the_index_reports_itself() {
    let articles: Vec<Article> = (100..105).map(post).collect();
    let board = board_with(&articles);
    let section = board.section(1).unwrap();

    assert!(matches!(
        query_section(&board, section, 1),
        Err(BrowseError::OutOfRange { sid: 1, page_id: 1 })
    ));
    assert!(matches!(
        query_section(&board, section, -1),
        Err(BrowseError::OutOfRange { page_id: -1, .. })
    ));
}

#[test]
fn an_empty_board_has_a_page_of_nothing() {
    let board = board_with(&[]);
    let page = page_of(&board, 0);
    assert_eq!(page.count(), 0);
    assert_eq!(page.ontop_start_offset, ROWS as i32);
}

/// `locate_article_in_section` with `direction == 0`, the page and visible row of a post.
#[test]
fn locate_names_the_page_and_row_of_a_post() {
    let articles: Vec<Article> = (100..130).map(post).collect();
    let board = board_with(&articles);
    let section = board.section(1).unwrap();

    let located =
        locate_article(&board, section, board.article_index(123).unwrap(), 0, 0).expect("located");
    assert_eq!(located.page_id, 1);
    assert_eq!(located.offset, 3);
    // Page 1 is the last indexed one, which carries the sticky tail in its count.
    assert_eq!(located.count, 10);
}

#[test]
fn locate_reports_nothing_when_the_walk_leaves_the_thread() {
    let articles: Vec<Article> = (100..105).map(post).collect();
    let board = board_with(&articles);
    let section = board.section(1).unwrap();
    let art = |aid: i32| board.article_index(aid).unwrap();

    // Every topic head stands alone, so a step in either direction has nowhere to go.
    assert_eq!(locate_article(&board, section, art(102), 1, 1), None);
    assert_eq!(locate_article(&board, section, art(102), -1, 1), None);
    // A direction C never asks for keeps its aid at zero.
    assert_eq!(locate_article(&board, section, art(102), 2, 1), None);
    // … and row 0 of the page is where a plain locate of an existing post lands.
    assert_eq!(
        locate_article(&board, section, art(100), 0, 0).map(|l| (l.page_id, l.offset)),
        Some((0, 0))
    );
}

/// The topic walks, which is what `=` and `\` do inside the reader.
#[test]
fn the_topic_walk_counts_only_the_posts_of_its_thread() {
    let articles = vec![
        post(100),
        reply(101, 100),
        post(102),
        reply(103, 100),
        reply(104, 100),
    ];
    let board = board_with(&articles);
    let section = board.section(1).unwrap();
    let art = |aid: i32| board.article_index(aid).unwrap();

    // Forward one step from the topic head lands on its first reply; the thread's own
    // post 102, which sits between them in the board, is not part of the walk.
    let located = locate_article(&board, section, art(100), 1, 1).expect("next");
    assert_eq!((located.page_id, located.offset), (0, 1));

    // Backward from the first reply reaches the head.
    let located = locate_article(&board, section, art(101), -1, 1).expect("prior");
    assert_eq!((located.page_id, located.offset), (0, 0));

    // A whole step finds the newest visible post of the thread.
    let located = locate_article(&board, section, art(100), 1, ARTICLE_LIMIT_PER_SECTION)
        .expect("last of thread");
    assert_eq!((located.page_id, located.offset), (0, 4));

    // Nothing newer, nothing older: the walk comes back to the row it was asked from,
    // which C folds into "not found".
    assert_eq!(locate_article(&board, section, art(104), 1, 1), None);
    assert_eq!(locate_article(&board, section, art(100), -1, 1), None);
}

#[test]
fn a_hidden_post_is_stepped_over_inside_the_thread() {
    let articles = vec![post(100), reply(101, 100), reply(102, 100)];
    let mut board = board_with(&articles);
    board.set_article_visible(1, 101, false).expect("hide");

    let section = board.section(1).unwrap();
    let located = locate_article(&board, section, board.article_index(100).unwrap(), 1, 1)
        .expect("next visible");
    assert_eq!((located.page_id, located.offset), (0, 1));
}

#[test]
fn the_newest_visible_post_is_walked_back_from_the_tail() {
    let mut articles: Vec<Article> = (100..110).map(post).collect();
    for article in &mut articles[5..] {
        article.visible = false;
    }
    let board = board_with(&articles);
    let section = board.section(1).unwrap();

    assert_eq!(
        last_article(&board, section).map(|art| board.article(art).unwrap().aid),
        Some(104)
    );

    // A board with nothing to show has no last article, which is C's `0`.
    let hidden: Vec<Article> = (200..205)
        .map(|aid| Article {
            aid,
            sid: 1,
            visible: false,
            ..Article::default()
        })
        .collect();
    let board = board_with(&hidden);
    assert_eq!(last_article(&board, board.section(1).unwrap()), None);
}

#[test]
fn scan_unread_wraps_over_the_end_of_the_list() {
    let mut articles: Vec<Article> = (100..105).map(post).collect();
    for article in &mut articles {
        article.uid = 2;
    }
    articles[1].uid = 1; // this reader's own post, which C counts as read
    let board = board_with(&articles);

    let view_log = ViewLog::from_base(1, vec![100, 102]);
    let art = |aid: i32| board.article_index(aid).unwrap();

    // From 100: 101 is the reader's own, 102 is already read, so 103 answers.
    assert_eq!(
        scan_unread(&board, art(100), 1, &view_log).map(|a| board.article(a).unwrap().aid),
        Some(103)
    );
    // From 104 the walk wraps to the head and passes the same three posts again, so it
    // answers 103 as well — the wrap only matters for the newest row not being last.
    assert_eq!(
        scan_unread(&board, art(104), 1, &view_log).map(|a| board.article(a).unwrap().aid),
        Some(103)
    );
    // … and a reader who has seen everything gets nothing.
    let all = ViewLog::from_base(1, vec![100, 102, 103, 104]);
    assert_eq!(scan_unread(&board, art(104), 1, &all), None);
}

#[test]
fn the_author_scan_follows_the_direction_of_the_key() {
    let mut articles: Vec<Article> = (100..106).map(post).collect();
    articles[2].uid = 7;
    articles[2].username = "someone".into();
    articles[4].username = "Someone".into();
    let board = board_with(&articles);
    let art = |aid: i32| board.article_index(aid).unwrap();

    assert_eq!(
        scan_by_uid(&board, art(100), 1, 7).map(|a| board.article(a).unwrap().aid),
        Some(102)
    );
    assert_eq!(scan_by_uid(&board, art(102), -1, 7), None);
    assert_eq!(
        scan_by_uid(&board, art(102), -1, 1).map(|a| board.article(a).unwrap().aid),
        Some(101)
    );

    // An author id that is not in the user list any more is matched by name, case first.
    assert_eq!(
        scan_by_username(&board, art(100), 1, "SOMEONE").map(|a| board.article(a).unwrap().aid),
        Some(102)
    );
}

#[test]
fn the_title_scan_matches_anywhere_in_the_subject() {
    let mut articles: Vec<Article> = (100..104).map(post).collect();
    articles[1].title = "Re: 关于 ExampleBBS 的 标题".into();
    let board = board_with(&articles);
    let art = |aid: i32| board.article_index(aid).unwrap();

    assert_eq!(
        scan_by_title(&board, art(100), 1, "examplebbs").map(|a| board.article(a).unwrap().aid),
        Some(101)
    );
    // Backward starts at `prior`, so the row the reader is on never matches itself …
    assert_eq!(scan_by_title(&board, art(101), -1, "examplebbs"), None);
    // … and the walk reaches it from its neighbour.
    assert_eq!(
        scan_by_title(&board, art(102), -1, "examplebbs").map(|a| board.article(a).unwrap().aid),
        Some(101)
    );
    assert_eq!(scan_by_title(&board, art(100), 1, "没有这个标题"), None);
}

/// `strftime`, in the two shapes the year boundary selects.
#[test]
fn the_date_column_switches_at_a_year() {
    // Both shapes fill the same seven cells: `%b %e ` pads the day to two, `%m/%Y` does
    // not need to.
    let recent = row_time(NOW - 3600, NOW);
    assert_eq!(recent.chars().count(), 7, "%b %e plus its trailing space");
    assert!(recent.ends_with(' '));
    assert!(recent[..3].chars().all(|ch| ch.is_ascii_alphabetic()));

    let old = row_time(NOW - 400 * DAY, NOW);
    assert_eq!(old.chars().count(), 7, "%m/%Y");
    assert!(old.contains('/'));
    assert!(!old.ends_with(' '));

    // Exactly a year old is last year's shape, as in the C comparison.
    assert!(row_time(NOW - 3600 * 24 * 365, NOW).contains('/'));
    assert!(!row_time(NOW - 3600 * 24 * 365 + 1, NOW).contains('/'));
}

#[test]
fn a_repeated_reply_prefix_collapses() {
    let mut article = reply(2, 1);
    article.title = "Re: Re: Re: 讨论".into();
    assert_eq!(row_title(&article), "Re: ... 讨论");

    article.title = "Re: 讨论".into();
    assert_eq!(row_title(&article), "Re: 讨论");

    // The head of a thread keeps whatever it wrote, marker first.
    let mut article = post(1);
    article.title = "Re: Re: 讨论".into();
    assert_eq!(row_title(&article), "● Re: Re: 讨论");

    article.transship = true;
    assert_eq!(row_title(&article), "● [转载]Re: Re: 讨论");
}

#[test]
fn the_flag_column_reads_the_two_marker_tables() {
    let view_log = ViewLog::from_base(1, vec![100]);
    let mut article = post(100);

    // Read, and not the reader's own: a space.
    article.uid = 2;
    assert_eq!(article_flag(&article, 1, &view_log), ' ');

    // Unread: the capital.
    article.aid = 101;
    assert_eq!(article_flag(&article, 1, &view_log), 'N');

    // Your own posts are read posts, even before the log says so.
    article.aid = 100;
    article.uid = 1;
    assert_eq!(article_flag(&article, 1, &ViewLog::anonymous()), ' ');

    article.excerption = true;
    assert_eq!(article_flag(&article, 1, &ViewLog::anonymous()), 'm');
    article.uid = 2;
    assert_eq!(article_flag(&article, 1, &ViewLog::anonymous()), 'M');

    article.excerption = false;
    article.lock = true;
    assert_eq!(article_flag(&article, 1, &view_log), 'x');
    // A locked post nobody has read keeps its capital: `lock` only shows once it is read.
    assert_eq!(article_flag(&article, 1, &ViewLog::anonymous()), 'N');
    // The reader's own post counts as read, so it is the lowercase marker again.
    article.uid = 1;
    assert_eq!(article_flag(&article, 1, &ViewLog::anonymous()), 'x');
}

#[test]
fn only_a_topic_head_can_be_starred() {
    let favor = FavorLog::from_base(1, vec![100]);
    let topic = post(100);
    let reply = reply(101, 100);
    assert_eq!(favor_column(&topic, &favor), '@');
    assert_eq!(favor_column(&reply, &favor), ' ');

    let article = post(101);
    assert_eq!(favor_column(&article, &favor), ' ');
}

#[test]
fn the_thread_colours_its_head_and_its_replies() {
    assert_eq!(topic_color(&post(100), 100), "\x1b[1;33m");
    assert_eq!(topic_color(&reply(101, 100), 100), "\x1b[1;36m");
    assert_eq!(topic_color(&reply(102, 100), 101), "");
    assert_eq!(topic_color(&post(100), -1), "");
}

/// `%*s` with a negative width, which is what an over-long name produces.
#[test]
fn a_negative_padding_width_pads_to_its_absolute_value() {
    assert_eq!(padding(5), 5);
    assert_eq!(padding(-3), 3);
    assert_eq!(padding(0), 0);
}

#[test]
fn the_name_column_is_the_nickname_at_half_its_width() {
    let article = post(100);
    assert_eq!(name_field(false), USERNAME_MAX_LEN as i64);
    assert_eq!(name_field(true), (NICKNAME_MAX_LEN / 2) as i64);

    let (name, budget) = name_column(&article, false);
    assert_eq!(name, "user100");
    assert_eq!(budget, USERNAME_MAX_LEN as i64 - 7);

    let (name, budget) = name_column(&article, true);
    assert_eq!(name, "nick100");
    assert_eq!(budget, (NICKNAME_MAX_LEN / 2) as i64 - 7);
}

#[test]
fn a_pinned_row_replaces_the_number_with_the_notice_marker() {
    let article = post(12345);
    let view_log = ViewLog::anonymous();
    let favor = FavorLog::anonymous();
    let env = RowEnv {
        uid: 2,
        view_log: &view_log,
        favor: &favor,
        display_nickname: false,
        topic_tid: -1,
        fixed: false,
        now: NOW,
    };

    let line = row_line(&article, false, &env);
    assert!(
        line.starts_with("    12345\x1b[m N user12345"),
        "an ordinary row shows its aid: {line}"
    );

    let line = row_line(&article, true, &env);
    assert!(
        line.starts_with("   \x1b[1;33m[提示]\x1b[m N user12345"),
        "a pinned row shows 提示 and no aid: {line}"
    );

    // The thread being read keeps its colour in both shapes.
    let env = RowEnv {
        topic_tid: 12345,
        ..env
    };
    assert!(row_line(&article, false, &env).starts_with("  \x1b[1;33m  12345"));
    assert!(row_line(&reply(12346, 12345), true, &env).contains("\x1b[1;36mtitle 12346\x1b[m"));
}

#[test]
fn the_reader_keys_answer_with_the_move_they_stand_for() {
    let mut ctx = DisplayCtx::default();
    let mut topic_mode = false;

    // `p` flips the thread mode and stays inside the pager.
    let mut ch = KEY_LOWER_P;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 0);
    assert!(topic_mode);
    assert_eq!(ch, 0);
    assert_eq!(ctx.msg, MSG_TOPIC);

    // `Ctrl-X` is the same key, and the status row is rewritten on every other press.
    let mut ch = KEY_CTRL_X;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 0);
    assert!(!topic_mode);
    assert_eq!(ctx.msg, MSG_NORMAL);

    let mut ch = 0;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 0);
    assert_eq!(ctx.msg, MSG_NORMAL);

    // `r` leaves with the key itself, `=` and `\` likewise.
    for key in [KEY_R, KEY_EQUAL, KEY_BACKSLASH] {
        let mut ch = key;
        assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 1);
        assert_eq!(ch, key);
    }

    // `k` and `j` are the list moves, in whichever mode the thread reading is in.
    let mut ch = KEY_LOWER_K;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 1);
    assert_eq!(ch, KEY_UP);
    topic_mode = true;
    let mut ch = KEY_LOWER_J;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 1);
    assert_eq!(ch, KEY_PGDN);

    // The scrolling keys follow the same mode and only leave the pager when the page
    // cannot scroll further.
    topic_mode = false;
    let mut ctx = DisplayCtx::default();
    let mut ch = KEY_UP;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 0);
    assert_eq!(ch, KEY_UP);

    ctx.reach_begin = true;
    let mut ch = KEY_HOME;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 1);
    assert_eq!(ch, KEY_UP);

    ctx.reach_end = true;
    let mut ch = KEY_PGDN;
    assert_eq!(article_key_handler(&mut ch, &mut ctx, &mut topic_mode), 1);
    assert_eq!(ch, KEY_DOWN);
}

#[test]
fn a_searched_author_is_a_letter_then_word_characters() {
    assert!(valid_username("a"));
    assert!(valid_username("Ab_1"));
    assert!(valid_username(&"a".repeat(USERNAME_MAX_LEN)));
    assert!(!valid_username(""));
    assert!(!valid_username(&"a".repeat(USERNAME_MAX_LEN + 1)));
    assert!(!valid_username("1abc"));
    assert!(!valid_username("_abc"));
    assert!(!valid_username("ab-c"));
}

/// The on-disk shape `section_aid_locations_save` writes for the C daemon to read back.
#[test]
fn saved_positions_are_pairs_of_native_integers() {
    let path = std::env::temp_dir().join(format!("lbbs-aid-loc-{}-{}", std::process::id(), NOW));

    let pairs = [(1i16, 100i32), (2, 0), (32_767, 1_234_567)];
    assert_eq!(aid_locations_write(&path, &pairs).unwrap(), 3);

    let mut expected = Vec::new();
    for (sid, aid) in pairs {
        expected.extend_from_slice(&(sid as i32).to_ne_bytes());
        expected.extend_from_slice(&aid.to_ne_bytes());
    }
    assert_eq!(std::fs::read(&path).unwrap(), expected);
    assert_eq!(aid_locations_read(&path).unwrap(), expected_pairs());

    fn expected_pairs() -> Vec<(i32, i32)> {
        vec![(1, 100), (2, 0), (32_767, 1_234_567)]
    }

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_missing_position_file_is_nothing_saved() {
    let path = std::env::temp_dir().join(format!("lbbs-aid-loc-missing-{}", NOW));
    assert!(aid_locations_read(&path).unwrap().is_empty());
}

#[test]
fn a_cut_short_pair_is_dropped_like_the_c_loop_drops_it() {
    let path =
        std::env::temp_dir().join(format!("lbbs-aid-loc-cut-{}-{}", std::process::id(), NOW));

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&7i32.to_ne_bytes());
    bytes.extend_from_slice(&700i32.to_ne_bytes());
    bytes.extend_from_slice(&8i32.to_ne_bytes()); // no aid behind it
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(aid_locations_read(&path).unwrap(), vec![(7, 700)]);
    std::fs::remove_file(&path).unwrap();
}

/// The same page walk over the real board pool, which is the only place its invariants are
/// worth checking: `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
/// Nothing here names a particular post, so a live site adding articles while it runs only
/// changes the counts the assertions compare, never their outcome.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn live_pages_tile_the_list_and_render_it() {
    let (_root, state) = reference_site().await;
    let board = state.board();
    let view_log = ViewLog::anonymous();
    let favor = FavorLog::anonymous();
    let env = RowEnv {
        uid: 0,
        view_log: &view_log,
        favor: &favor,
        display_nickname: true,
        topic_tid: -1,
        fixed: false,
        now: session::now(),
    };

    let mut listed = 0usize;
    for section in board.sections.iter() {
        if section.visible_article_count == 0 {
            continue;
        }
        let page_count = section.page_count_with_ontop();
        assert!(page_count > 0, "{} has no page", section.sname);
        assert!(
            matches!(
                query_section(&board, section, page_count),
                Err(BrowseError::OutOfRange { .. })
            ),
            "one page past the index of {} is out of range",
            section.sname
        );

        let mut rows_of_visible = 0usize;
        for page_id in 0..page_count {
            let page = query_section(&board, section, page_id).expect("page in range");
            assert_eq!(page.page_count, page_count);
            assert!(!page.rows.is_empty(), "{} page {page_id}", section.sname);
            // The C `<=` test lets one page hold a row too many; a second one means the
            // index and the walk have diverged.
            assert!(
                page.rows.len() <= ROWS + 1,
                "{} page {page_id} holds {} rows",
                section.sname,
                page.rows.len()
            );

            for (row, &art) in page.rows.iter().enumerate() {
                let article = board.article(art).expect("row");
                assert_eq!(article.sid, section.sid, "a row of another board");
                // The rows from `ontop_start_offset` on are the stickies this page repeats.
                // Before it the page is the index itself, which holds a pinned post in its
                // ordinary place as well — the duplication C's last page shows.
                let sticky = i32::try_from(row).unwrap() >= page.ontop_start_offset;
                if sticky {
                    assert!(article.ontop, "aid {} is not pinned", article.aid);
                }

                let line = row_line(article, sticky, &env);
                if sticky {
                    assert!(line.starts_with("   \x1b[1;33m[提示]\x1b[m"));
                } else {
                    assert!(line.starts_with("  "));
                    // C's `%7d` is the only place a row says which post it is.
                    assert!(line.contains(&article.aid.to_string()), "{line}");
                }

                if !sticky {
                    rows_of_visible += 1;
                    // The cursor on this row must find this row again: the same page, and
                    // the row of the page's indexed part, which is what the offset counts.
                    let located = locate_article(&board, section, art, 0, 0)
                        .unwrap_or_else(|| panic!("locate aid {}", article.aid));
                    assert_eq!((located.page_id, located.offset), (page_id, row as i32));
                }
            }
        }
        // The stickies a page repeats are already counted in their own place in the index,
        // so the indexed rows of all pages are exactly the visible posts.
        assert_eq!(
            rows_of_visible,
            usize::try_from(section.visible_article_count).unwrap(),
            "{} pages",
            section.sname
        );
        listed += 1;
    }
    assert!(listed > 0, "the loader published no board with posts");
}

/// The reference config, the loaded board pool, and the server root the config sits in —
/// what every live check of this module starts with. The root is the same tree the other
/// live tests read `conf/` and `var/menu_merged.conf` from, so its `var/articles/` article
/// cache is reachable too. Nothing writes into it: the cache is only read, and a reading
/// position stays in memory because only [`Session::save_user_logs`] flushes it.
///
/// Nothing is written to the database either — `online_update` is the
/// `UPDATE … WHERE SID = ?` of the heartbeat, so the synthetic session id of [`guest_session`]
/// matches no row.
async fn reference_site() -> (std::path::PathBuf, std::sync::Arc<crate::state::AppState>) {
    let conf = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
    let root = Path::new(&conf)
        .ancestors()
        .nth(2)
        .expect("LBBS_REF_CONF must be <root>/conf/bbsd.conf")
        .to_path_buf();

    let text = std::fs::read_to_string(&conf).unwrap();
    let (config, _warnings) = crate::config::parse(&text);
    let db = crate::db::Db::connect(&config.db).await.unwrap();
    let state = std::sync::Arc::new(crate::state::AppState::new(config, db));
    let mut loader = crate::loader::Loader::new(state.clone(), &root);
    loader.initial_load().await.unwrap();

    (root, state)
}

/// A guest, with the privileges the reference database gives uid 0.
async fn guest_session(state: &std::sync::Arc<crate::state::AppState>, root: &Path) -> Session {
    let mut session = Session::new(
        state.clone(),
        root,
        "lbbs-smoke".to_string(),
        "127.0.0.1".to_string(),
        0,
        false,
    );
    session.privs = crate::privilege::load(&state.db, 0).await.unwrap();
    session
}

/// The first board a reader may list that holds posts.
fn first_readable_board<'a>(board: &'a Board, privs: &crate::privilege::UserPriv) -> &'a Section {
    board
        .sections
        .iter()
        .filter(|s| s.visible_article_count > 0)
        .find(|s| privs.checkpriv(s.sid, S_LIST))
        .expect("the guest may read at least one board with posts")
}

/// One guest sitting down at a real board and leaving again, which is the part of the list
/// the two checks above cannot reach: the drawing, the key loop and the position the reader
/// ends up on. `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
#[tokio::test]
#[ignore = "needs the live board database and LBBS_REF_CONF"]
async fn a_guest_lists_a_real_board_and_leaves() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let (root, state) = reference_site().await;
    let mut session = guest_session(&state, &root).await;

    // The first board the guest may read that holds posts, with the row page 0 starts on.
    let (sname, sid, first_aid) = {
        let board = state.board();
        let section = first_readable_board(&board, &session.privs);
        let page = query_section(&board, section, 0).expect("page 0 of a board with posts");
        let aid = board.article(page.rows[0]).expect("first row").aid;
        (section.sname.clone(), section.sid, aid)
    };

    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    // The key arrives once the input reset has gone quiet, as `igetch_reset` requires.
    let keyed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\x1b[D");
        client
    });

    // A key that never arrives would idle out only after `BBS_max_user_idle_time`, so the
    // listing has to end on the one pushed above.
    tokio::time::timeout(
        Duration::from_secs(30),
        Listing::run(&mut term, &mut session, &sname, 0),
    )
    .await
    .expect("the listing ends on its key")
    .expect("the guest listing");
    let mut client = keyed.await.unwrap();

    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(drawn.contains("文章标题"), "the column header row: {drawn}");
    assert!(
        drawn.contains(&format!("讨论区 [{sname}]")),
        "the board title row"
    );
    assert!(
        drawn.contains(&first_aid.to_string()),
        "the first row shows the number of the post it is on"
    );
    // The page counter of the footer row, and the cursor the list drew at row 0.
    assert!(drawn.contains("页]"), "the page counter of the footer");
    assert!(drawn.contains('>'), "the cursor");

    // Leaving stored where the reader got to, which is the row the cursor was on.
    assert_eq!(session.aid_locations.get(&sid), Some(&first_aid));
}

/// The same guest opening a post, so the read path runs against the body files the site's
/// web side writes rather than a fixture: the newest post of a readable board whose cache is
/// still on disk, reached the way a `LOCATE_ARTICLE` menu item reaches one.
#[tokio::test]
#[ignore = "needs the live board database, LBBS_REF_CONF and the reference article cache"]
async fn a_guest_reads_the_newest_post_of_a_real_board() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let (root, state) = reference_site().await;
    let mut session = guest_session(&state, &root).await;

    let (sname, sid, aids, cache_dir) = {
        let board = state.board();
        let section = first_readable_board(&board, &session.privs);
        // The posts to try, newest first, which is where a reader who has been here before
        // is left and what the list's own `last_article` answers with.
        let mut aids = Vec::new();
        let mut art = last_article(&board, section);
        while let Some(at) = art {
            let article = board.article(at).expect("row of the section");
            aids.push((article.aid, article.cid));
            if aids.len() >= 40 {
                break;
            }
            art = Some(article.prior);
        }
        (
            section.sname.clone(),
            section.sid,
            aids,
            root.join(VAR_ARTICLE_CACHE_DIR),
        )
    };
    let cache_dir = cache_dir.to_string_lossy().into_owned();

    let (aid, cached) = aids
        .iter()
        .find_map(|(aid, cid)| {
            cache::load(&cache_dir, *cid)
                .ok()
                .flatten()
                .map(|cache| (*aid, cache))
        })
        .expect("none of the 40 newest posts of this board has a cached body");

    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    // Enter opens the row the cursor is on, and `←` closes it again: the pager's own
    // `返回[←,ESC]` answers with KEY_LEFT, which `read_article` hands back to the list —
    // where the same key leaves. The arrows therefore follow the reader out one step at a
    // time, and the extras land on a pipe nobody is reading.
    let keyed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.push(b"\r");
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            client.push(b"\x1b[D");
        }
        client
    });

    // The listing would otherwise sit in `read_key_wait` for the whole idle limit, so a key
    // that does not reach it has to fail here rather than after ten minutes.
    tokio::time::timeout(
        Duration::from_secs(30),
        Listing::run(&mut term, &mut session, &sname, aid),
    )
    .await
    .expect("the listing ends on its keys")
    .expect("the guest listing that starts on one post");
    let mut client = keyed.await.unwrap();
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();

    assert!(
        drawn.contains(&format!("讨论区 [{sname}]")),
        "the board header"
    );
    assert!(drawn.contains(&aid.to_string()), "the row of aid {aid}");
    // The pager's own status bar, which no row of the list writes.
    assert!(drawn.contains("回复["), "the pager bar");

    // A line of the body the cache holds, taken where the pager cannot have altered it: no
    // control sequences, and short enough to leave the terminal whole.
    let body_line = cached
        .data
        .split(|&byte| byte == b'\n')
        .map(|line| {
            let mut end = line.len();
            while end > 0 && matches!(line[end - 1], b'\r' | b' ') {
                end -= 1;
            }
            &line[..end]
        })
        .find(|line| {
            !line.is_empty()
                && !line.contains(&0x1b)
                && text::display_width(line, false, false) <= 76
        })
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .expect("the cached body has no plain printable line");
    assert!(
        drawn.contains(&body_line),
        "the pager wrote the body of the cache file"
    );

    // Back in the list the cursor is still on the post that was opened, and leaving saves it.
    assert_eq!(session.aid_locations.get(&sid), Some(&aid));
}

/// The fixture board's list, positioned on the rows `page_of` read.
fn listing_on(board: &Board, page: &Page) -> Listing {
    let section = board.section(1).expect("fixture section");
    Listing {
        sid: 1,
        sname: section.sname.clone(),
        stitle: section.stitle.clone(),
        master_list: section.master_list.clone(),
        rows: page.rows.clone(),
        count: page.count(),
        page_count: page.page_count,
        ontop_start_offset: page.ontop_start_offset,
        page_id: 0,
        selected: 0,
        topic_mode: false,
        topic_tid: -1,
        title_search: String::new(),
    }
}

/// The list with an empty roster and one post of `uid = 1` under its cursor, which is all
/// both profile commands need: neither reaches a profile screen because no row is there.
fn author_listing() -> (Listing, Session) {
    let board = board_with(&[post(100)]);
    let page = page_of(&board, 0);
    let listing = listing_on(&board, &page);
    let session = crate::session::offline_session();
    *session.state.board_mut() = board;
    session.state.publish_roster(Roster::default());
    (listing, session)
}

/// A key typed once the screen's input reset has gone quiet, which is what `igetch_reset`
/// requires; anything earlier is drained away and never read.
fn type_later(client: Pipe, delay: u64, keys: &[u8]) -> tokio::task::JoinHandle<Pipe> {
    let keys = keys.to_vec();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        client.push(&keys);
        client
    })
}

/// `Ctrl-A` on a row whose author the roster does not hold (`section_list_display.c:1062`
/// with `query_user_info_by_uid` answering 0).
#[tokio::test]
async fn the_author_key_of_a_row_no_roster_holds_says_so() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let (mut listing, mut session) = author_listing();
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let typed = type_later(client, 250, b" ");

    let flow = tokio::time::timeout(
        Duration::from_secs(5),
        listing.dispatch(&mut term, &mut session, SelectCmd::QueryUser),
    )
    .await
    .expect("the message ends on its key")
    .expect("dispatch");
    assert_eq!(flow, Flow::Redraw, "C draws the list again");

    let mut client = typed.await.unwrap();
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(
        drawn.contains("\x1b[2J"),
        "the screen is cleared first: {drawn}"
    );
    assert!(drawn.contains("该用户已升天"), "{drawn}");
    assert!(drawn.contains("按任意键继续"), "{drawn}");
}

/// `u` (`section_list_display.c:1336`): the search screen prompts, and a prefix nothing
/// starts with is answered where it was typed.
#[tokio::test]
async fn the_search_key_prompts_and_answers_a_prefix_nothing_matches() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let (mut listing, mut session) = author_listing();
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let typed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"amy\r");
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b" ");
        client
    });

    let flow = tokio::time::timeout(
        Duration::from_secs(5),
        listing.dispatch(&mut term, &mut session, SelectCmd::SearchUser),
    )
    .await
    .expect("the search ends on its keys")
    .expect("dispatch");
    assert_eq!(flow, Flow::Redraw);

    let mut client = typed.await.unwrap();
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(drawn.contains("查找谁: "), "the prompt: {drawn}");
    assert!(
        drawn.contains("没有找到符合条件的用户"),
        "an empty roster matches nothing: {drawn}"
    );
    assert!(!drawn.contains("存在多个匹配的用户"), "{drawn}");
}

/// The digest tree `gen_ex_list` writes for board 1: the screen every block points at, the
/// board's root directory, and the one post it holds. An article item's name is its `AID`,
/// which is what `VIEW_EX_ARTICLE` is handed as its parameter.
fn ex_menu_set() -> crate::menu::MenuSet {
    crate::menu::MenuSet::parse(
            concat!(
                "%S_EX_DIR\n",
                "  返回[\x1b[1;32m←\x1b[0;37m] 进入[\x1b[1;32m→\x1b[0;37m]\n",
                "%\n",
                "%menu M_EX_1_0\n",
                "title       0, 0, \"精华区 > 测试板\"\n",
                "screen      2, 0, S_EX_DIR\n",
                "page        4, 1, 20\n",
                "@VIEW_EX_ARTICLE   4, 4, 1, 0, \"100\", \"    100  title 100                          2023.11.14\"\n",
                "%\n",
            )
            .as_bytes(),
        )
        .expect("the fixture must parse")
}

/// A board whose digest tree is loaded and whose post has a cache file, in a temp server root
/// the reader can open. `name` keeps the roots of the tests that use it apart.
fn digest_session(name: &str) -> (Session, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("lbbs-ex-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(VAR_ARTICLE_CACHE_DIR)).unwrap();

    let mut board = board_with(&[]);
    let article = Article {
        aid: 100,
        cid: 9001,
        ..post(100)
    };
    board.append_article(&article).expect("append");
    {
        let section = board.section_mut(1).expect("section");
        section.ex_menu_tm = NOW;
        section.ex_menu_set = Some(std::sync::Arc::new(ex_menu_set()));
    }

    let mut session = crate::session::offline_session();
    session.root = root.clone();
    // The generated digest items ask for the `P_USER` bit — the `1` of their `…, 4, 1, 0`
    // fields — which a guest does not hold, and a menu of hidden items is C's 没有可选项.
    session.privs.g_priv |= crate::privilege::P_USER;
    let dir = root.join(VAR_ARTICLE_CACHE_DIR);
    cache::generate(
        &dir.to_string_lossy(),
        "MyBBS",
        "www.bbs.test",
        &article,
        board.section(1).expect("section"),
        "精华区的第一帖\n".as_bytes(),
        "203.0.113.7",
        true,
        false,
    )
    .expect("the cache file must be written");

    *session.state.board_mut() = board;
    (session, root)
}

/// `section_list_display.c:1392-1399`: a board the generator never stamped is not an empty
/// digest tree — it is one line and a key press, and the menu never opens.
#[tokio::test]
async fn a_board_that_was_never_stamped_says_its_digest_is_closed() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let mut session = crate::session::offline_session();
    *session.state.board_mut() = board_with(&[]);
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let typed = type_later(client, 250, b" ");

    let opened = tokio::time::timeout(
        Duration::from_secs(5),
        list_ex_dir(&mut term, &mut session, 1),
    )
    .await
    .expect("the message ends on its key");
    assert!(opened, "C answers a closed digest with 0");

    let mut client = typed.await.unwrap();
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(drawn.contains("该版块精华区未开放"), "{drawn}");
    assert!(!drawn.contains("精华区 > 测试板"), "{drawn}");
}

/// The whole `LIST_EX_SECTION` visit: the tree draws, `Enter` runs the item's
/// `VIEW_EX_ARTICLE` through the same dispatch table the main menu uses, and the two Escapes
/// that follow leave the reader and then the tree.
#[tokio::test]
async fn a_digest_item_reads_its_post_and_hands_the_tree_back() {
    use crate::term::{ByteIo, Pipe, Terminal};

    let (mut session, root) = digest_session("item");
    let (client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let typed = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\r");
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\x1b");
        tokio::time::sleep(Duration::from_millis(250)).await;
        client.push(b"\x1b");
        client
    });

    let answered = tokio::time::timeout(
        Duration::from_secs(6),
        list_ex_section(&mut term, &mut session, "test"),
    )
    .await
    .expect("the tree leaves on its second Escape");
    assert!(answered, "a board the pool has always asks for a redraw");

    let mut client = typed.await.unwrap();
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(drawn.contains("精华区 > 测试板"), "{drawn}");
    assert!(drawn.contains("精华区的第一帖"), "{drawn}");
    // The digest pager: `display_ex_article_key_handler` answers every key with the bar that
    // has neither the thread switch nor the reply.
    assert!(drawn.contains("帮助[\x1b[32mh\x1b[33m] |"), "{drawn}");
    assert!(!drawn.contains("回复[\x1b[32mr\x1b[33m]"), "{drawn}");

    std::fs::remove_dir_all(&root).unwrap();
}

/// `menu_proc.c:263-268`: the parameter is a board name, and a name the pool does not have is
/// the handler's `-1` — `menu_control` hands it to the caller's `NOREDRAW` arm, which leaves
/// the menu that ran it untouched. A board that is there but whose menu never loaded is C's
/// `-3`, which its caller logs and still answers `REDRAW` with (`:268-274`).
#[tokio::test]
async fn a_missing_digest_board_redraws_nothing_and_a_broken_menu_does() {
    use crate::menu::Action;
    use crate::term::{ByteIo, Pipe, Terminal};

    let mut board = board_with(&[]);
    board.section_mut(1).expect("section").ex_menu_tm = NOW;

    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));
    let mut session = crate::session::offline_session();
    *session.state.board_mut() = board;

    let answered = crate::bbs::bbs_do_choice(
        &mut term,
        &mut session,
        Action::ListExSection,
        "nope",
        false,
    )
    .await;
    assert_eq!(answered, MenuReturn::NoRedraw, "Section nope not found");
    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(!drawn.contains("该版块精华区未开放"), "{drawn}");

    let answered = crate::bbs::bbs_do_choice(
        &mut term,
        &mut session,
        Action::ListExSection,
        "test",
        false,
    )
    .await;
    assert_eq!(answered, MenuReturn::Redraw, "a stamped board with no menu");
}

/// `menu_proc.c:224-238`: a parameter that is not a number is `atoi`'s 0, and a post whose
/// cache file the loader never wrote both answer `NOREDRAW` — no screen, no database.
#[tokio::test]
async fn a_digest_post_that_is_not_there_leaves_the_screen_alone() {
    use crate::menu::Action;
    use crate::term::{ByteIo, Pipe, Terminal};

    let (mut session, root) = digest_session("uncached");
    {
        // A second post, whose cache file is deliberately not generated.
        let article = Article {
            aid: 200,
            cid: 9002,
            ..post(200)
        };
        session
            .state
            .board_mut()
            .append_article(&article)
            .expect("append");
    }
    let (mut client, server) = Pipe::new();
    let mut term = Terminal::new(ByteIo::Pipe(server));

    for (param, why) in [
        ("", "atoi of an empty name is 0, which is no article"),
        ("200", "the pool has the post but no cache file for it"),
    ] {
        let answered =
            crate::bbs::bbs_do_choice(&mut term, &mut session, Action::ViewExArticle, param, false)
                .await;
        assert_ne!(answered, MenuReturn::Redraw, "{why}");
    }

    let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(!drawn.contains("精华区的第一帖"), "{drawn}");
    std::fs::remove_dir_all(&root).unwrap();
}
