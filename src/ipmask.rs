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
mod tests {
    use super::*;

    #[test]
    fn a_nonpositive_level_leaves_the_address_alone() {
        assert_eq!(mask("202.38.64.1", 0, b'x'), "202.38.64.1");
        assert_eq!(mask("202.38.64.1", -1, b'x'), "202.38.64.1");
    }

    #[test]
    fn each_level_replaces_that_many_trailing_octets() {
        // Every masked octet becomes exactly one mask character, dots still between them.
        assert_eq!(mask("202.38.64.1", 1, b'x'), "202.38.64.x");
        assert_eq!(mask("202.38.64.1", 2, b'x'), "202.38.x.x");
        assert_eq!(mask("202.38.64.1", 3, b'x'), "202.x.x.x");
        assert_eq!(mask("202.38.64.1", 4, b'x'), "x.x.x.x");
    }

    #[test]
    fn a_level_above_four_is_four() {
        assert_eq!(mask("202.38.64.1", 9, b'*'), "*.*.*.*");
    }

    #[test]
    fn too_few_separators_means_no_masking() {
        // An IPv6 address has no dots, so the scan gives up on the first octet.
        assert_eq!(mask("2001:db8::1", 1, b'%'), "2001:db8::1");
        assert_eq!(mask("10.0.0", 1, b'%'), "10.0.0");
        assert_eq!(mask("", 1, b'%'), "");
    }

    #[test]
    fn a_short_address_is_overwritten_the_way_c_wrote_past_its_end() {
        // `ip_mask` walks two dots and then writes `level` octets with no regard for where
        // the original terminator was; C only escaped a crash because the buffer was bigger.
        assert_eq!(mask("10.0.0", 2, b'%'), "10.0.%.%");
    }

    #[test]
    fn extra_octets_are_cut_at_the_mask() {
        // C terminates the string where the first masked octet starts, so anything past
        // it disappears — including the separator of a fifth octet.
        assert_eq!(mask("1.2.3.4.5", 1, b'x'), "1.2.3.x");
        assert_eq!(mask("1.2.3.4.5", 2, b'x'), "1.2.x.x");
    }

    #[test]
    fn the_login_throttle_pattern_ends_in_a_wildcard() {
        assert_eq!(subnet_pattern("202.38.64.1"), "202.38.64.%");
        assert_eq!(subnet_pattern("::1"), "::1");
        assert_eq!(anonymize("202.38.64.1", 3), "202.*.*.*");
        assert_eq!(anonymize("202.38.64.1", 1), "202.38.64.*");
    }
}
