use super::*;

fn addr(ip: &str) -> SocketAddr {
    format!("{ip}:2323").parse().unwrap()
}

/// The accepted slot, kept alive by the caller until it is dropped.
fn accepted(admission: Admission) -> Admitted {
    match admission {
        Admission::Accepted(admitted) => admitted,
        Admission::Rejected(Reject::Total(live, max)) => {
            panic!("refused for the total ({live} >= {max})")
        }
        Admission::Rejected(Reject::PerIp(count, max)) => {
            panic!("refused per IP ({count} >= {max})")
        }
    }
}

/// `SYS_child_process_count - 1 < BBS_max_client`, read against the limit the config holds
/// now: raising `bbs_max_client` opens a slot immediately, which a gate sized at startup
/// would not.
#[tokio::test]
async fn the_total_gate_follows_the_reloaded_limit() {
    let session = crate::session::offline_session();
    let state = session.state.clone();
    state.publish_limits(2, 10);
    let gateway = Gateway::new(state.clone(), std::path::PathBuf::from("/srv/lbbs"));

    let mut held = vec![
        accepted(gateway.admit(addr("10.0.0.1")).await),
        accepted(gateway.admit(addr("10.0.0.2")).await),
    ];
    assert_eq!(gateway.live(), 2);

    let refused = gateway.admit(addr("10.0.0.3")).await;
    assert!(
        matches!(refused, Admission::Rejected(Reject::Total(2, 2))),
        "the third connection is over the limit the config still holds"
    );

    state.publish_limits(3, 10);
    held.push(accepted(gateway.admit(addr("10.0.0.3")).await));
    assert_eq!(gateway.live(), 3);

    // The lower limit takes effect on the next accept, without waiting for the sessions
    // that are already running to leave.
    state.publish_limits(2, 10);
    assert!(matches!(
        gateway.admit(addr("10.0.0.4")).await,
        Admission::Rejected(Reject::Total(3, 2))
    ));

    drop(held);
    assert_eq!(gateway.live(), 0, "a dropped slot is a counted release");
}

/// C tested the total before the per-IP limit, so a connection refused for its address had
/// already taken the parent's slot; the port hands that reservation back rather than leaking
/// one per refused connection.
#[tokio::test]
async fn a_per_ip_rejection_gives_the_total_slot_back() {
    let session = crate::session::offline_session();
    let state = session.state.clone();
    state.publish_limits(5, 1);
    let gateway = Gateway::new(state.clone(), std::path::PathBuf::from("/srv/lbbs"));

    let first = accepted(gateway.admit(addr("10.0.0.1")).await);
    let second = accepted(gateway.admit(addr("10.0.0.2")).await);

    let refused = gateway.admit(addr("10.0.0.1")).await;
    assert!(
        matches!(refused, Admission::Rejected(Reject::PerIp(1, 1))),
        "the second connection from one address is refused for that address"
    );
    assert_eq!(
        gateway.live(),
        2,
        "the refused connection must not keep the slot it reserved"
    );

    drop(first);
    // One address is free again, so its next connection is admitted even though the count
    // for the map entry was cleared with it.
    let again = accepted(gateway.admit(addr("10.0.0.1")).await);
    assert_eq!(again.same_ip, 0);
    assert_eq!(gateway.live(), 2);
    drop(second);
    drop(again);
    assert_eq!(gateway.live(), 0);
}
