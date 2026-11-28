use super::*;

/// A visible topic head.
fn post(aid: i32, sid: i16) -> Article {
    Article {
        aid,
        sid,
        visible: true,
        ..Article::default()
    }
}

/// A visible reply to `tid`.
fn reply(aid: i32, sid: i16, tid: i32) -> Article {
    Article {
        aid,
        sid,
        tid,
        visible: true,
        ..Article::default()
    }
}

fn board() -> Board {
    let mut board = Board::default();
    assert!(
        board
            .create_section(1, "test", "测试板", "master ")
            .is_some()
    );
    assert!(board.create_section(2, "other", "另一个板", "").is_some());
    board
}

fn fill(board: &mut Board, sid: i16, articles: &[Article]) {
    for article in articles {
        assert_eq!(article.sid, sid);
        board.append_article(article).expect("append");
    }
}

#[test]
fn a_page_breaks_at_the_twenty_first_visible_article() {
    let mut board = board();
    let articles: Vec<Article> = (100..121).map(|aid| post(aid, 1)).collect();
    fill(&mut board, 1, &articles);

    let section = board.section(1).unwrap();
    assert_eq!(section.article_count, 21);
    assert_eq!(section.visible_article_count, 21);
    assert_eq!(section.topic_count, 21);
    assert_eq!(section.visible_topic_count, 21);
    assert_eq!(section.page_count(), 2);
    assert_eq!(section.last_page_visible_article_count, 1);
    // The list runs oldest to newest through `next`, so the head is the first article the
    // loader appended.
    assert_eq!(section.head, board.article_index(100));
    assert_eq!(section.tail, board.article_index(120));
    assert_eq!(section.page_first[1], board.article_index(120).unwrap());
}

#[test]
fn an_invisible_article_never_opens_a_page() {
    let mut board = board();
    let mut articles: Vec<Article> = (100..120).map(|aid| post(aid, 1)).collect();
    articles.extend((120..130).map(|aid| Article {
        aid,
        sid: 1,
        visible: false,
        ..Article::default()
    }));
    fill(&mut board, 1, &articles);

    let section = board.section(1).unwrap();
    assert_eq!(section.article_count, 30);
    assert_eq!(section.visible_article_count, 20);
    assert_eq!(section.page_count(), 1);
    assert_eq!(section.last_page_visible_article_count, 20);
}

#[test]
fn a_reply_to_a_missing_topic_is_dropped_but_still_counted() {
    let mut board = board();
    // `section_list_append_article` raises `visible_article_count` before it looks the
    // topic up, and the reset it performs on failure does not take that back.
    fill(&mut board, 1, &[reply(100, 1, 999)]);

    let section = board.section(1).unwrap();
    assert_eq!(section.article_count, 1);
    assert_eq!(section.visible_article_count, 1);
    assert_eq!(section.topic_count, 1);
    assert_eq!(section.visible_topic_count, 0);
    let article = board.article_by_aid(100).unwrap();
    assert_eq!(article.tid, 0);
    assert!(!article.visible);
}

#[test]
fn a_topic_walk_returns_to_its_head() {
    let mut board = board();
    fill(
        &mut board,
        1,
        &[post(100, 1), reply(101, 1, 100), reply(102, 1, 100)],
    );

    assert_eq!(board.topic_len(100), 3);
    let head = board.article_index(100).unwrap();
    let articles = board.articles();
    // Replies join at the tail, so the head's `topic_prior` is the newest of them.
    assert_eq!(
        articles[head].topic_prior,
        board.article_index(102).unwrap()
    );
    let mut visited = vec![head];
    let mut art = articles[head].topic_next;
    while art != head {
        visited.push(art);
        art = articles[art].topic_next;
    }
    assert_eq!(
        visited.iter().map(|&i| articles[i].aid).collect::<Vec<_>>(),
        vec![100, 101, 102]
    );
}

/// `section_list.c:1101` and `:1144`: a sticky list that has lost step with the article
/// flags is reported rather than quietly corrected.
#[test]
fn a_sticky_list_out_of_step_with_the_flags_is_logged() {
    let mut board = board();
    board
        .append_article(&Article {
            ontop: true,
            ..post(100, 1)
        })
        .unwrap();
    let stuck = board.articles.len() - 1;
    board.append_article(&post(200, 1)).unwrap();
    let plain = board.articles.len() - 1;

    let (common, error) = crate::log::captured(|| {
        // Both calls re-run the op for an article whose flag already says what the list
        // says, which is the state C treats as inconsistent.
        board.update_article_ontop(1, stuck).unwrap();
        board.update_article_ontop(1, plain).unwrap();
    });

    assert!(
        error.contains("Inconsistent state found: article 100 already ontop in section 1\n"),
        "{error}"
    );
    assert!(
        error.contains("Inconsistent state found: article 200 not ontop in section 1\n"),
        "{error}"
    );
    // Both are `log_error()` calls; nothing reaches the common stream.
    assert_eq!(common, "", "{common}");
    assert_eq!(error.matches('\n').count(), 2, "{error}");
}

#[test]
fn the_sticky_list_holds_the_ten_largest_aids() {
    let mut board = board();
    let articles: Vec<Article> = (1..13)
        .map(|aid| Article {
            aid,
            sid: 1,
            visible: true,
            ontop: true,
            ..Article::default()
        })
        .collect();
    fill(&mut board, 1, &articles);

    let section = board.section(1).unwrap();
    assert_eq!(section.ontop_article_count(), 10);
    assert_eq!(
        section.sticky_aids(board.articles()),
        (3..13).collect::<Vec<i32>>()
    );
}

#[test]
fn an_older_sticky_article_is_skipped_when_the_list_is_full() {
    let mut board = board();
    let full: Vec<i32> = (0..10).map(|i| 100 + i * 10).collect();
    for aid in &full {
        board
            .append_article(&Article {
                ontop: true,
                ..post(*aid, 1)
            })
            .unwrap();
    }
    assert_eq!(
        board.section(1).unwrap().sticky_aids(board.articles()),
        full
    );

    // Older than everything already stuck on the board: the C original drops the new
    // article instead of evicting the oldest, which only happens once the list is full.
    board.articles.push(Article {
        ontop: true,
        ..post(50, 1)
    });
    board.sections[0].article_count += 1;
    let art = board.articles.len() - 1;
    board.update_article_ontop(1, art).unwrap();
    assert_eq!(
        board.section(1).unwrap().sticky_aids(board.articles()),
        full
    );

    // In the middle: the oldest goes, and the new article slots in before its successor.
    board.articles.push(Article {
        ontop: true,
        ..post(105, 1)
    });
    board.sections[0].article_count += 1;
    let art = board.articles.len() - 1;
    board.update_article_ontop(1, art).unwrap();
    assert_eq!(
        board.section(1).unwrap().sticky_aids(board.articles()),
        vec![105, 110, 120, 130, 140, 150, 160, 170, 180, 190]
    );
}

#[test]
fn sticky_articles_extend_the_last_page() {
    let mut board = board();
    let empty = board.section(1).unwrap();
    assert_eq!(empty.page_count_with_ontop(), 0);
    assert_eq!(empty.page_article_count_with_ontop(0), 0);

    fill(
        &mut board,
        1,
        &(100..125).map(|aid| post(aid, 1)).collect::<Vec<_>>(),
    );
    let section = board.section(1).unwrap();
    // 25 visible articles index as two pages (20 + 5), and `BBS_article_limit_per_page`
    // is what both helpers divide by.
    assert_eq!(section.page_count(), 2);
    assert_eq!(section.last_page_visible_article_count, 5);
    assert_eq!(section.page_count_with_ontop(), 2);
    assert_eq!(section.page_article_count_with_ontop(0), 20);
    assert_eq!(section.page_article_count_with_ontop(1), 5);
    assert_eq!(section.page_article_count_with_ontop(2), 0);
}

#[test]
fn find_article_reports_where_a_missing_aid_would_go() {
    let mut board = board();
    fill(
        &mut board,
        1,
        &(100..121).map(|aid| post(aid, 1)).collect::<Vec<_>>(),
    );
    let section = board.section(1).unwrap();
    let articles = board.articles();

    let at = section.find_article_with_offset(articles, 110).unwrap();
    assert_eq!((at.page, at.offset), (0, 10));
    assert_eq!(at.article, board.article_index(110));
    assert_eq!(at.next, board.article_index(111));

    // Below the oldest article: page 0, offset 0, inserted before the head.
    let at = section.find_article_with_offset(articles, 99).unwrap();
    assert_eq!((at.page, at.offset, at.article), (0, 0, None));
    assert_eq!(at.next, board.article_index(100));

    // Above the newest: the walk wraps, and the insertion point is the head again.
    let at = section.find_article_with_offset(articles, 121).unwrap();
    assert_eq!((at.page, at.offset, at.article), (1, 1, None));
    assert_eq!(at.next, board.article_index(100));

    let at = section.find_article_with_offset(articles, 120).unwrap();
    assert_eq!((at.page, at.offset), (1, 0));
}

#[test]
fn recalculating_pages_reproduces_the_append_index() {
    let mut board = board();
    let mut articles: Vec<Article> = (100..118).map(|aid| post(aid, 1)).collect();
    articles.push(Article {
        aid: 118,
        sid: 1,
        ..Article::default()
    });
    articles.push(post(119, 1));
    fill(&mut board, 1, &articles);

    let before: Vec<i32> = board.section(1).unwrap().page_aids(board.articles());
    board.calculate_page(1, 0).unwrap();
    let after: Vec<i32> = board.section(1).unwrap().page_aids(board.articles());
    assert_eq!(before, after);
    // 19 visible articles, none of them enough to open a second page.
    assert_eq!(after.len(), 1);
    assert_eq!(
        board.section(1).unwrap().last_page_visible_article_count,
        19
    );
}

#[test]
fn hiding_a_topic_hides_its_replies_and_showing_it_does_not() {
    let mut board = board();
    fill(
        &mut board,
        1,
        &[
            post(100, 1),
            reply(101, 1, 100),
            reply(102, 1, 100),
            post(103, 1),
        ],
    );

    // head + two replies, which is the C `affected_count`.
    assert_eq!(board.set_article_visible(1, 100, false).unwrap(), 3);
    {
        let section = board.section(1).unwrap();
        assert_eq!(section.visible_article_count, 1);
        assert_eq!(section.visible_topic_count, 1);
    }
    assert_eq!(board.set_article_visible(1, 100, true).unwrap(), 1);
    let section = board.section(1).unwrap();
    assert_eq!(section.visible_article_count, 2);
    assert_eq!(section.visible_topic_count, 2);
    assert!(board.article_by_aid(100).unwrap().visible);
    assert!(!board.article_by_aid(101).unwrap().visible);
    // The page index is not rebuilt by either call, so it still counts the replies.
    assert_eq!(section.page_count(), 1);
    assert_eq!(section.last_page_visible_article_count, 4);
}

#[test]
fn the_arena_only_grows() {
    let mut board = board();
    assert_eq!(board.last_aid(), 0);
    board.append_article(&post(5, 1)).unwrap();
    assert_eq!(board.last_aid(), 5);
    assert_eq!(
        board.append_article(&post(4, 1)),
        Err(BoardError::AidNotAscending)
    );
    assert_eq!(board.append_article(&post(6, 3)), Err(BoardError::NotFound));

    board.sections[0].article_count = ARTICLE_LIMIT_PER_SECTION;
    assert_eq!(
        board.append_article(&post(7, 1)),
        Err(BoardError::SectionFull)
    );
}

#[test]
fn moving_a_topic_updates_both_lists_and_counters() {
    let mut board = board();
    fill(
        &mut board,
        1,
        &[
            post(101, 1),
            reply(102, 1, 101),
            reply(103, 1, 101),
            post(104, 1),
            post(105, 1),
        ],
    );
    fill(&mut board, 2, &[post(201, 2), post(202, 2)]);

    assert_eq!(board.move_topic(1, 2, 101).unwrap(), 3);

    let src = board.section(1).unwrap();
    assert_eq!(
        (
            src.article_count,
            src.topic_count,
            src.visible_article_count,
            src.visible_topic_count
        ),
        // 104 and 105 stay, each the head of its own topic.
        (2, 2, 2, 2)
    );
    assert_eq!(src.page_count(), 1);
    assert_eq!(src.last_page_visible_article_count, 2);

    let dest = board.section(2).unwrap();
    assert_eq!(
        (
            dest.article_count,
            dest.topic_count,
            dest.visible_article_count,
            dest.visible_topic_count
        ),
        // Three articles arrive but only their head is a new topic.
        (5, 3, 5, 3)
    );
    assert_eq!(board.topic_len(101), 3);
    // The destination list stays ordered by aid.
    let moved: Vec<i32> = dest
        .articles_oldest_first(board.articles())
        .map(|a| a.aid)
        .collect();
    assert_eq!(moved, vec![101, 102, 103, 201, 202]);
}

#[test]
fn moving_to_the_front_of_a_board_leaves_its_page_index_behind() {
    let mut board = board();
    fill(&mut board, 1, &[post(101, 1), post(102, 1)]);
    fill(&mut board, 2, &[post(201, 2), post(202, 2)]);

    board.move_topic(1, 2, 101).unwrap();

    // The moved article lands before the destination's page head, so the rebuild that
    // would index it cannot find it and reports an error — which `move_topic` only logs,
    // exactly as the C original does. The board still lists the article; it just is not
    // reachable by page number until the next full rebuild.
    let dest = board.section(2).unwrap();
    assert_eq!(dest.page_aids(board.articles()), vec![201]);
    assert_eq!(dest.article_count, 3);

    board.calculate_page(2, 0).unwrap();
    let dest = board.section(2).unwrap();
    assert_eq!(dest.page_aids(board.articles()), vec![101]);
    assert_eq!(dest.last_page_visible_article_count, 3);
}

#[test]
fn pool_labels_are_cut_to_the_c_buffer_sizes() {
    let mut article = Article::default();
    article.set_labels(
        Some(&"u".repeat(20)),
        Some(&"n".repeat(50)),
        Some(&"t".repeat(200)),
    );
    assert_eq!(
        (
            article.username.len(),
            article.nickname.len(),
            article.title.len()
        ),
        (USERNAME_MAX_LEN, NICKNAME_MAX_LEN, ARTICLE_TITLE_MAX_LEN)
    );

    // `strncpy` counts bytes and would split the last character; the port keeps whole ones.
    let mut article = Article::default();
    article.set_labels(Some("一二三四五"), None, None);
    assert_eq!(article.username, "一二三四");
}
