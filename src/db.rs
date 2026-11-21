//! MySQL access: the connection bootstrap of `src/database.c` plus the statements the
//! server issues, transcribed from `reference/sql_inventory.md`.
//!
//! Deliberate differences from the C original:
//!
//! * Values that were spliced into the SQL after `mysql_real_escape_string` are bound as
//!   parameters instead. The escape and MySQL's unescaping cancelled out, so the stored
//!   bytes are identical. The exceptions are the `LIKE` patterns and the `sign_N` column
//!   name, which cannot be bound and are built from validated input.
//! * The C server opened one connection per forked session and bracketed multi-statement
//!   work with `SET autocommit=0` / `BEGIN` / `COMMIT`. Here a small pool is shared by all
//!   tasks, and those blocks become a [`sqlx::Transaction`] passed to the `_tx` helpers.
//! * Column aliases were added where the original selected a bare `UNIX_TIMESTAMP(...)`;
//!   the expressions themselves are unchanged.
//!
//! Timestamps never cross the wire as `DATETIME`: queries select `UNIX_TIMESTAMP()` and
//! write `NOW()`, so they stay correct under the session `time_zone` without any
//! client-side offset assumption.

use std::path::Path;
use std::time::Duration;

use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};
use sqlx::{MySql, QueryBuilder, Row, Transaction};

use crate::config::DbConfig;
use crate::paths::CONF_CA_CERT;

/// Sessions share this many connections; the C original used one per forked child.
const POOL_SIZE: u32 = 8;

/// `mysql_set_character_set(db, "utf8")`: utf8mb3, which decides the collation used by the
/// string comparison in `check_user` and rejects 4-byte sequences. Not interchangeable
/// with the tables' utf8mb4.
const WIRE_CHARSET: &str = "utf8";

/// `user_online.SID` is `varchar(32)`, and the web application keys on the prefix the C
/// server wrote, so per-session identifiers extend it.
pub const SID_PREFIX: &str = "Telnet_Process_";

/// `SQL_BUFFER_LEN` from `include/database.h`, the C side's largest statement.
pub const SQL_BUFFER_LEN: usize = 10240;

/// How many rows one of the incremental `INSERT IGNORE` statements carries. The C writers
/// count this out inline — `(i + 1) % 100 == 0` (`article_view_log.c:154`) and
/// `cnt_pending_add >= 100` (`article_favor.c:175`) — and both use the same number, so the
/// port names it once. The increment that holds these rows is capped at
/// [`crate::viewlog::MAX_VIEWED_AID_INC_CNT`], which is a multiple of it.
const ROWS_PER_STATEMENT: usize = 100;

#[derive(Debug, Clone)]
pub struct Db {
    pool: MySqlPool,
}

impl Db {
    /// `connect_db()`: the C client asks for the server's default port and, since
    /// `conf/ca_cert.pem` exists on the live host, for `SSL_MODE_VERIFY_CA`.
    ///
    /// sqlx appends `PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION` to `@@sql_mode` by default. The
    /// C client never touched `sql_mode`, so both are switched off to leave the session on
    /// the server's own setting: whether an over-long `bbs.title` is truncated or rejected
    /// follows from that, and the port must not change it.
    pub fn options(db: &DbConfig, ca_cert: &Path) -> MySqlConnectOptions {
        let options = MySqlConnectOptions::new()
            .host(&db.host)
            .username(&db.username)
            .password(&db.password)
            .database(&db.database)
            .charset(WIRE_CHARSET)
            .timezone(db.timezone.clone())
            .pipes_as_concat(false)
            .no_engine_substitution(false);

        if ca_cert.exists() {
            options.ssl_mode(MySqlSslMode::VerifyCa).ssl_ca(ca_cert)
        } else {
            options.ssl_mode(MySqlSslMode::Preferred)
        }
    }

    pub async fn connect(db: &DbConfig) -> Result<Db, sqlx::Error> {
        let pool = MySqlPoolOptions::new()
            .max_connections(POOL_SIZE)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(Self::options(db, Path::new(CONF_CA_CERT)))
            .await?;
        Ok(Db { pool })
    }

    /// A handle that opens its first connection on the first query, so a value that only
    /// has to exist — a [`crate::session::Session`] under test, say — can be built without
    /// a server. Any statement against it still waits for the usual acquire timeout.
    pub fn detached(db: &DbConfig) -> Db {
        Db {
            pool: MySqlPool::connect_lazy_with(Self::options(db, Path::new(CONF_CA_CERT))),
        }
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    /// The C idiom `SET autocommit=0; BEGIN;` … `COMMIT;`.
    pub async fn begin(&self) -> Result<Transaction<'_, MySql>, sqlx::Error> {
        self.pool.begin().await
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Bulk load: `section_list_loader.c`
// ---------------------------------------------------------------------------

/// `load_section_config_from_db` (`:62`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SectionRow {
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "sname")]
    pub sname: Option<String>,
    #[sqlx(rename = "title")]
    pub title: Option<String>,
    #[sqlx(rename = "CID")]
    pub cid: i16,
    #[sqlx(rename = "read_user_level")]
    pub read_user_level: i16,
    #[sqlx(rename = "write_user_level")]
    pub write_user_level: i16,
    /// `section_config.enable * section_class.enable`, which MySQL widens to `int unsigned`.
    /// The original feeds it through `atoi` and only tests it for truth (`:156`), and the
    /// product of two `tinyint unsigned` flags is 0 or 1, so it is read as a flag.
    #[sqlx(rename = "enable")]
    pub enable: bool,
    #[sqlx(rename = "ex_menu_tm")]
    pub ex_menu_tm: Option<i64>,
}

/// One row of the article bulk load (`:216`), content included.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArticleRow {
    #[sqlx(rename = "AID")]
    pub aid: i32,
    #[sqlx(rename = "TID")]
    pub tid: i32,
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "CID")]
    pub cid: i32,
    #[sqlx(rename = "UID")]
    pub uid: i32,
    #[sqlx(rename = "visible")]
    pub visible: bool,
    #[sqlx(rename = "excerption")]
    pub excerption: bool,
    #[sqlx(rename = "ontop")]
    pub ontop: bool,
    #[sqlx(rename = "lock")]
    pub lock: bool,
    #[sqlx(rename = "transship")]
    pub transship: bool,
    #[sqlx(rename = "username")]
    pub username: Option<String>,
    #[sqlx(rename = "nickname")]
    pub nickname: Option<String>,
    #[sqlx(rename = "title")]
    pub title: Option<String>,
    #[sqlx(rename = "sub_dt")]
    pub sub_dt: Option<i64>,
    #[sqlx(rename = "sub_ip")]
    pub sub_ip: Option<String>,
    #[sqlx(rename = "content")]
    pub content: Option<String>,
}

/// One row of `bbs_article_op`; the letter is the whole state transition.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OpRow {
    #[sqlx(rename = "MID")]
    pub mid: i32,
    #[sqlx(rename = "AID")]
    pub aid: i32,
    #[sqlx(rename = "type")]
    pub kind: String,
}

/// The new CID and content of a modified article (`:516`); the CID change is what
/// invalidates the rendered file.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContentRow {
    #[sqlx(rename = "CID")]
    pub cid: i32,
    #[sqlx(rename = "sub_ip")]
    pub sub_ip: Option<String>,
    #[sqlx(rename = "content")]
    pub content: Option<String>,
}

/// The one column of the section-master query (`:88`).
#[derive(Debug, Clone, sqlx::FromRow)]
struct MasterNameRow {
    #[sqlx(rename = "username")]
    username: Option<String>,
}

/// `char master_list[(BBS_username_max_len + 1) * 3 + 1]` (`section_list_loader.c:49`): one
/// run of up to three masters, the NUL excluded.
const MASTER_LIST_BYTES: usize = crate::paths::MASTER_LIST_BYTES;

/// `strncat(dst, src, sizeof(master_list) - 1 - strnlen(dst, sizeof(master_list)))`.
fn append_bounded(dst: &mut String, src: &str, cap: usize) {
    let room = cap.saturating_sub(dst.len());
    // `strncat` counts bytes and can split a UTF-8 sequence; `user_list.username` is
    // `varchar(20)` of `utf8mb4`, so the port drops the partial character rather than keep
    // invalid UTF-8. Registered usernames are ASCII, where the two agree.
    let mut end = room.min(src.len());
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    dst.push_str(&src[..end]);
}

impl Db {
    pub async fn sections(&self) -> Result<Vec<SectionRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT section_config.SID, sname, section_config.title, section_config.CID,
                    read_user_level, write_user_level,
                    section_config.enable * section_class.enable AS enable,
                    UNIX_TIMESTAMP(ex_menu_tm) AS ex_menu_tm
             FROM section_config INNER JOIN section_class ON section_config.CID = section_class.CID
             ORDER BY section_config.SID",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The masters of one section, joined into the single string the board list shows (`:88`).
    pub async fn section_masters(&self, sid: i16) -> Result<String, sqlx::Error> {
        let rows: Vec<MasterNameRow> = sqlx::query_as(
            "SELECT username FROM section_master INNER JOIN user_list ON section_master.UID = user_list.UID
             WHERE SID = ? AND section_master.enable AND (NOW() BETWEEN begin_dt AND end_dt)
             ORDER BY major DESC, begin_dt ASC LIMIT 3",
        )
        .bind(sid)
        .fetch_all(&self.pool)
        .await?;

        // `section_list_loader.c:108-113` appends `"<username> "` per row, each append bounded
        // by the room left in the buffer, so a third long name is cut short and loses its
        // trailing space. `user_list.username` is `varchar(20)`, wider than the 12 the buffer
        // budgets, which is what makes the truncation reachable.
        let mut list = String::new();
        for name in rows.into_iter().map(|row| row.username).flatten() {
            append_bounded(&mut list, &name, MASTER_LIST_BYTES);
            append_bounded(&mut list, " ", MASTER_LIST_BYTES);
        }
        Ok(list)
    }

    /// Keyset pagination over `AID`, which is sparse (`:216`).
    pub async fn articles_from(&self, from_aid: i32, limit: i32) -> Result<Vec<ArticleRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT bbs.AID, TID, SID, bbs.CID, UID, visible, excerption, ontop, `lock`, transship,
                    username, nickname, title, UNIX_TIMESTAMP(sub_dt) AS sub_dt, sub_ip, bbs_content.content
             FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID
             WHERE bbs.AID >= ? ORDER BY bbs.AID LIMIT ?",
        )
        .bind(from_aid)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// The incremental watermark (`:375`).
    pub async fn last_op_id(&self) -> Result<Option<i32>, sqlx::Error> {
        sqlx::query("SELECT MID FROM bbs_article_op ORDER BY MID DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.and_then(|r| r.try_get("MID").ok()))
    }

    /// Ops to replay (`:426`). `'A'` is excluded because new articles arrive through the
    /// `AID` scan, and replaying it would insert them twice.
    pub async fn ops_after(&self, mid: i32, limit: i32) -> Result<Vec<OpRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT MID, AID, type FROM bbs_article_op WHERE MID > ? AND type NOT IN ('A') ORDER BY MID LIMIT ?",
        )
        .bind(mid)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Re-read for op `'M'` (`:516`).
    pub async fn article_content(&self, aid: i32) -> Result<Option<ContentRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT bbs.CID, sub_ip, bbs_content.content
             FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID WHERE bbs.AID = ?",
        )
        .bind(aid)
        .fetch_optional(&self.pool)
        .await
    }

    /// Re-read for op `'T'` (`:559`).
    pub async fn article_sid(&self, aid: i32) -> Result<Option<i16>, sqlx::Error> {
        sqlx::query("SELECT SID FROM bbs WHERE AID = ?")
            .bind(aid)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.and_then(|r| r.try_get("SID").ok()))
    }
}

// ---------------------------------------------------------------------------
// Users and presence: `user_list.c`, `login.c`
// ---------------------------------------------------------------------------

/// The enabled roster (`:168`). The username ordering is load-bearing: it fixes both the
/// display order and the index the binary search runs over.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    #[sqlx(rename = "UID")]
    pub uid: i32,
    #[sqlx(rename = "username")]
    pub username: String,
    #[sqlx(rename = "nickname")]
    pub nickname: Option<String>,
    /// `char(1)`; the C code reads its first byte.
    #[sqlx(rename = "gender")]
    pub gender: String,
    #[sqlx(rename = "gender_pub")]
    pub gender_pub: bool,
    #[sqlx(rename = "life")]
    pub life: i16,
    #[sqlx(rename = "exp")]
    pub exp: Option<i32>,
    #[sqlx(rename = "visit_count")]
    pub visit_count: i32,
    #[sqlx(rename = "signup_dt")]
    pub signup_dt: Option<i64>,
    #[sqlx(rename = "last_login_dt")]
    pub last_login_dt: Option<i64>,
    #[sqlx(rename = "last_logout_dt")]
    pub last_logout_dt: Option<i64>,
    #[sqlx(rename = "birthday")]
    pub birthday: Option<i64>,
    #[sqlx(rename = "introduction")]
    pub introduction: Option<String>,
}

/// A row of `user_online` (`:273`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OnlineRow {
    #[sqlx(rename = "SID")]
    pub sid: String,
    #[sqlx(rename = "UID")]
    pub uid: i32,
    #[sqlx(rename = "ip")]
    pub ip: String,
    #[sqlx(rename = "current_action")]
    pub current_action: String,
    #[sqlx(rename = "login_tm")]
    pub login_tm: i64,
    #[sqlx(rename = "last_tm")]
    pub last_tm: Option<i64>,
}

/// `load_user_info` (`login.c:421`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PubInfoRow {
    #[sqlx(rename = "life")]
    pub life: i16,
    #[sqlx(rename = "last_login_dt")]
    pub last_login_dt: Option<i64>,
    #[sqlx(rename = "user_timezone")]
    pub user_timezone: String,
    #[sqlx(rename = "exp")]
    pub exp: Option<i32>,
    #[sqlx(rename = "nickname")]
    pub nickname: Option<String>,
}

impl Db {
    pub async fn roster(&self) -> Result<Vec<UserRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT user_list.UID AS UID, username, nickname, gender, gender_pub, life, exp, visit_count,
                    UNIX_TIMESTAMP(signup_dt) AS signup_dt,
                    UNIX_TIMESTAMP(last_login_dt) AS last_login_dt,
                    UNIX_TIMESTAMP(last_logout_dt) AS last_logout_dt,
                    UNIX_TIMESTAMP(birthday) AS birthday, `introduction`
             FROM user_list INNER JOIN user_pubinfo ON user_list.UID = user_pubinfo.UID
                            INNER JOIN user_reginfo ON user_list.UID = user_reginfo.UID
             WHERE enable ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The online list over the last `window` seconds, most recently active first.
    pub async fn online(&self, window: i64) -> Result<Vec<OnlineRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT SID, UID, ip, current_action, UNIX_TIMESTAMP(login_tm) AS login_tm,
                    UNIX_TIMESTAMP(last_tm) AS last_tm
             FROM user_online WHERE last_tm >= SUBDATE(NOW(), INTERVAL ? SECOND) ORDER BY last_tm DESC",
        )
        .bind(window)
        .fetch_all(&self.pool)
        .await
    }

    /// The "login count" shown on the top page is the highest log id, not a row count
    /// (`user_list.c:382`).
    pub async fn login_count(&self) -> Result<Option<i32>, sqlx::Error> {
        sqlx::query("SELECT ID FROM user_login_log ORDER BY ID DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.and_then(|r| r.try_get("ID").ok()))
    }

    pub async fn pub_info(&self, uid: i32) -> Result<Option<PubInfoRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT life, UNIX_TIMESTAMP(last_login_dt) AS last_login_dt, user_timezone, exp, nickname
             FROM user_pubinfo WHERE UID = ?",
        )
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn visit_log(&self, ip: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO visit_log(dt, IP) VALUES(NOW(), ?)")
            .bind(ip)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// `user_online_login`: insert, and the caller deletes on logout (`login.c:541`).
    pub async fn online_login(&self, sid: &str, uid: i32, ip: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO user_online(SID, UID, ip, current_action, login_tm, last_tm)
             VALUES(?, ?, ?, 'LOGIN', NOW(), NOW())",
        )
        .bind(sid)
        .bind(uid)
        .bind(ip)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    pub async fn online_delete(&self, sid: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM user_online WHERE SID = ?")
            .bind(sid)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// `user_online_update`: the current action doubles as the idle heartbeat.
    pub async fn online_action(&self, sid: &str, action: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE user_online SET current_action = ?, last_tm = NOW() WHERE SID = ?")
            .bind(action)
            .bind(sid)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// The logout bookkeeping (`login.c:576`): one exp per 300 seconds online, capped at
    /// 288, computed by the server from the two timestamps.
    pub async fn online_logout(&self, uid: i32) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE user_pubinfo SET exp = exp + FLOOR(LEAST(TIMESTAMPDIFF(SECOND,
                 GREATEST(last_login_dt, IF(last_logout_dt IS NULL, last_login_dt, last_logout_dt)), NOW())
                 / 60 / 5, 12 * 24)), last_logout_dt = NOW() WHERE UID = ?",
        )
        .bind(uid)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Login: `login.c:129-380`
// ---------------------------------------------------------------------------

/// The row `check_user` looks for (`:245`). The password is hashed by the server, so the
/// clear-text value is what travels to MySQL over the wire.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccountRow {
    #[sqlx(rename = "UID")]
    pub uid: i32,
    #[sqlx(rename = "username")]
    pub username: String,
    #[sqlx(rename = "p_login")]
    pub p_login: bool,
}

/// Failures from any address in the same /24 within the throttle window (`:182`).
///
/// `pattern` is the client address with its last octet already replaced by `%`, which is
/// how the C code builds it; `login.c` adds no trailing wildcard.
pub async fn count_errors_by_ip(
    tx: &mut Transaction<'_, MySql>,
    minutes: i64,
    pattern: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "SELECT COUNT(*) AS err_count FROM user_err_login_log
         WHERE login_dt >= SUBDATE(NOW(), INTERVAL ? MINUTE) AND login_ip LIKE ?",
    )
    .bind(minutes)
    .bind(pattern)
    .fetch_one(&mut **tx)
    .await
    .and_then(|row| row.try_get("err_count").map_err(Into::into))
}

/// Failures against one account since its last successful login (`:213`).
pub async fn count_errors_by_user(
    tx: &mut Transaction<'_, MySql>,
    username: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "SELECT COUNT(*) AS err_count FROM user_err_login_log
         LEFT JOIN user_list ON user_err_login_log.username = user_list.username
         LEFT JOIN user_pubinfo ON user_list.UID = user_pubinfo.UID
         WHERE user_err_login_log.username = ?
           AND (user_err_login_log.login_dt >= user_pubinfo.last_login_dt
                OR user_pubinfo.last_login_dt IS NULL)",
    )
    .bind(username)
    .fetch_one(&mut **tx)
    .await
    .and_then(|row| row.try_get("err_count").map_err(Into::into))
}

pub async fn find_account(
    tx: &mut Transaction<'_, MySql>,
    username: &str,
    password: &str,
) -> Result<Option<AccountRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT UID, username, p_login FROM user_list
         WHERE username = ? AND password = SHA2(?, 256) AND enable",
    )
    .bind(username)
    .bind(password)
    .fetch_optional(&mut **tx)
    .await
}

pub async fn record_login(
    tx: &mut Transaction<'_, MySql>,
    uid: i32,
    ip: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO user_login_log(UID, login_dt, login_ip) VALUES(?, NOW(), ?)")
        .bind(uid)
        .bind(ip)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// The failure row. `user_err_login_log.password` holds the clear-text password in the
/// reference schema, which the web application's unlock flow depends on; storing it is
/// preserved rather than extended.
pub async fn record_login_error(
    tx: &mut Transaction<'_, MySql>,
    username: &str,
    password: &str,
    ip: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO user_err_login_log(username, password, login_dt, login_ip) VALUES(?, ?, NOW(), ?)",
    )
    .bind(username)
    .bind(password)
    .bind(ip)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

impl Db {
    /// `login.c:369`, run after the login transaction has committed.
    pub async fn mark_login(&self, uid: i32) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE user_pubinfo SET visit_count = visit_count + 1, last_login_dt = NOW() WHERE UID = ?")
            .bind(uid)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Privileges: `user_priv.c:113-280`
// ---------------------------------------------------------------------------

/// The `verified` gate on posting and mailing (`:130`). No row means the account is not
/// verified, which is what clears the bits.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserFlagsRow {
    #[sqlx(rename = "p_post")]
    pub p_post: bool,
    #[sqlx(rename = "p_msg")]
    pub p_msg: bool,
}

/// A section the user moderates (`:173`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MasterRow {
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "major")]
    pub major: bool,
}

/// The per-section level gates, fetched once at startup (`:198`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SectionLevelRow {
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "exp_get")]
    pub exp_get: bool,
    #[sqlx(rename = "read_user_level")]
    pub read_user_level: i16,
    #[sqlx(rename = "write_user_level")]
    pub write_user_level: i16,
}

impl Db {
    pub async fn user_flags(&self, uid: i32) -> Result<Option<UserFlagsRow>, sqlx::Error> {
        sqlx::query_as("SELECT p_post, p_msg FROM user_list WHERE UID = ? AND verified")
            .bind(uid)
            .fetch_optional(&self.pool)
            .await
    }

    /// `major` of the first `admin_config` row: truthy means the major administrator,
    /// falsy a normal one (`:151`).
    pub async fn admin_major(&self, uid: i32) -> Result<Option<bool>, sqlx::Error> {
        sqlx::query(
            "SELECT major FROM admin_config WHERE UID = ? AND enable AND (NOW() BETWEEN begin_dt AND end_dt)",
        )
        .bind(uid)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.and_then(|r| r.try_get("major").ok()))
    }

    pub async fn masterships(&self, uid: i32) -> Result<Vec<MasterRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT section_master.SID, major FROM section_master
             INNER JOIN section_config ON section_master.SID = section_config.SID
             WHERE UID = ? AND section_master.enable AND section_config.enable
               AND (NOW() BETWEEN begin_dt AND end_dt)",
        )
        .bind(uid)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn section_levels(&self) -> Result<Vec<SectionLevelRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT SID, exp_get, read_user_level, write_user_level FROM section_config
             INNER JOIN section_class ON section_config.CID = section_class.CID
             WHERE section_config.enable AND section_class.enable ORDER BY SID",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Sections the user is currently banned from posting in (`:233`).
    pub async fn banned_sids(&self, uid: i32) -> Result<Vec<i16>, sqlx::Error> {
        sqlx::query(
            "SELECT SID FROM ban_user_list WHERE UID = ? AND enable AND (NOW() BETWEEN ban_dt AND unban_dt)",
        )
        .bind(uid)
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().filter_map(|r| r.try_get("SID").ok()).collect())
    }

    pub async fn favorite_sids(&self, uid: i32) -> Result<Vec<i16>, sqlx::Error> {
        sqlx::query("SELECT SID FROM section_favorite WHERE UID = ?")
            .bind(uid)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.into_iter().filter_map(|r| r.try_get("SID").ok()).collect())
    }

    /// `article_view_log_load` (`article_view_log.c:56-59`): every thread this reader has
    /// opened, ascending. The `ORDER BY` is what lets the C code binary-search its array, so
    /// it stays in the query rather than becoming a sort afterwards.
    pub async fn view_log_aids(&self, uid: i32) -> Result<Vec<i32>, sqlx::Error> {
        sqlx::query("SELECT AID FROM view_article_log WHERE UID = ? ORDER BY AID")
            .bind(uid)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.into_iter().filter_map(|r| r.try_get("AID").ok()).collect())
    }

    /// `article_view_log_save_inc` (`article_view_log.c:144-171`): the aids this session read,
    /// as [`ROWS_PER_STATEMENT`] `-row` statements. `INSERT IGNORE` because the key is
    /// `(AID, UID)` and a reader can reach the same thread from another session — or, after
    /// the merge C logs as a duplicate, from this one.
    ///
    /// The `VALUES ` keyword comes from `push_values`, so the header above stops at the
    /// column list where C's `snprintf` buffer kept going.
    pub async fn view_log_insert(&self, uid: i32, aids: &[i32]) -> Result<u64, sqlx::Error> {
        let mut affected = 0;

        for chunk in aids.chunks(ROWS_PER_STATEMENT) {
            let mut builder =
                QueryBuilder::<MySql>::new("INSERT IGNORE INTO view_article_log(AID, UID, dt)");
            builder.push_values(chunk.iter().copied(), |mut row, aid| {
                row.push_bind(aid).push_bind(uid).push("NOW()");
            });

            affected += builder.build().execute(&self.pool).await?.rows_affected() as u64;
        }

        Ok(affected)
    }

    /// `article_favor_load` (`article_favor.c:56-59`): the topic-root aids this account
    /// starred, ascending, with C's 10 001st-record cap dropped.
    pub async fn favor_aids(&self, uid: i32) -> Result<Vec<i32>, sqlx::Error> {
        sqlx::query("SELECT AID FROM article_favorite WHERE UID = ? ORDER BY AID")
            .bind(uid)
            .fetch_all(&self.pool)
            .await
            .map(|rows| rows.into_iter().filter_map(|r| r.try_get("AID").ok()).collect())
    }

    /// `article_favor_save_inc` (`article_favor.c:143-215`): the stars added this session as
    /// batched `INSERT IGNORE`s, and the ones it removed as `DELETE ... AID IN (...)`
    /// statements, which is the pair C builds side by side in one walk of the increment.
    ///
    /// Both counts come back because the log line C writes needs them both:
    /// `Saved %d and deleted %d article_favorite records for uid=%d`.
    pub async fn favor_update(
        &self,
        uid: i32,
        add: &[i32],
        remove: &[i32],
    ) -> Result<(u64, u64), sqlx::Error> {
        let mut inserted = 0;

        for chunk in add.chunks(ROWS_PER_STATEMENT) {
            let mut builder =
                QueryBuilder::<MySql>::new("INSERT IGNORE INTO article_favorite(AID, UID)");
            builder.push_values(chunk.iter().copied(), |mut row, aid| {
                row.push_bind(aid).push_bind(uid);
            });

            inserted += builder.build().execute(&self.pool).await?.rows_affected() as u64;
        }

        let mut deleted = 0;

        for chunk in remove.chunks(ROWS_PER_STATEMENT) {
            let mut builder =
                QueryBuilder::<MySql>::new("DELETE FROM article_favorite WHERE UID = ");
            builder.push_bind(uid).push(" AND AID IN (");
            let mut separated = builder.separated(", ");
            for aid in chunk.iter().copied() {
                separated.push_bind(aid);
            }
            separated.push_unseparated(")");

            deleted += builder.build().execute(&self.pool).await?.rows_affected() as u64;
        }

        Ok((inserted, deleted))
    }

    /// `sign_1` … `sign_3` of `user_pubinfo` (`:258`), the three signature slots the C code
    /// addresses as `sign_%d`. A column name cannot be a bind parameter, so the statements
    /// are spelled out per slot rather than interpolated.
    pub async fn signature(&self, uid: i32, slot: SignSlot) -> Result<Option<String>, sqlx::Error> {
        sqlx::query(slot.query())
            .bind(uid)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.and_then(|r| r.try_get("sign").ok()))
    }

    /// `SELECT introduction FROM user_pubinfo WHERE UID = %d` (`user_info_update.c:61`).
    pub async fn introduction(&self, uid: i32) -> Result<Option<Option<String>>, sqlx::Error> {
        one_text(
            "SELECT introduction FROM user_pubinfo WHERE UID = ?",
            uid,
            &self.pool,
        )
        .await
    }

    /// [`Db::signature`] with the two reasons `user_info_update.c:237-252` separates them: an
    /// empty signature is a NULL column, a missing row is the error of `:249`.
    pub async fn signature_row(
        &self,
        uid: i32,
        slot: SignSlot,
    ) -> Result<Option<Option<String>>, sqlx::Error> {
        one_text(slot.query(), uid, &self.pool).await
    }
}

/// The one-column `SELECT` behind the two profile editors: the outer `Option` is C's
/// `mysql_fetch_row` test, which treats no row at all as the error of `:79-83` and `:249-252`;
/// the inner one is the NULL column that `row[0] ? row[0] : ""` (`:76`, `:245`) opens the
/// editor on. `sqlx` reads a `mediumtext` as `Option<String>` either way.
async fn one_text(
    sql: &'static str,
    uid: i32,
    pool: &MySqlPool,
) -> Result<Option<Option<String>>, sqlx::Error> {
    let rows = sqlx::query(sql).bind(uid).fetch_optional(pool).await?;
    rows.map(|row| row.try_get::<Option<String>, _>(0)).transpose()
}

/// The signature columns of `user_pubinfo`, addressed by index as in the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignSlot {
    One,
    Two,
    Three,
}

impl SignSlot {
    pub fn column(self) -> &'static str {
        match self {
            SignSlot::One => "sign_1",
            SignSlot::Two => "sign_2",
            SignSlot::Three => "sign_3",
        }
    }

    pub fn query(self) -> &'static str {
        match self {
            SignSlot::One => "SELECT sign_1 AS sign FROM user_pubinfo WHERE UID = ?",
            SignSlot::Two => "SELECT sign_2 AS sign FROM user_pubinfo WHERE UID = ?",
            SignSlot::Three => "SELECT sign_3 AS sign FROM user_pubinfo WHERE UID = ?",
        }
    }

    /// The [`Db::update_signature`] of each slot, spelled out for the same reason as
    /// [`SignSlot::query`].
    pub fn update_query(self) -> &'static str {
        match self {
            SignSlot::One => "UPDATE user_pubinfo SET sign_1 = ? WHERE UID = ?",
            SignSlot::Two => "UPDATE user_pubinfo SET sign_2 = ? WHERE UID = ?",
            SignSlot::Three => "UPDATE user_pubinfo SET sign_3 = ? WHERE UID = ?",
        }
    }

    /// `sign_id` of `user_sign_edit` (`:215-220`): the number the screen asks for is one past
    /// the slot, and anything else is C's `ret = -1`.
    pub fn from_index(index: i64) -> Option<SignSlot> {
        match index {
            1 => Some(SignSlot::One),
            2 => Some(SignSlot::Two),
            3 => Some(SignSlot::Three),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The write path: `article_del.c`, `article_post.c`
//
// Every statement here is one C `snprintf` + `mysql_query` pair. The C code opens a
// connection, switches it to `SET autocommit=0`, sends `BEGIN`, and reaches `COMMIT` only on
// the success path — an error `goto cleanup` ends in `mysql_close`, which rolls back. A
// [`Transaction`] that is dropped without `commit` does exactly the same, so the C failure
// ladder maps onto `?` without any explicit rollback.
// ---------------------------------------------------------------------------

/// The two flags `article_del.c:103-105` re-reads from the database while holding the row.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct ArticleStatusRow {
    #[sqlx(rename = "visible")]
    pub visible: bool,
    #[sqlx(rename = "excerption")]
    pub excerption: bool,
}

/// `SELECT visible, excerption FROM bbs WHERE AID = %d FOR UPDATE`.
pub async fn lock_article_status(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
) -> Result<Option<ArticleStatusRow>, sqlx::Error> {
    sqlx::query_as("SELECT visible, excerption FROM bbs WHERE AID = ? FOR UPDATE")
        .bind(aid)
        .fetch_optional(&mut **tx)
        .await
}

/// `UPDATE bbs SET visible = 0, reply_count = 0, m_del = %d WHERE (AID = %d OR TID = %d) AND
/// visible` (`article_del.c:152-156`): the whole thread at once, which is why `reply_count`
/// is zeroed rather than decremented. `m_del` is 0 for an author and 1 for a moderator.
pub async fn hide_thread(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    m_del: i32,
) -> Result<u64, sqlx::Error> {
    sqlx::query("UPDATE bbs SET visible = 0, reply_count = 0, m_del = ? WHERE (AID = ? OR TID = ?) AND visible")
        .bind(m_del)
        .bind(aid)
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|r| r.rows_affected())
}

/// `UPDATE bbs SET reply_count = reply_count - 1 WHERE AID = %d` (`article_del.c:204-206`),
/// only for a reply; the topic root keeps the zeroed count from [`hide_thread`].
pub async fn dec_reply_count(tx: &mut Transaction<'_, MySql>, tid: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bbs SET reply_count = reply_count - 1 WHERE AID = ?")
        .bind(tid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// The five columns `article_excerption_set` re-reads from the row it locks: the author the
/// experience is billable to, the topic the price is read off, the board the row has to belong
/// to, and the flag the requested change is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct ExcerptionRow {
    #[sqlx(rename = "UID")]
    pub uid: i32,
    #[sqlx(rename = "TID")]
    pub tid: i32,
    #[sqlx(rename = "SID")]
    pub sid: i16,
    #[sqlx(rename = "transship")]
    pub transship: bool,
    #[sqlx(rename = "excerption")]
    pub excerption: bool,
}

/// `SELECT UID, TID, SID, transship, excerption FROM bbs WHERE AID = %d AND visible FOR UPDATE`
/// (`article_op.c:82-86`). No row is C's `ret = -2`: the `visible` in the `WHERE` is what makes a
/// post deleted since the list was draw look absent rather than merely hidden.
pub async fn lock_article_for_excerption(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
) -> Result<Option<ExcerptionRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT UID, TID, SID, transship, excerption FROM bbs WHERE AID = ? AND visible FOR UPDATE",
    )
    .bind(aid)
    .fetch_optional(&mut **tx)
    .await
}

/// `UPDATE bbs SET excerption = %d WHERE AID = %d` (`article_op.c:126-130`): the flag the digest
/// marker is drawn from, and the one [`article_del`](hide_thread) refuses to work on.
pub async fn set_excerption(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    set: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bbs SET excerption = ? WHERE AID = ?")
        .bind(i32::from(set))
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `UPDATE bbs SET gen_ex = 0, static = 0 WHERE AID = %d OR TID = %d` (`article_op.c:138-142`),
/// sent only on the way *out* of the digest: whatever the generator made of the thread, and the
/// flag that froze it, go with it. `static` is quoted because it is a keyword — the dump of the
/// table quotes it too.
pub async fn clear_generated_digest(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bbs SET gen_ex = 0, `static` = 0 WHERE AID = ? OR TID = ?")
        .bind(aid)
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `DELETE FROM ex_file WHERE AID = %d` (`article_op.c:148-156`): the digest directory entry of a
/// topic head, dropped when the head leaves the digest. A reply has no entry of its own, so the
/// caller only sends this for `tid == 0`.
pub async fn delete_ex_dir(tx: &mut Transaction<'_, MySql>, aid: i32) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM ex_file WHERE AID = ?")
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `UPDATE upload_file SET UID = %d WHERE ref_AID = %d AND deleted = 0`
/// (`article_op.c:162-168`): the attachments of an excerpted post move to owner `0` — out of the
/// reach of the account that uploaded them, since the digest outlives it — and back to the author
/// when the flag comes off.
pub async fn reassign_attachments(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    uid: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE upload_file SET UID = ? WHERE ref_AID = ? AND deleted = 0")
        .bind(uid)
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `UPDATE user_pubinfo SET exp = exp + %d WHERE UID = %d`, the one statement the post,
/// reply and delete paths all share. The delta is negative on a delete, and the C arithmetic
/// is signed `int` either way.
pub async fn add_exp(tx: &mut Transaction<'_, MySql>, uid: i32, delta: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE user_pubinfo SET exp = exp + ? WHERE UID = ?")
        .bind(delta)
        .bind(uid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip)`: the audit letter — `A` for a
/// post or reply, `M` for an edit, `D` for an author delete, `X` for a moderator delete, and
/// `E`/`O` for putting an article into the digest and taking it out. The loader replays the
/// last two onto the pools ([`crate::loader`]), which is how a change made by one process
/// reached the others.
pub async fn log_article_op(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    uid: i32,
    kind: char,
    ip: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO bbs_article_op(AID, UID, type, op_dt, op_ip) VALUES(?, ?, ?, NOW(), ?)")
        .bind(aid)
        .bind(uid)
        .bind(kind.to_string())
        .bind(ip)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `INSERT INTO bbs_content(AID, content) values(%d, '%s')` and the `mysql_insert_id` that
/// follows it (`article_post.c:332-346`, `:670-684`). A new post inserts with `AID = 0` and
/// links the row afterwards, because the article id does not exist yet; a modification
/// inserts the new body with the article id it already has.
pub async fn insert_content(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    content: &[u8],
) -> Result<i32, sqlx::Error> {
    sqlx::query("INSERT INTO bbs_content(AID, content) values(?, ?)")
        .bind(aid)
        // The column is `mediumblob`-compatible text; bytes keep a body that is not valid
        // UTF-8 — which a GBK terminal can produce — exactly as `mysql_real_escape_string`
        // passed it through.
        .bind(content)
        .execute(&mut **tx)
        .await
        // `(int32_t)mysql_insert_id(db)`: C truncates, and neither path checks for a zero id.
        .map(|r| r.last_insert_id() as i32)
}

/// `UPDATE bbs_content SET AID = %d WHERE CID = %d` (`article_post.c:370-372`, `:1259-1261`).
pub async fn link_content(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    cid: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bbs_content SET AID = ? WHERE CID = ?")
        .bind(aid)
        .bind(cid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// The fifteen columns of `INSERT INTO bbs(…)` (`article_post.c:352-358`, `:1225-1232`).
/// `sub_dt`, `last_reply_dt` and the `NOW()` defaults are left to the server; `icon` is the
/// literal `1` both paths write.
#[derive(Debug, Clone)]
pub struct NewArticleRow {
    pub sid: i16,
    pub tid: i32,
    pub uid: i32,
    pub username: String,
    pub nickname: String,
    pub title: String,
    pub cid: i32,
    pub transship: bool,
    pub reply_note: bool,
    pub exp: i32,
    pub length: i32,
    pub ip: String,
}

/// `INSERT INTO bbs(…)`, returning the `mysql_insert_id` that becomes the new `AID`.
pub async fn insert_article(
    tx: &mut Transaction<'_, MySql>,
    row: &NewArticleRow,
) -> Result<i32, sqlx::Error> {
    sqlx::query(
        "INSERT INTO bbs(SID, TID, UID, username, nickname, title, CID, transship, \
         sub_dt, sub_ip, reply_note, exp, last_reply_dt, icon, length) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, NOW(), ?, ?, ?, NOW(), 1, ?)",
    )
    .bind(row.sid)
    .bind(row.tid)
    .bind(row.uid)
    .bind(&row.username)
    .bind(&row.nickname)
    .bind(&row.title)
    .bind(row.cid)
    .bind(row.transship as i8)
    .bind(&row.ip)
    .bind(row.reply_note as i8)
    .bind(row.exp)
    .bind(row.length)
    .execute(&mut **tx)
    .await
    .map(|r| r.last_insert_id() as i32)
}

/// `UPDATE bbs SET CID = %d, length = %d, reply_note = %d, excerption = 0 WHERE AID = %d`
/// (`article_modify` at `:690-692`). The explicit `excerption = 0` is C's belt-and-braces
/// against "a rare condition" in which an excerpted post reaches the editor at all.
pub async fn apply_edit(
    tx: &mut Transaction<'_, MySql>,
    aid: i32,
    cid: i32,
    length: i32,
    reply_note: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bbs SET CID = ?, length = ?, reply_note = ?, excerption = 0 WHERE AID = ?")
        .bind(cid)
        .bind(length)
        .bind(reply_note as i8)
        .bind(aid)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// `SELECT `lock` FROM bbs WHERE AID = %d` (`article_reply` at `:814-816`), asked of the
/// topic root. No row reads as unlocked, which is C's `if ((row = mysql_fetch_row(rs)))`.
pub async fn topic_lock(db: &Db, aid: i32) -> Result<bool, sqlx::Error> {
    sqlx::query("SELECT `lock` FROM bbs WHERE AID = ?")
        .bind(aid)
        .fetch_optional(&db.pool)
        .await
        .map(|row| row.and_then(|r| r.try_get::<i8, _>("lock").ok()).unwrap_or(0) != 0)
}

/// The joined content row both `article_modify` (`:483-487`) and `article_reply`
/// (`:854-857`) ask for; `reply_note` is the third column only of the former.
#[derive(Debug, Clone)]
pub struct EditableContent {
    pub cid: i32,
    pub content: Option<String>,
    pub reply_note: Option<i8>,
}

/// The body of a stored article. `article_reply` reads it through the transaction-free
/// connection the C code closes before it starts quoting, so this takes a [`Db`].
pub async fn editable_content(db: &Db, aid: i32, with_reply_note: bool) -> Result<Option<EditableContent>, sqlx::Error> {
    let sql = if with_reply_note {
        "SELECT bbs_content.CID, bbs_content.content, reply_note \
         FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID WHERE bbs.AID = ?"
    } else {
        "SELECT bbs_content.CID, bbs_content.content \
         FROM bbs INNER JOIN bbs_content ON bbs.CID = bbs_content.CID WHERE bbs.AID = ?"
    };

    let row = sqlx::query(sql).bind(aid).fetch_optional(&db.pool).await?;
    let Some(row) = row else { return Ok(None) };

    Ok(Some(EditableContent {
        cid: row.try_get("CID")?,
        content: row.try_get("content").ok(),
        reply_note: if with_reply_note { row.try_get("reply_note").ok() } else { None },
    }))
}

/// `UPDATE user_pubinfo SET introduction = '%s' WHERE UID = %d` (`user_info_update.c:166-168`).
/// C opens its own connection for this one statement, so the [`Db`] is the pool rather than a
/// transaction.
pub async fn update_introduction(
    db: &Db,
    uid: i32,
    text: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE user_pubinfo SET introduction = ? WHERE UID = ?")
        .bind(text)
        .bind(uid)
        .execute(&db.pool)
        .await
        .map(|_| ())
}

/// `UPDATE user_pubinfo SET sign_%d = '%s' WHERE UID = %d` (`user_info_update.c:336-338`), the
/// column from [`SignSlot::update_query`] because a column name cannot be a bind parameter.
pub async fn update_signature(
    db: &Db,
    uid: i32,
    slot: SignSlot,
    text: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(slot.update_query())
        .bind(text)
        .bind(uid)
        .execute(&db.pool)
        .await
        .map(|_| ())
}

/// `UPDATE bbs SET reply_count = reply_count + 1, last_reply_dt = NOW(), last_reply_UID = …`
/// (`article_reply` at `:1244-1249`), aimed at the topic root.
pub async fn touch_topic(
    tx: &mut Transaction<'_, MySql>,
    tid: i32,
    uid: i32,
    username: &str,
    nickname: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE bbs SET reply_count = reply_count + 1, last_reply_dt = NOW(), \
         last_reply_UID = ?, last_reply_username = ?, last_reply_nickname = ? WHERE AID = ?",
    )
    .bind(uid)
    .bind(username)
    .bind(nickname)
    .bind(tid)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

/// `SELECT DISTINCT UID FROM bbs WHERE (AID = %d OR AID = %d) AND visible AND reply_note
/// AND UID <> %d` (`article_reply` at `:1271-1274`): everybody who asked to be told.
pub async fn reply_notice_uids(
    tx: &mut Transaction<'_, MySql>,
    tid: i32,
    aid: i32,
    sender_uid: i32,
) -> Result<Vec<i32>, sqlx::Error> {
    sqlx::query(
        "SELECT DISTINCT UID FROM bbs WHERE (AID = ? OR AID = ?) AND visible AND reply_note AND UID <> ?",
    )
    .bind(tid)
    .bind(aid)
    .bind(sender_uid)
    .fetch_all(&mut **tx)
    .await
    .map(|rows| rows.into_iter().filter_map(|r| r.try_get("UID").ok()).collect())
}

/// `INSERT INTO bbs_msg(fromUID, toUID, content, send_dt, send_ip)` (`article_reply` at
/// `:1299-1302`), the notice itself, sent from `BBS_sys_id`.
pub async fn insert_message(
    tx: &mut Transaction<'_, MySql>,
    from_uid: i32,
    to_uid: i32,
    content: &str,
    ip: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO bbs_msg(fromUID, toUID, content, send_dt, send_ip) VALUES(?, ?, ?, NOW(), ?)")
        .bind(from_uid)
        .bind(to_uid)
        .bind(content)
        .bind(ip)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_config() -> DbConfig {
        DbConfig {
            host: "db.example".into(),
            username: "boarduser".into(),
            password: "not-a-real-secret".into(),
            database: "boarddb".into(),
            timezone: "+08:00".into(),
        }
    }

    #[test]
    fn the_session_reproduces_the_c_client_settings() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let options = Db::options(&db_config(), &manifest.join("Cargo.toml"));
        assert_eq!(options.get_host(), "db.example");
        assert_eq!(options.get_username(), "boarduser");
        assert_eq!(options.get_database(), Some("boarddb"));
        // `mysql_set_character_set(db, "utf8")` names no collation, so the server's default
        // for the charset applies; sqlx has to be kept off its utf8mb4 default to match.
        assert_eq!(options.get_charset(), WIRE_CHARSET);
        assert_eq!(options.get_collation(), None);
        assert!(matches!(
            options.get_ssl_mode(),
            MySqlSslMode::VerifyCa
        ));

        // No `conf/ca_cert.pem` is `SSL_MODE_PREFERRED`, which the C client falls back to.
        let options = Db::options(&db_config(), &manifest.join("no-such-ca.pem"));
        assert!(matches!(options.get_ssl_mode(), MySqlSslMode::Preferred));
    }

    #[test]
    fn sign_slot_statements_name_their_own_column() {
        for slot in [SignSlot::One, SignSlot::Two, SignSlot::Three] {
            let expected = format!("SELECT {} AS sign FROM user_pubinfo WHERE UID = ?", slot.column());
            assert_eq!(slot.query(), expected);
            let expected =
                format!("UPDATE user_pubinfo SET {} = ? WHERE UID = ?", slot.column());
            assert_eq!(slot.update_query(), expected);
        }

        // `sign_id` of `user_sign_edit:215-220`: the three slots, and nothing else.
        assert_eq!(SignSlot::from_index(0), None);
        assert_eq!(SignSlot::from_index(-1), None);
        assert_eq!(SignSlot::from_index(4), None);
        assert_eq!(SignSlot::from_index(1), Some(SignSlot::One));
        assert_eq!(SignSlot::from_index(3), Some(SignSlot::Three));
    }

    #[test]
    fn master_run_fills_the_buffer_then_cuts_the_next_name() {
        let mut list = String::new();
        for _ in 0..3 {
            append_bounded(&mut list, "abcdefghijkl", MASTER_LIST_BYTES);
            append_bounded(&mut list, " ", MASTER_LIST_BYTES);
        }
        // Three names of exactly `BBS_username_max_len` fit, trailing space included.
        assert_eq!(list, "abcdefghijkl abcdefghijkl abcdefghijkl ");
        assert_eq!(list.len(), MASTER_LIST_BYTES);

        // A wider name than the buffer budgets for, as `varchar(20)` allows, and a last one
        // that runs out of room mid-word: `strncat` stops at the byte, and the space that
        // would follow it has no room left either.
        let mut list = String::new();
        for name in ["abcdefghijklmnopqrst", "second", "third", "fourth", "fifth"] {
            append_bounded(&mut list, name, MASTER_LIST_BYTES);
            append_bounded(&mut list, " ", MASTER_LIST_BYTES);
        }
        assert_eq!(list, "abcdefghijklmnopqrst second third fourt");
        assert_eq!(list.len(), MASTER_LIST_BYTES);
    }

    #[test]
    fn a_cut_in_a_multi_byte_name_keeps_whole_characters() {
        let mut list = String::new();
        // `中` is three bytes, so `room = 4` would cut the second one in half.
        append_bounded(&mut list, "中文", MASTER_LIST_BYTES);
        assert_eq!(list, "中文");
        let mut list = String::new();
        append_bounded(&mut list, "中文", 4);
        assert_eq!(list, "中");
    }

    /// Opt-in check against the real database, which is the only way to prove the type
    /// decisions above and the session bootstrap at the same time:
    /// `LBBS_REF_CONF=<C tree>/conf/bbsd.conf cargo test --lib -- --ignored`.
    ///
    /// Read-only. The login path is exercised with a wrong password so its `SELECT` runs and
    /// no `INSERT` does, and the transaction is dropped instead of committed.
    #[tokio::test]
    #[ignore = "needs the live board database and LBBS_REF_CONF"]
    async fn live_database_decodes_as_the_reference_types() {
        use sqlx::Row;

        let path = std::env::var("LBBS_REF_CONF").expect("set LBBS_REF_CONF to conf/bbsd.conf");
        let text = std::fs::read_to_string(path).unwrap();
        let (config, _warnings) = crate::config::parse(&text);
        let db = Db::connect(&config.db).await.unwrap();

        let row = sqlx::query(
            "SELECT @@session.character_set_client AS client, @@session.character_set_results AS results,
                    @@session.collation_connection AS collation, @@session.time_zone AS tz,
                    @@session.sql_mode AS session_mode, @@global.sql_mode AS global_mode",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        // What `mysql_set_character_set(db, "utf8")` leaves behind, collation included.
        assert_eq!(row.try_get::<String, _>("client").unwrap(), "utf8mb3");
        assert_eq!(row.try_get::<String, _>("results").unwrap(), "utf8mb3");
        assert_eq!(
            row.try_get::<String, _>("collation").unwrap(),
            "utf8mb3_general_ci"
        );
        assert_eq!(row.try_get::<String, _>("tz").unwrap(), config.db.timezone);

        // sqlx advertises `CLIENT_IGNORE_SPACE` in the handshake and libmysqlclient does not,
        // which makes the server add `IGNORE_SPACE` to the session. It only changes how a
        // space between a built-in function name and its parenthesis parses, and no statement
        // in this layer has one. Everything else must stay the server's own mode: sqlx's
        // default `PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION` append is off in `Db::options`.
        let session_mode = row.try_get::<String, _>("session_mode").unwrap();
        assert_eq!(
            session_mode.strip_prefix("IGNORE_SPACE,").unwrap_or(&session_mode),
            row.try_get::<String, _>("global_mode").unwrap()
        );

        let sections = db.sections().await.unwrap();
        assert!(!sections.is_empty());
        db.section_masters(sections[0].sid).await.unwrap();
        assert!(!db.section_levels().await.unwrap().is_empty());

        let articles = db.articles_from(0, 3).await.unwrap();
        assert!(!articles.is_empty());
        let aid = articles[0].aid;
        db.article_content(aid).await.unwrap();
        db.article_sid(aid).await.unwrap();
        let watermark = db.last_op_id().await.unwrap().unwrap();
        assert!(watermark > 0);
        db.ops_after(0, 3).await.unwrap();

        assert!(!db.roster().await.unwrap().is_empty());
        db.online(900).await.unwrap();
        assert!(db.login_count().await.unwrap().unwrap() > 0);

        let (uid, username): (i32, String) = sqlx::query_as(
            "SELECT UID, username FROM user_list WHERE enable ORDER BY UID LIMIT 1",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        db.pub_info(uid).await.unwrap();
        db.user_flags(uid).await.unwrap();
        db.admin_major(uid).await.unwrap();
        db.masterships(uid).await.unwrap();
        db.banned_sids(uid).await.unwrap();
        db.favorite_sids(uid).await.unwrap();
        for slot in [SignSlot::One, SignSlot::Two, SignSlot::Three] {
            db.signature(uid, slot).await.unwrap();
            db.signature_row(uid, slot).await.unwrap();
        }
        db.introduction(uid).await.unwrap();

        let mut tx = db.begin().await.unwrap();
        count_errors_by_ip(&mut tx, 10, "127.0.0%").await.unwrap();
        count_errors_by_user(&mut tx, &username).await.unwrap();
        assert!(find_account(&mut tx, &username, "wrong-on-purpose").await.unwrap().is_none());
        drop(tx);
    }
}
