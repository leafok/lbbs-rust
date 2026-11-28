//! IP address masking, ported from `src/ip_mask.c`.
//!
//! One function, used for two jobs: privacy (show `1.2.x.x` to other users) and the
//! login throttle, which asks for the /24 by masking the last octet with a literal `%`
//! and putting the result straight into a `LIKE` clause (`login.c:187`).
//!
//! `level` counts the octets that get replaced, so `1` keeps three octets and `4` keeps
//! none. Anything above `4` behaves like `4`, anything at or below `0` is a no-op, and a
//! string with fewer than `4 - level` dots comes back untouched — which is how an IPv6
//! address escapes masking entirely.

/// The number of octets the mask applies to.
const OCTETS: usize = 4;

/// `ip_mask(s, level, mask)`, over a copy instead of in place.
pub fn mask(addr: &str, level: i32, mask_char: u8) -> String {
    if level <= 0 {
        return addr.to_string();
    }
    let level = OCTETS.min(level as usize);
    let bytes = addr.as_bytes();

    // Walk past the separator that precedes the first masked octet.
    let mut pos = 0;
    for _ in 0..(OCTETS - level) {
        match bytes[pos..].iter().position(|b| *b == b'.') {
            Some(offset) => pos += offset + 1,
            None => return addr.to_string(),
        }
    }

    let mut out = bytes[..pos].to_vec();
    for i in 0..level {
        out.push(mask_char);
        if i + 1 < level {
            out.push(b'.');
        }
    }

    // The C code writes a terminator where the first masked octet began, so the tail is
    // gone rather than padded; the surviving prefix can only be ASCII digits and dots.
    String::from_utf8_lossy(&out).into_owned()
}

/// The masked address as users see it (`ip_mask(s, 3, '*')` is the display form).
pub fn anonymize(addr: &str, level: i32) -> String {
    mask(addr, level, b'*')
}

/// The `LIKE` pattern for the caller's /24: `login.c:187` masks one octet with `%`.
pub fn subnet_pattern(addr: &str) -> String {
    mask(addr, 1, b'%')
}

#[cfg(test)]
#[path = "../tests/test_ipmask.rs"]
mod tests;
