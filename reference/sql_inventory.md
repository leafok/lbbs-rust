# SQL inventory (phase 1) — verbatim from the C server

Source of truth: `../lbbs/src/*.c`. Line numbers are C-side.
Rule for the port: bind parameters with the **pre-escape** value, because
`mysql_real_escape_string` + MySQL's unescape cancel out. Exceptions noted inline.

## Connection bootstrap (`database.c`)

- `mysql_set_character_set(db, "utf8")` → `SET NAMES utf8`, which on MySQL 8.x means **utf8mb3**,
  while the tables are `utf8mb4_0900_ai_ci`. Reproduce (charset choice affects collation of
  `username`/`sname` comparisons and rejects 4-byte sequences).
- `SET time_zone = '<db_timezone>'` (`database.c:102`), sent on every connection. All
  `NOW()` / `UNIX_TIMESTAMP()` / `SUBDATE()` math depends on it; server default is `SYSTEM`.
- TLS: `SSL_MODE_VERIFY_CA` with `conf/ca_cert.pem` if that file exists, else `PREFERRED`.
- One connection per operation in C (no pool). `SQL_BUFFER_LEN = 10240`.

## Loader / startup bulk load (`section_list_loader.c`)

```sql
-- :62  load_section_config_from_db
SELECT section_config.SID, sname, section_config.title, section_config.CID,
       read_user_level, write_user_level,
       section_config.enable * section_class.enable AS enable,
       UNIX_TIMESTAMP(ex_menu_tm) AS ex_menu_tm
FROM section_config INNER JOIN section_class ON section_config.CID = section_class.CID
ORDER BY section_config.SID;

-- :88  masters of one section (LIMIT 3, joined into "<name> " per row, max 3)
SELECT username FROM section_master INNER JOIN user_list ON section_master.UID = user_list.UID
WHERE SID = ? AND section_master.enable AND (NOW() BETWEEN begin_dt AND end_dt)
ORDER BY major DESC, begin_dt ASC LIMIT 3;

-- :216 keyset pagination, 1000 at a time; AIDs are sparse (live db starts at 163)
SELECT bbs.AID, TID, SID, bbs.CID, UID, visible, excerption, ontop, `lock`, transship,
       username, nickname, title, UNIX_TIMESTAMP(sub_dt) AS sub_dt, sub_ip, bbs_content.content
FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID
WHERE bbs.AID >= ? ORDER BY bbs.AID LIMIT ?;

-- :375 watermark
SELECT MID FROM bbs_article_op ORDER BY MID DESC LIMIT 1;

-- :426 op replay; 'A' is excluded because new articles arrive through :216
SELECT MID, AID, type FROM bbs_article_op WHERE MID > ? AND type NOT IN ('A') ORDER BY MID LIMIT ?;

-- :516 op 'M' (modify): new CID is what invalidates the render cache
SELECT bbs.CID, sub_ip, bbs_content.content
FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID WHERE bbs.AID = ?;

-- :559 op 'T' (move between sections)
SELECT SID FROM bbs WHERE AID = ?;
```

Op-letter table as applied by the loader (`:483-636`): `D,X`→visible=0 · `S`→visible=1 ·
`L`→lock=1 · `U`→lock=0 · `M`→re-fetch CID/content + excerption=0 · `T`→move topic ·
`E`→excerption=1 · `O`→excerption=0 · `F`→ontop=1 · `V`→ontop=0 · `Z`→transship=1 ·
`A`→never reaches here (filtered) · anything else, incl. `N`/`R` seen in the live db,→ ignored.
If the target AID is not loaded yet the watermark does **not** advance, so the op replays next tick.

## User list / online (`user_list.c`)

```sql
-- :168  roster, ORDER BY username (load-bearing: display order + uid-index rebuild check)
SELECT user_list.UID AS UID, username, nickname, gender, gender_pub, life, exp, visit_count,
       UNIX_TIMESTAMP(signup_dt), UNIX_TIMESTAMP(last_login_dt), UNIX_TIMESTAMP(last_logout_dt),
       UNIX_TIMESTAMP(birthday), `introduction`
FROM user_list INNER JOIN user_pubinfo ON user_list.UID = user_pubinfo.UID
               INNER JOIN user_reginfo ON user_list.UID = user_reginfo.UID
WHERE enable ORDER BY username;

-- :273  online list, 900s window, most-recently-active first
SELECT SID, UID, ip, current_action, UNIX_TIMESTAMP(login_tm), UNIX_TIMESTAMP(last_tm)
FROM user_online WHERE last_tm >= SUBDATE(NOW(), INTERVAL ? SECOND) ORDER BY last_tm DESC;

-- :382  "login count" is the max auto-increment, not COUNT(*)
SELECT ID FROM user_login_log ORDER BY ID DESC LIMIT 1;
```
`gender` is read as `row[3][0]` (a `char(1)`), not as an int. A user without a `user_reginfo`
row is absent from the roster but can still log in.

## Login (`login.c`, `check_user`)

Transaction idiom everywhere in this codebase is `SET autocommit=0` / `BEGIN` / … / `COMMIT`;
there is no explicit `ROLLBACK` — closing the connection rolls back.

```sql
SET autocommit=0;                              -- :164
BEGIN;                                         -- :171
-- :182  throttle by /24 prefix. The pattern is an IP with the last octet replaced by '%'
--        (e.g. 10.0.0.5 -> '10.0.0%'). If the address has < 3 dots there is NO wildcard.
--        Bind the masked string literally into LIKE; do not append '%'.
SELECT COUNT(*) AS err_count FROM user_err_login_log
WHERE login_dt >= SUBDATE(NOW(), INTERVAL ? MINUTE) AND login_ip LIKE ?;

-- :213  per-account lockout since last successful login
SELECT COUNT(*) AS err_count FROM user_err_login_log
LEFT JOIN user_list ON user_err_login_log.username = user_list.username
LEFT JOIN user_pubinfo ON user_list.UID = user_pubinfo.UID
WHERE user_err_login_log.username = ?
  AND (user_err_login_log.login_dt >= user_pubinfo.last_login_dt OR user_pubinfo.last_login_dt IS NULL);

-- :245  password hashed by the server; bind the RAW password
SELECT UID, username, p_login FROM user_list
WHERE username = ? AND password = SHA2(?, 256) AND enable;

INSERT INTO user_login_log(UID, login_dt, login_ip) VALUES(?, NOW(), ?);   -- :272
COMMIT;                                                                   -- :284
-- failure branch, plaintext password is stored in user_err_login_log:
INSERT INTO user_err_login_log(username, password, login_dt, login_ip) VALUES(?, ?, NOW(), ?);  -- :303
COMMIT;                                                                   -- :315
SET autocommit=1;                                                         -- :328
UPDATE user_pubinfo SET visit_count = visit_count + 1, last_login_dt = NOW() WHERE UID = ?;  -- :369
```

Note: the `p_login == 0` rejection happens **after** the `COMMIT` of the login-log insert, so a
login by a `p_login=0` account is still recorded. Validate before touching the DB:
username `^[A-Za-z][A-Za-z0-9_]*$` len 3..12, password `^[A-Za-z0-9]+$` len 5..12 (`:129-150`).
Thresholds: 10 failures per /24 per 10 minutes, 3 failures per account.

```sql
-- :421  load_user_info  (life in {333,365,666,999} = immortal, else now-last_login > 86400*life)
SELECT life, UNIX_TIMESTAMP(last_login_dt), user_timezone, exp, nickname FROM user_pubinfo WHERE UID = ?;
```

## Presence / online bookkeeping (`login.c:523-631`)

```sql
DELETE FROM user_online WHERE SID = 'Telnet_Process_<id>';                                   -- :558
INSERT INTO user_online(SID, UID, ip, current_action, login_tm, last_tm)
  VALUES('Telnet_Process_<id>', ?, ?, 'LOGIN', NOW(), NOW());                               -- :541
INSERT INTO visit_log(dt, IP) VALUES(NOW(), ?);                                              -- :527
UPDATE user_online SET current_action = ?, last_tm = NOW() WHERE SID = 'Telnet_Process_<id>';-- :617
-- logout exp: +1 per 300s online, capped 288, computed server-side
UPDATE user_pubinfo SET exp = exp + FLOOR(LEAST(TIMESTAMPDIFF(SECOND,
    GREATEST(last_login_dt, IF(last_logout_dt IS NULL, last_login_dt, last_logout_dt)), NOW())
    / 60 / 5, 12 * 24)), last_logout_dt = NOW() WHERE UID = ?;                               -- :576
```

**Port adaptation:** C uses the forked child's PID as the session key. A single-process Rust
server must derive a unique `Sid` per session while keeping the `Telnet_Process_` prefix that the
web app and any operator tooling expect, within `varchar(32)`.

## Permissions (`user_priv.c:113-280`) — order-dependent, accumulate with `|=` and `&= ~`

```sql
SELECT p_post, p_msg FROM user_list WHERE UID = ? AND verified;                     -- :130  S_POST / S_MSG
SELECT major FROM admin_config WHERE UID = ? AND enable AND (NOW() BETWEEN begin_dt AND end_dt);  -- :151  first row only
SELECT section_master.SID, major FROM section_master INNER JOIN section_config ON section_master.SID = section_config.SID
WHERE UID = ? AND section_master.enable AND section_config.enable AND (NOW() BETWEEN begin_dt AND end_dt);  -- :173  all rows
SELECT SID, exp_get, read_user_level, write_user_level FROM section_config
  INNER JOIN section_class ON section_config.CID = section_class.CID
WHERE section_config.enable AND section_class.enable ORDER BY SID;                  -- :198  level gates
SELECT SID FROM ban_user_list WHERE UID = ? AND enable AND (NOW() BETWEEN ban_dt AND unban_dt);  -- :233  clears S_POST
SELECT SID FROM section_favorite WHERE UID = ?;                                     -- :255  is_favor flag
```

Start: `level = if uid==0 {P_GUEST} else {P_USER}`, `g_priv = S_DEFAULT(0x3)`.
`major` truthy → `P_ADMIN_M|S_ALL`, falsy → `P_ADMIN_S|S_ADMIN`; section master truthy →
`P_MAN_M|S_MAN_M`, falsy → `P_MAN_S|S_MAN_S`.
`read/write_user_level` are compared **numerically** (`level < value` clears `S_LIST`/`S_POST`)
against a bitmask `level` — that mixing is intentional in the reference, keep it.
`getpriv` for an unknown sid falls back to `g_priv`.

## Post / reply / modify / delete / digest

Circular link maintained by hand — content row first with `AID=0`, then the article, then back-fill:

```sql
INSERT INTO bbs_content(AID, content) values(0, ?);                       -- :328 / :1190  → cid = LAST_INSERT_ID()
INSERT INTO bbs(SID, TID, UID, username, nickname, title, CID, transship, sub_dt, sub_ip,
                reply_note, exp, last_reply_dt, icon, length)
VALUES(?, ?, ?, ?, ?, ?, ?, ?, NOW(), ?, ?, ?, NOW(), 1, ?);              -- :347 (TID=0) / :1209 (reply)
UPDATE bbs_content SET AID = ? WHERE CID = ?;                             -- :366 / :1244
UPDATE user_pubinfo SET exp = exp + ? WHERE UID = ?;                      -- +15/+5 post, +3 reply, if S_GETEXP
INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip)VALUES(?, ?, 'A', NOW(), ?);   -- :392 / :1314
```
`length` is the **display-width** count of the content (`str_length(content, 1)`), not bytes.
`exp` written into `bbs` is a snapshot of the author's exp at post time.

```sql
-- pre-tx reads on the reply path
SELECT `lock` FROM bbs WHERE AID = ?;                    -- :809  AID = (tid==0 ? aid : tid); nonzero → refuse
SELECT bbs_content.CID, bbs_content.content FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID WHERE bbs.AID = ?;  -- :849
SELECT sign_? AS sign FROM user_pubinfo WHERE UID = ?;   -- :258 / :1120  column name from {1,2,3}, whitelist it
UPDATE bbs SET reply_count = reply_count + 1, last_reply_dt = NOW(), last_reply_UID=?,
    last_reply_username = ?, last_reply_nickname = ? WHERE AID = ?;      -- :1228
SELECT DISTINCT UID FROM bbs WHERE (AID = ? OR AID = ?) AND visible AND reply_note AND UID <> ?;  -- :1255
INSERT INTO bbs_msg(fromUID, toUID, content, send_dt, send_ip) VALUES(?, ?, ?, NOW(), ?);          -- :1283
```
The `DISTINCT UID` select must be fully buffered before the inserts run on the same connection.
`bbs_msg.fromUID` is `BBS_sys_id`. Known C quirk: the notification text embeds the
*MySQL-escaped* title and is then escaped again, so a `"` is stored as `\"`. Reproduced on
purpose in `post.rs` to keep the stored bytes identical.

```sql
-- modify (:665 onwards) — inserts with the real AID, no 0-then-link, orphans the old content row
INSERT INTO bbs_content(AID, content) values(?, ?);
UPDATE bbs SET CID = ?, length = ?, reply_note = ?, excerption = 0 WHERE AID = ?;
INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip)VALUES(?, ?, 'M', NOW(), ?);

-- delete (:103 onwards)
SELECT visible, excerption FROM bbs WHERE AID = ? FOR UPDATE;
UPDATE bbs SET visible = 0, reply_count = 0, m_del = ? WHERE (AID = ? OR TID = ?) AND visible;
UPDATE user_pubinfo SET exp = exp + ? WHERE UID = ?;   -- self -20/-5, by others -50/-15; NO S_GETEXP guard
INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip)VALUES(?, ?, ?, NOW(), ?);   -- 'D' self / 'X' moderator
UPDATE bbs SET reply_count = reply_count - 1 WHERE AID = ?;   -- only when the deleted article was a reply

-- digest 精华 (:82 onwards)
SELECT UID, TID, SID, transship, excerption FROM bbs WHERE AID = ? AND visible FOR UPDATE;
UPDATE bbs SET excerption = ? WHERE AID = ?;
UPDATE bbs SET gen_ex = 0, static = 0 WHERE AID = ? OR TID = ?;               -- unset only
DELETE FROM ex_file WHERE AID = ?;                                            -- unset and tid==0
UPDATE upload_file SET UID = ? WHERE ref_AID = ? AND deleted = 0;
UPDATE user_pubinfo SET exp = exp + ? WHERE UID = ?;   -- ±50 topic / ±20 transship topic / ±10 reply, to the author
INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip) VALUES(?, ?, ?, NOW(), ?);   -- 'E' / 'O'
```

## Favorites and view log (baseline + increment, flushed at logout)

```sql
SELECT AID FROM article_favorite WHERE UID = ? ORDER BY AID;    -- :56   ascending order is required by binary search
SELECT AID FROM view_article_log WHERE UID = ? ORDER BY AID;    -- :56
INSERT IGNORE INTO article_favorite(AID, UID) VALUES (?, ?), ...;         -- batches of 100
DELETE FROM article_favorite WHERE UID = ? AND AID IN (?, ...);
INSERT IGNORE INTO view_article_log(AID, UID, dt) VALUES (?, ?, NOW()), ...;
```
Guest (`uid == 0`) issues no SQL for either. Read once at login, written at logout or when the
1000-entry increment fills.

## Data-state facts the port must tolerate

- `bbs.AID` starts at 163 → sparse AIDs; keyset pagination must not assume density.
- 327 orphaned `bbs_content` rows (modify leftovers) and 12 with `AID = 0` (aborted posts).
- `bbs.title` is `varchar(80)` while the C title buffer is 160 → over-long titles are truncated
  by the server; identical behaviour as long as the port does not change `sql_mode`.
- `bbs.CID` is `mediumint` while `bbs_content.CID` is `int`; `upload_file.ref_AID` is `mediumint`.
- `bbs.view_count`, `section_config.comment`, `ex_dir`, `email`, `friend_list`, `user_score*`
  and others are never touched by the C server at all.
- Op letters `S L U T F V Z N R` are produced by the companion web app, never by `bbsd`.

## Verified against the live server (MySQL 8.4.13, phase 1)

Probed with a scratch `libmysqlclient` program repeating `db_open()` verbatim, and with
`src/db.rs`'s ignored test (`LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`).

- After `mysql_set_character_set(db, "utf8")`: `character_set_client =
  character_set_results = utf8mb3`, `collation_connection = utf8mb3_general_ci`. sqlx's
  `charset("utf8")` without a collation reproduces all three.
- `libmysqlclient` does **not** negotiate `CLIENT_IGNORE_SPACE`, so its session `sql_mode`
  equals the global one: `ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,
  ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION`. sqlx always advertises that capability,
  which prefixes `IGNORE_SPACE` to the session mode. Inert here — no statement puts a space
  between a built-in function and its `(` — so it is documented rather than worked around.
  `PIPES_AS_CONCAT` / `NO_ENGINE_SUBSTITUTION` are switched off so nothing else is appended.
- Result-column types the port must decode accordingly:
  `section_config.enable * section_class.enable` → **`int unsigned`** (sqlx rejects that for
  `i64`, accepts it for `bool`, and C only tests it for truth after `atoi`),
  `UNIX_TIMESTAMP(datetime)` → `bigint` signed and NULL-able, `COUNT(*)` → `bigint` signed.
  Every flag column is `tinyint unsigned`, which sqlx will only hand to `bool`.
- `conf/bbsd.conf` has **CRLF** line endings and `key value` (no `=`); keys match case
  insensitively on a whole token, and a third token is accepted only when it starts with `#`
  (`init.c:64-118`, `CONF_DELIM_WITH_SPACE = " \t\r\n"`). `db_timezone` is `+08:00`.
- `strncpy(…, sizeof(DB_*) - 1)` bounds (`database.h:18-22`): host 256,
  username/password/database/timezone 50.
- `master_list` is `char[(12 + 1) * 3 + 1]` (`section_list_loader.c:49`); each row appends the
  name then a space with `strncat(…, sizeof - 1 - strnlen(…, …))`. Three 12-byte names plus
  their spaces exactly fill the 39 usable bytes, and `user_list.username` is `varchar(20)`, so
  a longer name really is cut mid-word and loses its trailing space.
- Row counts of the copy the port is tested against: 60 users (inner join of all three user
  tables), 28222 `bbs`, 28548 `bbs_content` (326 orphans), 1324 `bbs_article_op`, 48 sections,
  150 `section_master`, 33 live `user_online`, `MIN(bbs.AID) = 163`.
