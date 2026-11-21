//! The board article list, and the reader it opens.
//!
//! Port of `src/section_list_display.c` plus the six list queries of
//! `src/section_list_loader.c` (`query_section_articles` `:868`, `locate_article_in_section`
//! `:954`, `last_article_in_section` `:1068`, `scan_unread_article_in_section` `:1110`,
//! `scan_article_in_section_by_{uid,username,title}` `:1155-1333`). The screen is the 20-row
//! list under two header lines, the cursor a `>` in column 1; every key either moves that
//! cursor, replaces the rows, or opens the article — which is the only place browsing marks a
//! post read and the only place a thread gets starred.
//!
//! # Deviations from C
//!
//! * `section_topic_view_mode` and `section_topic_view_tid` are process statics that a forked
//!   child inherits and that every board of that session shares. They are fields of
//!   [`Listing`] here, so the topic colouring starts fresh at every board and one connection
//!   cannot change another's. `display_nickname` moves one step further, onto
//!   [`Session::display_nickname`], because C's function-local `static int` is what makes the
//!   `n` key stick across every board of the session.
//! * `section_aid_locations[BBS_max_section]` is indexed by a board's position in the
//!   shared-memory pool. The port keys it by `sid` ([`Session::aid_locations`]), which
//!   survives a pool rebuild. The file format is unchanged: pairs of native `int32_t`, `sid`
//!   then `aid`, one pair per board.
//! * `p_articles[BBS_article_limit_per_page]` is a fixed C array whose index can be reached
//!   one past the end by `query_section_articles`' `<= BBS_article_limit_per_page` loop
//!   condition (`section_list_loader.c:920`) — the same condition that lets it log
//!   `Inconsistent visible article count %d detected in section %d page %d`. The row list here
//!   has no capacity, so the loop condition is kept and the extra row exists instead of
//!   overwriting the stack.
//! * `locate_article_in_section` returns "found" (`1`) on its dead-loop break without having
//!   written the page or the offset, so C's caller re-queries the page it was already showing.
//!   The port reports `None`, which the callers treat as "stay put": the same rows, one query
//!   less.
//! * `section_list_rd_lock` / `_rd_unlock` are the per-board semaphores between the loader and
//!   the children. [`crate::state::AppState::board`] is the same mutual exclusion, so those
//!   calls have no counterparts here; every list query takes the guard and drops it before the
//!   next `.await`.
//! * `article_cache_load` failing means the child cannot show the post, and C leaves the
//!   reader for it (`break` out of the do/while at `:781`). Nothing generates a missing cache
//!   file on the browse path — that is the loader's job — so the port does the same.
//! * `Ctrl-P`, `E`, `d`, `Ctrl-Q` and the reader's `r` reach the write and article-info
//!   screens, which are not ported yet. The privilege tests each of them runs first are here,
//!   so the keys that are allowed to do something are already the right ones. `Ctrl-A` and `u`
//!   are ported: the author's profile and the name search that ends in it.

use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use chrono::TimeZone;

use crate::board::{Artp, Article, Board, Section};
use crate::cache::{self, Cached};
use crate::favorlog::FavorLog;
use crate::paths::{
    ARTICLE_LIMIT_PER_PAGE, ARTICLE_LIMIT_PER_SECTION, ARTICLE_TITLE_MAX_LEN, DATA_READ_HELP,
    MAX_USER_IDLE_TIME, NICKNAME_MAX_LEN, SCREEN_ROWS, USERNAME_MAX_LEN, VAR_ARTICLE_CACHE_DIR,
};
use crate::menu::{Draw, MenuFlow, MenuNav, MenuReturn};
use crate::privilege::{S_LIST, S_MAN_S, S_POST};
use crate::post;
use crate::screen::{self, DisplayCtx, PagerHooks};
use crate::session::{self, Session};
use crate::term::key::{
    CR, KEY_DOWN, KEY_END, KEY_ESC, KEY_HOME, KEY_LEFT, KEY_NULL, KEY_PGDN, KEY_PGUP, KEY_RIGHT,
    KEY_TIMEOUT, KEY_UP,
};
use crate::term::Terminal;
use crate::text;
use crate::viewlog::ViewLog;

/// `TITLE_SEARCH_MAX_LEN` (`section_list_display.c:40`).
pub const TITLE_SEARCH_MAX_LEN: usize = 60;

/// `BBS_article_limit_per_page` (`section_list.h:27`), the rows between `moveto(4, 1)` and
/// `moveto(23, 1)`.
pub const ROWS: usize = ARTICLE_LIMIT_PER_PAGE as usize;

/// The first list row, `moveto(4 + i, 1)`.
pub const FIRST_ROW: usize = 4;

/// `section_list_draw_items` clears `clrline(4, 23)` before it writes its rows.
pub const LAST_ROW: usize = 23;

/// `BBS_section_list_load_interval` (`bbs.c:38`): how long a keyless listing may go before the
/// screen re-queries itself.
pub const SECTION_LIST_LOAD_INTERVAL: i64 = 1;

/// `igetch(100)`, the resolution of the key loop.
pub const SELECT_KEY_TIMEOUT: Duration = Duration::from_millis(100);

/// The pager status bar in normal mode.
pub const MSG_NORMAL: &str = "| 返回[\x1b[32m←\x1b[33m,\x1b[32mESC\x1b[33m] \
    移动[\x1b[32m↑\x1b[33m/\x1b[32m↓\x1b[33m/\x1b[32mPgUp\x1b[33m/\x1b[32mPgDn\x1b[33m] \
    切换[\x1b[32mp\x1b[33m] 回复[\x1b[32mr\x1b[33m] 帮助[\x1b[32mh\x1b[33m] |";

/// …and in topic mode, where the up and down keys move inside the thread.
pub const MSG_TOPIC: &str = "| 返回[\x1b[32m←\x1b[33m,\x1b[32mESC\x1b[33m] \
    同主题阅读[\x1b[32m↑\x1b[33m/\x1b[32m↓\x1b[33m] \
    切换[\x1b[32mp\x1b[33m] 回复[\x1b[32mr\x1b[33m] 帮助[\x1b[32mh\x1b[33m] |";

/// …and for a post read out of a digest menu, whose pager offers neither the thread switch nor
/// the reply: `display_ex_article_key_handler` (`menu_proc.c:203-216`).
pub const MSG_EX: &str = "| 返回[\x1b[32m←\x1b[33m,\x1b[32mESC\x1b[33m] | \
    移动[\x1b[32m↑\x1b[33m/\x1b[32m↓\x1b[33m/\x1b[32mPgUp\x1b[33m/\x1b[32mPgDn\x1b[33m] | \
    帮助[\x1b[32mh\x1b[33m] |";

/// `enum select_cmd_t`, the return of `section_list_select`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectCmd {
    /// `EXIT_SECTION`, which is also how a broken pipe and an idle reader leave.
    ExitSection,
    ViewArticle,
    ChangePage,
    ShowHelp,
    ChangeNameDisplay,
    PostArticle,
    EditArticle,
    DeleteArticle,
    QueryArticle,
    QueryUser,
    SetFavorArticle,
    UnsetFavorArticle,
    SetExcerptionArticle,
    FirstTopicArticle,
    LastTopicArticle,
    LastSectionArticle,
    ScanNewArticle,
    ScanArticleBackwardByUser,
    ScanArticleForwardByUser,
    ScanArticleBackwardByTitle,
    ScanArticleForwardByTitle,
    ViewExDir,
    ShowTop10,
    SearchUser,
}

/// The negative returns of `section_list_display`, each of which C logs at the failing call.
#[derive(Debug)]
pub enum BrowseError {
    /// `log_error("Section %s not found", sname)` (`:625`), and the same from a list query
    /// after the loader dropped the board out from under the listing.
    SectionNotFound,
    /// `log_error("Forbid access to unauthorized section, sid=%d, uid=%d", …)` (`:631`).
    Forbidden { sid: i16, uid: i32 },
    /// `log_debug("Invalid page_id=%d, not in range [0, %d)")` (`section_list_loader.c:899`).
    OutOfRange { sid: i16, page_id: i32 },
    /// `log_error("article_cache_load(aid=%d, cid=%d) error", …)` (`:776`).
    CacheLoad { aid: i32, cid: i32 },
}

/// What an action leaves the screen doing, which is the tail of the `switch (ret)` arm:
/// [`Flow::Redraw`] is C's `section_list_draw_screen()` call, [`Flow::Stay`] the bare `break`
/// back to `section_list_draw_items`, and [`Flow::Out`] the `return 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Redraw,
    Stay,
    Out,
}

/// `Ctrl(c)`, the control keys of the C switch.
const fn ctrl(ch: u8) -> u32 {
    (ch & 0x1f) as u32
}

const KEY_CTRL_P: u32 = ctrl(b'P');
const KEY_CTRL_A: u32 = ctrl(b'A');
const KEY_CTRL_Q: u32 = ctrl(b'Q');
const KEY_CTRL_X: u32 = ctrl(b'X');

const KEY_N: u32 = b'n' as u32;
const KEY_LOWER_P: u32 = b'p' as u32;
const KEY_R: u32 = b'r' as u32;
const KEY_E: u32 = b'E' as u32;
const KEY_D: u32 = b'd' as u32;
const KEY_F: u32 = b'F' as u32;
const KEY_M: u32 = b'm' as u32;
const KEY_MINUS: u32 = b'-' as u32;
const KEY_UPPER_P: u32 = b'P' as u32;
const KEY_UPPER_N: u32 = b'N' as u32;
const KEY_LOWER_K: u32 = b'k' as u32;
const KEY_LOWER_J: u32 = b'j' as u32;
const KEY_DOLLAR: u32 = b'$' as u32;
const KEY_EQUAL: u32 = b'=' as u32;
const KEY_BACKSLASH: u32 = b'\\' as u32;
const KEY_UPPER_S: u32 = b'S' as u32;
const KEY_UPPER_A: u32 = b'A' as u32;
const KEY_LOWER_A: u32 = b'a' as u32;
const KEY_QUESTION: u32 = b'?' as u32;
const KEY_SLASH: u32 = b'/' as u32;
const KEY_U: u32 = b'u' as u32;
const KEY_H: u32 = b'h' as u32;
const KEY_X: u32 = b'x' as u32;
const KEY_UPPER_H: u32 = b'H' as u32;

// ---------------------------------------------------------------------------
// The list queries
// ---------------------------------------------------------------------------

/// One page of the list: `query_section_articles`' array and its two out-parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    /// `p_articles[]`: arena indices of the rows, in the order the screen writes them.
    pub rows: Vec<Artp>,
    /// `*p_page_count`, which is [`Section::page_count_with_ontop`].
    pub page_count: i32,
    /// `*p_ontop_start_offset`: the first row that is a sticky post.
    pub ontop_start_offset: i32,
}

impl Page {
    /// `*p_article_count`.
    pub fn count(&self) -> i32 {
        self.rows.len() as i32
    }
}

/// `query_section_articles` (`section_list_loader.c:868`): the visible articles of one page of
/// the list index, then the sticky posts once the last indexed page is reached.
///
/// `Err` is C's `-3`, an out-of-range page. An empty board and a page beyond the index both
/// answer with no rows.
pub fn query_section(board: &Board, section: &Section, page_id: i32) -> Result<Page, BrowseError> {
    let sid = section.sid;
    let mut page = Page {
        rows: Vec::new(),
        page_count: section.page_count_with_ontop(),
        // C's `*p_ontop_start_offset = BBS_article_limit_per_page;`, the "no room left" value
        // an empty board keeps.
        ontop_start_offset: ROWS as i32,
    };

    if section.visible_article_count == 0 {
        return Ok(page);
    }
    if page_id < 0 || page_id >= page.page_count {
        tracing::debug!("Invalid page_id={page_id}, not in range [0, {})", page.page_count);
        return Err(BrowseError::OutOfRange { sid, page_id });
    }

    let articles = board.articles();

    if page_id <= section.page_count() - 1 {
        if let Some(first) = section.page_first.get(page_id as usize).copied() {
            // `p_next_page_first_article`: the next page's first article, and for the last
            // indexed page the head of the section, which the circular list reaches from its
            // tail.
            let last_indexed = page_id == section.page_count() - 1;
            let stop = if last_indexed {
                section.head
            } else {
                section.page_first.get(page_id as usize + 1).copied()
            };

            let mut art = first;
            loop {
                if articles[art].visible {
                    page.rows.push(art);
                }
                art = articles[art].next;
                // The C condition is `(*p_article_count) <= BBS_article_limit_per_page` after
                // the push, which is the one past the end described in the module header.
                if Some(art) == stop || page.count() > ARTICLE_LIMIT_PER_PAGE {
                    break;
                }
            }

            let expected = if page_id < section.page_count() - 1 {
                ARTICLE_LIMIT_PER_PAGE
            } else {
                section.last_page_visible_article_count
            };
            if page.count() != expected {
                tracing::error!(
                    "Inconsistent visible article count {} detected in section {sid} page {page_id}",
                    page.count()
                );
            }
        }
    }

    page.ontop_start_offset = page.count();

    if page_id >= section.page_count() - 1 {
        // The stickies of this page: from the top for the last indexed page, and further down
        // the list for the pages past it.
        let mut i = if page_id == section.page_count() - 1 {
            0
        } else {
            (page_id - section.page_count() + 1) * ARTICLE_LIMIT_PER_PAGE
                - section.last_page_visible_article_count
        };
        while page.rows.len() < ROWS && i < section.ontop_article_count() {
            if let Some(&art) = section.ontop.get(i as usize) {
                page.rows.push(art);
            }
            i += 1;
        }
    }

    Ok(page)
}

/// The out-parameters of `locate_article_in_section` (`section_list_loader.c:954`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub page_id: i32,
    /// `*p_visible_offset`: the row of the found article, counting visible posts only.
    pub offset: i32,
    /// `*p_article_count` when the function returns: the page's rows with the stickies.
    pub count: i32,
}

/// `locate_article_in_section` for the three `direction` values the C callers use:
///
/// * `0` — the page and row holding `cur` itself, which is how a `LOCATE_ARTICLE` menu item, a
///   saved reading position and every scan result find their place.
/// * `1` and `-1` — `step` visible posts along the topic list, newer and older. A page turn
///   inside a thread asks for `step = 1`; the first and last post of a thread ask for the whole
///   section limit.
pub fn locate_article(
    board: &Board,
    section: &Section,
    cur: Artp,
    direction: i32,
    step: i32,
) -> Option<Located> {
    let articles = board.articles();
    let current = articles.get(cur)?;

    let aid = match direction {
        0 => current.aid,
        1 => {
            let mut last = cur;
            let mut art = current.topic_next;
            let mut left = step;
            while left > 0 && articles[art].aid > current.aid {
                if articles[art].visible {
                    left -= 1;
                    last = art;
                }
                art = articles[art].topic_next;
            }
            let found = &articles[last];
            if found.aid > current.aid && found.visible {
                found.aid
            } else {
                0
            }
        }
        -1 => {
            let mut last = cur;
            let mut art = current.topic_prior;
            let mut left = step;
            while left > 0 && articles[art].aid < current.aid {
                if articles[art].visible {
                    left -= 1;
                    last = art;
                }
                art = articles[art].topic_prior;
            }
            let found = &articles[last];
            if found.aid < current.aid && found.visible {
                found.aid
            } else {
                0
            }
        }
        // C initialises `aid = 0` and only overwrites it for the three cases above.
        _ => 0,
    };

    if aid <= 0 {
        return None;
    }

    // `section_list_find_article_with_offset`, whose page the rest of the function walks
    // again, because its `offset` counts every article of the page and the row index must
    // count only the visible ones.
    let at = section.find_article_with_offset(articles, aid)?;
    at.article?;
    let page_id = at.page;

    let mut count = if page_id >= section.page_count() - 1 {
        section.last_page_visible_article_count
    } else {
        ARTICLE_LIMIT_PER_PAGE
    };
    let first = section.page_first.get(page_id as usize).copied()?;

    let mut art = first;
    let mut offset = 0;
    let result = loop {
        if offset >= count {
            break None;
        }
        if articles[art].visible {
            if articles[art].aid == aid {
                break Some((page_id, offset));
            }
            offset += 1;
            if offset >= count {
                tracing::error!("Visible article (aid={aid}) not found in page {page_id}");
                break None;
            }
        }
        art = articles[art].next;
        if art == first {
            tracing::error!("Dead loop detected at page={page_id}, article_count={count}");
            break None;
        }
    };

    count = section.page_article_count_with_ontop(page_id);
    result.map(|(page_id, offset)| Located {
        page_id,
        offset,
        count,
    })
}

/// `last_article_in_section` (`:1068`): the newest post still on show, walked back from the
/// tail of the section list. `None` is C's `0`, a board whose every article is hidden.
pub fn last_article(board: &Board, section: &Section) -> Option<Artp> {
    let articles = board.articles();
    let mut art = section.tail?;
    while Some(art) != section.head && !articles[art].visible {
        art = articles[art].prior;
    }
    articles.get(art).filter(|a| a.visible).map(|_| art)
}

/// `scan_unread_article_in_section` (`:1110`): forward from `cur`, over the end of the list,
/// for the first post someone else wrote that this reader has not seen.
pub fn scan_unread(board: &Board, cur: Artp, uid: i32, view_log: &ViewLog) -> Option<Artp> {
    let articles = board.articles();
    let mut art = articles[cur].next;
    while art != cur {
        let article = &articles[art];
        if article.visible && article.uid != uid && !view_log.is_viewed(article.aid) {
            return Some(art);
        }
        art = articles[art].next;
    }
    None
}

/// `scan_article_in_section_by_uid` (`:1155`). `direction >= 0` walks the newer posts,
/// `< 0` the older.
pub fn scan_by_uid(board: &Board, cur: Artp, direction: i32, uid: i32) -> Option<Artp> {
    scan(board, cur, direction, |article| article.uid == uid)
}

/// `scan_article_in_section_by_username` (`:1215`), for an author that is not in the user list
/// any more: C's `strcasecmp`, which for the ASCII-only username buffer is a lower-case
/// compare.
pub fn scan_by_username(board: &Board, cur: Artp, direction: i32, username: &str) -> Option<Artp> {
    let needle = username.to_lowercase();
    scan(board, cur, direction, |article| {
        article.username.to_lowercase() == needle
    })
}

/// `scan_article_in_section_by_title` (`:1275`): C's `strcasestr`, a case-insensitive
/// substring of the title.
pub fn scan_by_title(board: &Board, cur: Artp, direction: i32, title: &str) -> Option<Artp> {
    let needle = title.to_lowercase();
    scan(board, cur, direction, |article| {
        article.title.to_lowercase().contains(&needle)
    })
}

/// The walk the three scans share. C guards it with `p_article != p_article_cur`, which is the
/// circular list coming back around to where it started.
fn scan(board: &Board, cur: Artp, direction: i32, matches: impl Fn(&Article) -> bool) -> Option<Artp> {
    let articles = board.articles();
    let link = |article: &Article| if direction >= 0 { article.next } else { article.prior };

    let mut art = link(&articles[cur]);
    while art != cur {
        let article = &articles[art];
        if article.visible && matches(article) {
            return Some(art);
        }
        art = link(article);
    }
    None
}

// ---------------------------------------------------------------------------
// The row text
// ---------------------------------------------------------------------------

/// The name column, `section_list_draw_items:182-187`: the nickname at half its field width,
/// or the username at full width.
pub fn name_column(article: &Article, display_nickname: bool) -> (&str, i64) {
    if display_nickname {
        let field = NICKNAME_MAX_LEN as i64 / 2;
        (
            &article.nickname,
            field - text::display_width(article.nickname.as_bytes(), true, false) as i64,
        )
    } else {
        (
            &article.username,
            USERNAME_MAX_LEN as i64 - text::display_width(article.username.as_bytes(), true, false) as i64,
        )
    }
}

/// `%*s` of the padding: a negative width makes `printf` left-justify and pad to the absolute
/// value, so an over-long name still pushes the date right — by fewer cells.
pub fn padding(budget: i64) -> usize {
    if budget < 0 {
        budget.unsigned_abs() as usize
    } else {
        budget as usize
    }
}

/// The name field the title budget of `59 - …` is measured against.
pub fn name_field(display_nickname: bool) -> i64 {
    if display_nickname {
        NICKNAME_MAX_LEN as i64 / 2
    } else {
        USERNAME_MAX_LEN as i64
    }
}

/// `strftime` of the submission time: `%b %e ` inside the last year, `%m/%Y` before it. The
/// trailing space of the first form is why the year rows sit one column narrower.
pub fn row_time(sub_dt: i64, now: i64) -> String {
    let Some(local) = chrono::Local.timestamp_opt(sub_dt, 0).single() else {
        return String::new();
    };
    if now - sub_dt < 3600 * 24 * 365 {
        local.format("%b %e ").to_string()
    } else {
        local.format("%m/%Y").to_string()
    }
}

/// The title with its topic, transship and `Re: Re: ` markers, before the width cut
/// (`section_list_draw_items:147-168`).
pub fn row_title(article: &Article) -> String {
    let mut out = String::new();
    if article.is_topic_head() {
        out.push_str("● ");
    }
    if article.transship {
        out.push_str("[转载]");
    }

    let mut skip = 0usize;
    if !article.is_topic_head() {
        while article.title[skip..].starts_with("Re: ") {
            skip += 4;
        }
        // Two literal `Re: ` prefixes collapse into `Re: ... `; one is written out in full.
        if skip >= 8 {
            out.push_str("Re: ... ");
        } else {
            skip = 0;
        }
    }
    out.push_str(&article.title[skip..]);
    out
}

/// The first flag column: `@` on a starred topic, a space on every reply.
pub fn favor_column(article: &Article, favor: &FavorLog) -> char {
    if article.is_topic_head() && favor.is_favor(article.aid) {
        '@'
    } else {
        ' '
    }
}

/// The second flag column, `section_list_draw_items:96-135`. A reader's own posts count as
/// read, which is what hides the `N` from one's own thread.
pub fn article_flag(article: &Article, uid: i32, view_log: &ViewLog) -> char {
    let is_viewed = article.uid == uid || view_log.is_viewed(article.aid);
    if article.excerption {
        if is_viewed {
            'm'
        } else {
            'M'
        }
    } else if article.lock && is_viewed {
        'x'
    } else if is_viewed {
        ' '
    } else {
        'N'
    }
}

/// The colour of a row's title: amber for the head of the thread being read, cyan for a post
/// in it, nothing for every other row.
pub fn topic_color(article: &Article, topic_tid: i32) -> &'static str {
    if article.aid == topic_tid {
        "\x1b[1;33m"
    } else if article.tid == topic_tid {
        "\x1b[1;36m"
    } else {
        ""
    }
}

/// What a row needs besides the article: the two marker tables, the name mode, the colouring
/// and the width mode. Built once per screen draw.
#[derive(Debug)]
pub struct RowEnv<'a> {
    pub uid: i32,
    pub view_log: &'a ViewLog,
    pub favor: &'a FavorLog,
    pub display_nickname: bool,
    pub topic_tid: i32,
    pub fixed: bool,
    pub now: i64,
}

/// One row of the list, from the two `prints` calls of `section_list_draw_items:179-215`: the
/// ordinary rows carry the article number, the pinned ones the `[提示]` marker in its place.
pub fn row_line(article: &Article, sticky: bool, env: &RowEnv) -> String {
    let title = row_title(article);
    let cells = (59 - name_field(env.display_nickname)).max(0) as usize;
    let split = text::split_line(title.as_bytes(), cells, true, env.fixed);
    let cut = split.bytes.min(title.len());
    let title = String::from_utf8_lossy(&title.as_bytes()[..cut]).into_owned();

    let (name, budget) = name_column(article, env.display_nickname);
    let pad = padding(budget);
    let color = topic_color(article, env.topic_tid);
    let favor = favor_column(article, env.favor);
    let flag = article_flag(article, env.uid, env.view_log);
    let time = row_time(article.sub_dt, env.now);

    if sticky {
        format!(
            "   \x1b[1;33m[提示]\x1b[m{favor}{flag} {name}{:pad$} {time} {color}{title}\x1b[m",
            "",
            pad = pad,
        )
    } else {
        format!(
            "  {color}{:>7}\x1b[m{favor}{flag} {name}{:pad$} {time} {color}{title}\x1b[m",
            article.aid,
            "",
            pad = pad,
        )
    }
}

// ---------------------------------------------------------------------------
// The saved reading positions
// ---------------------------------------------------------------------------

/// `section_aid_locations_save` (`:1473`): one native `int32_t` pair per board of the pool, in
/// pool order. The port keeps an `i16` sid in memory and widens it here, so the file stays the
/// shape the C daemon reads back.
pub fn aid_locations_write(path: &Path, pairs: &[(i16, i32)]) -> io::Result<usize> {
    let mut bytes = Vec::with_capacity(pairs.len() * 8);
    for (sid, aid) in pairs {
        bytes.extend_from_slice(&(*sid as i32).to_ne_bytes());
        bytes.extend_from_slice(&aid.to_ne_bytes());
    }
    std::fs::write(path, &bytes)?;
    Ok(pairs.len())
}

/// `section_aid_locations_load` (`:1514`). A missing file is C's `return 0` — nothing saved,
/// no error — and a pair cut short by the end of the file is dropped the way C's `feof` loop
/// drops it.
pub fn aid_locations_read(path: &Path) -> io::Result<Vec<(i32, i32)>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut pairs = Vec::new();
    loop {
        let mut sid = [0u8; 4];
        if !read_pair_or_stop(&mut file, &mut sid)? {
            break;
        }
        let mut aid = [0u8; 4];
        if !read_pair_or_stop(&mut file, &mut aid)? {
            break;
        }
        pairs.push((i32::from_ne_bytes(sid), i32::from_ne_bytes(aid)));
    }

    Ok(pairs)
}

/// `fread(...) != 1` with `ferror(fp) == 0`, which is the end of the file; a part-written
/// column is C's `-2` and is dropped here too.
fn read_pair_or_stop(file: &mut impl Read, buf: &mut [u8; 4]) -> io::Result<bool> {
    let mut read = 0usize;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => return Ok(false),
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// The screen
// ---------------------------------------------------------------------------

/// The state of one visit to a board, which is `section_list_display`'s frame.
#[derive(Debug, Clone)]
pub struct Listing {
    pub sid: i16,
    pub sname: String,
    pub stitle: String,
    pub master_list: String,
    /// `p_articles[]`.
    pub rows: Vec<Artp>,
    /// `article_count`.
    pub count: i32,
    /// `page_count`: the with-sticky count the footer prints.
    pub page_count: i32,
    /// `ontop_start_offset`.
    pub ontop_start_offset: i32,
    pub page_id: i32,
    /// `selected_index`.
    pub selected: i32,
    /// `section_topic_view_mode`.
    pub topic_mode: bool,
    /// `section_topic_view_tid`.
    pub topic_tid: i32,
    /// The `title` frame local of `section_list_display`, which the `/` and `?` prompts
    /// pre-fill with what the last search in this board typed.
    pub title_search: String,
}

impl Listing {
    /// `section_list_display(sname, aid)` up to its first screen: the board lookup, the
    /// permission test, the board's own strings and the position the list starts at.
    pub fn open(session: &mut Session, sname: &str, aid: i32) -> Result<Self, BrowseError> {
        let board = session.state.board();
        let Some(section) = board.section_by_name(sname) else {
            tracing::error!("Section {sname} not found");
            return Err(BrowseError::SectionNotFound);
        };

        let sid = section.sid;
        if !session.privs.checkpriv(sid, S_LIST) {
            let uid = session.privs.uid;
            drop(board);
            tracing::error!("Forbid access to unauthorized section, sid={sid}, uid={uid}");
            return Err(BrowseError::Forbidden { sid, uid });
        }

        let mut listing = Listing {
            sid,
            sname: section.sname.clone(),
            stitle: section.stitle.clone(),
            master_list: section.master_list.clone(),
            rows: Vec::new(),
            count: 0,
            page_count: 1,
            ontop_start_offset: ROWS as i32,
            page_id: 0,
            selected: 0,
            topic_mode: false,
            topic_tid: -1,
            title_search: String::new(),
        };

        // `aid == 0` reads the board's saved position; anything else is the post the caller
        // asked for, which is how a `LOCATE_ARTICLE` item and a top-ten entry land on it.
        let aid_location = if aid == 0 {
            session.aid_locations.get(&sid).copied().unwrap_or(0)
        } else {
            aid
        };

        if aid_location > 0 {
            let Some(at) = board.article_index(aid_location) else {
                drop(board);
                tracing::error!("article_block_find_by_aid({aid_location}) error");
                return Err(BrowseError::SectionNotFound);
            };
            if let Some(located) = locate_article(&board, section, at, 0, 0) {
                listing.page_id = located.page_id;
                listing.selected = located.offset;
            }
        }

        Ok(listing)
    }

    /// `query_section_articles` for the page the cursor is on, with C's two cursor rules: an
    /// empty page starts at row 0, a short one pulls the cursor back inside it.
    fn query(&mut self, session: &Session) -> Result<(), BrowseError> {
        let board = session.state.board();
        let Some(section) = board.section(self.sid) else {
            return Err(BrowseError::SectionNotFound);
        };
        let page = query_section(&board, section, self.page_id)?;

        self.count = page.rows.len() as i32;
        self.rows = page.rows;
        self.page_count = page.page_count;
        self.ontop_start_offset = page.ontop_start_offset;

        if self.count == 0 {
            self.selected = 0;
        } else if self.selected >= self.count {
            self.selected = self.count - 1;
        }

        Ok(())
    }

    /// The row the cursor is on, `p_articles[selected_index]`.
    fn current(&self) -> Option<Artp> {
        if self.selected < 0 || self.selected >= self.count {
            return None;
        }
        self.rows.get(self.selected as usize).copied()
    }

    /// `section_list_draw_screen` (`:222`).
    fn draw_screen(&self, term: &mut Terminal, session: &Session) {
        let master = if self.master_list.is_empty() {
            "诚征版主中".to_string()
        } else {
            format!("版主: {}", self.master_list)
        };

        screen::clearscr(term);
        screen::show_top(
            term,
            &master,
            &self.stitle,
            &format!("讨论区 [{}]", self.sname),
        );

        term.moveto(2, 0);
        term.print(
            "返回[\x1b[1;32m←\x1b[0;37m,\x1b[1;32mESC\x1b[0;37m] 选择[\x1b[1;32m↑\x1b[0;37m,\x1b[1;32m↓\x1b[0;37m] \
             阅读[\x1b[1;32m→\x1b[0;37m,\x1b[1;32mENTER\x1b[0;37m] 发表[\x1b[1;32mCtrl-P\x1b[0;37m] ",
        );
        term.print(if session.display_nickname {
            "用户名[\x1b[1;32mn\x1b[0;37m] 精华区[\x1b[1;32mx\x1b[0;37m] 帮助[\x1b[1;32mh\x1b[0;37m]\x1b[m"
        } else {
            "昵称[\x1b[1;32mn\x1b[0;37m] 精华区[\x1b[1;32mx\x1b[0;37m] 帮助[\x1b[1;32mh\x1b[0;37m]\x1b[m"
        });

        term.moveto(3, 0);
        term.print(if session.display_nickname {
            "\x1b[44;37m  \x1b[1;37m 编  号   发布者昵称           日  期  文章标题                               \x1b[m"
        } else {
            "\x1b[44;37m  \x1b[1;37m 编  号   发 布 者     日  期  文章标题                                       \x1b[m"
        });
    }

    /// `section_list_draw_items` (`:75`). The rows are composed under the board lock and
    /// written after it is released, because nothing here may hold a lock across a key.
    fn draw_items(&self, term: &mut Terminal, session: &Session) {
        screen::clrline(term, FIRST_ROW, LAST_ROW);

        let env = RowEnv {
            uid: session.privs.uid,
            view_log: &session.view_log,
            favor: &session.favor,
            display_nickname: session.display_nickname,
            topic_tid: self.topic_tid,
            fixed: term.fixed_width(),
            now: session::now(),
        };

        let board = session.state.board();
        let lines: Vec<String> = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, &art)| match board.article(art) {
                Some(article) => row_line(article, (index as i32) >= self.ontop_start_offset, &env),
                None => String::new(),
            })
            .collect();
        drop(board);

        for (index, line) in lines.into_iter().enumerate() {
            term.moveto(FIRST_ROW + index, 1);
            term.print(&line);
        }
    }

    /// The footer between the rows and the key: `\033[33m[第\033[36m%d\033[33m/\033[36m%d\
    /// [33m页]`, an empty board shown as page 1/1.
    fn show_page_info(&self, term: &mut Terminal, session: &Session) {
        let info = format!(
            "\x1b[33m[第\x1b[36m{}\x1b[33m/\x1b[36m{}\x1b[33m页]",
            self.page_id + 1,
            self.page_count.max(1)
        );
        screen::show_bottom(term, &info, &session.username, session.login_tm);
    }

    /// `section_list_select` (`:253`): the cursor, the key, and the movement rules with their
    /// documented fall-throughs — `HOME` sets the page and then behaves like `PgUp`, which
    /// behaves like `k`.
    async fn select(&mut self, term: &mut Terminal, session: &mut Session) -> SelectCmd {
        let old_page_id = self.page_id;
        let mut old_selected = self.selected;
        let last_refresh_tm = session::now();

        if self.count > 0 && self.selected >= 0 {
            term.moveto(FIRST_ROW + self.selected as usize, 1);
            term.print(">");
            let _ = term.flush().await;
        }

        loop {
            if session.state.is_exiting() {
                return SelectCmd::ExitSection;
            }

            let ch = term.read_key(SELECT_KEY_TIMEOUT).await;

            if ch != KEY_NULL && ch != KEY_TIMEOUT {
                session.record_activity().await;
            }

            match ch {
                KEY_NULL => {
                    tracing::debug!("KEY_NULL");
                    return SelectCmd::ExitSection;
                }
                KEY_TIMEOUT => {
                    if session::now() - session.last_access_tm >= MAX_USER_IDLE_TIME as i64 {
                        tracing::debug!("User input timeout");
                        return SelectCmd::ExitSection;
                    }
                    continue;
                }
                KEY_ESC | KEY_LEFT => return SelectCmd::ExitSection,
                KEY_N => return SelectCmd::ChangeNameDisplay,
                CR | KEY_R | KEY_RIGHT => {
                    if self.count > 0 {
                        return SelectCmd::ViewArticle;
                    }
                }
                KEY_CTRL_P => return SelectCmd::PostArticle,
                KEY_E => {
                    if self.count > 0 {
                        return SelectCmd::EditArticle;
                    }
                }
                KEY_D => {
                    if self.count > 0 {
                        return SelectCmd::DeleteArticle;
                    }
                }
                KEY_CTRL_Q => {
                    if self.count > 0 {
                        return SelectCmd::QueryArticle;
                    }
                }
                KEY_CTRL_A => {
                    if self.count > 0 {
                        return SelectCmd::QueryUser;
                    }
                }
                KEY_F => {
                    if self.count > 0 {
                        return SelectCmd::SetFavorArticle;
                    }
                }
                KEY_MINUS => {
                    if self.count > 0 {
                        return SelectCmd::UnsetFavorArticle;
                    }
                }
                KEY_M => {
                    if self.count > 0 {
                        return SelectCmd::SetExcerptionArticle;
                    }
                }
                KEY_HOME | KEY_UPPER_P | KEY_PGUP | KEY_LOWER_K | KEY_UP => {
                    if ch == KEY_HOME {
                        self.page_id = 0;
                    }
                    if matches!(ch, KEY_HOME | KEY_UPPER_P | KEY_PGUP) {
                        self.selected = 0;
                    }
                    if self.selected <= 0 {
                        if self.page_id > 0 {
                            self.page_id -= 1;
                            self.selected = ARTICLE_LIMIT_PER_PAGE - 1;
                        } else if matches!(ch, KEY_UP | KEY_LOWER_K) {
                            // Rotate to the tail of the section list.
                            if self.page_count > 0 {
                                self.page_id = self.page_count - 1;
                            }
                            if self.count > 0 {
                                self.selected = self.count - 1;
                            }
                        }
                    } else {
                        self.selected -= 1;
                    }
                }
                KEY_DOLLAR | KEY_END | KEY_UPPER_N | KEY_PGDN | KEY_LOWER_J | KEY_DOWN => {
                    if matches!(ch, KEY_DOLLAR | KEY_END)
                        && self.page_id + 1 == self.page_count
                        && self.selected + 1 == self.count
                    {
                        // Press END at the end of the list.
                        return SelectCmd::LastSectionArticle;
                    }
                    if matches!(ch, KEY_DOLLAR | KEY_END) && self.page_count > 0 {
                        self.page_id = self.page_count - 1;
                    }
                    if matches!(ch, KEY_DOLLAR | KEY_END | KEY_UPPER_N | KEY_PGDN) && self.count > 0 {
                        self.selected = self.count - 1;
                    }
                    if self.selected + 1 >= self.count {
                        if self.page_id + 1 < self.page_count {
                            self.page_id += 1;
                            self.selected = 0;
                        } else if matches!(ch, KEY_DOWN | KEY_LOWER_J) {
                            // Rotate to the head of the section list.
                            self.page_id = 0;
                            self.selected = 0;
                        }
                    } else {
                        self.selected += 1;
                    }
                }
                KEY_EQUAL => {
                    if self.count > 0 {
                        return SelectCmd::FirstTopicArticle;
                    }
                }
                KEY_BACKSLASH => {
                    if self.count > 0 {
                        return SelectCmd::LastTopicArticle;
                    }
                }
                KEY_UPPER_S => {
                    if self.count > 0 {
                        return SelectCmd::ScanNewArticle;
                    }
                }
                KEY_UPPER_A => {
                    if self.count > 0 {
                        return SelectCmd::ScanArticleBackwardByUser;
                    }
                }
                KEY_LOWER_A => {
                    if self.count > 0 {
                        return SelectCmd::ScanArticleForwardByUser;
                    }
                }
                KEY_QUESTION => {
                    if self.count > 0 {
                        return SelectCmd::ScanArticleBackwardByTitle;
                    }
                }
                KEY_SLASH => {
                    if self.count > 0 {
                        return SelectCmd::ScanArticleForwardByTitle;
                    }
                }
                KEY_U => return SelectCmd::SearchUser,
                KEY_H => return SelectCmd::ShowHelp,
                KEY_X => return SelectCmd::ViewExDir,
                KEY_UPPER_H => return SelectCmd::ShowTop10,
                _ => {}
            }

            if old_page_id != self.page_id {
                return SelectCmd::ChangePage;
            }

            if self.count > 0 && old_selected != self.selected {
                if old_selected >= 0 {
                    term.moveto(FIRST_ROW + old_selected as usize, 1);
                    term.print(" ");
                }
                if self.selected >= 0 {
                    term.moveto(FIRST_ROW + self.selected as usize, 1);
                    term.print(">");
                }
                let _ = term.flush().await;

                old_selected = self.selected;
            }

            if session.last_access_tm - last_refresh_tm >= SECTION_LIST_LOAD_INTERVAL {
                return SelectCmd::ChangePage; // force section list refresh
            }
        }
    }

    /// `section_list_display` (`:590`) from its first screen on. `Ok(())` is C's `return 0`,
    /// reached both by the exit key and by the daemon shutting down; `Err` is one of the
    /// negative returns, whose message is already in the log.
    pub async fn run(
        term: &mut Terminal,
        session: &mut Session,
        sname: &str,
        aid: i32,
    ) -> Result<(), BrowseError> {
        let mut listing = Listing::open(session, sname, aid)?;

        listing.draw_screen(term, session);
        listing.query(session)?;
        listing.topic_tid = -1;

        if aid > 0 {
            // Update the current topic and the board's saved position from the row the caller
            // asked for.
            let board = session.state.board();
            if let Some(article) = listing.current().and_then(|art| board.article(art)) {
                listing.topic_tid = if article.tid == 0 { article.aid } else { article.tid };
                session.aid_locations.insert(listing.sid, article.aid);
            }
        }

        while !session.state.is_exiting() {
            listing.draw_items(term, session);
            listing.show_page_info(term, session);
            let _ = term.flush().await;

            let action = listing.sname.clone();
            if let Err(e) = session.online_update(Some(&action)).await {
                tracing::error!("user_online_update({action}) error: {e}");
            }

            let cmd = listing.select(term, session).await;
            match listing.dispatch(term, session, cmd).await? {
                Flow::Redraw => listing.draw_screen(term, session),
                Flow::Stay => {}
                Flow::Out => return Ok(()),
            }
        }

        Ok(())
    }

    /// The `switch (ret)` of `section_list_display`'s loop. `Redraw` is the
    /// `section_list_draw_screen()` call that ends the arm, `Stay` the bare `break` back to
    /// `section_list_draw_items`.
    async fn dispatch(
        &mut self,
        term: &mut Terminal,
        session: &mut Session,
        cmd: SelectCmd,
    ) -> Result<Flow, BrowseError> {
        match cmd {
            SelectCmd::ExitSection => {
                self.save_aid_location(session)?;
                Ok(Flow::Out)
            }
            SelectCmd::ChangePage => {
                self.query(session)?;
                Ok(Flow::Stay)
            }
            SelectCmd::ViewArticle => {
                self.read_article(term, session).await?;
                Ok(Flow::Redraw)
            }
            SelectCmd::ChangeNameDisplay => {
                session.display_nickname = !session.display_nickname;
                Ok(Flow::Redraw)
            }
            SelectCmd::SetFavorArticle | SelectCmd::UnsetFavorArticle => {
                // A reply stars its whole thread, which is what `tid == 0 ? aid : tid` says.
                let set = cmd == SelectCmd::SetFavorArticle;
                let target = {
                    let board = session.state.board();
                    let Some(art) = self.current() else {
                        return Err(BrowseError::SectionNotFound);
                    };
                    let article = board.article(art).expect("row of the board");
                    if article.is_topic_head() {
                        article.aid
                    } else {
                        article.tid
                    }
                };
                if let Err(e) = session.favor.mark(&session.state.db, target, set).await {
                    tracing::error!(
                        "article_favor_set(aid={target}, {}) error: {e}",
                        set as i32
                    );
                }
                Ok(Flow::Stay)
            }
            SelectCmd::FirstTopicArticle | SelectCmd::LastTopicArticle => {
                let direction = if cmd == SelectCmd::FirstTopicArticle { -1 } else { 1 };
                let located = self.step(session, direction, ARTICLE_LIMIT_PER_SECTION)?;
                self.jump(session, located).await
            }
            SelectCmd::LastSectionArticle => {
                let art = self.newest_visible(session)?;
                let located = match art {
                    Some(art) => self.locate(session, art, 0, 0)?,
                    None => None,
                };
                self.jump(session, located).await
            }
            SelectCmd::ScanNewArticle => {
                let located = {
                    let found = {
                        let board = session.state.board();
                        let cur = self.row_under_cursor()?;
                        scan_unread(&board, cur, session.privs.uid, &session.view_log)
                    };
                    match found {
                        Some(art) => self.locate(session, art, 0, 0)?,
                        None => None,
                    }
                };
                self.jump(session, located).await
            }
            SelectCmd::ScanArticleBackwardByUser | SelectCmd::ScanArticleForwardByUser => {
                let direction = if cmd == SelectCmd::ScanArticleForwardByUser { 1 } else { -1 };
                let located = self.scan_author(term, session, direction).await?;
                self.jump(session, located).await
            }
            SelectCmd::ScanArticleBackwardByTitle | SelectCmd::ScanArticleForwardByTitle => {
                let direction = if cmd == SelectCmd::ScanArticleForwardByTitle { 1 } else { -1 };
                let located = self.scan_title(term, session, direction).await?;
                self.jump(session, located).await
            }
            SelectCmd::ViewExDir => {
                self.view_ex_dir(term, session).await;
                Ok(Flow::Redraw)
            }
            SelectCmd::ShowTop10 => {
                crate::bbs::show_top10_menu(term, session).await;
                Ok(Flow::Redraw)
            }
            SelectCmd::ShowHelp => {
                session.display_file(term, DATA_READ_HELP, 1).await;
                Ok(Flow::Redraw)
            }
            SelectCmd::PostArticle => {
                if let Err(e) = session.online_update(Some("POST_ARTICLE")).await {
                    tracing::error!("user_online_update(POST_ARTICLE) error: {e}");
                }
                // C re-queries the page when the post returns 1; the arena still holds nothing
                // new until the loader runs, so that is the same redraw as any other exit.
                if let Err(e) =
                    post::article_post(term, session, self.sid, &self.sname, &self.stitle).await
                {
                    tracing::error!("article_post(sid={}) error: {e}", self.sid);
                }
                Ok(Flow::Redraw)
            }
            SelectCmd::EditArticle => {
                let (allowed, target) = {
                    let board = session.state.board();
                    let article = self.current().and_then(|art| board.article(art));
                    let mine = article.is_some_and(|a| a.uid == session.privs.uid);
                    let allowed = session.privs.checkpriv(self.sid, S_POST) && mine;
                    (allowed, article.map(|a| (a.aid, a.excerption)))
                };
                if !allowed {
                    return Ok(Flow::Stay); // No permission
                }
                let Some((aid, excerption)) = target else {
                    tracing::error!("article_modify(sid={}) error: the cursor is not on an article", self.sid);
                    return Ok(Flow::Redraw);
                };

                if let Err(e) = session.online_update(Some("EDIT_ARTICLE")).await {
                    tracing::error!("user_online_update() error: {e}");
                }

                if let Err(e) = post::article_modify(term, session, aid, excerption).await {
                    tracing::error!("article_modify(aid={aid}) error: {e}");
                }
                Ok(Flow::Redraw)
            }
            SelectCmd::DeleteArticle => {
                let (allowed, target) = {
                    let board = session.state.board();
                    let article = self.current().and_then(|art| board.article(art));
                    let mine = article.is_some_and(|a| a.uid == session.privs.uid);
                    let allowed = session.privs.checkpriv(self.sid, S_POST)
                        && (session.privs.checkpriv(self.sid, S_MAN_S) || mine);
                    (allowed, article.map(post::Deletable::from))
                };
                if !allowed {
                    return Ok(Flow::Stay); // No permission
                }

                let Some(target) = target else {
                    // The row vanished between the draw and the key; C would have deleted a
                    // `NULL` article, which its own guard returns `-1` for.
                    tracing::error!("article_del(aid={}) error: the cursor is not on an article", self.sid);
                    return Ok(Flow::Redraw);
                };

                // C re-queries the page when this returns 1, which after a delete is only a
                // redraw: the arena still holds the article until the loader drops it.
                if let Err(e) = post::article_del(term, session, target).await {
                    tracing::error!("article_del(aid={}) error: {e}", target.aid);
                }
                Ok(Flow::Redraw)
            }
            SelectCmd::SetExcerptionArticle => {
                // `section_list_display.c:1103-1113`: the gate is `S_POST | S_MAN_S`, and a user
                // who fails it is refused in silence — no message, no beep, the list as it was.
                if !session.privs.checkpriv(self.sid, S_POST | S_MAN_S) {
                    return Ok(Flow::Stay);
                }

                // The key is its own toggle and asks for no confirmation: C hands
                // `excerption ? 0 : 1` straight to the write.
                let (aid, set) = {
                    let board = session.state.board();
                    let Some(article) = self.current().and_then(|art| board.article(art)) else {
                        tracing::error!("article_excerption_set(sid={}) error: the cursor is not on an article", self.sid);
                        return Ok(Flow::Redraw);
                    };
                    (article.aid, !article.excerption)
                };

                if let Err(e) = post::article_excerption_set(session, self.sid, aid, set).await {
                    tracing::error!(
                        "article_excerption_set(sid={}, aid={aid}, set={}) error: {e}",
                        self.sid,
                        i32::from(set)
                    );
                }
                // The row's marker is the whole response, so the list has to be drawn again —
                // which is what C's loop top does after every command.
                Ok(Flow::Redraw)
            }
            SelectCmd::QueryArticle => {
                tracing::error!("display_article_meta is not implemented");
                Ok(Flow::Redraw)
            }
            SelectCmd::QueryUser => {
                let uid = {
                    let board = session.state.board();
                    let Some(article) = self.current().and_then(|art| board.article(art)) else {
                        tracing::error!("query_user_info_by_uid error: the cursor is not on an article");
                        return Ok(Flow::Redraw);
                    };
                    article.uid
                };
                // C tells a uid that is not in the roster (`ret == 0`) from a shared-memory
                // lock it could not take (`ret < 0`, which leaves the list). The port's `Arc`
                // always loads, so one roster miss is all one screen can mean.
                let roster = session.state.roster();
                match roster.by_uid(uid) {
                    None => {
                        screen::clearscr(term);
                        term.print("该用户已升天");
                        screen::press_any_key(term).await;
                    }
                    Some(user) => crate::userinfo::user_info_display(term, session, user).await,
                }
                Ok(Flow::Redraw)
            }
            SelectCmd::SearchUser => {
                crate::userinfo::user_list_search(term, session).await;
                Ok(Flow::Redraw)
            }
        }
    }

    /// `p_articles[selected_index]`, or C's `-1` through [`BrowseError::SectionNotFound`].
    fn row_under_cursor(&self) -> Result<Artp, BrowseError> {
        self.current().ok_or(BrowseError::SectionNotFound)
    }

    /// [`locate_article`] for the row under the cursor, which is the shape every movement key
    /// of the list has.
    fn step(
        &self,
        session: &Session,
        direction: i32,
        step: i32,
    ) -> Result<Option<Located>, BrowseError> {
        let board = session.state.board();
        let section = board.section(self.sid).ok_or(BrowseError::SectionNotFound)?;
        let cur = self.row_under_cursor()?;
        Ok(locate_article(&board, section, cur, direction, step))
    }

    /// [`locate_article`] for an article the scan handed back.
    fn locate(
        &self,
        session: &Session,
        art: Artp,
        direction: i32,
        step: i32,
    ) -> Result<Option<Located>, BrowseError> {
        let board = session.state.board();
        let section = board.section(self.sid).ok_or(BrowseError::SectionNotFound)?;
        Ok(locate_article(&board, section, art, direction, step))
    }

    /// `page_id_cur = page_id`, `locate`, then re-query only when the page moved — the sequence
    /// the scans and the topic moves repeat verbatim.
    async fn jump(
        &mut self,
        session: &Session,
        located: Option<Located>,
    ) -> Result<Flow, BrowseError> {
        let Some(located) = located else {
            return Ok(Flow::Stay);
        };
        let page_id_cur = self.page_id;
        self.page_id = located.page_id;
        self.selected = located.offset;
        if self.page_id != page_id_cur {
            self.query(session)?;
        }
        Ok(Flow::Stay)
    }

    /// [`last_article`] under its own lock.
    fn newest_visible(&self, session: &Session) -> Result<Option<Artp>, BrowseError> {
        let board = session.state.board();
        let section = board.section(self.sid).ok_or(BrowseError::SectionNotFound)?;
        Ok(last_article(&board, section))
    }

    /// The `EXIT_SECTION` arm (`:726`): the row the cursor leaves on becomes the board's saved
    /// position, and a cursor on the sticky tail saves the newest readable post instead.
    fn save_aid_location(&mut self, session: &mut Session) -> Result<(), BrowseError> {
        if self.selected < 0 || self.selected >= self.count {
            return Ok(());
        }

        let stored = if self.selected >= self.ontop_start_offset {
            let art = self.newest_visible(session)?;
            match art {
                Some(art) => {
                    let board = session.state.board();
                    board.article(art).map(|a| a.aid).unwrap_or(0)
                }
                None => 0,
            }
        } else {
            let board = session.state.board();
            self.current()
                .and_then(|art| board.article(art))
                .map(|a| if a.visible { a.aid } else { 0 })
                .unwrap_or(0)
        };

        session.aid_locations.insert(self.sid, stored);
        Ok(())
    }

    /// The author scan (`:1197`): the prompt, whose default is the current post's author, the
    /// username format check, and the uid-or-name lookup that decides which of the two scans
    /// runs.
    async fn scan_author(
        &mut self,
        term: &mut Terminal,
        session: &mut Session,
        direction: i32,
    ) -> Result<Option<Located>, BrowseError> {
        let initial = {
            let board = session.state.board();
            self.current()
                .and_then(|art| board.article(art))
                .map(|a| a.username.clone())
                .unwrap_or_default()
        };

        term.moveto(SCREEN_ROWS, 1);
        term.clrtoeol();

        let prompt = if direction == 1 { "↓搜寻作者: " } else { "↑搜寻作者: " };
        let typed = screen::get_data(
            term,
            SCREEN_ROWS,
            1,
            prompt,
            initial.as_bytes(),
            USERNAME_MAX_LEN + 1,
            USERNAME_MAX_LEN,
            MAX_USER_IDLE_TIME,
        )
        .await;

        let username = match typed {
            None => return Ok(None),
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        if username.is_empty() || !valid_username(&username) {
            return Ok(None);
        }

        let uid = session
            .state
            .roster()
            .users
            .iter()
            .find(|user| user.username.eq_ignore_ascii_case(&username))
            .map(|user| user.uid)
            .unwrap_or(0);

        let found = {
            let board = session.state.board();
            let cur = self.row_under_cursor()?;
            if uid > 0 {
                scan_by_uid(&board, cur, direction, uid)
            } else {
                scan_by_username(&board, cur, direction, &username)
            }
        };

        Ok(match found {
            Some(art) => self.locate(session, art, 0, 0)?,
            None => None,
        })
    }

    /// The title scan (`:1289`), whose prompt pre-fills with the last title searched in this
    /// board because `title` is a frame local that `get_data` writes into.
    async fn scan_title(
        &mut self,
        term: &mut Terminal,
        session: &mut Session,
        direction: i32,
    ) -> Result<Option<Located>, BrowseError> {
        term.moveto(SCREEN_ROWS, 1);
        term.clrtoeol();

        let prompt = if direction == 1 { "↓搜寻标题: " } else { "↑搜寻标题: " };
        let typed = screen::get_data(
            term,
            SCREEN_ROWS,
            1,
            prompt,
            self.title_search.as_bytes(),
            ARTICLE_TITLE_MAX_LEN + 1,
            TITLE_SEARCH_MAX_LEN,
            MAX_USER_IDLE_TIME,
        )
        .await;

        let title = match typed {
            None => return Ok(None),
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        self.title_search = title.clone();
        if title.is_empty() {
            return Ok(None);
        }

        let found = {
            let board = session.state.board();
            let cur = self.row_under_cursor()?;
            scan_by_title(&board, cur, direction, &title)
        };

        Ok(match found {
            Some(art) => self.locate(session, art, 0, 0)?,
            None => None,
        })
    }

    /// The `VIEW_EX_DIR` arm (`section_list_display.c:1353-1363`): the board's digest menu, and
    /// C's `log_error` of a negative return — the list redraws either way, which the caller's
    /// [`Flow::Redraw`] is.
    async fn view_ex_dir(&self, term: &mut Terminal, session: &mut Session) {
        if !list_ex_dir(term, session, self.sid).await {
            tracing::error!("section_list_ex_dir_display(sid={}) error", self.sid);
        }
    }

    /// The `VIEW_ARTICLE` arm (`:769`): the reader, the keys its pager hands back, and the row
    /// the cursor ends on. `loop` is C's `do … while (loop)`, which keeps the reader open after
    /// a move that landed on another post.
    async fn read_article(
        &mut self,
        term: &mut Terminal,
        session: &mut Session,
    ) -> Result<(), BrowseError> {
        loop {
            let mut again = false;

            let (cur, aid, cid) = {
                let board = session.state.board();
                let cur = self.row_under_cursor()?;
                let article = board.article(cur).ok_or(BrowseError::SectionNotFound)?;
                (cur, article.aid, article.cid)
            };

            let dir = session.root.join(VAR_ARTICLE_CACHE_DIR);
            let cache = match cache::load(&dir.to_string_lossy(), cid) {
                Ok(Some(cached)) => cached,
                Ok(None) => {
                    tracing::error!("article_cache_load(aid={aid}, cid={cid}) error");
                    break;
                }
                Err(error) => {
                    tracing::error!("article_cache_load(aid={aid}, cid={cid}) error: {error}");
                    break;
                }
            };

            if let Err(e) = session.online_update(Some("VIEW_ARTICLE")).await {
                tracing::error!("user_online_update(VIEW_ARTICLE) error: {e}");
            }

            let ret = self.display_cache(term, session, &cache).await;

            if let Err(e) = session.view_log.mark_viewed(&session.state.db, aid).await {
                tracing::error!("article_view_log_set_viewed(aid={aid}) error: {e}");
            }

            match ret {
                KEY_UP => {
                    if self.selected <= 0 {
                        if self.page_id > 0 {
                            self.page_id -= 1;
                            self.selected = ARTICLE_LIMIT_PER_PAGE - 1;
                            self.query(session)?;
                            if self.count == ARTICLE_LIMIT_PER_PAGE {
                                again = true;
                            } else {
                                // The page is not full: land on its last row and read it there.
                                self.selected = (self.count - 1).max(0);
                            }
                        }
                    } else {
                        self.selected -= 1;
                        again = true;
                    }
                }
                KEY_DOWN => {
                    if self.selected + 1 >= self.count {
                        if self.page_id + 1 < self.page_count {
                            self.page_id += 1;
                            self.selected = 0;
                            self.query(session)?;
                            if self.count > 0 {
                                again = true;
                            }
                        }
                    } else {
                        self.selected += 1;
                        again = true;
                    }
                }
                KEY_PGUP | KEY_PGDN => {
                    let direction = if ret == KEY_PGUP { -1 } else { 1 };
                    if let Some(located) = self.locate(session, cur, direction, 1)? {
                        self.page_id = located.page_id;
                        self.selected = located.offset;
                        self.query(session)?;
                        again = true;
                    }
                }
                KEY_R => {
                    // The arena row is copied out before the flow starts: a board guard cannot
                    // be held across its `.await`s.
                    let quoted = {
                        let board = session.state.board();
                        board.article(cur).map(post::Quoted::from)
                    };

                    if let Err(e) = session.online_update(Some("REPLY_ARTICLE")).await {
                        tracing::error!("user_online_update(REPLY_ARTICLE) error: {e}");
                    }

                    match quoted {
                        Some(target) => {
                            if let Err(e) = post::article_reply(
                                term,
                                session,
                                self.sid,
                                &self.sname,
                                &self.stitle,
                                &target,
                            )
                            .await
                            {
                                tracing::error!("article_reply(aid={aid}) error: {e}");
                            }
                        }
                        None => {
                            tracing::error!(
                                "article_reply(aid={aid}) error: the cursor is not on an article"
                            );
                        }
                    }
                    again = true;
                }
                KEY_EQUAL | KEY_BACKSLASH => {
                    let direction = if ret == KEY_EQUAL { -1 } else { 1 };
                    if let Some(located) = self.locate(session, cur, direction, ARTICLE_LIMIT_PER_SECTION)? {
                        // C only re-queries when the page moved, and always keeps reading.
                        self.jump(session, Some(located)).await?;
                        again = true;
                    }
                }
                _ => {}
            }

            if !again {
                // Update current topic, which is what colours the thread in the list.
                let board = session.state.board();
                if let Some(article) = self.current().and_then(|art| board.article(art)) {
                    self.topic_tid = if article.tid == 0 { article.aid } else { article.tid };
                }
                break;
            }
        }

        Ok(())
    }

    /// The pager itself: `display_data(cache.p_data, …, display_article_key_handler,
    /// DATA_READ_HELP)` with `eof_exit == 0`, which is C's "the keys, forever".
    async fn display_cache(
        &mut self,
        term: &mut Terminal,
        session: &mut Session,
        cache: &Cached,
    ) -> u32 {
        let lines = cache.wrapped(term.fixed_width());
        let mut hooks = ReaderHooks {
            session,
            topic_mode: &mut self.topic_mode,
        };
        screen::display_data(
            term,
            &cache.data,
            &lines,
            0,
            &mut hooks,
            DATA_READ_HELP,
            MAX_USER_IDLE_TIME,
        )
        .await
    }
}

/// The username format `section_list_display.c:1216-1230` accepts: a letter first, then
/// letters, digits and underscores, at most `BBS_username_max_len` of them.
pub fn valid_username(username: &str) -> bool {
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > USERNAME_MAX_LEN {
        return false;
    }
    bytes.iter().enumerate().all(|(i, &ch)| {
        ch.is_ascii_alphabetic() || (i > 0 && (ch.is_ascii_digit() || ch == b'_'))
    })
}

/// `PagerHooks` for a post being read: the session does the bookkeeping, the listing owns the
/// topic mode that `p` flips.
struct ReaderHooks<'a> {
    session: &'a mut Session,
    topic_mode: &'a mut bool,
}

impl PagerHooks for ReaderHooks<'_> {
    async fn on_activity(&mut self) {
        Session::record_activity(self.session).await;
    }

    async fn key_handler(&mut self, ch: &mut u32, ctx: &mut DisplayCtx) -> i32 {
        article_key_handler(ch, ctx, self.topic_mode)
    }

    fn help_enabled(&mut self) -> bool {
        PagerHooks::help_enabled(self.session)
    }

    fn set_help_enabled(&mut self, enabled: bool) {
        PagerHooks::set_help_enabled(self.session, enabled);
    }

    async fn show_help(&mut self, term: &mut Terminal, help_filename: &str) -> u32 {
        self.session.display_file(term, help_filename, 1).await
    }
}

/// `display_article_key_handler` (`:503`). The `p` and `Ctrl-X` cases fall through to the one
/// that only writes the status bar, which is why switching modes does not leave the pager.
pub fn article_key_handler(ch: &mut u32, ctx: &mut DisplayCtx, topic_mode: &mut bool) -> i32 {
    /// The `case 0:` of the C switch: write the status bar for the current mode and swallow
    /// the key.
    fn set_msg(ch: &mut u32, ctx: &mut DisplayCtx, topic_mode: bool) {
        ctx.msg = if topic_mode {
            MSG_TOPIC.to_string()
        } else {
            MSG_NORMAL.to_string()
        };
        *ch = 0;
    }

    match *ch {
        KEY_LOWER_P | KEY_CTRL_X => {
            *topic_mode = !*topic_mode;
            set_msg(ch, ctx, *topic_mode);
            0
        }
        0 => {
            set_msg(ch, ctx, *topic_mode);
            0
        }
        // Reply, and the two ends of a thread: stop the pager with the key.
        KEY_R | KEY_EQUAL | KEY_BACKSLASH => 1,
        KEY_UP | KEY_PGUP | KEY_HOME => {
            if ctx.reach_begin {
                *ch = if *topic_mode { KEY_PGUP } else { KEY_UP };
                return 1;
            }
            0
        }
        KEY_LOWER_K => {
            *ch = if *topic_mode { KEY_PGUP } else { KEY_UP };
            1
        }
        KEY_DOWN | KEY_PGDN | KEY_END => {
            if ctx.reach_end {
                *ch = if *topic_mode { KEY_PGDN } else { KEY_DOWN };
                return 1;
            }
            0
        }
        KEY_LOWER_J => {
            *ch = if *topic_mode { KEY_PGDN } else { KEY_DOWN };
            1
        }
        _ => 0,
    }
}

/// `list_section` (`menu_proc.c:40`): the menu's `LIST_SECTION` item, whose parameter is the
/// board name and which always asks for a redraw afterwards.
pub async fn list_section(term: &mut Terminal, session: &mut Session, sname: &str) {
    if let Err(e) = Listing::run(term, session, sname, 0).await {
        tracing::error!("section_list_display({sname}) error: {e:?}");
    }
}

/// `section_list_ex_dir_display` (`section_list_display.c:1380-1471`): the digest tree of one
/// board, walked by this session until it leaves it. `false` is one of the C negative returns —
/// the board gone from the pool, or a menu the loader could not read — which every caller logs
/// before going on as if the visit had succeeded.
pub async fn list_ex_dir(term: &mut Terminal, session: &mut Session, sid: i16) -> bool {
    let (ex_menu_tm, ex_menu_set) = {
        let board = session.state.board();
        let Some(section) = board.section(sid) else {
            tracing::error!("NULL pointer error");
            return false;
        };
        (section.ex_menu_tm, section.ex_menu_set.clone())
    };

    if ex_menu_tm == 0 {
        term.moveto(2, 1);
        term.clrtoeol();
        term.print("该版块精华区未开放");
        let _ = term.flush().await;
        screen::press_any_key(term).await;
        return true;
    }

    // `get_section_ex_menu_set` (`:1401-1405`) is the copy out of the pool that the board lock
    // already gave us; `get_menu_shm_readonly` (`:1406-1410`) is attaching the tree, and the one
    // way that fails here is the state a failed `load_menu` leaves — a timestamp and no menu.
    let Some(menus) = ex_menu_set else {
        tracing::error!("get_menu_shm_readonly(sid={sid}) error");
        return false;
    };

    let mut nav = MenuNav::new(&menus);
    // `section_list_loader.c:174` sets `allow_exit` on the menu set as it loads it; the port sets
    // it on the walker, which is where the flag lives.
    nav.set_allow_exit(true);

    screen::clearscr(term);
    screen::show_bottom(term, "", &session.username, session.login_tm);

    // `:1415`: `if (display_menu(&ex_menu_set) == 0)`. An empty digest tree pauses on one key
    // press and C never opens the read loop — that press is the whole visit.
    if nav.draw(term, session).await != Draw::Show {
        return true;
    }

    while !session.state.is_exiting() {
        let ch = term.read_key(SELECT_KEY_TIMEOUT).await;

        if ch != KEY_NULL && ch != KEY_TIMEOUT {
            session.record_activity().await;
        }

        match ch {
            KEY_NULL => {
                tracing::debug!("KEY_NULL");
                break;
            }
            KEY_TIMEOUT => {
                if session::now() - session.last_access_tm >= MAX_USER_IDLE_TIME as i64 {
                    tracing::debug!("User input timeout");
                    break;
                }
                continue;
            }
            _ => {}
        }

        match nav.control(term, session, ch).await {
            MenuFlow::ExitMenu => break,
            MenuFlow::Redraw => {
                screen::clearscr(term);
                screen::show_bottom(term, "", &session.username, session.login_tm);
                nav.draw(term, session).await;
            }
            // The inner switch has no `EXITBBS` case: an item that asks to leave the BBS is
            // C's `default:`, and the digest loop goes back to waiting for a key.
            MenuFlow::ExitBbs | MenuFlow::NoRedraw => {}
            MenuFlow::Run(action, param) => {
                // The handler's return code reaches the same switch as `menu_control`'s own, and
                // only `REDRAW` repaints. `nested` is `false`: the `static int show_top10` guard
                // of `menu_proc.c:280-288` belongs to the top-ten loop, not to this one.
                if crate::bbs::bbs_do_choice(term, session, action, &param, false).await
                    == MenuReturn::Redraw
                {
                    screen::clearscr(term);
                    screen::show_bottom(term, "", &session.username, session.login_tm);
                    nav.draw(term, session).await;
                }
            }
        }
        let _ = term.flush().await;
    }

    true
}

/// `list_ex_section` (`menu_proc.c:259-276`): a `LIST_EX_SECTION` item names a board, and the
/// board's digest tree opens instead of its article list. `false` is C's `-1` — a board the pool
/// does not have, which `menu_control` passes straight back and its caller treats as `NOREDRAW`;
/// every other answer, including a menu that failed to open, asks for a redraw.
pub async fn list_ex_section(term: &mut Terminal, session: &mut Session, sname: &str) -> bool {
    let Some(sid) = session
        .state
        .board()
        .section_by_name(sname)
        .map(|section| section.sid)
    else {
        tracing::error!("Section {sname} not found");
        return false;
    };

    if !list_ex_dir(term, session, sid).await {
        tracing::error!("section_list_ex_dir_display(sid={sid}) error");
    }

    true
}

/// `view_ex_article` (`menu_proc.c:218-257`): a `VIEW_EX_ARTICLE` item carries the `AID` as its
/// name, so the digest reader starts from an id rather than from a row of a list. `false` is C's
/// `NOREDRAW`, which leaves the digest menu on the screen exactly as the reader left it.
pub async fn view_ex_article(term: &mut Terminal, session: &mut Session, param: &str) -> bool {
    // `atoi(param)`: anything that is not a number is 0, and 0 is not an article.
    let aid = param.parse::<i32>().unwrap_or_default();

    let cid = {
        let board = session.state.board();
        match board.article_by_aid(aid) {
            Some(article) => article.cid,
            None => {
                tracing::error!("article_block_find_by_aid({aid}) error");
                return false;
            }
        }
    };

    let dir = session.root.join(VAR_ARTICLE_CACHE_DIR);
    let cache = match cache::load(&dir.to_string_lossy(), cid) {
        Ok(Some(cached)) => cached,
        Ok(None) => {
            tracing::error!("article_cache_load(aid={aid}, cid={cid}) error");
            return false;
        }
        Err(error) => {
            tracing::error!("article_cache_load(aid={aid}, cid={cid}) error: {error}");
            return false;
        }
    };

    if let Err(e) = session.online_update(Some("VIEW_ARTICLE")).await {
        tracing::error!("user_online_update(VIEW_ARTICLE) error: {e}");
    }

    let lines = cache.wrapped(term.fixed_width());
    let mut hooks = ExArticleHooks { session };
    screen::display_data(
        term,
        &cache.data,
        &lines,
        0,
        &mut hooks,
        DATA_READ_HELP,
        MAX_USER_IDLE_TIME,
    )
    .await;

    if let Err(e) = session.view_log.mark_viewed(&session.state.db, aid).await {
        tracing::error!("article_view_log_set_viewed(aid={aid}) error: {e}");
    }

    true
}

/// `PagerHooks` for a post read out of a digest menu: the session does the bookkeeping and the
/// status bar is the one `display_ex_article_key_handler` writes.
struct ExArticleHooks<'a> {
    session: &'a mut Session,
}

impl PagerHooks for ExArticleHooks<'_> {
    async fn on_activity(&mut self) {
        Session::record_activity(self.session).await;
    }

    async fn key_handler(&mut self, ch: &mut u32, ctx: &mut DisplayCtx) -> i32 {
        ex_article_key_handler(ch, ctx)
    }

    fn help_enabled(&mut self) -> bool {
        PagerHooks::help_enabled(self.session)
    }

    fn set_help_enabled(&mut self, enabled: bool) {
        PagerHooks::set_help_enabled(self.session, enabled);
    }

    async fn show_help(&mut self, term: &mut Terminal, help_filename: &str) -> u32 {
        self.session.display_file(term, help_filename, 1).await
    }
}

/// `display_ex_article_key_handler` (`menu_proc.c:203-216`): only the `case 0` of the C switch,
/// which writes the status bar. Every key answers 0 and no key is rewritten, so the digest
/// reader has no thread switch, no reply and no end-of-file exit; it leaves with the Esc and
/// Left that [`screen::display_data`] handles itself, or with `h` into the help file.
pub fn ex_article_key_handler(ch: &mut u32, ctx: &mut DisplayCtx) -> i32 {
    if *ch == 0 {
        ctx.msg = MSG_EX.to_string();
    }
    0
}

/// `locate_article` (`menu_proc.c:350`): a `LOCATE_ARTICLE` item's `sname|aid` parameter,
/// which is how the top-ten menu opens a board on one post. `false` is C's `NOREDRAW` for a
/// parameter that does not split.
pub async fn locate_article_menu(term: &mut Terminal, session: &mut Session, param: &str) -> bool {
    let Some((sname, aid)) = param.split_once('|') else {
        tracing::error!("top10_locate({param}) error: invalid parameter");
        return false;
    };
    let aid = aid.parse::<i32>().unwrap_or(0);

    if let Err(e) = Listing::run(term, session, sname, aid).await {
        tracing::error!("section_list_display({sname}, {aid}) error: {e:?}");
    }
    true
}

#[cfg(test)]
mod tests {
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
        assert!(board.create_section(1, "test", "测试板", "master").is_some());
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
        assert_eq!(page.ontop_start_offset, 20, "no room for a sticky on a full page");

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

        let located = locate_article(&board, section, board.article_index(123).unwrap(), 0, 0)
            .expect("located");
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
        let located = locate_article(
            &board,
            section,
            art(100),
            1,
            ARTICLE_LIMIT_PER_SECTION,
        )
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
            scan_by_username(&board, art(100), 1, "SOMEONE")
                .map(|a| board.article(a).unwrap().aid),
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
            scan_by_title(&board, art(100), 1, "examplebbs")
                .map(|a| board.article(a).unwrap().aid),
            Some(101)
        );
        // Backward starts at `prior`, so the row the reader is on never matches itself …
        assert_eq!(scan_by_title(&board, art(101), -1, "examplebbs"), None);
        // … and the walk reaches it from its neighbour.
        assert_eq!(
            scan_by_title(&board, art(102), -1, "examplebbs")
                .map(|a| board.article(a).unwrap().aid),
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
        assert!(row_line(&reply(12346, 12345), true, &env)
            .contains("\x1b[1;36mtitle 12346\x1b[m"));
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
        let path = std::env::temp_dir().join(format!(
            "lbbs-aid-loc-{}-{}",
            std::process::id(),
            NOW
        ));

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
        let path = std::env::temp_dir().join(format!(
            "lbbs-aid-loc-cut-{}-{}",
            std::process::id(),
            NOW
        ));

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
    async fn guest_session(
        state: &std::sync::Arc<crate::state::AppState>,
        root: &Path,
    ) -> Session {
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
    fn first_readable_board<'a>(
        board: &'a Board,
        privs: &crate::privilege::UserPriv,
    ) -> &'a Section {
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

        assert!(drawn.contains(&format!("讨论区 [{sname}]")), "the board header");
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
        assert!(drawn.contains("\x1b[2J"), "the screen is cleared first: {drawn}");
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
            "枫林在线",
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

    /// `menu_proc.c:262-266`: the parameter is a board name, and a name the pool does not have is
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

        let answered =
            crate::bbs::bbs_do_choice(&mut term, &mut session, Action::ListExSection, "nope", false)
                .await;
        assert_eq!(answered, MenuReturn::NoRedraw, "Section nope not found");
        let drawn = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(!drawn.contains("该版块精华区未开放"), "{drawn}");

        let answered =
            crate::bbs::bbs_do_choice(&mut term, &mut session, Action::ListExSection, "test", false)
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
            session.state.board_mut().append_article(&article).expect("append");
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
}
