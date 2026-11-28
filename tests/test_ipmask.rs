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
