//! Badword filter, ported from `src/bwf.c`.
//!
//! The C implementation reads one PCRE pattern per line, joins them into a single
//! caseless alternation `(p1)|(p2)|...`, and *masks* every match in place rather than
//! rejecting the text. Byte-level matching is used here because the masked output is no
//! longer guaranteed to be valid UTF-8, and because ASCII-only case folding is exactly
//! what PCRE2 does without `PCRE2_UTF`.

use std::path::Path;

const MAX_PATTERN_LEN: usize = 63;
const MAX_PATTERN_BUF: usize = 65 * 1000;

#[derive(Debug, PartialEq, Eq)]
pub enum BwfError {
    Unreadable(String),
    /// A source line did not fit the record buffer (`bwf_load` returns -3).
    Truncated {
        line: usize,
        len: usize,
    },
    Overflow,
    /// The joined pattern failed to compile (`bwf_compile` returns -1).
    Compile(String),
}

/// The `log_error` calls of `bwf.c:51`, `:60`, `:84` and `:115`, in the same words, so a caller
/// that only has the error can still write C's line.
impl std::fmt::Display for BwfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The string is the path and the OS reason, as the loader recorded them.
            BwfError::Unreadable(cause) => write!(f, "fopen({cause}) error"),
            BwfError::Truncated { line, len } => {
                write!(f, "Data line {line} (len={len}) is truncated")
            }
            BwfError::Overflow => {
                write!(f, "Data exceed length limit {}", MAX_PATTERN_BUF - 1)
            }
            BwfError::Compile(cause) => write!(f, "pcre2_compile() error: {cause}"),
        }
    }
}

impl std::error::Error for BwfError {}

pub struct Badwords {
    pattern: Option<regex::bytes::Regex>,
    pub patterns: usize,
}

impl Default for Badwords {
    fn default() -> Self {
        Self::empty()
    }
}

impl Badwords {
    /// No patterns configured. Masking is a no-op; without this the joined empty
    /// alternation would match at every offset, as it does in the C original.
    pub fn empty() -> Self {
        Badwords {
            pattern: None,
            patterns: 0,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, BwfError> {
        let text = std::fs::read(path.as_ref())
            .map_err(|e| BwfError::Unreadable(format!("{}: {e}", path.as_ref().display())))?;

        let mut joined = String::new();
        let mut count = 0usize;
        for (index, raw) in text.split(|&b| b == b'\n').enumerate() {
            // A trailing newline splits into a final empty element, not a record.
            let mut line: Vec<u8> = raw.to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            trim_trailing_lf(&mut line);

            // C keeps two cases apart here: a line blank after the newline strip is skipped, and
            // since b2e764c a zero-length read aborts as truncated. Splitting on `\n` cannot tell
            // them apart — only a file with a NUL byte at the start of a line could — so both skip.
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_PATTERN_LEN {
                return Err(BwfError::Truncated {
                    line: index,
                    len: line.len(),
                });
            }

            if !joined.is_empty() {
                joined.push('|');
            }
            if joined.len() + line.len() + 2 > MAX_PATTERN_BUF - 1 {
                return Err(BwfError::Overflow);
            }
            joined.push('(');
            // Patterns are byte strings; lossy conversion keeps the alternation compiling
            // even if an operator hand-edit made a line invalid.
            joined.push_str(&String::from_utf8_lossy(&line));
            joined.push(')');
            count += 1;
        }

        if count == 0 {
            return Ok(Badwords::empty());
        }

        let re = regex::bytes::RegexBuilder::new(&joined)
            .case_insensitive(true)
            .build()
            .map_err(|e| BwfError::Compile(format!("{e} ({} patterns)", count)))?;

        Ok(Badwords {
            pattern: Some(re),
            patterns: count,
        })
    }

    /// `check_badwords`: repeatedly match from the end of the previous match and overwrite
    /// each matched byte range with `mask`. Returns the number of masked matches, which is
    /// what callers test to decide whether to re-display the text.
    pub fn mask(&self, buf: &mut [u8], mask: u8) -> usize {
        let Some(re) = &self.pattern else { return 0 };

        let mut start = 0usize;
        let mut hits = 0usize;
        while start < buf.len() {
            let Some(m) = re.find_at(buf, start) else {
                break;
            };
            let (from, to) = (m.start(), m.end());
            if to == from {
                // A zero-width match would spin forever; step past it instead.
                start = from + 1;
                continue;
            }
            for byte in &mut buf[from..to] {
                *byte = mask;
            }
            hits += 1;
            start = to;
        }
        hits
    }
}

fn trim_trailing_lf(line: &mut Vec<u8>) {
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("lbbs_bwf_{name}_{}", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn masks_the_whole_line_for_open_ended_patterns() {
        // The shipped config wraps entries in [^\n]*...[^\n]*, which erases the line.
        let path = write_tmp("line", b"[^\\n]*foo[^\\n]*\n");
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut buf = b"xx foo yy".to_vec();
        assert_eq!(bwf.mask(&mut buf, b'*'), 1);
        assert_eq!(buf, b"*********");
    }

    #[test]
    fn masks_only_the_span_of_bare_patterns() {
        let path = write_tmp("bare", b"secret\n");
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut buf = b"a secret b secret".to_vec();
        assert_eq!(bwf.mask(&mut buf, b'*'), 2);
        assert_eq!(buf, b"a ****** b ******");
    }

    #[test]
    fn case_is_folded_ascii_style_like_pcre2_without_utf() {
        let path = write_tmp("case", b"FUCK\n");
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut buf = b"how fuck you".to_vec();
        assert_eq!(bwf.mask(&mut buf, b'*'), 1);
    }

    #[test]
    fn posix_space_class_matches_cjk_across_whitespace() {
        // Mirrors `法[[:space:]]*轮` from conf/badwords.conf.
        let path = write_tmp("posix", "[^\\n]*法[[:space:]]*轮[^\\n]*\n".as_bytes());
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut buf = "标题 法 轮 内容".as_bytes().to_vec();
        assert_eq!(bwf.mask(&mut buf, b'*'), 1);
        assert!(buf.iter().all(|&b| b == b'*'));
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let path = write_tmp("crlf", b"alpha\r\n\r\nbeta\r\n");
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bwf.patterns, 2);
    }

    #[test]
    fn overlong_pattern_is_reported_as_truncated() {
        let long = format!("{}x", "a".repeat(70));
        let path = write_tmp("long", long.as_bytes());
        assert!(matches!(
            Badwords::load(&path),
            Err(BwfError::Truncated { len: 71, .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn empty_config_never_loops() {
        let path = write_tmp("empty", b"");
        let bwf = Badwords::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut buf = b"hello".to_vec();
        assert_eq!(bwf.mask(&mut buf, b'*'), 0);
        assert_eq!(buf, b"hello");
    }
}
