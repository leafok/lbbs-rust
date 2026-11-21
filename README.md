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
| Version | 1.7.12 — revision `b7a8b16` on `main` (the release has no `v1.7.12` tag yet) |
| Language | C (about 29,850 lines across `src/*.c` and `include/*.h`) |
| License | GPL-3.0-or-later |
| Author | Leaflet &lt;leaflet@leafok.com&gt; |
| Reference copy on this machine | `~/Development/lbbs` — read-only for us; **we never modify it** |

LBBS is a **bulletin board server you connect to from a text terminal**. Users connect over telnet or SSH,
press menu keys, read boards, and write posts. There is no mouse and no images: everything is drawn with text
plus **terminal control sequences** (strings like `ESC[...H` that set colours and cursor position).

The other important fact: **LBBS does not own its data.** Post bodies and attachments live in files on disk,
and the index of users, boards and posts lives in a **MySQL database**. The site's **web bulletin board** reads
that same database. So LBBS is really a *terminal front-end to that web board*, and that gives us a
hard constraint: **we must not change the schema.**

The reference 1.7.12 is `v1.7.11..b7a8b16`: eighteen commits, 19 files, +106/−40 lines, and the working tree
matches `b7a8b16`. Every `file.c:NNN` citation in the Rust sources is anchored to that one revision, so
`git -C ~/Development/lbbs show b7a8b16:src/<file>` prints exactly the text the comment describes. [Section
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
| Phase 2 | the digest tree, statistics, the user-profile editor, GBK polish, SSH window size | not started |

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
| [src/paths.rs](src/paths.rs) | 113 | `include/bbs.h` etc. | screen size, field lengths, path names | done |
| [src/daemon.rs](src/daemon.rs) | 415 | `src/main.c`, `src/daemon.c` | command line, `--help`/`--version`, `fork` into background | done |
| [src/config.rs](src/config.rs) | 384 | `src/init.c` | reads `conf/bbsd.conf` | done |
| [src/log.rs](src/log.rs) | 497 | `src/log.c` | the two append-mode log files, `log_restart()` | done |
| [src/text.rs](src/text.rs) | 357 | `src/str_process.c` | display width (wide chars = 2 columns) and line wrapping | done |
| [src/term/mod.rs](src/term/mod.rs) | 584 | `src/io.c` | byte pipes, GBK transcoding, `prints`/`iflush`, `igetch` | done |
| [src/term/key.rs](src/term/key.rs) | 504 | `src/io.c` | turns ESC sequences into key codes | done |
| [src/screen.rs](src/screen.rs) | 1116 | `src/screen.c` | clearing, the echo handoff `set_input_echo`, line input `str_input`, pager `display_data` | done |
| [src/db.rs](src/db.rs) | 1368 | `src/database.c` etc. | MySQL queries through sqlx | done |
| [src/board.rs](src/board.rs) | 1470 | `src/section_list_loader.c` | in-memory pools of sections, articles, users | done |
| [src/browse.rs](src/browse.rs) | 2890 | `src/section_list_display.c`, `src/section_list_loader.c` | the article list, its keys, reading a post, the saved reading position | done |
| [src/editor.rs](src/editor.rs) | 1812 | `src/editor.c` | the full-screen editor: rows, window, status row, key handling | done |
| [src/post.rs](src/post.rs) | 1488 | `src/article_post.c`, `src/article_del.c`, `src/article_op.c` | post, reply, modify, delete, the 精华 toggle, and the transaction each writes in | done |
| [src/viewlog.rs](src/viewlog.rs) | 366 | `src/article_view_log.c` | which posts this user has read | done |
| [src/favorlog.rs](src/favorlog.rs) | 446 | `src/article_favor.c` | which posts this user starred | done |
| [src/favorlist.rs](src/favorlist.rs) | 768 | `src/article_favor_display.c`, `src/article_favor.c` (the page query), `src/menu_proc.c` (the `FAVOR_TOPIC` item) | the 我收藏的主题 list: two name columns, the star removed in place, and the board each row opens | done |
| [src/roster.rs](src/roster.rs) | 661 | `src/user_list_loader.c` | the online-user roster and the article counters | done |
| [src/userinfo.rs](src/userinfo.rs) | 923 | `src/user_info_display.c`, `src/user_list_display.c` (the search), `src/bbs.c` (the level and sign tables) | one user's profile page, and the name search that reaches it | done |
| [src/userlist.rs](src/userlist.rs) | 1024 | `src/user_list_display.c`, `src/menu_proc.c` (the two roster items) | the 线上使用者 and 已注册用户 pages: titles, columns, paging, and the keys between them | done |
| [src/loader.rs](src/loader.rs) | 1406 | `src/section_list_loader.c` | the incremental refresh task, and the article cache files it writes | done |
| [src/state.rs](src/state.rs) | 413 | the shared-memory segments | what the loader publishes and a session reads | done |
| [src/cache.rs](src/cache.rs) | 554 | `src/article_cache.c` | render and load `var/articles/<CID>` with its line index | done |
| [src/menu.rs](src/menu.rs) | 2351 | `src/menu.c` | `menu.conf` parsing and navigation | parsing/navigation done, feature bodies not |
| [src/lml.rs](src/lml.rs) | 689 | `src/lml.c` | LML markup rendering | done |
| [src/bwf.rs](src/bwf.rs) | 241 | `src/trie_dict.c` etc. | badword matching and masking | done |
| [src/ipmask.rs](src/ipmask.rs) | 109 | `src/ip_mask.c` | address masking (`202.38.64.x`) | done |
| [src/privilege.rs](src/privilege.rs) | 385 | `src/user_priv.c` | privilege words and levels | done |
| [src/session.rs](src/session.rs) | 613 | the `BBS_*` globals | one connection's state + online bookkeeping | done |
| [src/login.rs](src/login.rs) | 580 | `src/login.c` | login, authentication, user-agreement (EULA) | done |
| [src/bbsnet.rs](src/bbsnet.rs) | 1767 | `src/bbs_net.c`, `src/menu_proc.c` (the `@mod:` dispatcher) | the station table, the shuttle grid, the outbound connect and the two-way relay | done |
| [src/bbs.rs](src/bbs.rs) | 761 | `src/bbs_main.c`, `src/bbs_cmd.c` | the session's own order of events, echo handoff included, `bbs_center` | prompts, menu loop and the browse commands done; a few menu actions still log and return |
| [src/net.rs](src/net.rs) | 359 | `src/net_server.c` | accept, connection limits, graceful drain | done |
| [src/ssh.rs](src/ssh.rs) | 493 | `src/net_server.c` (the libssh half) | host keys, password auth, the session channel | done |
| [src/main.rs](src/main.rs) | 542 | `src/main.c`, `src/net_server.c` | startup order, signal handling, `SIGHUP` reload, shutdown | done |
| [src/batch.rs](src/batch.rs) | 374 | `utils/lib/db_open.inc.php` + the lines the three scripts repeat | one generator's life: arguments, server root, `conf/bbsd.conf`, database, the PHP's own stdout text and exit bytes | done |
| [src/section_list.rs](src/section_list.rs) | 149 | `utils/lib/section_list.inc.php` | the class/board hierarchy the generators walk, with the caller's per-board values | done |
| [src/gen_top.rs](src/gen_top.rs) | 305 | `utils/bin/gen_top.php` | `var/bbs_top.txt` and `var/bbs_top_menu.conf`: the ten hottest posts of the week | done |
| [src/gen_section_menu.rs](src/gen_section_menu.rs) | 681 | `utils/bin/gen_section_menu.php` | `var/menu_merged.conf`: `conf/menu.conf` plus the six generated menu blocks | done |
| [src/gen_ex_list.rs](src/gen_ex_list.rs) | 394 | `utils/bin/gen_ex_list.php` | one `var/gen_ex/<SID>` digest tree per board the daemon flagged | done |
| [src/bin/gen_top.rs](src/bin/gen_top.rs), [gen_section_menu.rs](src/bin/gen_section_menu.rs), [gen_ex_list.rs](src/bin/gen_ex_list.rs) | 30 | the `php utils/bin/*.php` cron lines | three binaries that call the three generators and return the script's exit byte | done |

30 428 lines in total, the tests included (they live in the modules they test). There are 452 tests: 446 in the
library and 6 in the binary. 438 of them need nothing but the checked-out tree; the other 14 are ignored because
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
4. **Character sets** — UTF-8 and GBK both work; a GBK character split across packets is rejoined correctly.
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

1. **The screens behind the write path**: the digest/excerpt tree, the user-profile editor, statistics and
   `display_article_meta`. Those keys log `... is not implemented` rather than pretending to work.
2. **Top-level menu actions** that resolve to one of those handlers: `bbs_do_choice` logs and returns, so the
   top menu still has keys that only log. `LIST_ONLINE_USER`, `LIST_USER`, `RunMBEM` and `FAVOR_TOPIC` are wired,
   and with the first two the profile page and the name search they lead to; `EDIT_INTRO`, `EDIT_SIGN`, `DATAMGR`,
   `VIEW_EX_ARTICLE`, `LIST_EX_SECTION` and `M_FAVOR_SECTION` still fall through to the log line.
3. **Phase 2 polish**: GBK corner cases, the SSH window size (deliberately ignored, see the header of
   `src/ssh.rs`), and a side-by-side comparison with C over the whole phase-1 surface.

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
cargo test             # 438 tests that need no database (14 more are opt-in, see 7.2)
```

A healthy run looks like this:

```
test result: ok. 432 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out
...
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

> **This project is developed without `cargo fmt` installed.** Formatting (4-space indent, naming) is done by
> hand, so read your own diff instead of relying on a tool to tidy it.

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

Pointing the same three variables at the C tree (`~/Development/lbbs/...`) works too and is the stricter test,
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
files for logrotate and `SIGTERM` drains the sessions before exiting.

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
| An edit into a document that already fills the [`MAX_EDITOR_DATA_LINES`](src/paths.rs) rows loses whatever needs a row the array has not got: the touched line is cut short with a `'\n'` glued to its last kept row (`:366-387`) and, for an edit further up, the row at the very end of the array is trimmed to one cell short (`:447-458`) | same, at the cap | the first port refused the edit instead, so a full article accepted nothing more; a real edit there now silently truncates the tail exactly as C does. One case is not reproduced: deleting the last row of a full array leaves C counting one row more than the document has — a tail entry it no longer maintains, which paints blank where the port paints the row that is really there. The saved bytes agree either way. |

### 10.2 C quirks we reproduced on purpose

The most dangerous thing in a port is fixing a bug "helpfully": the display and the data change, and the web
app disagrees with us. These behaviours are therefore reproduced verbatim and pinned by tests.

| Original behaviour | Where | Why it must not be "fixed" |
| --- | --- | --- |
| The privilege list's binary search is broken: depending on the order boards were added, a stored board can become **unfindable** | [`privilege.rs`](src/privilege.rs) | repairing it silently changes what a person is allowed to do |
| With an empty list, slot 0 (section id 0 = the global word) is inspected | same file | same reason |
| `ip_mask` **truncates the string at the first masked octet**, so an address with a fifth octet loses its tail | [`ipmask.rs`](src/ipmask.rs) | the throttle's `LIKE '202.38.64.%'` depends on exactly this expression |
| The username/password **format check issues no SQL at all**, so a malformed attempt is never logged | [`login.rs`](src/login.rs) | the web unlock feature counts those rows |
| A disabled account (`p_login = 0`) still has its login row **committed before** the rejection | same | otherwise the visit counters move |
| The EULA path updates neither visit counters nor the online list | same | same reason |
| `moveto` prints a negative coordinate with its sign, and with a negative row emits a bare `\r` | [`Terminal::moveto_signed`](src/term/mod.rs#L231) | menu cursors rely on it |
| A key sequence split across packets is dropped and reported as `ESC` | [`Terminal::read_key`](src/term/mod.rs#L275) | pager key handling must feel identical |
| `S_DEFAULT = 0x3` (list + experience) and **no post bit** | [`privilege.rs`](src/privilege.rs) | making new accounts "more convenient" would diverge from the web app |
| A station that **refuses** the connection is only reported when the ten-second budget runs out, and then as `连接超时！` | [`bbsnet::connect`](src/bbsnet.rs) | C's wait loop keeps spinning on a socket that is writable but unconnected, and the keys pressed while the bar runs are spent on the connect; failing fast would change both the screen and what reaches the station |

If any of these is ever repaired, it should be a coordinated change with the data and the web application —
never smuggled into a port.

---

## 11. Rules for working on this code

- **Four-space indent.** We do not rely on `cargo fmt` (it is not installed here).
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
| then | the menu actions left: the profile editor items, `DATAMGR`, the digest tree's two filters | the top menu has no key that only logs |
| last | a side-by-side comparison with C over the whole phase-1 surface | the same actions produce the same screen |

---

## 14. Further reading

- **The original C source**: `~/Development/lbbs/src/` and `~/Development/lbbs/include/` — read-only.
- [reference/schema.sql](reference/schema.sql) — the full database schema. **Start here when a table is confusing.**
- [reference/sql_inventory.md](reference/sql_inventory.md) — every statement the C server issues. **Start here when hunting for a missing port.**
- [tests/fixtures/](tests/fixtures) — sample article files and golden output captured from the C build.
- The header comment of each module — what it ports and how it deviates. **That is the detailed documentation.**

## 15. License

GPL-3.0-or-later, the same as the original LBBS.
Original author: Leaflet &lt;leaflet@leafok.com&gt; (Copyright (C) 2004-2026).
