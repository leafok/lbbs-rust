# lbbs-rust — a Rust rewrite of LBBS

> English is the default version of this document and the one kept up to date.
> A Simplified Chinese translation is available in [README.zh-CN.md](README.zh-CN.md).

This document explains **what the project is for someone who has never run a BBS and is new to Rust**.
The sections build on each other, so reading top to bottom works.

---

## 1. What this is

### 1.1 The original software (LBBS)

| Item | Value |
| --- | --- |
| Name | LBBS (Leaflet BBS Daemon) |
| Version | 1.7.12 — revision `49805a5` on `main` (the release has no `v1.7.12` tag yet) |
| Language | C (about 30,340 lines across `src/*.c` and `include/*.h`) |
| License | GPL-3.0-or-later |
| Author | Leaflet &lt;leaflet@leafok.com&gt; |
| Reference copy, alongside this repo | `../lbbs` — read-only for us; **we never modify it** |

LBBS is a **bulletin board server you connect to from a text terminal**. Users connect over telnet or SSH,
press menu keys, read boards, and write posts. There is no mouse and no images: everything is drawn with text
plus **terminal control sequences** (strings like `ESC[...H` that set colours and cursor position).

The other important fact: **LBBS does not own its data.** Post bodies and attachments live in files on disk,
and the index of users, boards and posts lives in a **MySQL database**. The site's **web bulletin board** reads
that same database. So LBBS is really a *terminal front-end to that web board*, and that gives us a
hard constraint: **we must not change the schema.**

The reference 1.7.12 is `v1.7.11..49805a5`: twenty commits, 23 files, +616/−57 lines, and the working tree
matches `49805a5`. Every `file.c:NNN` citation in the Rust sources is anchored to that one revision, so
`git -C ../lbbs show 49805a5:src/<file>` prints exactly the text the comment describes. [Section
10.3](#103-the-1712-round-what-each-reference-fix-means-here) records how the 1.7.12 fixes are represented here.

### 1.2 This project (lbbs-rust)

We are rewriting that C server in Rust.

- The structure of the code (which module does what) is deliberately mapped one-to-one onto the C original.
- We use **the same database** and read/write **the same configuration and data file formats**, so the web
  application keeps working unchanged. No data migration is needed.
- Details a user can see — output, key handling, UTF-8/GBK character sets — are kept as close to the original
  as we can get them.
- The **server body is in**: `main.rs` starts the daemon, the loader task and both listeners, and a
  connection runs the real C order of events — charset prompts, welcome, login, top menu. A guest can
  open a board, page through its articles and read one, and the write path (post, reply, modify, delete,
  plus the editor) is wired behind the same privilege gates C uses.
  [Section 6](#6-what-works-and-what-does-not) states what is still missing.

> **Common misconception**: "`cargo run` starts a bulletin board." — **It does now**, and a guest can
> reach the top menu, open a board and read a post.

---

## 2. Mini dictionary

Words that appear over and over in this document and in the source.

| Term | Meaning |
| --- | --- |
| BBS | Bulletin board system. People connect to it and read/write posts |
| telnet | The old way of attaching a terminal over plain TCP. LBBS speaks a little telnet protocol itself (echo and go-ahead negotiation) |
| SSH | Encrypted connection. LBBS implements the server side; users arrive with `ssh -p 2322` |
| session | One connection: from the first prompt to disconnect |
| section / board | A category of the board (e.g. "Poetry", "Computers"). Table `section`, its id is **SID** |
| article | One post on a board. Its id is **AID** |
| user | A person who logs in. Their id is **UID**, split across tables `user_list` (credentials, flags) and `user_pubinfo` (profile) |
| privilege | The bits saying whether this person may read, post, moderate, administer. In C these live in the global `BBS_priv` |
| menu | The item table in `menu.conf`; it decides which key runs which function |
| LML | LBBS Markup Language — the board's own colour/coordinate markup, used when rendering posts |
| badwords | A list of banned terms that get masked to `*` in posts automatically |
| AID location (`section_aid_loc`) | The files that remember "which article was last read here" and similar state |
| GBK / UTF-8 | Chinese character encodings; the user picks one at connect time |
| the web board | The bulletin board software that shares this database and these files |

---

## 3. How the thing works (the C design)

Understanding the original makes every porting decision later fall into place.

```
                  ┌──────────────────────────────────────────────┐
 terminal ─ telnet►  bbsd (parent process)                       │
 terminal ─ ssh ──►  waits for connections → fork() a child     │
                  └───────────────┬──────────────────────────────┘
                                  │ one child = one session
                                  │  · keeps its state in process-global BBS_* variables
                                  │  · sends raw SQL to MySQL through libmysqlclient
                                  │  · reads boards/articles/users from shared memory
                                  ▼
                        MySQL (the same database the web board uses)
                        var/articles/<CID>, data/*.txt, var/*.conf
```

Two things define that design:

1. **One connection = one forked process.** That is why the C code can use global variables without shame —
   and why every connection opens its own MySQL connection.
2. **Boards, articles and users stay resident in shared memory**, refreshed on signals such as `SIGUSR1`,
   and refreshed *incrementally*: only rows that changed are re-read.

---

## 4. The four design decisions for the Rust version

Each one was chosen deliberately, and each has a reason.

### 4.1 One process, Tokio async tasks (no fork)

`fork()`, POSIX shared memory and System V semaphores fight Rust's safety model and only add raw `libc` calls.
Instead the whole server is one process running **Tokio** tasks.

- C's per-process globals → fields of a [`Session`](src/session.rs) struct.
- C's shared memory → [`Arc<RwLock<...>>`](src/state.rs) inside the process.

> New to async? An `async fn` is a function that has to wait for something. At each `.await` it hands the
> thread to another task, so a single thread can serve many connections. The trap is that code which does not
> await is not finished running — that is the biggest difference from synchronous code.

### 4.2 Same database, same file formats (coexist with the web app)

We read and write `conf/bbsd.conf`, `conf/menu.conf`, `var/menu_merged.conf`, `var/bbs_top_menu.conf`,
`data/*.txt` and `var/articles/<CID>` exactly as the C server does. Authentication keeps the original
`SHA2(?, 256)` expression evaluated inside MySQL, so accounts created through the web site log in fine here.

### 4.3 Deliver in phases, starting with the core loop

| Phase | Contents | Status |
| --- | --- | --- |
| Phase 1a | daemon startup, both listeners, connection limits, logging, signals, charset prompts, login, menus | **done**, and smoke-tested against the real database |
| Phase 1b | board list, article list, reading (pager), post/reply, badwords wired to menu actions | **done**, including the write path and the editor |
| Phase 2 | the digest tree, statistics, the user-profile editor, GBK polish, SSH window size | partly: browsing the digest tree is done, the rest not |

Every phase ends with **a clean compile and a green test run**. We do not pile up broken code and hope.

### 4.4 SSH through russh, reusing the existing host keys

The private keys in `conf/ssh_host_*` are reused, and the `russh` crate's server implementation calls the same
session code the telnet path uses. The plan is to replace only the transport, not the behaviour, of what C
achieved with libssh.

---

## 5. How to read the source

The rule is **one Rust module ↔ one C file**. Every module begins with a doc comment that says which C file and
line ranges it ports, followed by a **`# Deviations from C`** list.

That layout is intentional: if you know the C, you can always find the Rust. And if something seems to behave
differently from C, read the deviations section first — the reason is written there.

### 5.1 Module map

| Rust file | Lines | Original C | Responsibility | Status |
| --- | ---:| --- | --- | --- |
| [src/paths.rs](src/paths.rs) | 118 | `include/bbs.h` etc. | screen size, field lengths, path names | done |
| [src/daemon.rs](src/daemon.rs) | 427 | `src/main.c`, `src/daemon.c` | command line, `--help`/`--version`, `fork` into background | done |
| [src/config.rs](src/config.rs) | 394 | `src/init.c` | reads `conf/bbsd.conf` | done |
| [src/log.rs](src/log.rs) | 513 | `src/log.c` | the two append-mode log files, `log_restart()` | done |
| [src/text.rs](src/text.rs) | 362 | `src/str_process.c` | display width (wide chars = 2 columns) and line wrapping | done |
| [src/term/mod.rs](src/term/mod.rs) | 812 | `src/io.c` | byte pipes, GBK transcoding, `prints`/`iflush`, `igetch` | done |
| [src/term/key.rs](src/term/key.rs) | 504 | `src/io.c` | turns ESC sequences into key codes | done |
| [src/screen.rs](src/screen.rs) | 1151 | `src/screen.c` | clearing, the echo handoff `set_input_echo`, line input `str_input`, pager `display_data` | done |
| [src/db.rs](src/db.rs) | 1580 | `src/database.c` etc. | MySQL queries through sqlx | done |
| [src/board.rs](src/board.rs) | 1553 | `src/section_list_loader.c` | in-memory pools of sections, articles, users | done |
| [src/browse.rs](src/browse.rs) | 3371 | `src/section_list_display.c`, `src/section_list_loader.c`, `src/menu_proc.c` (the two digest items) | the article list, its keys, reading a post, the saved reading position, and a board's digest tree | done |
| [src/editor.rs](src/editor.rs) | 1853 | `src/editor.c` | the full-screen editor: rows, window, status row, key handling | done |
| [src/post.rs](src/post.rs) | 1617 | `src/article_post.c`, `src/article_del.c`, `src/article_op.c` | post, reply, modify, delete, the 精华 toggle, the article's web link, and the transaction each write runs in | done |
| [src/viewlog.rs](src/viewlog.rs) | 369 | `src/article_view_log.c` | which posts this user has read | done |
| [src/favorlog.rs](src/favorlog.rs) | 452 | `src/article_favor.c` | which posts this user starred | done |
| [src/favorlist.rs](src/favorlist.rs) | 785 | `src/article_favor_display.c`, `src/article_favor.c` (the page query), `src/menu_proc.c` (the `FAVOR_TOPIC` item) | the 我收藏的主题 list: two name columns, the star removed in place, and the board each row opens | done |
| [src/roster.rs](src/roster.rs) | 705 | `src/user_list_loader.c` | the online-user roster and the article counters | done |
| [src/userinfo.rs](src/userinfo.rs) | 954 | `src/user_info_display.c`, `src/user_list_display.c` (the search), `src/bbs.c` (the level and sign tables) | one user's profile page, and the name search that reaches it | done |
| [src/user_info_update.rs](src/user_info_update.rs) | 642 | `src/user_info_update.c`, `src/menu_proc.c` (the two profile items) | the 说明档 and 签名档 editors: one `user_pubinfo` column through the editor, masked, counted and written back | done |
| [src/userlist.rs](src/userlist.rs) | 1053 | `src/user_list_display.c`, `src/menu_proc.c` (the two roster items) | the 线上使用者 and 已注册用户 pages: titles, columns, paging, and the keys between them | done |
| [src/loader.rs](src/loader.rs) | 1603 | `src/section_list_loader.c` | the incremental refresh task, the article cache files it writes, and each board's digest menu | done |
| [src/state.rs](src/state.rs) | 410 | the shared-memory segments | what the loader publishes and a session reads | done |
| [src/cache.rs](src/cache.rs) | 650 | `src/article_cache.c` | render and load `var/articles/<CID>` with its line index | done |
| [src/data_file_mgr.rs](src/data_file_mgr.rs) | 452 | `src/data_file_mgr.c`, `src/bbs_cmd.c` (the `DATAMGR` entry) | the `@DATAMGR` list: pick one of the eight site texts by number, edit it, preview it, and write the result to both the packaged file and its `.user` copy | done |
| [src/menu.rs](src/menu.rs) | 2440 | `src/menu.c` | `menu.conf` parsing and navigation | parsing/navigation done, feature bodies not |
| [src/lml.rs](src/lml.rs) | 720 | `src/lml.c` | LML markup rendering | done |
| [src/bwf.rs](src/bwf.rs) | 242 | `src/trie_dict.c` etc. | badword matching and masking | done |
| [src/ipmask.rs](src/ipmask.rs) | 109 | `src/ip_mask.c` | address masking (`202.38.64.x`) | done |
| [src/privilege.rs](src/privilege.rs) | 487 | `src/user_priv.c` | privilege words and levels | done |
| [src/session.rs](src/session.rs) | 619 | the `BBS_*` globals | one connection's state + online bookkeeping | done |
| [src/login.rs](src/login.rs) | 569 | `src/login.c` | login, authentication, user-agreement (EULA) | done |
| [src/bbsnet.rs](src/bbsnet.rs) | 1890 | `src/bbs_net.c`, `src/menu_proc.c` (the `@mod:` dispatcher) | the station table, the shuttle grid, the outbound connect and the two-way relay | done |
| [src/chicken.rs](src/chicken.rs) | 2289 | `utils/game/chicken.c` | 电子小鸡: one line of fourteen numbers and a name, the naming prompt, the nine things you do with a chicken, and the death that hatches the next egg | done |
| [src/money.rs](src/money.rs) | 243 | `utils/game/money.c` | 糖糖: the balance a session caches, the deposit that stops at the cap, and the withdrawal that is all or nothing | done |
| [src/bbs.rs](src/bbs.rs) | 973 | `src/bbs_main.c`, `src/bbs_cmd.c` | the session's own order of events, echo handoff included, `bbs_center` | menu loop done, and every action `conf/menu.conf` binds has a handler (the favourites filter is a menu filter, not an action) |
| [src/net.rs](src/net.rs) | 358 | `src/net_server.c` | accept, connection limits, graceful drain | done |
| [src/ssh.rs](src/ssh.rs) | 504 | `src/net_server.c` (the libssh half) | host keys, password auth, the session channel | done |
| [src/main.rs](src/main.rs) | 555 | `src/main.c`, `src/net_server.c` | startup order, signal handling, `SIGHUP` reload, shutdown | done |
| [src/batch.rs](src/batch.rs) | 383 | `utils/lib/db_open.inc.php` + the lines the three scripts repeat | one generator's life: arguments, server root, `conf/bbsd.conf`, database, the PHP's own stdout text and exit bytes | done |
| [src/section_list.rs](src/section_list.rs) | 149 | `utils/lib/section_list.inc.php` | the class/board hierarchy the generators walk, with the caller's per-board values | done |
| [src/gen_top.rs](src/gen_top.rs) | 306 | `utils/bin/gen_top.php` | `var/bbs_top.txt` and `var/bbs_top_menu.conf`: the ten hottest posts of the week | done |
| [src/gen_section_menu.rs](src/gen_section_menu.rs) | 691 | `utils/bin/gen_section_menu.php` | `var/menu_merged.conf`: `conf/menu.conf` plus the six generated menu blocks | done |
| [src/gen_ex_list.rs](src/gen_ex_list.rs) | 424 | `utils/bin/gen_ex_list.php` | one `var/gen_ex/<SID>` digest tree per board the daemon flagged | done |
| [src/bin/gen_top.rs](src/bin/gen_top.rs), [gen_section_menu.rs](src/bin/gen_section_menu.rs), [gen_ex_list.rs](src/bin/gen_ex_list.rs) | 30 | the `php utils/bin/*.php` cron lines | three binaries that call the three generators and return the script's exit byte | done |

36 358 lines in total, the tests included (they live in the modules they test). There are 524 tests: 518 in the
library and 6 in the binary. 510 of them need nothing but the checked-out tree; the other 14 are ignored because
they read the real database and files.

> Counts drift as work continues. Trust `cargo test --lib` output over this table.

---

## 6. What works, and what does not

Being honest about status is part of the method: everything listed as working is covered by a test, and nothing
that does not run is described as if it did.

### 6.1 ✅ Done (and tested)

1. **Configuration parsing** — `conf/bbsd.conf` tokenised exactly like C (space, tab, CR, LF as separators;
   keys case-insensitive).
2. **Display-width arithmetic** — wide CJK characters count as two columns, control sequences as zero, using
   the same rules as C's `str_process.c`.
3. **Key decoding** — `ESC [ A` arrives as "up arrow"; a sequence split across packets is discarded and reported
   as a bare `ESC`, the way `igetch` does it.
4. **Character sets** — UTF-8 and GBK both work; a GBK character split across packets is rejoined correctly,
   and so is a `0xff` control escape. `io_buf_conv` (io.c:1142-1176) keeps such an escape and the byte behind
   it away from the converter, which matters because that is exactly what a telnet client's `IAC` answer to
   the session's negotiation is: transcoded as text it reached the login line as junk and swallowed the keys
   typed after it, which is why output looked wrong after `(G)GBK`.
5. **Line input (`str_input`)** and **pager (`display_data`)** — hidden password entry (a `NOECHO` prompt
   prints C's `*`/`**` mask), `q` to quit, and the rest of C's key behaviour.
6. **MySQL queries** — credential checks, board and article indexes, online list, experience accrual.
7. **In-memory pools** of sections/articles/users (the replacement for shared memory) **plus the incremental
   update logic**.
8. **`menu.conf` parsing**, cursor navigation, and privilege-based filtering of items.
9. **LML rendering** and **badword masking**.
10. **The privilege model** (`user_priv.c`) and **IP masking** (`ip_mask.c`).
11. **Login**: three attempts, `guest` and `new` special cases, brute-force throttles (per /24 and per account),
    re-acceptance of the user agreement, and the rule that staff accounts must arrive over SSH.
12. **The server body**: `conf/bbsd.conf` → shared state → the loader task → both listeners, with C's command
    line, `--foreground`/`--display-log`, the `fork()` before the runtime exists, the two log files and
    `log_restart()`, `SIGHUP` reload, `SIGTERM` drain with a 15-second grace and a 1-second escalation. The
    periodic task is the very loader that finished the startup pass, which is what the fork gave the C child:
    `last_article_op_log_mid` is the cursor a pass needs to reach `bbs_article_op` at all, so a loop started
    from a fresh one keeps appending new articles while every change to an old one goes unseen.
13. **Connection limits**: total `bbs_max_client` and per-IP `bbs_max_client_per_ip`, counted the way the C
    parent counted its children, with the log lines it logged.
14. **SSH** through russh on the same host keys: password-only auth, the same three tries, one session channel,
    and the session running the same `bbs_main` as telnet.
15. **The session's opening**, in C's order: [`screen::set_input_echo`](src/screen.rs)
    sending `IAC WILL ECHO, IAC WILL SGA` so the client stops echoing what it types, then the two 5-second
    prompts, `bbs_info`, `data/welcome.txt`, the counts line, `var/bbs_top.txt` and the top menu with its
    ten-second board panel.
16. **The article list of a board** — pages, the pinned rows the last page repeats, the cursor and every key
    that moves it, the thread colours, the `[提示]`/`@`/`N`/`x` markers, the two date shapes, and the
    scans by author, title and unread posts. Reading a post goes through `var/articles/<CID>` and the same
    pager, and leaving the board writes `var/section_aid_loc/<UID>` where C wrote it.
17. **Which posts are read and which are starred**, per user, in the same tables the web site reads, flushed
    at the same point of the session as C flushed them.
18. **The write path**, driven from the article list's keys: `article_post`, `article_reply`, `article_modify`
   and `article_del`, each behind the same privilege gate as C, with the title/class prompts, the post header,
   the `article`/`thread`/`section` index updates, the experience change and the `article_log` row in one
   transaction — the shape of C's `*_tx` helpers. A guest pressing `^P` gets `您没有权限在本版块发表文章`
   before any SQL runs. The `m` key is `article_excerption_set` (`article_op.c:34-269`), the 精华 toggle: the
   locked row's five columns, the flag, the generated-digest columns and the `ex_file` row on the way out, the
   attachments' owner, the author's experience — 50 for an original topic, 20 for a reprint, 10 for a reply, all
   of it at the same rate backwards — and the `'E'`/`'O'` audit row, then the pool's copy of the flag before the
   commit that C's failed commit gives back. It draws nothing: the row's marker is the whole response, and for a
   user who fails `S_POST | S_MAN_S` the response is the list as it was, without a word or a bell.
   `Ctrl-Q` is the same file's other function, `display_article_meta` (`article_op.c:22-32`): the cleared screen,
   the `Web版 文章链接:` label on row 3, the site's own `https://<BBS_server>/bbs/view_article.php?id=<AID>#<AID>`
   on row 4, then the wait for a key. C's `log_error` on its return cannot fire — the function only ever
   answers `0` — so the port has nothing to report either.
19. **The full-screen editor** (`editor.c`): insert and overwrite, the row and window arithmetic, `Ctrl-K` and
   `Ctrl-Y` deletes, backspace joining rows, wide characters deleted whole, `Ctrl-W`/`Ctrl-X` leaving without
   repainting, the status row, and the `MAX_EDITOR_DATA_LINES` limit.
20. **Writing the article cache.** [`cache::generate`](src/cache.rs)
   is C's `article_cache_generate` down to the byte, and the loader calls it for every post a pass appends and
   every `'M'` a replay applies (`section_list_loader.c:327`, `:542`), so `var/articles/<CID>` appears on its
   own — within a second of the post landing, the same as C.
21. **The two roster pages and the profile behind them** — `user_list_display(1)` and `user_list_display(0)`
    ([`userlist::user_list_display`](src/userlist.rs)), reached from the
    top menu's `线上使用者`/`已注册用户` items and from nothing else. Both are one screen with the other flag: the
    title line, the column headers, 20 rows a page, and the keys `k`/`j`/`p`/`n`/`↑`/`↓`/`Home`/`End`/`PgUp`/`PgDn`
    over them — including C's habit of letting one key do both jobs at once, so `Home` on the last row of a page
    sets page one *and* moves the cursor, and `Enter` on a row opens that user's profile
    ([`userinfo::user_info_display`](src/userinfo.rs)) before the page
    repaints. `s` runs the name search inside the list, `h` shows the reader help, `N` rotates to the tail and `P`
    to the head. The three duration columns are `gmtime` of a raw second count read as a date, which is why an hour
    of idleness prints `0"` and a login a year old prints `1年0天`.
22. **The sysop's 关闭系统 item** — `shutdown_bbs` (`menu_proc.c:176-193`), question and all. Its answer is one
   character wide and starts out holding the default `N`, so `get_data` refuses what is typed over it with a bell
   and a `Y` only gets in once the `N` is erased; Enter, `Esc` and an idle timeout each leave the system running,
   and the handler returns `REDRAW` either way, so the menu repaints over the question.
23. **The site shuttle** — `bbs_net` ([`bbsnet::bbs_net`](src/bbsnet.rs))
   behind the 特别服务区 menu's `【 网络连线 】` item, reached the way C reaches it: `@RunMBEM` whose parameter
   `N@mod:lib/libbbs.so#bbs_net` carries the entry name after the `#`, and whose `REDRAW` repaints the menu
   whatever the shuttle answered. The station table comes out of `conf/bbsnet.conf` with C's field limits (40, 40,
   253, 5 and 20 bytes), its `#` comments and its `strtol`-then-`1..65535` port test, so a broken line is skipped
   rather than half-read. The grid is four stations to a row — `'A'…'Z'` then `'a'…'z'`, at row `2+id/4` and column
   `5+id%4*20` — above the detail block of the selected station, and `↑`/`↓` walk a whole row while `←`/`→` step
   one station. Enter runs `bbsnet_connect`: the ten-second budget beside the inverted progress bar, the
   resolve-bind-non-blocking-connect sequence whose refused answer C only reports once the budget has gone (kept),
   `IPTOS_LOWDELAY`, then the two-way relay that converts to and from the station's own charset — and for a station
   marked `SSH`, only for a session that arrived over SSH, the libssh leg rebuilt on russh with its four
   known-host answers and its five messages. A key typed while the bar is running is spent on the connect, as in C;
   `Ctrl+C` and `Esc` cancel it, and every answer redraws the frame.
24. **The 我收藏的主题 list** — `article_favor_display` (`article_favor_display.c:296-401`) behind the top menu's
   `@FAVOR_TOPIC` item (`menu_proc.c:372-380`), whose parameter C ignores. Each row is `"  %7d %c %s%*s %s %s"` of
   the article number, the read flag, whichever of the two columns `n` has chosen (the poster in 12 cells, the board
   name in 20), the date and the title cut to the 59 cells that are left — with `● ` for a thread head and `[转载]`
   for a transship, but without the board list's collapsing of repeated `Re: ` prefixes. The star list is read
   through `query_favor_articles` (`article_favor.c:450-518`), whose flush-and-merge runs first, so `-` removes a row
   for good before the page is drawn again. C's three loose ends are kept: `KEY_NULL` is logged and then behaves like
   `Esc`; `UNSET_FAVOR` has no `break` and falls into the refresh `CHANGE_PAGE` does; and after `LOCATE_ARTICLE` only
   rows 4-23 and the bottom row are redrawn, never the header, so the board the reader came back from leaves its own
   title on the screen. A removal that makes the retained `page_id` unreachable — the last row of the last page —
   ends the list instead of clamping to the new tail, as C's out-of-range check does.
25. **The three batch generators** — `utils/bin/gen_top.php`, `gen_section_menu.php` and `gen_ex_list.php`, now
   [`gen_top::run`](src/gen_top.rs),
   [`gen_section_menu::run`](src/gen_section_menu.rs) and
   [`gen_ex_list::run`](src/gen_ex_list.rs) behind three binaries in
   `bin/`, on top of the shared [`batch`](src/batch.rs) and
   [`section_list`](src/section_list.rs). They read the database the
   daemon already uses, through the same `conf/bbsd.conf`, and write the same files: the ten-post text and menu,
   the six generated menu blocks appended to the hand-written `conf/menu.conf`, and one digest tree per board whose
   `ex_menu_update` flag the daemon set. Where the PHP and a plain Rust reading of it disagreed, the PHP won: a
   heredoc loses the newline its last body line ends on, so a block is one line per item with no blank line inside
   it; `str_repeat` of a negative count is a fatal error that leaves the boards after it unwritten; an escaped title
   keeps the padding it was measured with before the backslash went in; a `NULL` date prints the current time; and
   `intval(log10($n))` counts the digits of a float, with `0` measured as if it were `1`.
26. **A board's digest tree** — `section_list_ex_dir_display` (`section_list_display.c:1380-1471`), entered from the
   article list's `x` key (`VIEW_EX_DIR`, `section_list_display.c:1353-1363`) and from the top menu's
   `@LIST_EX_SECTION` (`menu_proc.c:259-276`), its posts read by `view_ex_article` (`menu_proc.c:218-257`). The tree
   is the `var/gen_ex/<SID>` file of item 25, parsed once per board by the loader — C's `update_gen_ex` half of
   `load_section_config_from_db` (`section_list_loader.c:162-179`) — and shared behind an `Arc` in place of the
   shared memory C re-attached for every reader. C's three ways out of a visit stay three: a board whose
   `ex_menu_tm` is still 0 prints 该版块精华区未开放 and waits for a key; a menu the loader could not read is the
   `get_menu_shm_readonly` failure, and its timestamp is left behind so the next pass retries it; a
   `VIEW_EX_ARTICLE` whose post is gone leaves the tree on screen exactly as the reader left it. The digest pager
   writes only the status bar of `display_ex_article_key_handler` (`menu_proc.c:203-216`) — no thread switch, no
   reply — and `Esc`/`←` is the only way out of it.
27. **The 数据文件 page** — `data_file_mgr` ([`data_file_mgr::data_file_mgr`](src/data_file_mgr.rs)), the
   `@DATAMGR` item of the top menu (`bbs_cmd.c:28`) and the one menu action that rewrites files under the site
   root. The eight rows are numbered on the screen and the answer is read two cells wide into a three-byte buffer,
   so what `atoi` makes of it is the whole selection: nothing typed, `0`, and a number past the list leave the same
   way they came. The file opens as `<path>.user` where the site has one and as the packaged path where it does
   not, and the editor and the preview below it are the same [`editor::editor_display`](src/editor.rs) and
   [`screen::display_file`](src/screen.rs) the write path uses. Two of C's loose ends are kept: `(S)` before
   anything has been previewed is not an answer, and an edit that ends because the daemon is going down writes
   nothing. A save goes to both paths — the `.user` copy first, since it is the copy a reinstall leaves alone — and
   the page then says a reload is what makes the change count, which is the truth:
   [`load_data_files`](src/state.rs) is what reads these files again.
28. **The two profile texts** — `user_intro_edit` and `user_sign_edit`
   ([`user_info_update::user_intro_edit`](src/user_info_update.rs)), the `@EDIT_INTRO`/`@EDIT_SIGN` pair of the top
   menu's toolbox (`conf/menu.conf:155-156`, `menu_proc.c:402-420`) and the only two items there that write a row.
   Each reads one `user_pubinfo` column — `introduction`, or the `sign_1`…`sign_3` whose number the signature
   screen asks for one cell at a time — hands it to the same editor as item 27, and asks
   `(S)保存, (C)取消 or (E)再编辑? [S]: ` until one answer settles it: `(C)` and a key that timed out leave with no
   SQL at all, `(E)` goes back through the editor, anything else redraws the question, and `CR`/`(S)` masks the
   text through the badword filter, renders it, counts its lines and writes it. Two of these screens' limits are
   not what their numbers say: the mask is one `*` per byte rather than per character, and the line count includes
   the empty display line a closing newline leaves behind, so nine lines are the longest save that passes the test
   against ten. The update binds the text where C escaped and spliced it, the introduction's closing line quotes
   the roster's own 60-second interval, and a text that does not fit the 4 KB buffer is C's `-3`, logged and
   dropped. `M_FAVOR_SECTION`, the third name that used to be listed beside these two, turns out never to have
   been an action: C asks its handler whether each row of the favourites submenu is visible
   (`menu.c:1141-1143`, `menu_proc.c:196-201`), which is what
   [`Session::filter_allows`](src/session.rs) already does.
29. **电子小鸡** — `chicken_main` ([`chicken::chicken_main`](src/chicken.rs)) behind the top menu's
   `【 电子小鸡 】` item (`conf/menu.conf:238`), reached the same way C reaches it — `@RunMBEM` with
   `C@mod:game/so/libgame.so#chicken_main`, the entry name after the `#` — and spending 糖糖 through
   [`money`](src/money.rs), the other half of the same `libgame.so`. The whole game lives in one line per user in
   `var/chicken/<UID>`: fourteen numbers and a name, written without truncation and read back with the age rebuilt
   from the birth day and month rather than stored. The screen is C's: the picture `age` picks, the `生日` month and
   day beside a `糖糖` balance padded to eight columns, the two help rows, and the nine actions that answer one key at
   a time — 吃饭, 吃零食, 清理鸡舍, 猜拳, 目前战绩, 买饲料, 买零食, 吃大补丸, 卖鸡 — plus `c` 改名字, `k` 杀鸡,
   `t` 休息 and `q` 退出. Every
   number moves the way C moves it, including the ones nobody would choose: the rest screen paints over a tired
   chicken and then runs the action anyway, `tiredstrong` only clears for a player who can pay fifty 糖糖, a deposit
   the database refuses leaves the chicken unsold with `无法存钱，放弃交易！`, and two hundred and one grams dies.
   Death is `呜...小鸡挂了....`, a line in `var/chicken/log`, the record unlinked and a new egg hatched in the middle
   of the action that asked for the kill — named from the same prompt that starts out holding `宝宝`.

### 6.2 Verified against the live board database

A guest telnet session answers the two 5-second prompts, takes `data/welcome.txt` in one screen the way C's file
cache wraps it, logs in as `guest`, opens `B` → a board → the article list, reads a real post out of
`var/articles/<CID>` through the pager, and is refused `^P`. The same list then takes the digest key `m` from a
guest: the gate refuses it in silence, so the only bytes that come back are the repaint of the rows.
The same session arrives over russh SSH: password
auth, the welcome page, the pager.
`SIGHUP` logs `Reload configuration`; `SIGTERM` logs `Main process exiting` and `Main process exit normally`
inside the drain, and every session deleted its own `user_online` row — the table picked up nothing from these
runs. No article, message or flag was written to the production database during verification.

The digest ladder is the part that could only be measured against the live schema, so
[`the_digest_ladder_rolls_everything_back`](src/post.rs) runs it there
in a transaction that is dropped instead of committed: the flag and the author's `exp` are read back while the
transaction is still open, and again after the rollback took them. A rolled-back `INSERT` does advance
`bbs_article_op`'s auto-increment counter, which is the only mark any of this left.

Booted with an empty `var/articles/`, the loader re-rendered every cache file the C server's tree holds — 28 220
of them, in about two seconds — and each byte matched the file C had written for the same post, including the
`strftime` header line, which is why the server installs the `en_US.UTF-8` locale before it starts. The one file
C's tree did not have was the newest post on the board.

What the idle daemon asks the database for is measurable without writing anything: `SHOW GLOBAL STATUS LIKE
'Com_select'` rises by 34 per 15 seconds with nobody connected — one AID scan and one op-log replay per second,
one online-list reload per five, one roster per minute. A loop that starts its own loader replays nothing and
measures 19; that gap is the whole difference between the two shapes, and it is why the periodic task takes the
loader from the startup pass instead of building one.

The generators were checked the same way: each PHP script was run from a sandbox copy against the live database —
read-only, with `gen_ex_list.php`'s `UPDATE` replaced by `SELECT 1` so no flag was cleared — and the Rust binary's
files compared byte for byte. `var/menu_merged.conf` matched at 36 966 bytes, with every class block, board row,
favourite row and digest row populated; the digest trees of the two biggest boards matched at 90 087 and 55 959
bytes; and the top-ten pair matched both in its empty shape (the live board had nothing in the last seven days, so
102 and 335 bytes of header) and in its populated one, which the sandbox reached by widening the date window in the
PHP and in `gen_top`'s `#[ignore]`-marked parity test to the same ten years (2 544 and 3 316 bytes).

### 6.3 ⬜ Not done

1. **Phase 2 polish**: GBK corner cases, the SSH window size (deliberately ignored, see the header of
   `src/ssh.rs`), and a side-by-side comparison with C over the whole phase-1 surface.

Two entries left this list on 2026-11-21, and neither of them was work waiting to be done. The 说明档/签名档
editors are item 28. The "statistics page" beside them was a name for `src/user_stat.c`, which is not a screen at
all and has no menu key: it is the in-memory article counter behind a profile's 发文 figure, ported as
[`roster::UserStatMap`](src/roster.rs) and read by [`userinfo::user_info_display`](src/userinfo.rs).
`M_FAVOR_SECTION` went the same way — a filter the menu asks per row, already answered, never a key that only
logged. So §13's "then" row is closed: of the twenty names `bbs_cmd.c:19-40` binds, the nineteen that are actions
all have handlers.

So today the repository is "the server runs, a guest can browse a board and read a post, and the write path is
wired and unit-tested". The first account that really posts should do it against a test database.

---

## 7. Setting up a development environment

### 7.1 Requirements

| What | Version | Notes |
| --- | --- | --- |
| Rust | 1.96 or newer | the crate uses `edition = "2024"` |
| MySQL | 8.x | the database that holds the board's data |
| Linux | — | development and verification happen on Linux |
| Locale | `en_US.UTF-8` | `locale -a` must list it: the server installs it before it writes any article cache, as C does, and refuses to start without it |

`cargo` is Rust's build-and-test tool and comes with the toolchain. Run everything from the `lbbs-rust/` directory.

```bash
cd lbbs-rust
cargo build            # compile the library and the binary
cargo test             # 510 tests that need no database (14 more are opt-in, see 7.2)
cargo fmt              # the tree is rustfmt-clean; run it before you commit
```

A healthy run looks like this:

```
test result: ok. 504 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out
...
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

> **Formatting is `cargo fmt`'s job, not yours.** The tree passes `cargo fmt --check`, so run it before committing
> and the diff you read contains only what you actually changed. Four-space indent and the naming below stay as
> they are.

### 7.2 The tests that use the real database and real files

Fourteen tests carry `#[ignore]` because they touch the live database or need the real generated files. The tree
ships those files now (§8), so they only need pointing at. `LBBS_REF_CONF` decides the server root the session
tests read `var/` from, so give it an absolute path:

```bash
export LBBS_REF_CONF=$PWD/conf/bbsd.conf     # add a db_password line first; see §8
export LBBS_REF_MENU=$PWD/var/menu_merged.conf
export LBBS_REF_TOP10=$PWD/var/bbs_top_menu.conf
cargo test --all-targets -- --include-ignored
```

Pointing the same three variables at the C tree (`../lbbs/...`) works too and is the stricter test,
since those are the files C generated.

To keep the run read-only — which is what you want against a database the live site is using — leave out the
two tests that write rows:

```bash
cargo test --lib -- --ignored --skip live_flush
```

The two generator parity tests belong to this read-only set: `gen_top`'s and `gen_ex_list`'s each run one `SELECT`,
write their output under `/tmp`, and compare it with the reference PHP's own files if those are present (§7.4).
`gen_ex_list` never runs the `UPDATE` that would clear a board's `ex_menu_update` flag, because that is a write.

These are the heart of the porting quality gate: they check that Rust interprets the files **C actually
generated** the same way C did. Four of them go further and drive a session against the live board pool —
the page tiling and the row text of every board, two guests who each open a real board, one of whom reads
a real post out of `var/articles/<CID>` and leaves, and both roster pages drawn from the loaded `user` and
`user_online` tables, paged to the end and closed with `Esc`.

> **Careful**: `--ignored` **connects to the real board database**. The session tests only read: the online
> heartbeat is an `UPDATE … WHERE SID = ?` that matches no row for their synthetic session ids, and a guest has
> no view log or reading position to flush. The view-log and favourite tests do write, so take a backup first.
> And the file you pass in `LBBS_REF_CONF` contains a password — never echo it or commit it.

### 7.3 Running the server

The binary expects to start inside a **server root** — the directory that holds `conf/`, `data/`, `var/` and a
writable `log/` — and it finds that root two levels up from the executable, the way C did with `bin/bbsd`. This
repository is such a root, so a development run means putting the binary in `bin/`:

```bash
cargo build --release
mkdir -p bin && cp target/release/bbsd bin/
./bin/bbsd --foreground           # reads conf/bbsd.conf, listens for telnet and SSH
telnet -c localhost 2323             # connect over telnet
ssh -p 2322 guest@localhost          # connect over SSH
```

The Cargo package is `lbbs`; the daemon it builds is `bbsd` — the same name as the C server's — and the three
generators from 7.4 build as `gen_top`, `gen_section_menu` and `gen_ex_list`. `cargo run` picks the daemon.

`cargo run` alone is still not enough to serve: with the binary under `target/debug/` the derived root is
`target/`, which has no `conf/` to read. `--foreground` (C's `-f`) keeps the server attached to the terminal;
without it the process detaches and leaves only the log files.

`--display-log` and `--display-error-log` send the two streams to stderr as well, and `--help`, `--version` and
`--compile-config` print what their names say. `SIGHUP` reloads the configuration, `SIGUSR1` reopens the log
files for logrotate and `SIGTERM` drains the sessions before exiting: the exit flag ends every key wait, so a
session parked at a prompt runs its own goodbye instead of waiting out its idle limit — which is what C's
children did when the parent's `kill(0, SIGTERM)` interrupted their `select()`.

The configuration in this tree listens on telnet `2323` and SSH `2322` (from `bbs_port` and `bbs_ssh_port`),
the same pair the reference C installation uses; the shipped template still has C's `23` and `22`.
When you connect over telnet, use `telnet -c`: the server tells the client to stop echoing locally, and `-c`
keeps the client from displaying characters twice before that negotiation lands.

> **Run one front end at a time against a given database.** The Rust `bin/bbsd` and the C `bbsd` write
> the same `user_online` and `visit_log` rows; they will not corrupt anything, but the online list and
> the counters double-count while both are up.

A guest session goes all the way through: the two prompts, the welcome page, login, the top menu, a board group,
a board, the article list, and a real post. [tools/guest_smoke.py](tools/guest_smoke.py)
drives exactly that walk and stops at the permission gate, so it never writes anything; run it with
`python3 tools/guest_smoke.py 2323`.

### 7.4 Running the batch generators

The three scripts the web admin used to cron are binaries now. They find their server root the way `bin/bbsd`
does — two levels up from the executable — so they live beside it:

```bash
cargo build --release
ln -s ../target/release/gen_top bin/gen_top                     # as with bin/bbsd
ln -s ../target/release/gen_section_menu bin/gen_section_menu
ln -s ../target/release/gen_ex_list bin/gen_ex_list
./bin/gen_top                    # var/bbs_top.txt, var/bbs_top_menu.conf
./bin/gen_section_menu           # var/menu_merged.conf: conf/menu.conf plus the database
./bin/gen_ex_list                # var/gen_ex/<SID> for every board the daemon flagged
```

None of them takes an argument: one more prints `Invalid usage` and leaves `255`, which is what the script did
under `php`. Whatever a generator's reason for stopping was goes to **stdout**, because a cron run mails that
stream rather than the log files. The statuses are PHP's own, seen through the shell's single byte: `255` for a
fatal step (`-1`), `254` for a failed query or a `conf/menu.conf` that could not be read (`-2`), `253` for the
failures one level out — a section list, or the `UPDATE` that stamps `ex_menu_tm` and clears a board's
`ex_menu_update` flag (`-3`). `gen_ex_list` writes into `var/gen_ex/` and, like the C daemon's own copy of that
directory, expects it to exist.

Those three reads of the database are ordinary `SELECT`s; the only write anywhere in the generators is that one
`UPDATE` per regenerated board. Against a database the live site is using, run `gen_top` and `gen_section_menu`
freely and leave `gen_ex_list` to the machine that owns the flag — its parity is checked instead by the
`#[ignore]`-marked `a_real_digest_tree_is_the_php_bytes`, which generates the tree without clearing anything.

Comparing a generator with the PHP it replaced is mechanical: run a sandbox copy of the script, with its `UPDATE`
replaced by a `SELECT 1` so no flag is cleared, and the binary over the same database, then `cmp` the two files.
The figures in §6.2 were measured that way.

---

## 8. Where the configuration and data live

The repository carries its own server root, so it runs instead of or next to `bbsd` with no C checkout in
reach. Everything a connection reads is in the tree; what stays out of git is only what belongs to a machine
or to a person — the password and the host keys.

```
lbbs-rust/                      ← the server root (§7.3)
├── bin/                       ← where the built binary goes — not in git
├── conf/
│   ├── bbsd.conf.default      ← main configuration template
│   ├── bbsd.conf              ← your copy, with the credentials — not in git, see below
│   ├── bbsnet.conf            ← the other stations this board knows (+ .default)
│   ├── badwords.conf          ← banned terms (+ .default)
│   ├── menu.conf              ← menu definitions, the source of var/menu_merged.conf
│   ├── ca_cert.pem            ← MySQL TLS verification (the C version uses it, so we do too)
│   ├── lbbs.service           ← systemd unit, and lbbs.logrotate for log rotation (+ the .in templates)
│   └── ssh_host_*             ← SSH host keys: generated, never committed, see below
├── data/                      ← the eleven texts the screens show: welcome, goodbye, eula, register,
│                                 login_error, license, copyright, version, active_board, read_help and
│                                 editor_help (the twelfth file the loader preloads is var/bbs_top.txt)
├── var/
│   ├── menu_merged.conf       ← the menu set the daemon actually reads
│   ├── bbs_top_menu.conf      ← the top-ten menu as generated
│   ├── bbs_top.txt            ← the screen shown just after login
│   ├── gen_ex/<SID>           ← one board's digest tree, rewritten by gen_ex_list
│   ├── chicken/<UID>          ← one user's 电子小鸡 record, with chicken/log beside it; made at startup
│   ├── articles/<CID>         ← article bodies — data, not configuration, not in git; see below
│   └── section_aid_loc/<UID>  ← "last read article" state, written as people browse
└── log/                       ← bbsd.log and error.log
```

Three of those need a word of explanation:

- **`conf/bbsd.conf` is the one file you write.** `cp conf/bbsd.conf.default conf/bbsd.conf`, then replace the
  sample `db_*` values (`user` / `db` are not a real account) and the two ports, which the template leaves at
  23 and 22. `.gitignore` names the file so a password cannot reach a commit by accident, and the template
  carries no working credential. Without a password the server does not start at all: it logs
  `Db::connect() error: ... Access denied for user '<who>'@'<host>' (using password: NO)` and exits.
- **`conf/ssh_host_{rsa,ed25519,ecdsa}_key` are yours to make.** The SSH listener reads all three and gives up
  if none loads (`ssh::load_host_keys`), but reusing the deployment's signing keys in a development tree is
  exactly what they are not for:

  ```bash
  for k in rsa ed25519 ecdsa; do ssh-keygen -t $k -f conf/ssh_host_${k}_key -N '' -q; done
  ```
- **`var/articles/` is content, and the server generates it.** The `article` table holds the index;
  the body a reader sees comes from `var/articles/<CID>`, and a missing file is a hard error, logged as
  `article_cache_load(aid=…, cid=…) error`, exactly as in C. The loader writes those files as it ingests a post
  ([`cache::generate`](src/cache.rs) is that renderer), so an empty
  directory fills itself from the database — the test board's 28 000 files and 130 MB in about two seconds —
  and nothing has to be copied from another tree. It stays out of git, which `.gitignore` already does.

Rust keeps these names in [src/paths.rs](src/paths.rs)
and resolves them relative to the server root held by [`Session`](src/session.rs).

Keys in `conf/bbsd.conf` (identical names, meanings and quirks as C):

| Key in the file | Rust field | Meaning |
| --- | --- | --- |
| `bbs_id` | `bbs_id` | board's internal name (e.g. `LBBS-devel`) |
| `bbs_name` | `bbs_name` | display name (e.g. 枫林在线) |
| `bbs_server` | `bbs_server` | domain / advertised URL |
| `bbs_address` | `bbs_address` | bind address |
| `bbs_port` | `telnet_port` | telnet port (2323 in the reference file) |
| `bbs_ssh_port` | `ssh_port` | SSH port (2322 in the reference file) |
| `bbs_max_client` | `max_client` | global concurrent-connection limit |
| `bbs_max_client_per_ip` | `max_client_per_ip` | per-IP concurrent-connection limit |
| `bbs_start_dt` | `start_dt` | opening date, written `YYYY-M-D` |
| `bbs_sys_id` | `sys_id` | system id |
| `db_host` / `db_username` / `db_password` / `db_database` / `db_timezone` | the `db` struct | MySQL connection settings |

The parsing quirks are ported too, and they are the usual reason "my setting had no effect":

- Separators are space, tab, CR and LF, so values cannot contain spaces (the file's own header says this).
- **Keys are case-insensitive** (C used `strcasecmp`).
- A line whose first token starts with `#` is a comment.
- A key with no value is skipped with a "Skip empty value" warning.
- A line with a **third token that does not start with `#` is skipped entirely**.
  Example: `bbs_port 2323 2324` → warning, the value is not applied and the **default (23) stays**.
  `bbs_port 2323 # note` → applied.
- Numbers are read like C's `atoi`: digits until the first non-digit, so `bbs_port 2323a` reads as 2323.
- Out-of-range numbers (`bbs_max_client 0`) are warned about and **not applied**.
- String values are truncated to the C buffer sizes (`bbs_id` 20, `bbs_name` 50, `bbs_server` 255).

### ⚠️ Security, things you must know

- `db_password` is stored **in plaintext** in the config file. That is inherited from the C version.
  **Do not commit that file; never print the value** to logs, error output or this repository. During the port,
  the password is only ever passed programmatically from the config file, never echoed. `.gitignore` is what
  keeps the promise: `conf/bbsd.conf` is ignored, `conf/bbsd.conf.default` is what ships, and no tree in this
  repository has ever held the value.
- The MySQL connection **verifies the server certificate** through `ca_cert.pem`, because the C client did.
- The original C version logged the **plaintext password** into `user_err_login_log` when a login failed.
  We reproduce it as-is for data compatibility (the web side reads the same column). Porting did **not** make
  this table safe: treat it as sensitive and consider it a real improvement candidate for the deployment, not a
  quiet change inside the port.
- Password hashing is `SHA2(plaintext, 256)` computed by MySQL, with no salt. Kept identical so web accounts work.

---

## 9. The life of one connection

What a user sees, in the C version's real order. The right-hand column gives the Rust location and its status.

| # | What the user sees | In C | In Rust / status |
| --- | --- | --- | --- |
| 1 | connects | `net_server.c` accepts, then forks | [`net::listen`](src/net.rs) accepts one task per connection, with C's limit checks and its `Accept ... connection from` log line ✅ |
| 1a | over SSH | `net_server.c`'s libssh child | [`ssh.rs`](src/ssh.rs): password auth, then the same session; `欢迎使用ssh方式访问` replaces the login prompt ✅ |
| 2 | asked `(U)UTF-8/(G)GBK? [U]` | `bbs_charset_select`, two 5-second prompts | [`bbs_charset_select`](src/bbs.rs) ✅ |
| 3 | `(V)变宽/(F)定宽` (width mode) | same prompt set | same call, both answers wired to [`Terminal::set_fixed_width`](src/term/mod.rs) ✅ |
| 4 | welcome screen | `data/welcome.txt` via `display_file(..., 2)` | [`Session::display_file`](src/session.rs#L217) ✅ |
| 5 | "please enter your account" | `bbs_login`, up to 3 tries | [`login()`](src/login.rs#L160) ✅ |
| 6 | typing `guest` or `new` | trial account / registration notice | ✅ (`new` shows the notice and ends the session) |
| 7 | "please enter your password" | `check_user` | [`check_user()`](src/login.rs#L235) ✅ |
| 8 | blocked for too many failures | 10 failures per /24 in 10 minutes; 3 per account | ✅ (`ipmask::subnet_pattern` + `db::count_errors_by_*`) |
| 9 | "您目前无权登陆..." | reproduced, including the log row written first | ✅ |
| 10 | "the user agreement changed" | `user_update_agreement` | [`update_agreement()`](src/login.rs#L418) ✅, called by both transports |
| 11 | online entry, visit counters | `user_online_add`, `visit_log` | [`Session::online_add`](src/session.rs#L162) ✅ |
| 12 | `var/bbs_top.txt` + the top menu | `display_file(VAR_BBS_TOP)` → `bbs_center` | [`bbs_main`](src/bbs.rs) ✅, with the ten-second `data/active_board.txt` panel |
| 13 | pick a board → pick an article → read it | menu item + `display_file` + `section_list_*` | [`menu`](src/menu.rs) parsing and navigation ✅, [`browse`](src/browse.rs) ✅ — this is the guest path `tools/guest_smoke.py` walks |
| 14 | post, reply, modify, delete | editor + writes into `var/articles/` + DB updates + badwords | [`editor`](src/editor.rs) ✅, [`post`](src/post.rs) ✅ (one transaction each, badwords on submit); a guest is refused before any SQL runs |
| 15 | experience accrual and goodbye | `user_online_exp` + `data/goodbye.txt` | [`Session::logout`](src/session.rs#L244) ✅ |
| 16 | removed from the online list | `user_online_del` | [`Session::cleanup`](src/session.rs#L260) ✅ |

The cells that are still ⬜ are the screens behind those keys — see §6.3.

---

## 10. What changed and what we refused to "fix"

### 10.1 Deliberate changes (mechanics)

| C's mechanism | Rust version | Why |
| --- | --- | --- |
| `fork()` per connection | Tokio tasks, one process | safety, resource use, and far fewer MySQL connections |
| process-global `BBS_*` | `Session` / `AppState` fields | state must be explicit once many sessions share a process |
| POSIX shared memory + semaphores + `mprotect` | `Arc<RwLock<...>>` | same reason |
| `mysql_real_escape_string` + `sprintf` SQL | sqlx **bound parameters** | removes the SQL-injection class of bugs at the root |
| transactions left open until `mysql_close` | explicit `commit` / `rollback` on every path | a pooled connection handed back with an open transaction corrupts the next user |
| `setenv("TZ", ...)` + `tzset()` | stored in `Session::user_tz` | mutating the process environment would leak into other sessions |
| pool refresh via `SIGUSR1` | `Loader::run` refreshes periodically | a task replaces the signal |
| `editor.c` re-splits only the data line an edit touched, then slides the display-pointer array | the same, per line ([`EditorData::recount_line`](src/editor.rs)) | the first port re-wrapped the **whole document** after every character, so a pasted article cost quadratically more work and pinned one CPU core while C stayed nearly idle |
| `srand((unsigned) time(NULL))` then `rand() % 3` inside every game of 猜拳 (`chicken.c:697-698`) | [`chicken::opponent`](src/chicken.rs) mixes the same one-second seed | glibc's additive-feedback generator is a property of libc, not of this game, and a port that replayed it would be a second libm to keep in step. The seed still changes once per second and still has three answers, so two guesses inside one second are still the same hand — which is the part the player sees. |
| The chicken record is read with `fscanf` of fourteen `%d` and one `%s` (`chicken.c:111-112`) | [`Chicken::parse`](src/chicken.rs) splits the line and reads each field with [`c_atoi`](src/config.rs) | the count C tested against `15` is the count reported here, and a field C leaves unread reads as `0` — the two agree on every line `save_chicken` and `create_a_egg` wrote |
| An edit into a document that already fills the [`MAX_EDITOR_DATA_LINES`](src/paths.rs) rows loses whatever needs a row the array has not got: the touched line is cut short with a `'\n'` glued to its last kept row (`:366-387`) and, for an edit further up, the row at the very end of the array is trimmed to one cell short (`:447-458`) | same, at the cap | the first port refused the edit instead, so a full article accepted nothing more; a real edit there now silently truncates the tail exactly as C does. One case is not reproduced: deleting the last row of a full array leaves C counting one row more than the document has — a tail entry it no longer maintains, which paints blank where the port paints the row that is really there. The saved bytes agree either way. |

### 10.2 C quirks we reproduced on purpose

The most dangerous thing in a port is fixing a bug "helpfully": the display and the data change, and the web
app disagrees with us. These behaviours are therefore reproduced verbatim and pinned by tests.

| Original behaviour | Where | Why it must not be "fixed" |
| --- | --- | --- |
| `ip_mask` **truncates the string at the first masked octet**, so an address with a fifth octet loses its tail | [`ipmask.rs`](src/ipmask.rs) | the throttle's `LIKE '202.38.64.%'` depends on exactly this expression |
| The username/password **format check issues no SQL at all**, so a malformed attempt is never logged | [`login.rs`](src/login.rs) | the web unlock feature counts those rows |
| A disabled account (`p_login = 0`) still has its login row **committed before** the rejection | same | otherwise the visit counters move |
| The EULA path updates neither visit counters nor the online list | same | same reason |
| `moveto` prints a negative coordinate with its sign, and with a negative row emits a bare `\r` | [`Terminal::moveto_signed`](src/term/mod.rs#L231) | menu cursors rely on it |
| A key sequence split across packets is dropped and reported as `ESC` | [`Terminal::read_key`](src/term/mod.rs#L275) | pager key handling must feel identical |
| `S_DEFAULT = 0x3` (list + experience) and **no post bit** | [`privilege.rs`](src/privilege.rs) | making new accounts "more convenient" would diverge from the web app |
| A station that **refuses** the connection is only reported when the ten-second budget runs out, and then as `连接超时！` | [`bbsnet::connect`](src/bbsnet.rs) | C's wait loop keeps spinning on a socket that is writable but unconnected, and the keys pressed while the bar runs are spent on the connect; failing fast would change both the screen and what reaches the station |
| The profile editors have **no `SYS_server_exit` guard** in front of their `UPDATE`, where `data_file_mgr.c:201` has one: an edit the shutdown interrupts between two rounds still writes, with the text the last save left behind — the one the line count had just rejected — or with nothing at all | [`user_info_update.rs`](src/user_info_update.rs) | the column belongs to the web application as much as to the board; a port that quietly skipped the write would leave the two servers recording different things about the same shutdown |
| The editors' **ten-line limit admits nine**: `split_data_lines` is a do/while, so the newline that closes the tenth line is counted as an eleventh line of its own | [`user_info_update.rs`](src/user_info_update.rs), [`text::split_data_lines`](src/text.rs) | counting the lines the way a reader does would accept texts C refuses with `长度超过限制 (10行)`, and the same counter decides what the profile page and the board lists fit on their screens |
| The chicken's **age is a calendar subtraction that can go backwards**: `(tm_mon + 1 + 12 - month) % 12 * 31 + (tm_mday - day)` (`chicken.c:119-120`) forgets the years, so a chicken born on the 28th looked at on the 21st is `−7` — no picture on the screen, a meal worth twelve grams instead of one, and no sale — while a chicken born on this day twelve months ago reads as just hatched | [`chicken::load_chicken`](src/chicken.rs) | the number is recomputed on every open and never stored, so "fixing" the wrap would repaint every coop in the site and change what `[1]` and `[9]` do to a real record |
| The naming prompt **starts out holding `宝宝`** (`chicken.c:146`) and `get_data` counts that prefill as the beginning of the line (`screen.c:326`), so Enter alone names the chicken 宝宝 and a typed name rides on it: `宝宝` then `新鸡` is accepted as `宝宝新鸡` | [`chicken::create_a_egg`](src/chicken.rs) | the same buffer seeds the 改名字 prompt with the current name, so an editor that cleared the prefill would rename the chicken to something its owner never typed |
| A rejected name **comes back masked and is then accepted**: `check_badwords(name_tmp, '*')` masks the buffer in place and `:151-166` asks again with it still there, so one more Enter lets `宝宝***蛋` through | [`chicken::create_a_egg`](src/chicken.rs) | refusing and re-prompting with an empty line would be friendlier and would not be C; the log line and the record both keep the masked text |
| `save_chicken` opens the record `"r+"` and **does not truncate** (`chicken.c:129`), so a chicken renamed shorter keeps the tail of the longer name in the file — visible in the reference tree's own records — and `%s` stops at the first space, so nothing ever reads it back | [`chicken::save_chicken`](src/chicken.rs) | the same open mode is what makes a record the session cannot write count as absent and hatch over |
| `chictime` is **read and never assigned**: `show_chicken` (`:192-200`) thins and dirties a chicken past 200 of them, and no line in `chicken.c` sets the field, so the starved-and-dirty coop C describes cannot happen | [`chicken::show_chicken`](src/chicken.rs) | it is a documented feature of the game that the code does not deliver; delivering it would be a design change, and the field is kept so the record still parses the same fourteen numbers |

One entry left this table on 2026-11-21, and only because upstream moved first: the privilege list's binary
search, which lost a stored board whenever the boards arrived out of `sid` order, and which — on an empty list —
inspected slot 0, the global word. The port had reproduced both and pinned them. `../lbbs` repaired them in
`9dd183a`, adding `src/test_user_priv.c` as its own proof, so the port followed the reference instead of deciding
the C was wrong: [`privilege.rs`](src/privilege.rs) now carries the `left <= right` loop with the `found` flag,
and `boards_that_arrive_out_of_order_stay_reachable` and `every_insertion_order_leaves_the_list_sorted_and_findable`
pin the list as sorted and every board as findable. Nothing moved for the web application, because the C server
computes the same words the Rust server now does. That is the rule this table runs by: an entry leaves it only
when the reference has already left, never because we decided the C was wrong.

If any of these is ever repaired, it should be a coordinated change with the data and the web application —
never smuggled into a port.

### 10.3 The 1.7.12 round: what each reference fix means here

`v1.7.11..49805a5` — twenty commits, 23 files, +616/−57 lines. Going through the hunks one by one, the answer
is that **only `9dd183a` needed a functional change in Rust** — it repaired exactly the quirk §10.2 used to
reproduce, and upstream moved first — while every other one needed a check, not a change. They leak, forget to
bound, mis-count, or treat user text as a `printf` format, and this project has owned buffers, RAII guards and
bound parameters instead; the logging `49805a5` adds already has a counterpart here. What each fix *means* is
written into the C file's replacement module; this table is only the index.

| Reference commit | The problem in C | Where the port says so |
| --- | --- | --- |
| `6012565` | `char tmp[]` accumulated escape sequences with no bound; the fix stops at `sizeof(tmp) - 1` | [`term/mod.rs`](src/term/mod.rs) — the decoder consumes a `Vec<u8>`, so nothing overflows |
| `de5fa0f` | error paths leaked an `fd` and two mappings | [`cache.rs`](src/cache.rs) — `File` and `Vec` release themselves on every return |
| `a2f6c10` | `article_post.c:276`, `:1149`: the length added was `snprintf`'s *wanted* length; `article_reply` ignored `lml_render`'s negative return | [`post.rs`](src/post.rs) — footer note |
| `37dfc47` | `screen.c` walked the screen pool with `sizeof(MENU_ITEM)` and misused `menu_item_count` as the cap | [`menu.rs`](src/menu.rs) — the comment above `set.screens.len() >= MAX_MENUS` |
| `c996502` | `lml.c:482-486` copied a `LML_TAG_PARAM_BUF_LEN`-long parameter into a buffer of the same length, overwriting the caller's NUL | [`lml.rs`](src/lml.rs) — the clamp at the parameter copy |
| `a02a212`, `c6444e2` | three failure paths in `db_open` skipped `mysql_close`; `data_file_mgr.c` called `munmap` on `MAP_FAILED` | [`db.rs`](src/db.rs) (a pool has no handle to leak), [`state.rs`](src/state.rs) and [`data_file_mgr.rs`](src/data_file_mgr.rs) (there is no mapping at all) |
| `b2e764c` | `bwf.c` treated a zero-length row as an empty one; `user_list.c` read `row[3][0]` without the NULL check its neighbours have | [`bwf.rs`](src/bwf.rs) — the empty-row skip; [`db.rs`](src/db.rs#L334) — `gender` decodes into a `String` that cannot be NULL, because the column is `char(1) NOT NULL DEFAULT 'M'` |
| `51b1c7f` | two `prints(buffer)` calls let user text's `%` be read as a format | [`Terminal::print`](src/term/mod.rs) — the Rust `print` takes no format string |
| `d7f2f5d` | `section_list_loader.c:145-152` left the board loop while still holding the write lock | [`loader.rs`](src/loader.rs) — the `load_section_config` docs; no guard here can outlive a refresh |
| `0e5224a` | `editor.c` cleared one byte per pointer when clearing `p_display_lines[]`, so a recycled pool node kept 7/8 of a dirty row | [`editor.rs`](src/editor.rs) — `recount()` derives display rows from data rows, so a dirty tail cannot survive an edit |
| `336b670` | `str_process.c` wrote the width of row `line_offsets_count` before checking the bound, so a file with many lines wrote past the offset array | [`text.rs`](src/text.rs) — `split_data_lines` checks, then truncates, pinned by `wrapped_respects_line_cap` |
| `038dd6b` | the version number | this section |
| `4553b19` | `article_favor.c:145`, `:211` wrote the delete prefix but measured it with `sizeof(sql_add)` — a real typo, harmless only because both buffers are `SQL_BUFFER_LEN` | [`db.rs`](src/db.rs#L772) — the `QueryBuilder` builds the prefix directly, so there is no "wrong variable's buffer size" |
| `ebaa144` | the `long[]` offset table placed after the text in shared memory landed at `data_len + 1`, usually unaligned; the new `shm_offsets_offset()` rounds the length, the writer and the reader alike | the port never builds that layout: [`state.rs`](src/state.rs) stores file bytes and [`screen.rs`](src/screen.rs) wraps per session (those two `file_loader.c:88` citations are the moved line) |
| `37b0660` | `log.c:114-117`: `log_printf` picked a `FILE *` by level and wrote to it, so logging before `log_begin()` was a NULL dereference; it now falls back to `stderr` | [`log::begin`](src/log.rs#L276) opens both streams and aborts startup if it cannot, and `main.rs:134` installs the subscriber before any session or loader task — there is no "not open yet" state to fall back from |
| `65ce0f7` | the trie self-test shifted a negative `int64_t` left (`TEST_VAL << i`), which is undefined in C | defined in Rust, and the tables C keeps in tries are `HashMap`s here ([`board.rs`](src/board.rs), [`roster.rs`](src/roster.rs)); the reference self-test has no counterpart |
| `b7a8b16` | `editor.c:395-414`: three `memmove`s shifted display rows by `display_line_total - last_display_line - 1` directly, so a dirty count became a huge `size_t` and both ends ran off; the fix requires `display_line_total >= last_display_line + 1` and logs the surprise otherwise | [`editor.rs`](src/editor.rs) — `EditorData::insert` changes data rows and `recount()` recomputes display rows, so there is no parallel array to shift and the negative length has no shape here |
| `9dd183a` | `search_priv` used `while (left < right)` and then decided a hit by comparing `s_priv_list[left]`, while `setpriv` shifted the tail with a linear loop: an insert could land in the wrong place, the list stopped being sorted, and a stored board became unfindable | [`privilege.rs`](src/privilege.rs) — the port had reproduced this on purpose; §10.2 records the repair, and `boards_that_arrive_out_of_order_stay_reachable` and `every_insertion_order_leaves_the_list_sorted_and_findable` pin the sorted, findable list |
| `49805a5` | `load_priv` dropped privileges once the list was full and said nothing; the new `count_priv_dropped()`/`log_priv_dropped()` (`:119-138`) report each stage | [`privilege.rs`](src/privilege.rs) — `Dropped::default()` plus `dropped.report(uid, "…")` is that logging, already ported |

**The last seven were still uncommitted at the previous sync.** Two of them move citation line numbers:
`b7a8b16` adds eight lines inside `editor_data_insert`, so every `editor.c` citation after that hunk in
[`editor.rs`](src/editor.rs) is +8; `ebaa144` adds seven lines above the `split_data_lines` the file pager
cites, moving it from `file_loader.c:81` to `:88`. None of this was applied by arithmetic: each new line number
was checked against the C text the old number pointed at, which is why `editor.c:344` and `:348-349` stay put
while the ones after the inserted check move. Only `file_loader.c:88` — it sits inside the changed hunk — had to
be re-read by hand against `git show 49805a5:src/file_loader.c`. `9dd183a` and `49805a5` likewise move
`user_priv.c`: `search_priv` `:30`→`:31`, `setpriv` `:60`→`:63`, `getpriv` `:99`→`:103`, `load_priv`
`:113`→`:141`, and the stages `:130`/`:151`/`:173`/`:198`/`:233`/`:255` move to `:162`/`:183`/`:205`/`:237`/
`:279`/`:308`.

---

## 11. Rules for working on this code

- **Four-space indent, and `cargo fmt` owns the rest.** Keep the tree passing `cargo fmt --check`; a hand-tidied
  block just fights the next format run.
- Every module header must state which C file and line ranges it ports, and carry a `# Deviations from C`
  section. **A port without both gets sent back.**
- C's constants (`BBS_max_section = 200`, `USERNAME_MAX_LEN = 12`, …) belong in `paths.rs`. No magic numbers.
- Tests that do not need the database are `#[test]` / `#[tokio::test]`. Anything touching the live DB is
  `#[ignore]` + an `LBBS_REF_*` environment variable.
- Need a `Session` in an offline test? Use [`session::offline_session()`](src/session.rs):
  it builds a detached pool that never opens a connection and issues no queries.
- Watch borrow conflicts when passing `&mut Session` around inside `async fn`.
  For example [`Session::display_file`](src/session.rs#L217)
  is recursive — the pager can ask to show help, which displays a file — so its future is boxed with
  `Pin<Box<dyn Future<...>>>`; a plain `async fn` cannot have a recursive return type.
- Before adding a SQL statement, read what C does around it, **especially where it commits**.
- Output is buffered in `Terminal` and only reaches the wire on `flush` (C's `iflush`). Reading a key always
  flushes first, so a prompt can never be swallowed.

### 11.1 The handful of types worth learning first

| Type | File | In one line |
| --- | --- | --- |
| `AppState` | `state.rs` | everything shared by all connections (config, DB pool, section/article/user pools) |
| `Session` | `session.rs` | one connection: uid, username, privileges, peer address, current action, timestamps |
| `Terminal` | `term/mod.rs` | that connection's buffered input and output |
| `Config` / `DbConfig` | `config.rs` | the parsed `bbsd.conf` |
| `Db` | `db.rs` | the sqlx pool; `Clone` is cheap because it wraps an `Arc` |
| `UserPriv` | `privilege.rs` | a person's level, global privilege word, and per-board words |
| `MenuSet` / `MenuItem` / `Action` | `menu.rs` | a menu's items and which function a key press maps to |
| `Board` | `board.rs` | the in-memory copy of sections, articles and users |

---

## 12. FAQ and common traps

**Q: Why chase C's every detail? Wouldn't a clean rewrite be better?**
A: Because the database and files are shared **live** with the web application. A post that displays one column
off, a body that truncates differently, or privileges that suddenly widen — all of those are failed ports.
The original behaviour is the closest thing this project has to a specification.

**Q: Why is the interface still in Chinese?**
A: The text comes from `data/*.txt` and from string literals in the C source. There is no localisation layer in
the original, and the web side shows the same strings, so translating during a port would diverge from the data.

**Q: My test fails with "this functionality requires a Tokio context".**
A: `Db::detached(...)` can only be created inside a runtime. Make that test `#[tokio::test]`.

**Q: Wide characters render misaligned.**
A: C has a fixed-width (F) / variable-width (V) choice that changes the width arithmetic. Go through
`Session`/`Terminal`'s `fixed_width` instead of calling `text::display_width` directly.

**Q: The characters I type as my password appear on screen.**
A: They should not. A session opens by sending `IAC WILL ECHO, IAC WILL SGA`
([`screen::set_input_echo`](src/screen.rs)), which is what tells a
telnet client this side does the echoing; the `NOECHO` prompt then prints one `*` per character and erases them.
If the clear text comes back, the six negotiation bytes are missing from what the transport wrote — the mask
alone cannot hide what the client echoes itself.

**Q: I want to try this next to a web installation.**
A: Point `conf/bbsd.conf` at a **backup or test database**, and keep `db_password` in that file only (never
committed, never printed).

**Q: Where is an article's body?**
A: `var/articles/<CID>`, indexed by `article.CID`. C opened it with `fopen`; the port opens it the same way
([`cache::load`](src/cache.rs), called from
[`browse`](src/browse.rs)) and writes it from the loader
([`cache::generate`](src/cache.rs), called from
[`loader`](src/loader.rs)). A live guest read of a real post is covered
by section 6.2.

**Q: My script drives a session and the keys do nothing.**
A: `display_data` scrolls **one line** per `Enter`/`↓` and a whole page only per space/`PgDn`
(`screen.c:772-814`), and a key it does not know prints nothing at all (`screen.c:837-839`) — so a fixed
`sleep`-and-send script reads a 40-line welcome page as a hang. Wait for the text you expect before sending the
next key, and page with space.

---

## 13. Plan from here

| Step | Work | Done when |
| --- | --- | --- |
| done | listeners: raw TCP + russh SSH, connection limits, the real `main.rs`, EULA hook-in | `cargo run -- --foreground` accepts a connection |
| done | live smoke test against the board database: guest login, menus, SSH auth, reload, log reopen | the session behaves like the C one on the same screens |
| done | browse features end to end: `list_section`, the article list, reading a post, `LOCATE_ARTICLE` | a menu key opens a board, and a live guest session reads a real post |
| done | write path: new/reply/edit/delete, the editor, index updates, cache generation, badwords on submit | unit-tested, and a live guest is refused before any SQL runs |
| done | the three `utils/bin` PHP generators as binaries: `gen_top`, `gen_section_menu`, `gen_ex_list` (§7.4) | their files match the PHP's byte for byte against the live database (§6.2) |
| next | one real post end to end against a **test** database, then the same reply/modify/delete | the web site shows what the Rust server wrote, with the header and cache files C would have produced |
| done | the menu actions left: the two profile editors (§6.1 item 28), and finding out what `M_FAVOR_SECTION` is — a menu filter, already ported | the top menu has no key that only logs |
| done | the two `libgame.so` entries `@RunMBEM` names: 电子小鸡 and the 糖糖 it spends (§6.1 item 29) | the chicken page, its nine actions and its death run against a record file, with no database write outside a money transaction |
| last | a side-by-side comparison with C over the whole phase-1 surface | the same actions produce the same screen |

---

## 14. Further reading

- **The original C source**: `../lbbs/src/` and `../lbbs/include/` — read-only.
- [reference/schema.sql](reference/schema.sql) — the full database schema. **Start here when a table is confusing.**
- [reference/sql_inventory.md](reference/sql_inventory.md) — every statement the C server issues. **Start here when hunting for a missing port.**
- [tests/fixtures/](tests/fixtures) — sample article files and golden output captured from the C build.
- The header comment of each module — what it ports and how it deviates. **That is the detailed documentation.**

## 15. License

GPL-3.0-or-later, the same as the original LBBS.
Original author: Leaflet &lt;leaflet@leafok.com&gt; (Copyright (C) 2004-2026).
