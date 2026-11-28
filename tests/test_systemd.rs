use super::*;

/// `sun_path` read back as bytes, which is what the address was built from. `libc` is a
/// `cfg(unix)` dependency, so this helper only exists where the address type does.
#[cfg(unix)]
fn sun_path_bytes(addr: &libc::sockaddr_un) -> Vec<u8> {
    addr.sun_path.iter().map(|byte| *byte as u8).collect()
}

/// `MAINPID=` is the pid systemd would signal, which is this process and not the shell that
/// ran the test.
#[test]
fn the_startup_state_carries_the_status_and_the_pid() {
    let payload = ready_payload("Listening at 0.0.0.0:23 (Telnet) and 0.0.0.0:22 (SSH2)");
    assert!(
        payload.starts_with("READY=1\nSTATUS=Listening at "),
        "{payload}"
    );
    assert!(
        payload.ends_with(&format!("\nMAINPID={}", std::process::id())),
        "{payload}"
    );
}

#[cfg(unix)]
#[test]
fn a_file_name_keeps_its_terminator_and_an_abstract_name_its_leading_nul() {
    let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);

    let (addr, len) = socket_addr("/run/systemd/notify").expect("a file name");
    assert_eq!(
        len as usize,
        offset + "/run/systemd/notify".len() + 1,
        "a file is terminated"
    );
    assert_eq!(addr.sun_family, libc::AF_UNIX as libc::sa_family_t);
    let bytes = sun_path_bytes(&addr);
    assert_eq!(
        &bytes[.."/run/systemd/notify".len() + 1],
        b"/run/systemd/notify\0"
    );

    let (addr, len) = socket_addr("@/systemd/notify").expect("an abstract name");
    assert_eq!(
        len as usize,
        offset + 1 + "/systemd/notify".len(),
        "an abstract name has no terminator"
    );
    let bytes = sun_path_bytes(&addr);
    assert_eq!(bytes[0], 0, "the leading NUL is what marks the namespace");
    assert_eq!(&bytes[1..1 + "/systemd/notify".len()], b"/systemd/notify");
}

#[cfg(unix)]
#[test]
fn a_name_that_does_not_fit_the_address_is_refused() {
    let sample: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let room = sample.sun_path.len();

    assert!(socket_addr("").is_none(), "an empty name notifies nobody");
    assert!(socket_addr("@").is_none(), "a bare marker is not a name");
    assert!(
        socket_addr(&"y".repeat(room)).is_none(),
        "a file name needs room for its terminator"
    );
    assert!(socket_addr(&format!("@{}", "x".repeat(room))).is_none());
    assert!(
        socket_addr(&format!("@{}", "x".repeat(room - 1))).is_some(),
        "the leading NUL is the only byte an abstract name spends"
    );
}

/// What systemd actually sees: one datagram, the whole state string, nothing else.
#[cfg(unix)]
#[test]
fn a_state_reaches_the_socket_named_for_it() {
    let path = std::env::temp_dir().join(format!("lbbs-notify-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let receiver = std::os::unix::net::UnixDatagram::bind(&path).expect("a notification socket");

    let state = "RELOADING=1";
    assert!(
        send_to(&path.to_string_lossy(), state),
        "{state} must go out"
    );
    let mut buf = [0u8; 64];
    let got = receiver.recv(&mut buf).expect("one datagram");
    assert_eq!(&buf[..got], state.as_bytes());

    drop(receiver);
    let _ = std::fs::remove_file(&path);
}
