/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `sd_notify()` without libsystemd.
//!
//! The C parent tells systemd three things, each behind `#ifdef HAVE_SYSTEMD_SD_DAEMON_H`:
//! `READY=1` once both listeners are bound (`net_server.c:695-701`), `RELOADING=1` and then
//! `READY=1` around a configuration reload (`:813`, `:883`), and `STOPPING=1` as soon as the
//! exit flag is raised (`:706-710`). The unit is `Type=notify`, so `systemctl start` waits for
//! the first of those and `systemctl reload` for the pair.
//!
//! All libsystemd does is write the state string as one unix datagram to the socket named by
//! `NOTIFY_SOCKET`, so that is all this module does: no `systemd/sd-daemon.h`, no dependency,
//! and the unit the C build installs serves this daemon unchanged.

/// The payload of the startup `READY=1`, which `net_server.c:697-700` sent with the listening
/// pair and the pid attached.
fn ready_payload(status: &str) -> String {
    format!("READY=1\nSTATUS={status}\nMAINPID={}", std::process::id())
}

/// `sd_notifyf(0, "READY=1\nSTATUS=...\nMAINPID=%d", getpid())`: startup is over.
pub fn ready(status: &str) -> bool {
    notify(&ready_payload(status))
}

/// `sd_notify(0, state)`, with `READY=1`, `RELOADING=1` and `STOPPING=1` the states the C
/// daemon uses. `false` covers both quiet cases: no socket named, and nothing sent.
pub fn notify(state: &str) -> bool {
    send(state)
}

/// Under a terminal there is no `NOTIFY_SOCKET`, and on Windows there is no socket to write.
#[cfg(not(unix))]
fn send(_state: &str) -> bool {
    false
}

#[cfg(unix)]
fn send(state: &str) -> bool {
    let socket = match std::env::var_os("NOTIFY_SOCKET") {
        Some(socket) => socket,
        None => return false,
    };
    match socket.to_str() {
        Some(socket) => send_to(socket, state),
        None => false,
    }
}

/// One datagram to one notification socket. A failure is not worth reporting: `sd_notify()`
/// hands it back to its caller, and every C call site dropped it on the floor.
#[cfg(unix)]
fn send_to(socket: &str, state: &str) -> bool {
    let (addr, len) = match socket_addr(socket) {
        Some(addr) => addr,
        None => return false,
    };
    let bytes = state.as_bytes();

    // `SOCK_CLOEXEC` is a Linux flag. Elsewhere the descriptor is marked close-on-exec afterwards,
    // so the socket still cannot outlive an `exec` on a thread that is not this one.
    #[cfg(target_os = "linux")]
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    #[cfg(not(target_os = "linux"))]
    let fd = {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
        if fd >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        fd
    };
    if fd < 0 {
        return false;
    }
    let sent = unsafe {
        libc::sendto(
            fd,
            bytes.as_ptr().cast(),
            bytes.len(),
            0,
            (&addr as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    unsafe { libc::close(fd) };

    sent == bytes.len() as libc::ssize_t
}

/// The `sockaddr_un` of a notification socket, whose name is abstract when it starts with `@`.
///
/// `sun_path` is 108 bytes, and both spellings spend one of them the name does not have: an
/// abstract name a leading `NUL` that marks the namespace, a file name the terminator the
/// kernel reads. The length follows the same split — a file keeps its terminator, an abstract
/// name keeps its leading `NUL` and nothing behind it.
#[cfg(unix)]
fn socket_addr(socket: &str) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    let (name, abstract_name) = match socket.strip_prefix('@') {
        Some(name) => (name.as_bytes(), true),
        None => (socket.as_bytes(), false),
    };

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let head = usize::from(abstract_name);
    let tail = usize::from(!abstract_name);
    if name.is_empty() || head + name.len() + tail > addr.sun_path.len() {
        return None;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let slot = addr.sun_path.as_mut_ptr().cast::<u8>();
    unsafe {
        if abstract_name {
            *slot = 0;
        }
        std::ptr::copy_nonoverlapping(name.as_ptr(), slot.add(head), name.len());
        if tail == 1 {
            *slot.add(head + name.len()) = 0;
        }
    }

    let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    Some((addr, (offset + head + name.len() + tail) as libc::socklen_t))
}

#[cfg(test)]
#[path = "../tests/test_systemd.rs"]
mod tests;
