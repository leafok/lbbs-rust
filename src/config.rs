//! `conf/bbsd.conf` loader, ported from `load_conf()` in `src/init.c:64-211`.
//!
//! The C parser tokenises on `" \t\r\n"`, so the live file's CRLF line endings are tolerated;
//! keys are matched case-insensitively; and a third token is only accepted as a comment.

use crate::paths::{MAX_CLIENT_LIMIT, MAX_CLIENT_PER_IP_LIMIT};
use std::fmt;

/// Delimiters used by `strtok_r` in `init.c:27` and `bbs_net.c:73`.
const DELIM: [u8; 4] = [b' ', b'\t', b'\r', b'\n'];

/// `sizeof(char DB_*[max_len + 1]) - 1`: the buffers of `database.c:25-29`, filled by the
/// `strncpy` calls of `init.c:179-200`.
const DB_HOST_MAX_LEN: usize = 256;
const DB_FIELD_MAX_LEN: usize = 50;

fn is_delim(b: u8) -> bool {
    DELIM.contains(&b)
}

/// C `atoi`/`atol`: leading blanks, optional sign, then digits; stops at the first
/// non-digit instead of rejecting the token.
pub fn c_atoi(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut value: i64 = 0;
    while let Some(&b) = bytes.get(i) {
        if !b.is_ascii_digit() {
            break;
        }
        value = value.saturating_mul(10).saturating_add((b - b'0') as i64);
        i += 1;
    }
    if negative { -value } else { value }
}

/// `strncpy(dst, src, n)` followed by a forced NUL: truncate to `max` bytes, then keep
/// whole characters so the result stays valid UTF-8.
fn c_trunc(src: &str, max: usize) -> String {
    if src.len() <= max {
        return src.to_string();
    }
    let mut end = max;
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    src[..end].to_string()
}

/// `snprintf("%4s年%2s月%2s日", ...)` — right-padded with spaces to a minimum width.
fn pad_min(field: &str, width: usize) -> String {
    let cells = field.chars().count();
    format!("{}{field}", " ".repeat(width.saturating_sub(cells)))
}

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub host: String,
    pub username: String,
    pub password: String,
    pub database: String,
    /// Sent as `SET time_zone = '...'` on every connection; all `NOW()` and
    /// `UNIX_TIMESTAMP()` arithmetic in the SQL depends on it.
    pub timezone: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bbs_id: String,
    pub bbs_name: String,
    pub bbs_server: String,
    pub bbs_address: String,
    pub telnet_port: u16,
    pub ssh_port: u16,
    pub max_client: usize,
    pub max_client_per_ip: usize,
    pub start_dt: String,
    pub sys_id: i32,
    pub db: DbConfig,
}

/// Warnings collected while parsing, so a malformed line is reported but does not stop
/// the server — the C parser logs and skips.
#[derive(Debug, Default)]
pub struct ConfigWarnings(pub Vec<String>);

impl ConfigWarnings {
    /// `load_conf()` reported every skipped line through `log_error()` as it went, which is the
    /// error stream. A server started in the foreground sees the same lines on its stderr.
    pub fn log(&self) {
        for warning in &self.0 {
            tracing::error!("{warning}");
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `load_conf(CONF_BBSD)`: read the file, parse it and log what was skipped.
///
/// The C loader worked from the current directory, which `main` has already moved to the server
/// root, so the path here is taken as given.
pub fn load(path: &str) -> Result<Config, LoadError> {
    let data = std::fs::read(path).map_err(|err| LoadError {
        path: path.to_string(),
        cause: err.to_string(),
    })?;
    let text = String::from_utf8(data).map_err(|err| LoadError {
        path: path.to_string(),
        cause: err.to_string(),
    })?;

    let (config, warnings) = parse(&text);
    warnings.log();
    Ok(config)
}

/// The configuration file could not be read at all, which `main.c` treated as fatal (exit
/// code -2). The message is C's `Open %s failed` with the OS reason appended.
#[derive(Debug)]
pub struct LoadError {
    pub path: String,
    pub cause: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Open {} failed: {}", self.path, self.cause)
    }
}

impl std::error::Error for LoadError {}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}) {}:{}+{} sys_id={}",
            self.bbs_name,
            self.bbs_id,
            self.bbs_address,
            self.telnet_port,
            self.ssh_port,
            self.sys_id
        )
    }
}

impl Default for Config {
    /// Defaults from `src/bbs.c:28-36` and `src/database.c` for the `db_*` keys.
    fn default() -> Self {
        Config {
            bbs_id: "Example BBS".to_string(),
            bbs_name: "Example Site Name".to_string(),
            bbs_server: "bbs.example.com".to_string(),
            bbs_address: "0.0.0.0".to_string(),
            telnet_port: 23,
            ssh_port: 22,
            max_client: MAX_CLIENT_LIMIT,
            max_client_per_ip: MAX_CLIENT_PER_IP_LIMIT,
            start_dt: "2000年 1月 1日".to_string(),
            sys_id: 1,
            db: DbConfig {
                host: "localhost".to_string(),
                username: "bbs".to_string(),
                password: String::new(),
                database: "bbs".to_string(),
                timezone: "SYSTEM".to_string(),
            },
        }
    }
}

/// Splits one line the way `strtok_r` does: runs of delimiters collapse, so
/// `2000--10-30` yields three fields, not four.
fn tokens(line: &str) -> Vec<String> {
    split_collapsing(line, |b| is_delim(b))
}

fn tokens_on(value: &str, sep: u8) -> Vec<String> {
    split_collapsing(value, |b| b == sep)
}

fn split_collapsing(text: &str, mut is_sep: impl FnMut(u8) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, &b) in text.as_bytes().iter().enumerate() {
        if is_sep(b) {
            if let Some(from) = start.take() {
                out.push(text[from..idx].to_owned());
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(from) = start {
        out.push(text[from..].to_owned());
    }
    out
}

pub fn parse(text: &str) -> (Config, ConfigWarnings) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();

    for line in text.split('\n') {
        let fields = tokens(line);
        let Some(key) = fields.first() else { continue };
        if key.starts_with('#') {
            continue;
        }

        let Some(value) = fields.get(1) else {
            warnings.push(format!("Skip empty value of config item: {key}"));
            continue;
        };

        if let Some(extra) = fields.get(2)
            && !extra.starts_with('#')
        {
            warnings.push(format!(
                "Skip config line with extra value {key} = {value} {extra}"
            ));
            continue;
        }

        // `strcasecmp` on the key.
        let lower = key.to_lowercase();
        match lower.as_str() {
            "bbs_id" => cfg.bbs_id = c_trunc(value, 20),
            "bbs_name" => cfg.bbs_name = c_trunc(value, 50),
            "bbs_server" => cfg.bbs_server = c_trunc(value, 255),
            "bbs_address" => cfg.bbs_address = c_trunc(value, 50),
            "bbs_port" => cfg.telnet_port = c_atoi(value) as u16,
            "bbs_ssh_port" => cfg.ssh_port = c_atoi(value) as u16,
            "bbs_max_client" => {
                let v = c_atoi(value);
                if v <= 0 || v > MAX_CLIENT_LIMIT as i64 {
                    warnings.push(format!(
                        "Ignore config bbs_max_client with incorrect value {v}"
                    ));
                    cfg.max_client = MAX_CLIENT_LIMIT;
                } else {
                    cfg.max_client = v as usize;
                }
            }
            "bbs_max_client_per_ip" => {
                let v = c_atoi(value);
                if v <= 0 || v > MAX_CLIENT_PER_IP_LIMIT as i64 {
                    warnings.push(format!(
                        "Ignore config bbs_max_client_per_ip with incorrect value {v}"
                    ));
                    cfg.max_client_per_ip = MAX_CLIENT_PER_IP_LIMIT;
                } else {
                    cfg.max_client_per_ip = v as usize;
                }
            }
            "bbs_start_dt" => {
                let parts = tokens_on(value, b'-');
                if parts.len() < 3 {
                    warnings.push("Ignore config bbs_start_dt with incorrect value".to_string());
                    continue;
                }
                cfg.start_dt = format!(
                    "{}年{}月{}日",
                    pad_min(&parts[0], 4),
                    pad_min(&parts[1], 2),
                    pad_min(&parts[2], 2)
                );
            }
            "bbs_sys_id" => {
                let v = c_atoi(value);
                if v <= 0 {
                    warnings.push(format!("Ignore config bbs_sys_id with incorrect value {v}"));
                    continue;
                }
                cfg.sys_id = v as i32;
            }
            // `strncpy(DB_*, q, sizeof(DB_*) - 1)` over `char DB_*[<name>_max_len + 1]`
            // (`database.h:18-22`).
            "db_host" => cfg.db.host = c_trunc(value, DB_HOST_MAX_LEN),
            "db_username" => cfg.db.username = c_trunc(value, DB_FIELD_MAX_LEN),
            "db_password" => cfg.db.password = c_trunc(value, DB_FIELD_MAX_LEN),
            "db_database" => cfg.db.database = c_trunc(value, DB_FIELD_MAX_LEN),
            "db_timezone" => cfg.db.timezone = c_trunc(value, DB_FIELD_MAX_LEN),
            _ => warnings.push(format!("Unknown config {key} = {value}")),
        }
    }

    (cfg, ConfigWarnings(warnings))
}

impl Config {
    pub fn telnet_addr(&self) -> String {
        format!("{}:{}", self.bbs_address, self.telnet_port)
    }

    pub fn ssh_addr(&self) -> String {
        format!("{}:{}", self.bbs_address, self.ssh_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_style_config_with_crlf_and_comments() {
        let text = "bbs_id LBBS-devel\r\nbbs_name 枫林在线\r\nbbs_port 2323\r\n\
                    bbs_max_client 1000 # Keep it as low as possible\r\n\
                    bbs_start_dt 2000-10-30\r\nbbs_sys_id 600\r\n\r\n";
        let (cfg, warns) = parse(text);
        assert_eq!(cfg.bbs_id, "LBBS-devel");
        assert_eq!(cfg.bbs_name, "枫林在线");
        assert_eq!(cfg.telnet_port, 2323);
        assert_eq!(cfg.max_client, 1000);
        assert_eq!(cfg.start_dt, "2000年10月30日");
        assert_eq!(cfg.sys_id, 600);
        assert!(warns.0.is_empty(), "{warns:?}");
    }

    #[test]
    fn keys_are_case_insensitive() {
        let (cfg, _) = parse("DB_HOST db.example.com\nBBS_Name Whatever\n");
        assert_eq!(cfg.db.host, "db.example.com");
        assert_eq!(cfg.bbs_name, "Whatever");
    }

    #[test]
    fn start_dt_pads_to_minimum_width() {
        let (cfg, _) = parse("bbs_start_dt 2000-1-5\n");
        assert_eq!(cfg.start_dt, "2000年 1月 5日");
    }

    #[test]
    fn out_of_range_client_limits_fall_back_to_the_hard_limit() {
        let (cfg, warns) = parse("bbs_max_client 0\nbbs_max_client_per_ip 99999\n");
        assert_eq!(cfg.max_client, MAX_CLIENT_LIMIT);
        assert_eq!(cfg.max_client_per_ip, MAX_CLIENT_PER_IP_LIMIT);
        assert_eq!(warns.0.len(), 2);
    }

    #[test]
    fn extra_uncommented_token_rejects_the_line() {
        let (cfg, warns) = parse("bbs_port 2323 2324\nbbs_sys_id 7\n");
        assert_eq!(
            cfg.telnet_port, 23,
            "line with an extra token must be skipped"
        );
        assert_eq!(cfg.sys_id, 7);
        assert_eq!(warns.0.len(), 1);
    }

    #[test]
    fn unknown_keys_are_logged_and_ignored() {
        let (cfg, warns) = parse("bbs_nonsense hi\n");
        assert_eq!(cfg.bbs_id, "Example BBS");
        assert_eq!(
            warns.0,
            vec!["Unknown config bbs_nonsense = hi".to_string()]
        );
    }

    #[test]
    fn atoi_stops_at_the_first_non_digit() {
        assert_eq!(c_atoi("2323abc"), 2323);
        assert_eq!(c_atoi("  -12x"), -12);
        assert_eq!(c_atoi("abc"), 0);
        assert_eq!(c_atoi("+7"), 7);
    }

    #[test]
    fn truncation_does_not_split_a_utf8_char() {
        assert_eq!(c_trunc("abcd", 2), "ab");
        assert_eq!(c_trunc("中文", 1), "");
        assert_eq!(c_trunc("中文", 4), "中");
    }
}
