/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The board in memory: `src/section_list.c`.
//!
//! The C daemon keeps three shared-memory regions for this — an append-only arena of
//! [`Article`]s carved into 1000-slot blocks, an array of 200 [`Section`]s and a pair of
//! semaphores per section. Here they are one value behind a single `RwLock` (see
//! [`crate::state`]): the port is one process, so the per-section reader/writer locks and
//! their 15-second deadlock guard have no work left to do. What is kept is what the locks
//! protected, namely the list invariants:
//!
//! * [`Board::articles`] only ever grows, and `aid`s are strictly ascending in it. That is
//!   what turns `article_block_find_by_aid`'s two-level binary search into
//!   [`Board::article_index`] and — more importantly — makes an index into the vector a
//!   stable replacement for the C `ARTICLE *` ([`Artp`]).
//! * Each section's articles form a circular doubly-linked list ordered by ascending `aid`,
//!   so `head` is the oldest and `tail` the newest article of the board.
//! * A topic is a second circular list through the same articles, threaded from its head
//!   (`tid == 0`) so that walking `topic_next` returns to the head.

use std::collections::HashMap;

use crate::paths::{
    ARTICLE_LIMIT_PER_PAGE, ARTICLE_LIMIT_PER_SECTION, ARTICLE_TITLE_MAX_LEN, MASTER_LIST_BYTES,
    MAX_SECTION, NICKNAME_MAX_LEN, ONTOP_ARTICLE_LIMIT, PAGE_INDEX_CAPACITY, SECTION_NAME_MAX_LEN,
    SECTION_TITLE_MAX_LEN, USERNAME_MAX_LEN,
};
use crate::roster::{Roster, UserStatMap};

/// The C `ARTICLE *`: a position in [`Board::articles`].
pub type Artp = usize;

/// `CALCULATE_PAGE_THRESHOLD`: how many articles [`Board::move_topic`] relocates before it
/// re-indexes the destination pages.
const CALCULATE_PAGE_THRESHOLD: i32 = 100;

/// The negative return codes of the `section_list_*` functions. The C callers log and move
/// on, so the port keeps them coarse and lets [`std::fmt::Debug`] describe them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardError {
    /// `section sid %d != article sid %d`.
    SidMismatch,
    /// `article_count reach limit in section %d` (`ARTICLE_LIMIT_PER_SECTION`).
    SectionFull,
    /// `aid <= last_aid`: the arena only accepts a newer article.
    AidNotAscending,
    /// No such `aid` / `sid`.
    NotFound,
    /// `article is not head of topic`, which is what a move works on.
    NotTopicHead,
}

/// `ARTICLE` (`include/section_list.h:32`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub aid: i32,
    pub tid: i32,
    pub sid: i16,
    pub cid: i32,
    pub uid: i32,
    /// `p_prior` / `p_next`: the section list.
    pub prior: Artp,
    pub next: Artp,
    /// `p_topic_prior` / `p_topic_next`: the same topic.
    pub topic_prior: Artp,
    pub topic_next: Artp,
    pub visible: bool,
    pub excerption: bool,
    pub ontop: bool,
    pub lock: bool,
    pub transship: bool,
    pub username: String,
    pub nickname: String,
    pub title: String,
    pub sub_dt: i64,
}

impl Default for Article {
    /// What the loader starts from (`section_list_loader.c:250` memsets the whole struct).
    /// The four links are placeholders: [`Section::append_article`] overwrites all of them
    /// before the article can be reached, so an unlinked article's links are never read.
    fn default() -> Self {
        Self {
            aid: 0,
            tid: 0,
            sid: 0,
            cid: 0,
            uid: 0,
            prior: 0,
            next: 0,
            topic_prior: 0,
            topic_next: 0,
            visible: false,
            excerption: false,
            ontop: false,
            lock: false,
            transship: false,
            username: String::new(),
            nickname: String::new(),
            title: String::new(),
            sub_dt: 0,
        }
    }
}

impl Article {
    /// The `tid == 0` test that the C code spells out everywhere: a topic is identified by
    /// its head article, and the head is its own topic.
    pub fn is_topic_head(&self) -> bool {
        self.tid == 0
    }

    /// `strncpy` into the pool's `username` / `nickname` / `title` buffers
    /// (`section_list_loader.c:266-271`), which is where the 12 / 40 / 160-byte limits bite.
    /// The C original feeds `strncpy` whatever the server returned, so a `NULL` column would
    /// crash it; the port reads an absent value as empty.
    pub fn set_labels(
        &mut self,
        username: Option<&str>,
        nickname: Option<&str>,
        title: Option<&str>,
    ) {
        self.username = trunc_bytes(username.unwrap_or(""), USERNAME_MAX_LEN);
        self.nickname = trunc_bytes(nickname.unwrap_or(""), NICKNAME_MAX_LEN);
        self.title = trunc_bytes(title.unwrap_or(""), ARTICLE_TITLE_MAX_LEN);
    }
}

/// `strncpy(dst, src, sizeof(dst) - 1)` into one of the pool's fixed buffers. The budget is
/// in bytes; a UTF-8 sequence that does not fit whole is dropped instead of split, which
/// only ever differs from the C original for a name longer than the buffer.
pub fn trunc_bytes(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_owned();
    }
    let mut end = cap;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// `SECTION_LIST` (`include/section_list.h:58`).
#[derive(Debug, Clone)]
pub struct Section {
    pub sid: i16,
    pub class_id: i32,
    pub enable: bool,
    pub sname: String,
    pub stitle: String,
    pub master_list: String,
    pub read_user_level: i32,
    pub write_user_level: i32,
    pub article_count: i32,
    pub topic_count: i32,
    pub visible_article_count: i32,
    pub visible_topic_count: i32,
    /// `p_article_head` / `p_article_tail`: oldest and newest article of the board.
    pub head: Option<Artp>,
    pub tail: Option<Artp>,
    /// `p_page_first_article`, the first article of every page of visible articles. Its
    /// length is the C `page_count`.
    pub page_first: Vec<Artp>,
    pub last_page_visible_article_count: i32,
    /// `p_ontop_articles`, ascending by `aid`.
    pub ontop: Vec<Artp>,
    /// `MENU_SET ex_menu_set` (`section_list.h:78`): the board's digest menu, the parsed
    /// `var/gen_ex/<sid>` that `section_list_loader.c:162-179` swaps in. C re-attached its shared
    /// memory per reader (`section_list_display.c:1401-1410`); the port shares the parsed tree
    /// behind an `Arc`. `None` with a non-zero [`Section::ex_menu_tm`] is a failed `load_menu`.
    pub ex_menu_set: Option<std::sync::Arc<crate::menu::MenuSet>>,
    pub ex_menu_tm: i64,
}

impl Section {
    /// `section_list_create` (`:560`). `class_id`, `enable` and the two levels are not
    /// arguments there — the loader assigns them from its own queries afterwards.
    pub fn new(sid: i16, sname: &str, stitle: &str, master_list: &str) -> Self {
        Self {
            sid,
            class_id: 0,
            enable: true,
            sname: trunc_bytes(sname, SECTION_NAME_MAX_LEN),
            stitle: trunc_bytes(stitle, SECTION_TITLE_MAX_LEN),
            master_list: trunc_bytes(master_list, MASTER_LIST_BYTES),
            read_user_level: 0,
            write_user_level: 0,
            article_count: 0,
            topic_count: 0,
            visible_article_count: 0,
            visible_topic_count: 0,
            head: None,
            tail: None,
            page_first: Vec::new(),
            last_page_visible_article_count: 0,
            ontop: Vec::new(),
            ex_menu_set: None,
            ex_menu_tm: 0,
        }
    }

    /// `p_section->page_count`.
    pub fn page_count(&self) -> i32 {
        self.page_first.len() as i32
    }

    /// `p_section->ontop_article_count`.
    pub fn ontop_article_count(&self) -> i32 {
        self.ontop.len() as i32
    }

    /// `section_list_update` (`:620`): the config fields may change under a loaded board,
    /// and only `sname` feeds a lookup index (the `by_sid` dictionary is not re-set).
    pub fn update(&mut self, sname: &str, stitle: &str, master_list: &str) {
        self.sname = trunc_bytes(sname, SECTION_NAME_MAX_LEN);
        self.stitle = trunc_bytes(stitle, SECTION_TITLE_MAX_LEN);
        self.master_list = trunc_bytes(master_list, MASTER_LIST_BYTES);
    }

    /// `section_list_reset_articles` (`:653`).
    pub fn reset_articles(&mut self) {
        self.article_count = 0;
        self.topic_count = 0;
        self.visible_article_count = 0;
        self.visible_topic_count = 0;
        self.head = None;
        self.tail = None;
        self.page_first.clear();
        self.last_page_visible_article_count = 0;
        self.ontop.clear();
    }

    /// `section_list_append_article` (`:810`). `articles` is the arena the new article is
    /// copied to the end of.
    pub fn append_article(
        &mut self,
        articles: &mut Vec<Article>,
        src: &Article,
    ) -> Result<Artp, BoardError> {
        if self.sid != src.sid {
            return Err(BoardError::SidMismatch);
        }
        if self.article_count >= ARTICLE_LIMIT_PER_SECTION {
            return Err(BoardError::SectionFull);
        }
        // AID of articles should be strictly ascending.
        if articles.last().is_some_and(|last| src.aid <= last.aid) {
            return Err(BoardError::AidNotAscending);
        }

        let idx = articles.len();
        articles.push(src.clone());
        self.article_count += 1;
        if articles[idx].visible {
            self.visible_article_count += 1;
        }

        // Link the appended article as the tail node of its topic.
        let (topic_head, topic_tail);
        if articles[idx].tid != 0 {
            match arena_index(articles, articles[idx].tid) {
                Some(head) => {
                    topic_head = head;
                    topic_tail = articles[head].topic_prior;
                }
                None => {
                    // Reset tid to 0 to avoid linking to a non-existing topic, invisible so
                    // it stays out of the topic list. `visible_article_count` was already
                    // raised above and — as in the C original, which reaches this branch the
                    // same way — is not corrected.
                    articles[idx].tid = 0;
                    articles[idx].visible = false;
                    self.topic_count += 1;
                    topic_head = idx;
                    topic_tail = idx;
                }
            }
        } else {
            self.topic_count += 1;
            if articles[idx].visible {
                self.visible_topic_count += 1;
            }
            topic_head = idx;
            topic_tail = idx;
        }
        articles[idx].topic_prior = topic_tail;
        articles[idx].topic_next = topic_head;
        articles[topic_head].topic_prior = idx;
        articles[topic_tail].topic_next = idx;

        // Link the appended article as the tail node of the section list.
        if self.head.is_none() {
            self.head = Some(idx);
            self.tail = Some(idx);
        }
        let (head, tail) = (
            self.head.expect("head was just set"),
            self.tail.expect("tail was just set"),
        );
        articles[idx].prior = tail;
        articles[idx].next = head;
        articles[head].prior = idx;
        articles[tail].next = idx;
        self.tail = Some(idx);

        // Update the page index.
        let visible = articles[idx].visible;
        if (visible && self.last_page_visible_article_count == ARTICLE_LIMIT_PER_PAGE)
            || self.page_count() == 0
        {
            // The C original writes past `p_page_first_article[2500]` once a board holds
            // more than 50000 articles; the port stops extending the index instead, which
            // leaves the articles reachable through the list but off the page jumps.
            if (self.page_count() as usize) < PAGE_INDEX_CAPACITY {
                self.page_first.push(idx);
            }
            self.last_page_visible_article_count = 0;
        }
        if visible {
            self.last_page_visible_article_count += 1;
        }

        if articles[idx].ontop {
            self.update_article_ontop(articles, idx)?;
        }

        Ok(idx)
    }

    /// `section_list_set_article_visible` (`:964`), returning the C `affected_count`.
    ///
    /// Hiding a topic head cascades to its visible replies; showing one does not bring them
    /// back, and neither direction re-indexes the pages — the caller decides whether to run
    /// [`Section::calculate_page`] (the C loader does not).
    pub fn set_article_visible(
        &mut self,
        articles: &mut Vec<Article>,
        aid: i32,
        visible: bool,
        stats: &mut UserStatMap,
    ) -> Result<i32, BoardError> {
        let idx = arena_index(articles, aid).ok_or(BoardError::NotFound)?;
        if self.sid != articles[idx].sid {
            return Err(BoardError::SidMismatch);
        }
        if articles[idx].visible == visible {
            return Ok(0);
        }

        let mut affected = 0;
        if !visible {
            self.visible_article_count -= 1;
            stats.article_cnt_inc(articles[idx].uid, -1);

            if articles[idx].tid == 0 {
                self.visible_topic_count -= 1;

                // Hide the still-visible replies of the topic.
                let mut reply = articles[idx].topic_next;
                while !articles[reply].is_topic_head() {
                    if articles[reply].tid != aid {
                        // `Inconsistent tid found in reply of topic` — the C original skips
                        // this reply and keeps walking.
                        reply = articles[reply].topic_next;
                        continue;
                    }
                    if articles[reply].visible {
                        articles[reply].visible = false;
                        self.visible_article_count -= 1;
                        affected += 1;
                        stats.article_cnt_inc(articles[reply].uid, -1);
                    }
                    reply = articles[reply].topic_next;
                }
            }
        } else {
            self.visible_article_count += 1;
            if articles[idx].tid == 0 {
                self.visible_topic_count += 1;
            }
            stats.article_cnt_inc(articles[idx].uid, 1);
        }

        articles[idx].visible = visible;
        affected += 1;

        Ok(affected)
    }

    /// `section_list_set_article_excerption` (`:1050`): 1 if the flag changed, 0 if it was
    /// already set.
    pub fn set_article_excerption(
        &mut self,
        articles: &mut [Article],
        aid: i32,
        excerption: bool,
    ) -> Result<i32, BoardError> {
        let idx = arena_index(articles, aid).ok_or(BoardError::NotFound)?;
        if self.sid != articles[idx].sid {
            return Err(BoardError::SidMismatch);
        }
        if articles[idx].excerption == excerption {
            return Ok(0);
        }
        articles[idx].excerption = excerption;
        Ok(1)
    }

    /// `section_list_update_article_ontop` (`:1100`): keeps [`Section::ontop`] ascending by
    /// `aid` and at [`ONTOP_ARTICLE_LIMIT`] entries long, evicting the oldest aid when it is
    /// full. Called for the flag the article already carries, which the C op handler assigns
    /// to the arena first.
    pub fn update_article_ontop(
        &mut self,
        articles: &[Article],
        art: Artp,
    ) -> Result<(), BoardError> {
        if self.sid != articles[art].sid {
            return Err(BoardError::SidMismatch);
        }
        let aid = articles[art].aid;

        if articles[art].ontop {
            let mut i = 0;
            while i < self.ontop.len() {
                let other = articles[self.ontop[i]].aid;
                if other == aid {
                    return Ok(()); // already listed; the C original logs the inconsistency
                } else if other > aid {
                    break;
                }
                i += 1;
            }

            if self.ontop.len() >= ONTOP_ARTICLE_LIMIT {
                if i == 0 {
                    // The new article is older than everything already stuck on the board.
                    return Ok(());
                }
                self.ontop.remove(0);
                self.ontop.insert(i - 1, art);
            } else {
                self.ontop.insert(i, art);
            }
        } else if let Some(i) = self.ontop.iter().position(|&x| articles[x].aid == aid) {
            self.ontop.remove(i);
        }

        Ok(())
    }

    /// The aids of [`Section::ontop`], oldest first.
    pub fn sticky_aids(&self, articles: &[Article]) -> Vec<i32> {
        self.ontop.iter().map(|&a| articles[a].aid).collect()
    }

    /// The aids [`Section::page_first`] indexes.
    pub fn page_aids(&self, articles: &[Article]) -> Vec<i32> {
        self.page_first.iter().map(|&a| articles[a].aid).collect()
    }

    /// `section_list_page_count_with_ontop` (`:1178`): the page count the browse screens
    /// show, which folds the sticky articles into the last page.
    pub fn page_count_with_ontop(&self) -> i32 {
        let count = self.page_count() - 1
            + (self.last_page_visible_article_count
                + self.ontop_article_count()
                + ARTICLE_LIMIT_PER_PAGE
                - 1)
                / ARTICLE_LIMIT_PER_PAGE;
        count.max(0)
    }

    /// `section_list_page_article_count_with_ontop` (`:1202`).
    pub fn page_article_count_with_ontop(&self, page_id: i32) -> i32 {
        if page_id < self.page_count() - 1 {
            ARTICLE_LIMIT_PER_PAGE
        } else {
            (self.last_page_visible_article_count + self.ontop_article_count()
                - ARTICLE_LIMIT_PER_PAGE * (page_id - self.page_count() + 1))
                .clamp(0, ARTICLE_LIMIT_PER_PAGE)
        }
    }

    /// `section_list_find_article_with_offset` (`:1231`).
    ///
    /// [`ArticleAt::article`] is `None` when the board does not hold `aid`, in which case
    /// [`ArticleAt::next`] is where it belongs — the property `move_topic` inserts by.
    pub fn find_article_with_offset(&self, articles: &[Article], aid: i32) -> Option<ArticleAt> {
        let page = match self.find_page(articles, aid) {
            Some(page) => page,
            None => return Some(ArticleAt::default()),
        };
        let mut art = self.page_first[page];
        let mut offset = 0i32;
        // page_first[page]->aid <= aid < page_first[page + 1]->aid
        let right = if page == self.page_first.len().saturating_sub(1) {
            i32::MAX
        } else {
            articles[self.page_first[page + 1]].aid
        };

        while aid > articles[art].aid {
            art = articles[art].next;
            offset += 1;

            if aid == articles[art].aid {
                return Some(ArticleAt {
                    page: page as i32,
                    offset,
                    article: Some(art),
                    next: Some(articles[art].next),
                });
            }

            // Ran past the last article of the page.
            if Some(art) == self.head || articles[art].aid >= right {
                let next = if Some(art) == self.head {
                    self.head
                } else {
                    Some(self.page_first[page + 1])
                };
                return Some(ArticleAt {
                    page: page as i32,
                    offset,
                    article: None,
                    next,
                });
            }
        }

        if aid < articles[art].aid {
            Some(ArticleAt {
                page: page as i32,
                offset,
                article: None,
                next: Some(art),
            })
        } else {
            Some(ArticleAt {
                page: page as i32,
                offset,
                article: Some(art),
                next: Some(articles[art].next),
            })
        }
    }

    /// The page whose first article is the last one not newer than `aid`, which is the
    /// binary search `section_list_find_article_with_offset` opens with. `None` means the
    /// section is empty, where the C function reports page 0, offset 0 and no next article.
    fn find_page(&self, articles: &[Article], aid: i32) -> Option<usize> {
        if self.article_count == 0 || self.page_first.is_empty() {
            return None;
        }
        let mut left = 0usize;
        let mut right = self.page_first.len() - 1;
        while left < right {
            // The page id above the midpoint of the two candidate page ids.
            let mid = left + (right - left + 1) / 2;
            if aid < articles[self.page_first[mid]].aid {
                right = mid - 1;
            } else {
                left = mid;
            }
        }
        Some(left)
    }

    /// `section_list_calculate_page` (`:1310`): rebuild the page index from `start_aid`
    /// (or from the head of the section when it is not positive).
    pub fn calculate_page(
        &mut self,
        articles: &[Article],
        start_aid: i32,
    ) -> Result<(), BoardError> {
        let Some(head) = self.head else {
            self.page_first.clear();
            self.last_page_visible_article_count = 0;
            return Ok(());
        };

        let mut art = head;
        let mut page = 0usize;

        if start_aid > 0 {
            let start = arena_index(articles, start_aid).ok_or(BoardError::NotFound)?;
            if self.sid != articles[start].sid {
                return Err(BoardError::SidMismatch);
            }
            let at = self
                .find_article_with_offset(articles, start_aid)
                .ok_or(BoardError::NotFound)?;
            let found = at.article.ok_or(BoardError::NotFound)?;
            page = at.page as usize;
            // An article in the middle of a page restarts the re-index at that page's head,
            // because the pages before it are untouched and stay correct.
            art = if at.offset > 0 {
                self.page_first[page]
            } else {
                found
            };
        }

        let mut visible = 0i32;
        let mut page_head_set = false;

        loop {
            if !page_head_set && visible == 0 {
                if page < self.page_first.len() {
                    self.page_first[page] = art;
                } else {
                    self.page_first.push(art);
                }
                page_head_set = true;
            }

            if articles[art].visible {
                visible += 1;
            }

            art = articles[art].next;

            // Skip the remaining invisible articles.
            while !articles[art].visible && Some(art) != self.head {
                art = articles[art].next;
            }

            if visible >= ARTICLE_LIMIT_PER_PAGE && Some(art) != self.head {
                page += 1;
                visible = 0;
                page_head_set = false;

                if page >= PAGE_INDEX_CAPACITY {
                    // `Count of page exceed limit in section %d`, and the rest of the board
                    // stays out of the index.
                    break;
                }
            }

            if Some(art) == self.head {
                break;
            }
        }

        // The last page only counts if it holds something visible; a run of invisible
        // articles at the tail is folded into the page before it.
        let page_count = page + usize::from(visible > 0);
        self.page_first.truncate(page_count);
        self.last_page_visible_article_count = if visible > 0 {
            visible
        } else if page > 0 {
            ARTICLE_LIMIT_PER_PAGE
        } else {
            0
        };

        Ok(())
    }

    /// The number of articles [`Board::move_topic`] would relocate: the head plus its
    /// replies (`article_count_of_topic`, `:1160`). 0 when the `aid` is unknown.
    pub fn topic_len(articles: &[Article], aid: i32) -> i32 {
        let Some(mut art) = arena_index(articles, aid) else {
            return 0;
        };
        let mut count = 0;
        loop {
            if !articles[art].is_topic_head() && articles[art].tid != aid {
                // `article %d not linked to the topic`
                break;
            }
            count += 1;
            art = articles[art].topic_next;
            if articles[art].aid == aid {
                break;
            }
        }
        count
    }

    /// The browse order: the board's articles, newest first, as the C display code walks
    /// them from `p_article_tail` through `p_prior`.
    pub fn articles<'a>(&self, articles: &'a [Article]) -> SectionArticles<'a> {
        SectionArticles::new(articles, self.tail, self.article_count, false)
    }

    /// The page-index order: oldest first, which is how [`Section::page_first`] counts.
    pub fn articles_oldest_first<'a>(&self, articles: &'a [Article]) -> SectionArticles<'a> {
        SectionArticles::new(articles, self.head, self.article_count, true)
    }
}

/// A walk of one section's circular list, bounded by the section's `article_count` so a
/// mis-linked list stops instead of spinning.
#[derive(Debug, Clone)]
pub struct SectionArticles<'a> {
    articles: &'a [Article],
    next: Option<Artp>,
    left: usize,
    forwards: bool,
}

impl<'a> SectionArticles<'a> {
    fn new(articles: &'a [Article], start: Option<Artp>, count: i32, forwards: bool) -> Self {
        Self {
            articles,
            next: start,
            left: usize::try_from(count).unwrap_or(0).min(articles.len()),
            forwards,
        }
    }
}

impl<'a> Iterator for SectionArticles<'a> {
    type Item = &'a Article;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let art = self.next?;
        self.left -= 1;
        self.next = Some(if self.forwards {
            self.articles[art].next
        } else {
            self.articles[art].prior
        });
        self.articles.get(art)
    }
}

/// The out-parameters of [`Section::find_article_with_offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleAt {
    pub page: i32,
    pub offset: i32,
    pub article: Option<Artp>,
    /// `pp_next`: the article after the one asked for, or the one it would precede.
    pub next: Option<Artp>,
}

impl Default for ArticleAt {
    /// The state `section_list_find_article_with_offset` writes before it looks at the
    /// section — which is also its answer for an empty section, page 0 offset 0.
    fn default() -> Self {
        Self {
            page: 0,
            offset: 0,
            article: None,
            next: None,
        }
    }
}

/// `p_section_list_pool`: the arena plus the boards, the two dictionaries the C code keeps
/// beside them (`p_trie_dict_section_by_name`, `p_trie_dict_section_by_sid`) and the
/// per-user article counters.
///
/// The counters are `p_user_list_pool->user_stat_map` in the C daemon, reached from
/// `section_list_set_article_visible` without taking the user-list lock — a race the original
/// lives with because the numbers are cosmetic. One `Board` makes the section edit and its
/// counter update atomic, and keeps the loader's statistics pass next to the arena it scans.
#[derive(Debug, Default)]
pub struct Board {
    /// `article_block_pool`: append-only, ascending by `aid`.
    pub articles: Vec<Article>,
    pub sections: Vec<Section>,
    pub stats: UserStatMap,
    by_name: HashMap<String, usize>,
}

/// `article_block_find_by_aid`, over a vector that only ever grows to the right.
fn arena_index(articles: &[Article], aid: i32) -> Option<Artp> {
    let pos = articles
        .binary_search_by(|article| article.aid.cmp(&aid))
        .ok()?;
    Some(pos)
}

impl Board {
    /// `get_section_info` copies a section's three strings out under a read lock; the port
    /// hands back borrowed data instead and lets [`crate::state`] hold the lock.
    pub fn section(&self, sid: i16) -> Option<&Section> {
        self.sections.iter().find(|s| s.sid == sid)
    }

    pub fn section_mut(&mut self, sid: i16) -> Option<&mut Section> {
        self.sections.iter_mut().find(|s| s.sid == sid)
    }

    /// `section_list_find_by_name` — exact bytes, like the trie it replaces.
    pub fn section_by_name(&self, sname: &str) -> Option<&Section> {
        self.by_name.get(sname).map(|&i| &self.sections[i])
    }

    /// `section_list_create`, which the loader calls for every `bbs_section` row; the C
    /// trie's uniqueness check on `sname` is the map insert.
    pub fn create_section(
        &mut self,
        sid: i16,
        sname: &str,
        stitle: &str,
        master_list: &str,
    ) -> Option<&mut Section> {
        if self.sections.len() >= MAX_SECTION {
            // `section_count reach limit %d`
            return None;
        }
        if self
            .by_name
            .insert(sname.to_owned(), self.sections.len())
            .is_some()
        {
            // `trie_dict_set` returns something other than 1 for an existing key.
            return None;
        }
        self.sections
            .push(Section::new(sid, sname, stitle, master_list));
        Some(self.sections.last_mut().expect("just pushed"))
    }

    /// `section_list_update`, reachable by name as well as by sid because that is how the
    /// loader's second pass looks a re-labelled board up.
    pub fn update_section(&mut self, sid: i16, sname: &str, stitle: &str, master_list: &str) {
        let Some(index) = self.sections.iter().position(|s| s.sid == sid) else {
            return;
        };
        self.sections[index].update(sname, stitle, master_list);
        self.by_name.insert(sname.to_owned(), index);
    }

    /// `article_block_find_by_aid`.
    pub fn article_index(&self, aid: i32) -> Option<Artp> {
        arena_index(&self.articles, aid)
    }

    pub fn article(&self, art: Artp) -> Option<&Article> {
        self.articles.get(art)
    }

    pub fn article_by_aid(&self, aid: i32) -> Option<&Article> {
        self.article_index(aid).map(|i| &self.articles[i])
    }

    /// `article_block_last_aid` — 0 while the arena is empty, which is what makes the first
    /// article of any `aid` acceptable.
    pub fn last_aid(&self) -> i32 {
        self.articles.last().map(|a| a.aid).unwrap_or(0)
    }

    /// `article_block_article_count`.
    pub fn article_count(&self) -> usize {
        self.articles.len()
    }

    /// `article_block_find_by_index`, used by the statistics pass.
    pub fn articles(&self) -> &[Article] {
        &self.articles
    }

    /// `section_list_append_article` after `section_list_find_by_sid`.
    pub fn append_article(&mut self, src: &Article) -> Result<Artp, BoardError> {
        let index = self
            .sections
            .iter()
            .position(|s| s.sid == src.sid)
            .ok_or(BoardError::NotFound)?;
        self.sections[index].append_article(&mut self.articles, src)
    }

    /// `section_list_set_article_visible`, including the `user_article_cnt_inc` calls the C
    /// function makes, and returning its `affected_count`.
    pub fn set_article_visible(
        &mut self,
        sid: i16,
        aid: i32,
        visible: bool,
    ) -> Result<i32, BoardError> {
        let index = self
            .sections
            .iter()
            .position(|s| s.sid == sid)
            .ok_or(BoardError::NotFound)?;
        self.sections[index].set_article_visible(&mut self.articles, aid, visible, &mut self.stats)
    }

    /// `user_stat_update` (`user_list.c:1619`) and the map update it drives: widen the
    /// counters with the users the roster gained and fold in the articles the previous pass
    /// did not see.
    ///
    /// The window is C's `get_user_id_list(uid_list, &user_cnt, p_map->last_uid + 1)`
    /// (`user_stat.c:47`). Re-reading the whole roster would append the older users after the
    /// newer ones, and the table has to stay in uid order because every counter lookup
    /// binary-searches it.
    pub fn update_stats(&mut self, roster: &Roster) {
        self.stats.update(roster, &self.articles);
    }

    /// `get_user_article_cnt`.
    pub fn user_article_count(&self, uid: i32) -> Option<i32> {
        self.stats.article_count(uid)
    }

    /// `section_list_set_article_excerption`.
    pub fn set_article_excerption(
        &mut self,
        sid: i16,
        aid: i32,
        excerption: bool,
    ) -> Result<i32, BoardError> {
        let index = self
            .sections
            .iter()
            .position(|s| s.sid == sid)
            .ok_or(BoardError::NotFound)?;
        self.sections[index].set_article_excerption(&mut self.articles, aid, excerption)
    }

    /// `section_list_update_article_ontop`, for the callers that set [`Article::ontop`] on
    /// the arena first — which is how the `O` operation reaches it in C.
    pub fn update_article_ontop(&mut self, sid: i16, art: Artp) -> Result<(), BoardError> {
        let index = self
            .sections
            .iter()
            .position(|s| s.sid == sid)
            .ok_or(BoardError::NotFound)?;
        self.sections[index].update_article_ontop(&self.articles, art)
    }

    /// `section_list_calculate_page`.
    pub fn calculate_page(&mut self, sid: i16, start_aid: i32) -> Result<(), BoardError> {
        let index = self
            .sections
            .iter()
            .position(|s| s.sid == sid)
            .ok_or(BoardError::NotFound)?;
        self.sections[index].calculate_page(&self.articles, start_aid)
    }

    /// `article_count_of_topic`.
    pub fn topic_len(&self, aid: i32) -> i32 {
        Section::topic_len(&self.articles, aid)
    }

    /// `section_list_move_topic`: walk the topic and re-thread every article into the
    /// destination list, re-indexing the destination pages every
    /// [`CALCULATE_PAGE_THRESHOLD`] articles and both boards at the end.
    pub fn move_topic(&mut self, src_sid: i16, dest_sid: i16, aid: i32) -> Result<i32, BoardError> {
        // `src and dest section are the same` is rejected before either is looked up, which
        // also keeps the split below from asking for one board twice.
        if src_sid == dest_sid {
            return Err(BoardError::SidMismatch);
        }
        let src = self
            .sections
            .iter()
            .position(|s| s.sid == src_sid)
            .ok_or(BoardError::NotFound)?;
        let dest = self
            .sections
            .iter()
            .position(|s| s.sid == dest_sid)
            .ok_or(BoardError::NotFound)?;
        // Two boards move at once, which needs the slice split so the borrow checker can see
        // them as distinct.
        let (src, dest) = if src < dest {
            let (head, tail) = self.sections.split_at_mut(dest);
            (&mut head[src], &mut tail[0])
        } else {
            let (head, tail) = self.sections.split_at_mut(src);
            (&mut tail[0], &mut head[dest])
        };
        move_topic_at(&mut self.articles, src, dest, aid)
    }
}

/// The body of `section_list_move_topic`. A free function so the two sections and the arena
/// can be borrowed at once, which is what the C pointers do implicitly.
fn move_topic_at(
    articles: &mut Vec<Article>,
    src: &mut Section,
    dest: &mut Section,
    aid: i32,
) -> Result<i32, BoardError> {
    if src.sid == dest.sid {
        return Err(BoardError::SidMismatch);
    }
    let start = arena_index(articles, aid).ok_or(BoardError::NotFound)?;
    if src.sid != articles[start].sid {
        return Err(BoardError::SidMismatch);
    }
    if !articles[start].is_topic_head() {
        // `article is not head of topic`
        return Err(BoardError::SidMismatch);
    }

    // The article before the run, which is where the source's page index stays valid.
    let last_unaffected_aid_src = match src.head {
        Some(head) if head == start => 0,
        _ => articles[articles[start].prior].aid,
    };

    let move_article_count = Section::topic_len(articles, aid);
    if move_article_count <= 0 {
        return Err(BoardError::NotFound);
    }
    if dest.article_count + move_article_count > ARTICLE_LIMIT_PER_SECTION {
        return Err(BoardError::SectionFull);
    }

    let dest_article_count_old = dest.article_count;
    let mut move_counter = 0i32;
    let mut first_inserted_aid_dest = aid;
    let mut art = start;

    loop {
        if src.sid != articles[art].sid {
            // `warning: src section sid != article sid` — skip this reply.
            art = articles[art].topic_next;
            if articles[art].aid == aid {
                break;
            }
            continue;
        }

        // Remove from the source section's list.
        if src.head == Some(art) {
            src.head = Some(articles[art].next);
        }
        if src.tail == Some(art) {
            src.tail = Some(articles[art].prior);
        }
        if src.head == Some(art) {
            // A single-element list: the head could not move.
            src.head = None;
            src.tail = None;
        } else {
            let (prior, next) = (articles[art].prior, articles[art].next);
            articles[prior].next = next;
            articles[next].prior = prior;
        }

        articles[art].sid = dest.sid;
        let moving = art;

        let at = match dest.find_article_with_offset(articles, articles[moving].aid) {
            Some(at) if at.article.is_some() => {
                // `warning: article already in section` — leave it where it is.
                art = articles[art].topic_next;
                if articles[art].aid == aid {
                    break;
                }
                continue;
            }
            Some(at) => at,
            None => ArticleAt::default(),
        };

        // Insert into the destination section's list.
        match at.next {
            None => {
                // An empty destination.
                dest.head = Some(moving);
                dest.tail = Some(moving);
                articles[moving].prior = moving;
                articles[moving].next = moving;
            }
            Some(next) => {
                if dest.head == Some(next) {
                    if articles[moving].aid < articles[next].aid {
                        dest.head = Some(moving);
                    } else {
                        dest.tail = Some(moving);
                    }
                }
                let prior = articles[next].prior;
                articles[moving].prior = prior;
                articles[moving].next = next;
                articles[prior].next = moving;
                articles[next].prior = moving;
            }
        }

        // Move the counters.
        src.article_count -= 1;
        dest.article_count += 1;
        if articles[moving].is_topic_head() {
            src.topic_count -= 1;
            dest.topic_count += 1;
        }
        if articles[moving].visible {
            src.visible_article_count -= 1;
            dest.visible_article_count += 1;
            if articles[moving].is_topic_head() {
                src.visible_topic_count -= 1;
                dest.visible_topic_count += 1;
            }
        }

        if dest.article_count == 1 {
            dest.page_first.clear();
            dest.page_first.push(moving);
            dest.last_page_visible_article_count = i32::from(articles[moving].visible);
        }

        art = articles[art].topic_next;
        move_counter += 1;
        if move_counter % CALCULATE_PAGE_THRESHOLD == 0 {
            // A failed re-index is only logged in the C original: the list links, which are
            // what the next iteration relies on, are already consistent.
            let _ = dest.calculate_page(articles, first_inserted_aid_dest);
            first_inserted_aid_dest = articles[art].aid;
        }
        if articles[art].aid == aid {
            break;
        }
    }

    debug_assert_eq!(
        dest.article_count - dest_article_count_old,
        move_article_count,
        "count of moved articles differs from the topic length, which the C original logs"
    );

    // Both page indexes are rebuilt from the first article that the moves left alone.
    let _ = src.calculate_page(articles, last_unaffected_aid_src);
    if move_counter % CALCULATE_PAGE_THRESHOLD != 0 {
        let _ = dest.calculate_page(articles, first_inserted_aid_dest);
    }

    Ok(move_article_count)
}

#[cfg(test)]
mod tests {
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
}
