use super::*;

#[test]
fn the_shared_state_crosses_task_boundaries() {
    // Every client task is handed `Arc<AppState>`, so the tables behind the locks have to
    // be shareable. This fails the moment a `Rc`/`RefCell` reaches into them.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppState>();
}

#[test]
fn a_poisoned_lock_still_serves_state() {
    let lock = RwLock::new(7u32);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = write(&lock);
        panic!("a client task died holding the board");
    }));
    assert!(panicked.is_err());

    // The daemon would have left the same torn segment mapped into every child, so the
    // port keeps the data and carries on.
    assert_eq!(*read(&lock), 7);
}

fn state() -> AppState {
    let config = Config::default();
    let db = Db::detached(&config.db);
    AppState::new(config, db)
}

/// A `TOPMENU` of `count` items, which is what the reload path swaps. `..` is the only
/// submenu reference that needs no target menu.
fn topmenu(count: usize) -> MenuSet {
    let mut items = String::new();
    for _ in 0..count {
        items.push_str("!.. 4, 5, 0, 0, \"Up\", \"(U) up\"\n");
    }
    let data = format!("%S_MAIN\n welcome\n%\n%menu TOPMENU\nscreen 1, 0, S_MAIN\n{items}%\n");
    MenuSet::parse(data.as_bytes()).expect("the fixture must parse")
}

/// The item count of the `TOPMENU` in a published set.
fn topmenu_len(set: &MenuSet) -> usize {
    set.menu(set.menu_by_name("TOPMENU").unwrap())
        .unwrap()
        .items
        .len()
}

#[tokio::test]
async fn a_published_menu_replaces_the_previous_one() {
    let state = state();
    assert_eq!(state.menus().menus.len(), 0);

    state.publish_menus(topmenu(1));
    assert_eq!(topmenu_len(&state.menus()), 1);

    // A task holding the old Arc is not disturbed by the swap.
    let held = state.menus();
    state.publish_menus(topmenu(2));
    assert_eq!(topmenu_len(&held), 1);
    assert_eq!(topmenu_len(&state.menus()), 2);
}

#[tokio::test]
async fn the_limits_are_clamped_the_way_load_conf_does() {
    let state = state();
    state.publish_limits(0, usize::MAX);
    assert_eq!(state.limits(), (1, crate::paths::MAX_CLIENT_PER_IP_LIMIT));

    state.publish_limits(50, 7);
    assert_eq!(state.limits(), (50, 7));
}

#[tokio::test]
async fn exiting_wakes_a_task_waiting_for_the_shutdown() {
    let state = Arc::new(state());
    let waiting = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            state.shutdown().await;
            state.is_exiting()
        })
    };

    state.exit();
    assert!(waiting.await.unwrap());
}

#[tokio::test]
async fn a_task_that_arrives_after_the_exit_does_not_hang() {
    let state = state();
    state.exit();
    // C's children saw the flag through shared memory, so a late child never waited for a
    // signal that had already been sent.
    state.shutdown().await;
}

#[tokio::test]
async fn the_badwords_are_swapped_as_a_whole() {
    let state = state();
    assert_eq!(state.badwords().patterns, 0);

    state.publish_badwords(Badwords::empty());
    assert_eq!(state.badwords().patterns, 0);
}
