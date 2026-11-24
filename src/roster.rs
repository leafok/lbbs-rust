/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The people on the board: `src/user_list.c` and `src/user_stat.c`.
//!
//! [`Roster`], [`OnlineList`] and [`UserStatMap`] are the three tables the C daemon keeps in
//! the `user_list` shared-memory segment. The C code loads a fresh copy into the inactive
//! half of a double buffer and then swaps two indices under a write lock, so a reader never
//! waits for a reload; [`crate::state`] keeps that property by publishing a new `Arc` instead
//! of editing the old table.
//!
//! Two identities are kept apart throughout, exactly as the C structs do:
//!
//! * `uid` is the database key (`user_list.UID`), and
//! * `id` is the position of a user in [`Roster::users`], which is ordered by `username`
//!   because that is the `ORDER BY` of the load query — so an `id` is only meaningful for as
//!   long as the roster version it came from is current. Sessions carry an `id` for this
//!   reason, and every lookup that survives a reload goes through `uid`.

use std::cmp::Ordering;

use crate::board::{Article, trunc_bytes};
use crate::db::{OnlineRow, UserRow};
use crate::paths::{
    CURRENT_ACTION_MAX_LEN, IP_ADDR_LEN, MAX_USER_COUNT, MAX_USER_ONLINE_COUNT, NICKNAME_MAX_LEN,
    SESSION_ID_LEN, USER_LIMIT_PER_PAGE, USERNAME_MAX_LEN,
};

/// `USER_ACTION_MAP` (`user_list.c:84`): the code a session writes into
/// `user_online.current_action` and the label the roster screens show for it.
pub const USER_ACTIONS: [(&str, &str); 14] = [
    ("ARTICLE_FAVOR", "浏览收藏"),
    ("BBS_NET", "站点穿梭"),
    ("CHICKEN", "电子小鸡"),
    ("EDIT_ARTICLE", "修改文章"),
    ("LOGIN", "进入大厅"),
    ("MENU", "菜单选择"),
    ("POST_ARTICLE", "撰写文章"),
    ("REPLY_ARTICLE", "回复文章"),
    ("TOP10_MENU", "十大热门"),
    ("USER_LIST", "查花名册"),
    ("USER_ONLINE", "环顾四周"),
    ("VIEW_ARTICLE", "阅读文章"),
    ("VIEW_FILE", "查看文档"),
    ("WWW", "Web浏览"),
];

/// `trie_dict_get(p_trie_action_dict, ...)`. `None` is an action the daemon does not know,
/// and [`OnlineList::load`] drops those rows the way `user_online_list_load` does.
pub fn action_title(action: &str) -> Option<&'static str> {
    USER_ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, title)| *title)
}

/// `USER_INFO` (`include/user_list.h:26`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserInfo {
    /// Position in [`Roster::users`]; the C pool's array index.
    pub id: i32,
    pub uid: i32,
    pub username: String,
    pub nickname: String,
    /// `char gender`, read as `toupper(gender) == 'M'` by the profile screens.
    pub gender: char,
    pub gender_pub: bool,
    /// Days of activity — the dead-account test of `check_user` reads it.
    pub life: i32,
    pub exp: i32,
    pub visit_count: i32,
    pub signup_dt: i64,
    pub last_login_dt: i64,
    pub last_logout_dt: i64,
    pub birthday: i64,
    /// `const char *intro`, which points into the pool's introduction buffer.
    pub intro: String,
}

impl UserInfo {
    /// One row of the roster query. `id` is assigned by [`Roster::load`], which knows the
    /// order; the buffers are the `strncpy` bounds of `user_list.c:191-207`.
    fn from_row(id: i32, row: &UserRow) -> Self {
        Self {
            id,
            uid: row.uid,
            username: trunc_bytes(&row.username, USERNAME_MAX_LEN),
            nickname: trunc_bytes(row.nickname.as_deref().unwrap_or(""), NICKNAME_MAX_LEN),
            gender: row.gender.chars().next().unwrap_or('\0'),
            gender_pub: row.gender_pub,
            life: i32::from(row.life),
            exp: row.exp.unwrap_or(0),
            visit_count: row.visit_count,
            signup_dt: row.signup_dt.unwrap_or(0),
            last_login_dt: row.last_login_dt.unwrap_or(0),
            last_logout_dt: row.last_logout_dt.unwrap_or(0),
            birthday: row.birthday.unwrap_or(0),
            intro: row.introduction.clone().unwrap_or_default(),
        }
    }
}

/// `USER_INFO_INDEX_UID`: the secondary index the `uid` lookups binary-search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UidIndex {
    uid: i32,
    id: i32,
}

/// `USER_LIST` (`include/user_list.h:52`).
#[derive(Debug, Clone, Default)]
pub struct Roster {
    pub users: Vec<UserInfo>,
    /// `index_uid`, sorted by `user_info_index_uid_comp`: by `uid`, then by `id`.
    index_uid: Vec<UidIndex>,
}

impl Roster {
    /// `user_list_load` (`:141`) over the rows its query returned.
    ///
    /// The C code only rebuilds `index_uid` when the user count or the last `uid` moved,
    /// because a sort of 100000 entries is not free in a 200-process server; rebuilding it
    /// every load costs microseconds here and cannot go stale, so the port always does.
    pub fn load(rows: &[UserRow]) -> Self {
        // `:225`: the query answered with more accounts than the pool holds, and the tail is
        // dropped on the floor.
        if rows.len() > MAX_USER_COUNT {
            tracing::error!("Too many users, exceed limit {MAX_USER_COUNT}");
        }
        let users: Vec<UserInfo> = rows
            .iter()
            .enumerate()
            .take(MAX_USER_COUNT)
            .map(|(id, row)| UserInfo::from_row(id as i32, row))
            .collect();
        let mut index_uid: Vec<UidIndex> = users
            .iter()
            .map(|u| UidIndex {
                uid: u.uid,
                id: u.id,
            })
            .collect();
        index_uid.sort_by(|a, b| a.uid.cmp(&b.uid).then(a.id.cmp(&b.id)));
        // `:243` and `:248`: the two lines that say what the sort covered.
        let last_uid = users.last().map(|u| u.uid).unwrap_or_default();
        tracing::debug!(
            "Rebuild index of {} users, last_uid={last_uid}",
            users.len()
        );
        tracing::debug!("Loaded {} users", users.len());
        Self { users, index_uid }
    }

    pub fn count(&self) -> i32 {
        self.users.len() as i32
    }

    /// `query_user_info` — by pool position, which is what a session remembers.
    pub fn by_id(&self, id: i32) -> Option<&UserInfo> {
        usize::try_from(id).ok().and_then(|i| self.users.get(i))
    }

    /// `query_user_info_by_uid` (`:1247`).
    pub fn by_uid(&self, uid: i32) -> Option<&UserInfo> {
        let pos = self.index_uid.binary_search_by_key(&uid, |e| e.uid).ok()?;
        self.by_id(self.index_uid[pos].id)
    }

    /// `query_user_info_by_username` (`:1313`): every user whose name starts with `prefix`,
    /// compared case-insensitively over the first `prefix.len()` bytes, because the pool is
    /// ordered by a case-insensitive MySQL collation.
    pub fn by_username_prefix(&self, prefix: &str, max: usize) -> Vec<&UserInfo> {
        if prefix.is_empty() {
            return Vec::new();
        }
        // The pool ascends by name, so `cmp_username(prefix, name)` runs Greater (names
        // before the prefix), Equal (the prefix match), then Less — two partition points
        // delimit the run, which is what the C function's three binary searches find.
        let first = self
            .users
            .partition_point(|u| cmp_username(prefix, &u.username) == Ordering::Greater);
        let len = self.users[first..]
            .partition_point(|u| cmp_username(prefix, &u.username) == Ordering::Equal);
        self.users[first..first + len].iter().take(max).collect()
    }

    /// `get_user_id_list` (`:1565`): up to `max` uids from `start_uid` upwards, in index
    /// order, which is how the statistics pass walks the roster.
    pub fn uid_list_from(&self, start_uid: i32, max: usize) -> Vec<i32> {
        let first = self.index_uid.partition_point(|e| e.uid < start_uid);
        self.index_uid[first..]
            .iter()
            .take(max)
            .map(|e| e.uid)
            .collect()
    }

    /// `BBS_user_limit_per_page` pages of the roster. `None` is `page_id` out of range, the
    /// C `-3`; the count it reports anyway is [`Roster::page_count`].
    pub fn page(&self, page_id: i32) -> Option<&[UserInfo]> {
        page_window(&self.users, page_id)
    }

    /// `*p_page_count` of `query_user_list` (`:1062`).
    pub fn page_count(&self) -> i32 {
        (self.users.len() as i32 + USER_LIMIT_PER_PAGE - 1) / USER_LIMIT_PER_PAGE
    }
}

/// `strncasecmp(prefix, name, strlen(prefix))`: the roster's ordering key compared against a
/// search term, byte by byte, ASCII case-folded. A name that ends early is smaller, matching
/// the NUL that `strncasecmp` compares.
fn cmp_username(prefix: &str, name: &str) -> Ordering {
    let mut name = name.bytes();
    for byte in prefix.bytes() {
        let found = name.next().unwrap_or(0);
        let order = byte.to_ascii_lowercase().cmp(&found.to_ascii_lowercase());
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

/// `query_user_list` / `query_user_online_list` paging:
/// `MIN(BBS_user_limit_per_page, count - page_id * BBS_user_limit_per_page)` entries, or none
/// at all when the page id is outside `[0, page_count)`.
fn page_window<T>(items: &[T], page_id: i32) -> Option<&[T]> {
    let per_page = USER_LIMIT_PER_PAGE as usize;
    let start = usize::try_from(page_id).ok()?.checked_mul(per_page)?;
    if start >= items.len() {
        return None;
    }
    Some(&items[start..(start + per_page).min(items.len())])
}

/// `USER_ONLINE_INFO` (`include/user_list.h:61`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineUser {
    /// Position in [`OnlineList::users`].
    pub id: i32,
    /// `user_online.SID`, which is how the web application and the daemon share the table.
    pub session_id: String,
    pub user: UserInfo,
    pub ip: String,
    pub current_action: String,
    /// `current_action_title`: `Some("")` for a session that has not set an action yet.
    pub current_action_title: Option<&'static str>,
    pub login_tm: i64,
    pub last_tm: i64,
}

/// `USER_ONLINE_LIST` (`include/user_list.h:74`).
#[derive(Debug, Clone, Default)]
pub struct OnlineList {
    pub users: Vec<OnlineUser>,
    /// `index_uid`, ordered by `uid` then `id`, so a user's several sessions are adjacent.
    index_uid: Vec<UidIndex>,
    /// Guests are counted but not listed: `user_online` rows with `UID = 0` keep an
    /// unauthenticated connection's heartbeat without appearing in 环顾四周.
    pub user_count: i32,
    pub guest_count: i32,
}

impl OnlineList {
    /// `user_online_list_load` (`:256`). A row is skipped when its user is no longer in the
    /// roster or its action is not one the daemon knows, which is what the C `continue`s do.
    ///
    /// One deliberate difference: the C code raises `user_count` before it resolves the row,
    /// so a dropped row leaves the list claiming a user it never stored — and then sorts an
    /// index over those missing slots. The port counts the entries it actually has.
    pub fn load(rows: &[OnlineRow], roster: &Roster) -> Self {
        let mut users: Vec<OnlineUser> = Vec::new();
        let mut guest_count = 0;

        for row in rows {
            if row.uid == 0 {
                guest_count += 1;
                continue;
            }
            let Some(user) = roster.by_uid(row.uid) else {
                continue;
            };
            let current_action = trunc_bytes(&row.current_action, CURRENT_ACTION_MAX_LEN);
            let current_action_title = if current_action.is_empty() {
                Some("")
            } else {
                match action_title(&current_action) {
                    Some(title) => Some(title),
                    None => continue,
                }
            };

            users.push(OnlineUser {
                id: users.len() as i32,
                session_id: trunc_bytes(&row.sid, SESSION_ID_LEN),
                user: user.clone(),
                ip: trunc_bytes(&row.ip, IP_ADDR_LEN),
                current_action,
                current_action_title,
                login_tm: row.login_tm,
                last_tm: row.last_tm.unwrap_or(0),
            });
            if users.len() >= MAX_USER_ONLINE_COUNT {
                // `:342`: the list stops at the pool's limit and says so.
                tracing::error!("Too many online users, exceed limit {MAX_USER_ONLINE_COUNT}");
                break;
            }
        }

        let mut index_uid: Vec<UidIndex> = users
            .iter()
            .map(|u| UidIndex {
                uid: u.user.uid,
                id: u.id,
            })
            .collect();
        index_uid.sort_by(|a, b| a.uid.cmp(&b.uid).then(a.id.cmp(&b.id)));

        let user_count = users.len() as i32;
        Self {
            users,
            index_uid,
            user_count,
            guest_count,
        }
    }

    /// `query_user_online_info`.
    pub fn by_id(&self, id: i32) -> Option<&OnlineUser> {
        usize::try_from(id).ok().and_then(|i| self.users.get(i))
    }

    /// `query_user_online_info_by_uid` (`:1474`): the sessions of one user, starting at index
    /// `start_id` so a screen can page through them.
    pub fn sessions_of(&self, uid: i32, start_id: i32, max: usize) -> Vec<&OnlineUser> {
        let Ok(start) = usize::try_from(start_id) else {
            return Vec::new();
        };
        if start >= self.index_uid.len() {
            return Vec::new();
        }
        let rest = &self.index_uid[start..];
        let first = rest.partition_point(|e| e.uid < uid);
        rest[first..]
            .iter()
            .take_while(|e| e.uid == uid)
            .take(max)
            .filter_map(|e| self.by_id(e.id))
            .collect()
    }

    /// `query_user_online_list`, with the same out-of-range answer as [`Roster::page`].
    pub fn page(&self, page_id: i32) -> Option<&[OnlineUser]> {
        page_window(&self.users, page_id)
    }

    /// `*p_page_count` of `query_user_online_list` (`:1118`).
    pub fn page_count(&self) -> i32 {
        (self.users.len() as i32 + USER_LIMIT_PER_PAGE - 1) / USER_LIMIT_PER_PAGE
    }

    /// `get_user_online_list_count`.
    pub fn counts(&self) -> (i32, i32) {
        (self.user_count, self.guest_count)
    }
}

/// `USER_STAT` and `USER_STAT_MAP` (`include/user_stat.h`): visible-article counters, held in
/// a table sorted by `uid` rather than in the roster, because the articles are scanned
/// separately from the users.
#[derive(Debug, Clone)]
pub struct UserStatMap {
    stats: Vec<(i32, i32)>,
    /// The highest `uid` the map covers: an article by a newer user is not counted, which is
    /// how the C map avoids inserting into a sorted table while scanning.
    last_uid: i32,
    /// How far [`UserStatMap::update`] has walked [`Board::articles`].
    last_article_index: i32,
}

/// The watermarks must be the `-1` of `user_stat_map_init`, not `0`: a derived default would
/// sit past the guest's uid 0 and past the first article of the arena, and both would go
/// uncounted for the life of the process.
impl Default for UserStatMap {
    fn default() -> Self {
        Self::new()
    }
}

impl UserStatMap {
    /// `user_stat_map_init`.
    pub fn new() -> Self {
        Self {
            stats: Vec::new(),
            last_uid: -1,
            last_article_index: -1,
        }
    }

    /// `user_stat_article_cnt_inc`, which reports whether the user is in the map at all.
    pub fn article_cnt_inc(&mut self, uid: i32, n: i32) -> bool {
        match self.stats.binary_search_by_key(&uid, |e| e.0) {
            Ok(pos) => {
                self.stats[pos].1 += n;
                true
            }
            Err(_) => false,
        }
    }

    /// `user_stat_get` / `get_user_article_cnt`.
    pub fn article_count(&self, uid: i32) -> Option<i32> {
        let pos = self.stats.binary_search_by_key(&uid, |e| e.0).ok()?;
        Some(self.stats[pos].1)
    }

    /// `p_map->last_uid + 1`, the first uid [`UserStatMap::update`] has not covered: what C
    /// hands `get_user_id_list()` (`user_stat.c:47`) so each pass only widens the tail.
    fn next_uid(&self) -> i32 {
        self.last_uid + 1
    }

    /// `user_stat_map_update` (`user_stat.c:36`): widen the table with the users the roster
    /// gained since the last pass, then count the articles the last pass did not see.
    ///
    /// C asks for the ids from `last_uid + 1` up (`user_stat.c:47`), so the table only ever
    /// grows at the tail and the binary searches in [`UserStatMap::article_count`] stay valid;
    /// `articles` is the whole arena, of which only `last_article_index + 1..` is scanned.
    pub fn update(&mut self, roster: &Roster, articles: &[Article]) {
        let new_uids = roster.uid_list_from(self.next_uid(), MAX_USER_COUNT);
        for &uid in &new_uids {
            if self.stats.len() >= MAX_USER_COUNT {
                // `BBS_max_user_count` is a hard ceiling in the C pool; a user past it simply
                // has no counter, and its articles are skipped by the `uid > last_uid` test.
                break;
            }
            debug_assert!(self.stats.last().is_none_or(|(last, _)| uid > *last));
            self.stats.push((uid, 0));
            self.last_uid = uid;
        }

        // `last_article_index` starts at -1, meaning "nothing scanned yet".
        for article in articles
            .iter()
            .skip((self.last_article_index + 1).max(0) as usize)
        {
            if !article.visible {
                continue;
            }
            if article.uid > self.last_uid {
                // `uid=%d of article(aid=%d) is greater than last_uid=%d`
                continue;
            }
            self.article_cnt_inc(article.uid, 1);
        }
        self.last_article_index = if articles.is_empty() {
            -1
        } else {
            i32::try_from(articles.len() - 1).unwrap_or(i32::MAX)
        };
    }

    pub fn covered_users(&self) -> usize {
        self.stats.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_row(uid: i32, username: &str) -> UserRow {
        UserRow {
            uid,
            username: username.to_owned(),
            nickname: None,
            gender: "M".to_owned(),
            gender_pub: true,
            life: 100,
            exp: None,
            visit_count: 0,
            signup_dt: None,
            last_login_dt: None,
            last_logout_dt: None,
            birthday: None,
            introduction: Some("简介".to_owned()),
        }
    }

    fn online_row(sid: &str, uid: i32, action: &str) -> OnlineRow {
        OnlineRow {
            sid: sid.to_owned(),
            uid,
            ip: "10.0.0.1".to_owned(),
            current_action: action.to_owned(),
            login_tm: 1000,
            last_tm: Some(2000),
        }
    }

    fn roster() -> Roster {
        // The load query is `ORDER BY username`, and the collation is case-insensitive, so
        // this is the order the pool holds them in.
        Roster::load(&[
            user_row(30, "amy"),
            user_row(10, "Ann"),
            user_row(11, "anna"),
            user_row(12, "annabelle"),
            user_row(20, "bob"),
        ])
    }

    /// `user_list.c:243` and `:248`: the load reports what it sorted and how many it kept. Both
    /// are `log_debug()`, which is the error stream here.
    #[test]
    fn the_load_reports_the_user_count_and_the_last_uid() {
        let (common, error) = crate::log::captured(|| {
            let roster = roster();
            assert_eq!(roster.count(), 5);
        });

        assert!(
            error.contains("Rebuild index of 5 users, last_uid=20\n"),
            "{error}"
        );
        assert!(error.contains("Loaded 5 users\n"), "{error}");
        assert_eq!(common, "", "{common}");
    }

    #[test]
    fn the_roster_is_ordered_by_username_and_indexed_by_uid() {
        let roster = roster();
        assert_eq!(roster.count(), 5);
        assert_eq!(roster.page_count(), 1);
        assert_eq!(
            roster
                .page(0)
                .unwrap()
                .iter()
                .map(|u| u.username.as_str())
                .collect::<Vec<_>>(),
            vec!["amy", "Ann", "anna", "annabelle", "bob"]
        );
        // `id` is the position in that order, `uid` the database key.
        assert_eq!(roster.by_id(0).unwrap().uid, 30);
        assert_eq!(roster.by_uid(20).unwrap().username, "bob");
        assert_eq!(roster.by_uid(11).unwrap().id, 2);
        assert_eq!(roster.by_uid(999), None);
        assert_eq!(roster.by_id(999), None);
        assert_eq!(roster.by_id(-1), None);
        assert_eq!(roster.by_uid(11).unwrap().intro, "简介");
    }

    #[test]
    fn a_username_prefix_search_ignores_case_and_respects_the_limit() {
        let roster = roster();
        let found = roster.by_username_prefix("ANN", 10);
        assert_eq!(
            found
                .iter()
                .map(|u| u.username.as_str())
                .collect::<Vec<_>>(),
            vec!["Ann", "anna", "annabelle"]
        );
        assert_eq!(roster.by_username_prefix("ann", 2).len(), 2);
        assert!(roster.by_username_prefix("zoe", 10).is_empty());
        assert!(roster.by_username_prefix("", 10).is_empty());
        // A prefix longer than a name cannot match it, which is what the NUL that
        // `strncasecmp` compares gives the C original.
        assert!(roster.by_username_prefix("anna-b", 10).is_empty());
    }

    #[test]
    fn paging_stops_past_the_last_page() {
        let rows: Vec<UserRow> = (0..21)
            .map(|i| user_row(i, &format!("user{i:03}")))
            .collect();
        let roster = Roster::load(&rows);
        assert_eq!(roster.page_count(), 2);
        assert_eq!(roster.page(0).unwrap().len(), 20);
        assert_eq!(roster.page(1).unwrap().len(), 1);
        assert_eq!(roster.page(2), None);
        assert_eq!(roster.page(-1), None);
    }

    #[test]
    fn the_online_list_counts_guests_and_drops_sessions_it_cannot_label() {
        let roster = roster();
        let rows = vec![
            online_row("Telnet_Process_1", 0, "MENU"),
            online_row("Telnet_Process_2", 10, "VIEW_ARTICLE"),
            online_row("Telnet_Process_3", 11, "NOT_AN_ACTION"),
            online_row("Telnet_Process_4", 12, ""),
            online_row("Telnet_Process_5", 777, "MENU"),
        ];
        let online = OnlineList::load(&rows, &roster);

        let (users, guests) = online.counts();
        assert_eq!((users, guests), (2, 1));
        assert_eq!(
            online
                .users
                .iter()
                .map(|u| u.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["Telnet_Process_2", "Telnet_Process_4"]
        );
        assert_eq!(online.users[0].user.username, "Ann");
        assert_eq!(online.users[0].current_action_title, Some("阅读文章"));
        // A session that has not chosen an action yet is listed with an empty label, which is
        // what `current_action_title = ""` means in the pool.
        assert_eq!(online.users[1].current_action_title, Some(""));
        assert_eq!(online.users[1].last_tm, 2000);
    }

    #[test]
    fn online_sessions_of_a_user_are_paged_by_index() {
        let roster = roster();
        let rows = vec![
            online_row("Telnet_Process_a", 10, "MENU"),
            online_row("Telnet_Process_b", 20, "MENU"),
            online_row("Telnet_Process_c", 10, "USER_ONLINE"),
        ];
        let online = OnlineList::load(&rows, &roster);

        let both = online.sessions_of(10, 0, 10);
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].session_id, "Telnet_Process_c");
        // `start_id` is an index into the uid-ordered sessions, not a skip count: index 1 is
        // the second session of uid 10, index 2 belongs to uid 20 and so finds nothing.
        assert_eq!(online.sessions_of(10, 1, 10).len(), 1);
        assert!(online.sessions_of(10, 2, 10).is_empty());
        assert_eq!(online.sessions_of(10, 0, 1).len(), 1);
        assert!(online.sessions_of(30, 0, 10).is_empty());
        assert!(online.sessions_of(10, 9, 10).is_empty());
        assert_eq!(online.page_count(), 1);
        assert_eq!(online.page(0).unwrap().len(), 3);
    }

    #[test]
    fn action_title_maps_the_codes_the_web_side_writes() {
        assert_eq!(action_title("VIEW_ARTICLE"), Some("阅读文章"));
        assert_eq!(action_title("LOGIN"), Some("进入大厅"));
        assert_eq!(action_title(""), None);
        assert_eq!(action_title("view_article"), None);
    }

    #[test]
    fn the_stat_map_counts_visible_articles_of_users_it_loaded() {
        let articles = vec![
            Article {
                aid: 100,
                sid: 1,
                uid: 1,
                visible: true,
                ..Article::default()
            },
            Article {
                aid: 101,
                sid: 1,
                uid: 1,
                visible: false,
                ..Article::default()
            },
            Article {
                aid: 102,
                sid: 1,
                uid: 5,
                visible: true,
                ..Article::default()
            },
        ];
        let first = Roster::load(&[user_row(1, "ann"), user_row(2, "bob"), user_row(3, "cy")]);
        let extended_roster = Roster::load(&[
            user_row(1, "ann"),
            user_row(2, "bob"),
            user_row(3, "cy"),
            user_row(5, "dee"),
        ]);

        let mut stats = UserStatMap::new();
        // `uid = 5` is not in the map yet, which is the C `last_uid` guard.
        stats.update(&first, &articles);
        assert_eq!(stats.article_count(1), Some(1));
        assert_eq!(stats.article_count(2), Some(0));
        assert_eq!(stats.article_count(5), None);

        // The watermark means a second pass only sees the users and articles added since.
        stats.update(&extended_roster, &articles);
        assert_eq!(stats.article_count(5), Some(0));
        let mut extended = articles.clone();
        extended.push(Article {
            aid: 103,
            sid: 1,
            uid: 5,
            visible: true,
            ..Article::default()
        });
        stats.update(&extended_roster, &extended);
        assert_eq!(stats.article_count(5), Some(1));
        assert_eq!(stats.covered_users(), 4);

        stats.article_cnt_inc(1, -1);
        assert_eq!(stats.article_count(1), Some(0));
        assert!(!stats.article_cnt_inc(99, 1));
    }

    #[test]
    fn a_reload_does_not_recount_the_users_already_covered() {
        // The regression: `update_stats` used to hand the whole roster over every tick, which
        // appended ids below `last_uid` and broke the sorted table the lookups assume.
        let all = roster();
        let articles = vec![Article {
            aid: 7,
            sid: 1,
            uid: 30,
            visible: true,
            ..Article::default()
        }];

        let mut stats = UserStatMap::new();
        for _ in 0..3 {
            stats.update(&all, &articles);
        }
        assert_eq!(stats.covered_users(), 5);
        assert_eq!(stats.article_count(30), Some(1));
    }

    #[test]
    fn uid_list_resumes_where_the_last_pass_stopped() {
        let roster = roster();
        assert_eq!(roster.uid_list_from(1, 10), vec![10, 11, 12, 20, 30]);
        assert_eq!(roster.uid_list_from(12, 2), vec![12, 20]);
        assert!(roster.uid_list_from(31, 10).is_empty());
    }

    /// `user_stat.c:54-64`: the watermark moves with the users a pass actually loaded, and with
    /// none when it loaded none. The line `8e02db3` replaced ran even for `i == 0`, reading
    /// `uid_list[-1]` — four bytes below the array — and publishing whatever was there.
    #[test]
    fn an_empty_window_leaves_the_watermark_where_it_was() {
        let nobody = Roster::load(&[]);
        let mut stats = UserStatMap::new();
        stats.update(&nobody, &[]);
        assert_eq!(
            stats.last_uid, -1,
            "a pass that loaded no user publishes no uid"
        );
        assert_eq!(stats.next_uid(), 0, "and asks for the same window again");

        stats.update(&roster(), &[]);
        let seen = stats.last_uid;
        assert_eq!(seen, 30);

        stats.update(&nobody, &[]);
        assert_eq!(stats.last_uid, seen, "and an empty pass cannot move it");
        assert_eq!(stats.next_uid(), seen + 1);
    }

    /// The map `build_map` (`tests/test_user_stat.c:65-79`) seeds: uids 1, 2, 3 and 5
    /// with counts 0, 5, 0 and 7, so uid 4 is the gap, uid 5 the last entry and two stored users
    /// read as zero.
    fn build_map(last_uid: i32, last_article_index: i32) -> UserStatMap {
        UserStatMap {
            stats: vec![(1, 0), (2, 5), (3, 0), (5, 7)],
            last_uid,
            last_article_index,
        }
    }

    /// `test_lookup` (`test_user_stat.c:112-158`), whose stated point is that reading a count
    /// must not depend on where the search settles: the two zero counts are found, and every
    /// neighbour of a stored uid — below the first, in the gap, after the last, far above, and
    /// the negative — reports no record rather than the slot next to it.
    #[test]
    fn the_reference_lookup_table() {
        let stats = build_map(100, -1);
        assert_eq!(
            UserStatMap::new().covered_users(),
            0,
            "user_stat_map_init: user_count"
        );
        assert_eq!(UserStatMap::new().last_uid, -1, "map_init: last_uid");
        assert_eq!(
            UserStatMap::new().last_article_index,
            -1,
            "map_init: last_article_index"
        );

        for (uid, want) in [(1, Some(0)), (2, Some(5)), (3, Some(0)), (5, Some(7))] {
            assert_eq!(stats.article_count(uid), want, "the count of uid {uid}");
        }
        for uid in [0, 4, 6, 1_000_000, -1] {
            assert_eq!(stats.article_count(uid), None, "no record for uid {uid}");
        }

        // A single entry is both the first and the last of the list.
        let only_one = UserStatMap {
            stats: vec![(1, 0)],
            ..build_map(100, -1)
        };
        assert_eq!(only_one.article_count(1), Some(0), "get of a single entry");
        assert_eq!(only_one.article_count(2), None, "get past a single entry");
        assert_eq!(
            only_one.article_count(3),
            None,
            "get far past a single entry"
        );

        // Counting up follows the same search and reports a miss instead of writing one.
        let mut stats = build_map(100, -1);
        assert!(stats.article_cnt_inc(3, 1), "inc uid 3");
        assert!(stats.article_cnt_inc(3, 4), "inc uid 3 again");
        assert_eq!(stats.article_count(3), Some(5), "increments accumulate");
        assert!(stats.article_cnt_inc(1, -2), "inc uid 1 by a negative");
        assert_eq!(stats.article_count(1), Some(-2), "the count went negative");

        for uid in [4, 6, 0] {
            assert!(
                !stats.article_cnt_inc(uid, 1),
                "inc of the absent uid {uid}"
            );
        }
        assert_eq!(stats.covered_users(), 4, "an absent uid adds no entry");
        assert_eq!(
            stats.article_count(2),
            Some(5),
            "an absent uid left uid 2 alone"
        );
        assert_eq!(
            stats.article_count(5),
            Some(7),
            "an absent uid left uid 5 alone"
        );
    }

    /// `test_update_without_users` and `test_update_at_the_end`
    /// (`test_user_stat.c:162-222`), over the same five articles the C file builds: uids
    /// 1, 1, 2, 3, 4 of which the third is hidden. No user is ever loaded in a test run, so the
    /// scan runs against a map the caller seeded and the watermarks have to stay where they are.
    #[test]
    fn the_reference_scan_table() {
        let article = |uid: i32, visible: bool| Article {
            aid: 0,
            sid: 1,
            uid,
            visible,
            ..Article::default()
        };
        let articles = vec![
            article(1, true),
            article(1, true),
            article(2, false),
            article(3, true),
            article(4, true),
        ];
        let nobody = Roster::load(&[]);

        let mut stats = build_map(100, -1);
        stats.update(&nobody, &articles);
        assert_eq!(stats.covered_users(), 4, "an empty pool keeps user_count");
        assert_eq!(stats.last_uid, 100, "an empty pool keeps last_uid");
        assert_eq!(
            stats.last_article_index, 4,
            "the cursor is the last article scanned"
        );
        assert_eq!(
            stats.article_count(1),
            Some(2),
            "both of uid 1's articles counted"
        );
        assert_eq!(
            stats.article_count(2),
            Some(5),
            "uid 2's only article is hidden"
        );
        assert_eq!(stats.article_count(3), Some(1), "uid 3 counted once");
        assert_eq!(
            stats.article_count(5),
            Some(7),
            "uid 5 has no article at all"
        );

        // A pass over no new article changes nothing.
        stats.update(&nobody, &articles);
        assert_eq!(stats.last_article_index, 4, "nothing new: cursor");
        assert_eq!(stats.article_count(1), Some(2), "nothing new: uid 1");
        assert_eq!(stats.article_count(3), Some(1), "nothing new: uid 3");

        // An author above `last_uid` waits for the pass that loads it.
        let mut stats = build_map(2, -1);
        stats.update(&nobody, &articles);
        assert_eq!(stats.last_uid, 2, "low last_uid: still nothing loaded");
        assert_eq!(
            stats.article_count(1),
            Some(2),
            "low last_uid: uid 1 is covered"
        );
        assert_eq!(
            stats.article_count(2),
            Some(5),
            "low last_uid: its only article is hidden"
        );
        assert_eq!(
            stats.article_count(3),
            Some(0),
            "low last_uid: uid 3 is above it, so its article is skipped"
        );
        assert_eq!(
            stats.article_count(5),
            Some(7),
            "low last_uid: uid 5 has no article"
        );

        // A map that already reached the end of the block recounts nothing.
        let mut stats = build_map(100, 4);
        stats.article_cnt_inc(1, 41);
        stats.update(&nobody, &articles);
        assert_eq!(
            stats.article_count(1),
            Some(41),
            "update at the end recounts nothing"
        );
        assert_eq!(
            stats.article_count(5),
            Some(7),
            "update at the end leaves the last entry"
        );
        assert_eq!(stats.last_uid, 100, "update at the end: last_uid");
        assert_eq!(stats.last_article_index, 4, "update at the end: cursor");
    }
}
