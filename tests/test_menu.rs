use super::*;
use crate::term::key::{KEY_CONTROL, KEY_F1, KEY_INS, KEY_NULL};
use crate::term::{ByteIo, Pipe};
use std::time::Duration;

fn pair() -> (Terminal, Pipe) {
    let (client, server) = Pipe::new();
    (Terminal::new(ByteIo::Pipe(server)), client)
}

/// `igetch_reset` swallows anything queued in its first 100 ms window, so a key answering
/// `press_any_key` has to arrive after it has gone quiet.
const AFTER_RESET: Duration = Duration::from_millis(250);

/// Hand the terminal a key once its input reset has settled, then give the pipe back so the
/// caller can read what was drawn.
fn push_key_later(client: Pipe, key: &'static [u8]) -> tokio::task::JoinHandle<Pipe> {
    tokio::spawn(async move {
        tokio::time::sleep(AFTER_RESET).await;
        client.push(key);
        client
    })
}

/// A root menu with a screen body, an action item, and a submenu that holds only the
/// parent marker.
const SAMPLE: &str = concat!(
    "%S_MAIN\n",
    "  welcome\n",
    "%\n",
    "%menu TOP\n",
    "title 0, 0, \"top\"\n",
    "screen 1, 0, S_MAIN\n",
    "!BOARD 4, 5, 0, 0, \"Board\", \"(B) boards\"\n",
    "@EXITBBS 0, 0, 0, 0, \"Goodbye\", \"(G) leave\"\n",
    "%\n",
    "%menu BOARD\n",
    "screen 1, 0, S_MAIN\n",
    "!.. 4, 5, 0, 0, \"Up\", \"(U) up\"\n",
    "%\n",
);

fn sample() -> MenuSet {
    MenuSet::parse(SAMPLE.as_bytes()).expect("sample menu config")
}

/// `checkpriv`/`checklevel2` against a fixed user, plus a favorite list for filtered menus.
#[derive(Default)]
struct Levels {
    priv_mask: i32,
    level: i32,
    favorites: Vec<i32>,
}

impl MenuView for Levels {
    fn permits(&self, item: &MenuItem) -> bool {
        is_permitted(self.priv_mask, self.level, item)
    }

    fn filter_allows(&self, _filter: Action, item: &MenuItem) -> bool {
        self.favorites.contains(&item.priv_mask)
    }

    fn bbs_name(&self) -> &str {
        "test bbs"
    }
}

/// A file with one menu of `items`, each an `@` action with the given priv and level.
fn one_menu(items: &[(i16, i16, i32, i32, &str)]) -> String {
    let mut data = String::from("%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n");
    for (n, (row, col, priv_mask, level, name)) in items.iter().enumerate() {
        data.push_str(&format!(
            "@EXITBBS {row}, {col}, {priv_mask}, {level}, \"{name}{n}\", \"({name})\"\n"
        ));
    }
    data.push_str("%\n");
    data
}

#[test]
fn a_menu_is_its_title_screen_and_items() {
    let set = sample();
    assert_eq!(set.menus.len(), 2);
    assert_eq!(set.screens.len(), 1);
    assert_eq!(set.screens[0].name, "S_MAIN");

    let top = set.menu(set.menu_by_name("TOP").unwrap()).unwrap();
    assert_eq!(top.items.len(), 2);
    assert!(top.title.show);
    assert_eq!(top.title.text, "top");
    assert!(top.screen_show);
    assert_eq!(top.screen_row, 1);
    assert_eq!(top.screen_col, 0);

    let board = set.item(top, 0).unwrap();
    assert!(board.submenu);
    assert_eq!(board.action, "BOARD");
    assert_eq!(board.row, 4);
    assert_eq!(board.col, 5);
    assert_eq!(board.name, "Board");
    assert_eq!(board.text, "(B) boards");
    assert_eq!(board.action_menu_id, set.menu_by_name("BOARD"));

    let goodbye = set.item(top, 1).unwrap();
    assert!(!goodbye.submenu);
    assert_eq!(goodbye.action_cmd, Some(Action::ExitBbs));
}

#[test]
fn the_screen_body_is_every_byte_between_the_markers() {
    let set = sample();
    // One `CTRL_SEQ_CLR_LINE` per line, and the newline paired with a carriage return.
    assert_eq!(set.screens[0].buf, b"\x1b[K  welcome\r\n".to_vec());
}

#[test]
fn a_bare_newline_in_a_screen_body_is_made_into_crlf() {
    let data = "%S_ONE\nfirst\nsecond\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    assert_eq!(
        set.screens[0].buf,
        b"\x1b[Kfirst\r\n\x1b[Ksecond\r\n".to_vec()
    );
}

#[test]
fn comments_and_blank_lines_are_not_directives() {
    let data = "%S_ONE\nx\n%\n# a comment at the top level\n%menu M\nscreen 0, 0, S_ONE\n# not a directive inside a menu\n\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    assert_eq!(set.menus[0].items.len(), 1);
}

#[test]
fn an_unknown_keyword_is_ignored_inside_a_menu_and_fatal_outside() {
    let inside = one_menu(&[(0, 0, 0, 0, "x")]);
    let mut inside = inside.replace("screen 0, 0, S_ONE\n", "screen 0, 0, S_ONE\nbogus 1, 2\n");
    inside = inside.replace("\"x0\"", "\"x\"");
    assert_eq!(
        MenuSet::parse(inside.as_bytes()).unwrap().menus[0]
            .items
            .len(),
        1
    );

    let outside = "%S_ONE\n%\nbogus 1, 2\n";
    let err = MenuSet::parse(outside.as_bytes()).unwrap_err();
    assert_eq!(err.to_string(), "Error in menu config line 3");
}

#[test]
fn a_screen_without_a_definition_fails_the_load() {
    let data = "%menu M\nscreen 1, 0, MISSING\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    let err = MenuSet::parse(data.as_bytes()).unwrap_err();
    assert!(
        matches!(&err, MenuError::Unresolved(m) if m == "Undefined menu screen [MISSING]"),
        "{err}"
    );
}

#[test]
fn an_unknown_name_of_any_kind_fails_the_load() {
    let action = "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@NOPE 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(
        format!("{:?}", MenuSet::parse(action.as_bytes()).unwrap_err())
            .contains("Undefined menu action cmd handler [NOPE]")
    );

    let submenu = "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n!NOPE 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(
        format!("{:?}", MenuSet::parse(submenu.as_bytes()).unwrap_err())
            .contains("Undefined sub menu id [NOPE]")
    );

    let filter = "%S_ONE\n%\n%menu NOPE\nuse_filter\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(
        format!("{:?}", MenuSet::parse(filter.as_bytes()).unwrap_err())
            .contains("Undefined menu filter handler [NOPE]")
    );
}

#[test]
fn a_filtered_menu_is_named_after_its_filter() {
    let data = concat!(
        "%S_ONE\n%\n",
        "%menu M_FAVOR_SECTION\n",
        "use_filter\n",
        "screen 0, 0, S_ONE\n",
        "@LIST_SECTION 4, 1, 7, 0, \"Board\", \"boards\"\n",
        "%\n"
    );
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    assert!(set.menus[0].use_filter);
    assert_eq!(set.menus[0].filter, Some(Action::FavorSectionFilter));
}

#[test]
fn names_are_ascii_identifiers_within_their_buffers() {
    let long = format!(
        "%S_ONE\n%\n%menu {}\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n",
        "M".repeat(MAX_MENU_NAME_LENGTH)
    );
    assert!(
        format!("{}", MenuSet::parse(long.as_bytes()).unwrap_err())
            .starts_with("Too longer menu name in menu config line 3")
    );

    // The C locale rejects a byte at or above 0x80, so a Chinese menu name is a parse error.
    let wide = "%S_ONE\n%\n%menu 菜单\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(
        format!("{}", MenuSet::parse(wide.as_bytes()).unwrap_err())
            .starts_with("Error menu name in menu config line 3")
    );

    // An action gets the same charset, which does allow `-` and `_` but nothing else, and
    // `..` is exempt from the test entirely.
    let bad_action =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXIT.BBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(
        format!("{}", MenuSet::parse(bad_action.as_bytes()).unwrap_err())
            .starts_with("Error menu item action in menu config line 5")
    );

    let ok_action =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@LIST_ONLINE_USER 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    assert!(MenuSet::parse(ok_action.as_bytes()).is_ok());
}

#[test]
fn quoted_names_escape_with_a_backslash_and_hold_no_space() {
    let escaped =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"a\\\"b\", \"text\"\n%\n";
    assert_eq!(
        MenuSet::parse(escaped.as_bytes()).unwrap().items[0].name,
        "a\"b"
    );

    let spaced =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"a b\", \"text\"\n%\n";
    assert!(
        format!("{}", MenuSet::parse(spaced.as_bytes()).unwrap_err())
            .starts_with("Error menu item name in menu config line 5")
    );

    // An unterminated name is the same failure.
    let unterminated =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"abc, \"text\"\n%\n";
    assert!(
        format!("{}", MenuSet::parse(unterminated.as_bytes()).unwrap_err())
            .starts_with("Error menu item name in menu config line 5")
    );
}

#[test]
fn the_text_field_keeps_spaces_and_the_name_does_not() {
    let data = "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"(G) leave  the  BBS\"\n%\n";
    assert_eq!(
        MenuSet::parse(data.as_bytes()).unwrap().items[0].text,
        "(G) leave  the  BBS"
    );

    // The text search starts at the first quote after the name, so junk in between is
    // swallowed — and junk after the text is the fatal extra content.
    let between =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\" junk, \"text\"\n%\n";
    let set = MenuSet::parse(between.as_bytes()).unwrap();
    assert_eq!(
        (set.items[0].name.as_str(), set.items[0].text.as_str()),
        ("x", "text")
    );

    let after =
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"text\" tail\n%\n";
    assert!(
        format!("{}", MenuSet::parse(after.as_bytes()).unwrap_err())
            .starts_with("Unknown extra content in menu config line 5")
    );
}

#[test]
fn item_fields_are_digits_only() {
    let data = "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 4x, 0, 0, 0, \"x\", \"text\"\n%\n";
    assert!(
        format!("{}", MenuSet::parse(data.as_bytes()).unwrap_err())
            .starts_with("Error menu item row in menu config line 5")
    );

    // A missing field is the same message, which is how C reports a short line.
    let short = "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 4, 0, 0\n%\n";
    assert!(
        format!("{}", MenuSet::parse(short.as_bytes()).unwrap_err())
            .starts_with("Error menu item level in menu config line 5")
    );
}

#[test]
fn the_item_buffers_cap_at_their_c_sizes() {
    let name = format!(
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"{}\", \"text\"\n%\n",
        "n".repeat(MAX_MENUITEM_NAME_LENGTH)
    );
    assert!(
        format!("{}", MenuSet::parse(name.as_bytes()).unwrap_err())
            .starts_with("Too longer menu name in menu config line 5")
    );

    let text = format!(
        "%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"{}\"\n%\n",
        "t".repeat(MAX_MENUITEM_TEXT_LENGTH)
    );
    assert!(
        format!("{}", MenuSet::parse(text.as_bytes()).unwrap_err())
            .starts_with("Too longer menu item text in menu config line 5")
    );

    let title = format!(
        "%S_ONE\n%\n%menu M\ntitle 0, 0, \"{}\"\nscreen 0, 0, S_ONE\n@EXITBBS 0, 0, 0, 0, \"x\", \"t\"\n%\n",
        "a".repeat(MAX_MENUTITLE_TEXT_LENGTH)
    );
    assert!(
        format!("{}", MenuSet::parse(title.as_bytes()).unwrap_err())
            .starts_with("Too longer menu title text in menu config line 4")
    );
}

#[test]
fn a_screen_reference_is_cut_to_its_buffer_and_then_fails_to_resolve() {
    // C `strncpy`s the reference with no length check; the truncated name is what the
    // post-pass looks up, so an overlong one reports as an undefined screen.
    let data = format!(
        "%S_ONE\n%\n%menu M\nscreen 0, 0, {}\n@EXITBBS 0, 0, 0, 0, \"x\", \"text\"\n%\n",
        "S".repeat(MAX_MENU_SCR_NAME_LENGTH + 4)
    );
    assert!(
        format!("{}", MenuSet::parse(data.as_bytes()).unwrap_err())
            .contains(&"S".repeat(MAX_MENU_SCR_NAME_LENGTH - 1))
    );
}

#[test]
fn a_screen_body_bigger_than_the_shared_buffer_is_fatal() {
    // `MAX_MENU_SCR_BUF_LENGTH * MAX_MENUS` bytes, with one byte kept spare.
    let line = "x".repeat(LINE_BUFFER_LEN - 1);
    let mut data = String::from("%S_BIG\n");
    for _ in 0..(SCREEN_BUF_CAPACITY / (line.len() + 3) + 2) {
        data.push_str(&line);
        data.push('\n');
    }
    data.push_str("%\n");
    assert!(
        format!("{}", MenuSet::parse(data.as_bytes()).unwrap_err())
            .contains("Menu screen buffer depleted")
    );
}

#[test]
fn a_line_longer_than_the_read_buffer_counts_as_several() {
    let long = "a".repeat(LINE_BUFFER_LEN * 2);
    let tmpl = format!("{long}\n");
    let lines = fgets_lines(tmpl.as_bytes());
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].len(), LINE_BUFFER_LEN - 1);
    assert!(lines[2].ends_with(b"\n"));

    // The pieces each count as a line, so a long screen body inflates the number an error
    // later in the file reports: 2049 bytes occupy lines 2, 3 and 4.
    let data = format!("%S_ONE\n{long}\n%\n%menu TOP\n!NO.PE 0, 0, 0, 0, \"x\", \"y\"\n%\n");
    assert_eq!(
        MenuSet::parse(data.as_bytes()).unwrap_err().to_string(),
        "Error menu item action in menu config line 7"
    );
}

#[test]
fn a_menu_without_a_screen_reference_fails_the_load() {
    // Every menu in the real files carries a `screen` line, because an empty reference
    // resolves as little as a wrong one does.
    let data = "%menu M\n@EXITBBS 0, 0, 0, 0, \"x\", \"y\"\n%\n";
    let err = MenuSet::parse(data.as_bytes()).unwrap_err();
    assert_eq!(err.to_string(), "Undefined menu screen []");
}

#[test]
fn the_first_menu_in_the_file_is_the_root() {
    let set = sample();
    let nav = MenuNav::new(&set);
    assert_eq!(nav.current_menu().unwrap().name, "TOP");
    assert_eq!(nav.depth(), 0);
}

#[test]
fn items_at_zero_zero_stack_below_the_last_written_position() {
    let data = one_menu(&[(4, 5, 0, 0, "a"), (0, 0, 0, 0, "b"), (0, 0, 0, 0, "c")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let mut nav = MenuNav::new(&set);
    assert!(nav.layout(set.menu(0).unwrap(), &Levels::default()));
    assert_eq!(&nav.r_row[..3], &[4, 5, 6]);
    assert_eq!(&nav.r_col[..3], &[5, 5, 5]);
    assert_eq!(nav.cursor(), 0);
}

#[test]
fn an_invisible_item_never_takes_a_row() {
    let data = one_menu(&[(4, 5, 128, 64, "admin"), (0, 0, 0, 0, "user")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();

    // The item asking for priv 128 and level 64 is hidden from a plain user.
    let mut nav = MenuNav::new(&set);
    assert!(nav.layout(set.menu(0).unwrap(), &Levels::default()));
    assert!(!nav.display_at(0));
    assert!(nav.display_at(1));
    assert_eq!(
        nav.r_row[1], 4,
        "the hidden item leaves the pen where it was"
    );
    assert_eq!(nav.cursor(), 1, "the cursor goes to the first visible item");

    let admin = Levels {
        priv_mask: 128,
        level: 64,
        favorites: vec![],
    };
    let mut nav = MenuNav::new(&set);
    assert!(nav.layout(set.menu(0).unwrap(), &admin));
    assert!(nav.display_at(0) && nav.display_at(1));
    assert_eq!(&nav.r_row[..2], &[4, 5]);
}

#[test]
fn a_missing_privilege_bit_or_level_hides_the_item() {
    let item = MenuItem {
        priv_mask: 128,
        level: 3,
        ..sample_item()
    };
    assert!(!is_permitted(0, 9, &item), "the bits are missing");
    assert!(!is_permitted(128, 2, &item), "the level is too low");
    assert!(is_permitted(255, 3, &item));

    let public = MenuItem {
        priv_mask: 0,
        level: 0,
        ..sample_item()
    };
    assert!(is_permitted(0, 0, &public));
}

fn sample_item() -> MenuItem {
    MenuItem {
        row: 0,
        col: 0,
        action: String::new(),
        action_menu_id: None,
        action_cmd: None,
        submenu: false,
        priv_mask: 0,
        level: 0,
        name: String::new(),
        text: String::new(),
    }
}

#[test]
fn a_page_break_restarts_the_pen_at_the_page_position() {
    let mut data = String::from("%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\npage 4, 1, 2\n");
    for (n, (row, col)) in [(4, 1), (0, 0), (5, 41), (0, 0)].iter().enumerate() {
        data.push_str(&format!(
            "@EXITBBS {row}, {col}, 0, 0, \"i{n}\", \"(i{n})\"\n"
        ));
    }
    data.push_str("%\n");
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let mut nav = MenuNav::new(&set);
    assert!(nav.layout(set.menu(0).unwrap(), &Levels::default()));
    assert_eq!(&nav.page_id[..4], &[0, 0, 1, 1]);
    assert_eq!(&nav.r_row[..4], &[4, 5, 5, 6]);
    assert_eq!(&nav.r_col[..4], &[1, 1, 41, 41]);
}

#[test]
fn a_filter_hides_the_items_it_is_not_handed() {
    let data = concat!(
        "%S_ONE\n%\n%menu M_FAVOR_SECTION\nuse_filter\nscreen 0, 0, S_ONE\n",
        "@LIST_SECTION 4, 1, 7, 0, \"kept\", \"(k)\"\n",
        "@LIST_SECTION 0, 0, 8, 0, \"dropped\", \"(d)\"\n",
        "%\n"
    );
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let view = Levels {
        favorites: vec![7],
        ..Levels::default()
    };
    let mut nav = MenuNav::new(&set);
    assert!(nav.layout(set.menu(0).unwrap(), &view));
    assert!(nav.display_at(0) && !nav.display_at(1));
}

#[test]
fn a_visible_cursor_position_keeps_its_place() {
    let data = one_menu(&[(4, 1, 0, 0, "a"), (0, 0, 0, 0, "b")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let mut nav = MenuNav::new(&set);
    nav.set_cursor(1);
    assert!(nav.layout(set.menu(0).unwrap(), &Levels::default()));
    assert_eq!(
        nav.cursor(),
        1,
        "the layout redraws but does not move the cursor"
    );
}

#[test]
fn a_cursor_that_can_no_longer_be_shown_moves_to_the_first_visible_item() {
    // The second item asks for a privilege the user does not have, so a cursor parked on it
    // has nowhere to stay and the scan hands it the first item the menu will show.
    let data = one_menu(&[
        (4, 1, 0, 0, "a"),
        (0, 0, 128, 0, "admin"),
        (0, 0, 0, 0, "c"),
    ]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let mut nav = MenuNav::new(&set);
    nav.set_cursor(1);
    assert!(nav.layout(set.menu(0).unwrap(), &Levels::default()));
    assert_eq!(nav.cursor(), 0);
}

#[tokio::test]
async fn going_back_up_keeps_the_item_the_parent_menu_was_on() {
    let data = concat!(
        "%S_ONE\n%\n",
        "%menu TOP\nscreen 0, 0, S_ONE\n",
        "@LIST_SECTION 4, 1, 0, 0, \"Intro\", \"(i)\"\n",
        "!BOARD 0, 0, 0, 0, \"Boards\", \"(b)\"\n",
        "@EXITBBS 0, 0, 0, 0, \"Goodbye\", \"(g)\"\n",
        "%\n",
        "%menu BOARD\nscreen 0, 0, S_ONE\n",
        "@LIST_SECTION 4, 1, 0, 0, \"Sub\", \"(s)\"\n",
        "%\n"
    );
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    assert_eq!(nav.cursor(), 0);

    // Down onto the submenu item, into it, and back out again.
    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    assert_eq!(nav.cursor(), 1);
    assert_eq!(
        nav.control(&mut term, &Levels::default(), CR).await,
        MenuFlow::NoRedraw
    );
    assert_eq!(nav.depth(), 1);
    assert_eq!(nav.cursor(), 0, "a submenu starts at its first item");

    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    assert_eq!(nav.cursor(), 0, "the submenu has one item to walk");

    assert_eq!(
        nav.control(&mut term, &Levels::default(), KEY_ESC).await,
        MenuFlow::Redraw
    );
    assert_eq!(nav.depth(), 0);
    nav.draw(&mut term, &Levels::default()).await;
    assert_eq!(nav.cursor(), 1, "the parent menu is where it was left");
}

#[tokio::test]
async fn entering_a_submenu_pushes_the_path_and_an_action_reports_itself() {
    let set = sample();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    assert_eq!(nav.draw(&mut term, &Levels::default()).await, Draw::Show);

    assert_eq!(
        nav.control(&mut term, &Levels::default(), CR).await,
        MenuFlow::NoRedraw
    );
    assert_eq!(nav.depth(), 1);
    assert_eq!(nav.current_menu().unwrap().name, "BOARD");

    // BOARD holds only the parent marker, whose CR walks back up.
    assert_eq!(nav.current_item().unwrap().action, "..");
    assert_eq!(
        nav.control(&mut term, &Levels::default(), CR).await,
        MenuFlow::Redraw
    );
    assert_eq!(nav.depth(), 0);
    // The session loop repaints on Redraw, which is what rebuilds the layout arrays.
    assert_eq!(nav.draw(&mut term, &Levels::default()).await, Draw::Show);

    nav.control(&mut term, &Levels::default(), KEY_END).await;
    assert_eq!(
        nav.control(&mut term, &Levels::default(), CR).await,
        MenuFlow::Run(Action::ExitBbs, "Goodbye".into())
    );
}

#[tokio::test]
async fn the_left_key_only_leaves_when_the_menu_set_allows_it() {
    let set = sample();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;

    // At the root of a set that does not allow exit, left jumps to the last public item.
    assert_eq!(
        nav.control(&mut term, &Levels::default(), KEY_LEFT).await,
        MenuFlow::NoRedraw
    );
    assert_eq!(nav.cursor(), 1);

    nav.set_allow_exit(true);
    assert_eq!(
        nav.control(&mut term, &Levels::default(), KEY_ESC).await,
        MenuFlow::ExitMenu
    );
}

#[tokio::test]
async fn down_steps_one_item_and_the_page_keys_cross_pages() {
    let mut data = String::from("%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\npage 4, 1, 2\n");
    for n in 0..3 {
        data.push_str(&format!("@EXITBBS 4, 1, 0, 0, \"i{n}\", \"(i{n})\"\n"));
    }
    data.push_str("%\n");
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    assert_eq!(nav.cursor(), 0);

    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    assert_eq!(nav.cursor(), 1);
    nav.control(&mut term, &Levels::default(), KEY_PGDN).await;
    assert_eq!(nav.cursor(), 2, "the first item of page 1");
    nav.control(&mut term, &Levels::default(), KEY_PGUP).await;
    assert_eq!(nav.cursor(), 1);
    nav.control(&mut term, &Levels::default(), KEY_UP).await;
    assert_eq!(nav.cursor(), 0);
    nav.control(&mut term, &Levels::default(), KEY_UP).await;
    assert_eq!(
        nav.cursor(),
        2,
        "the walk wraps past the top, onto the last page"
    );
}

#[tokio::test]
async fn hidden_items_are_walked_over_and_home_and_end_find_the_ends() {
    let data = one_menu(&[
        (4, 1, 128, 0, "admin"),
        (0, 0, 0, 0, "user"),
        (0, 0, 0, 0, "open"),
        (0, 0, 128, 0, "admin2"),
    ]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    assert_eq!(nav.cursor(), 1, "the first item is hidden");

    nav.control(&mut term, &Levels::default(), KEY_END).await;
    assert_eq!(nav.cursor(), 2, "the last visible item");
    nav.control(&mut term, &Levels::default(), KEY_HOME).await;
    assert_eq!(nav.cursor(), 1);
    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    assert_eq!(nav.cursor(), 2, "the hidden item at 3 is skipped");
    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    assert_eq!(
        nav.cursor(),
        1,
        "wrapping only offers the other visible item"
    );
}

/// `99c5e32` bounded the two scans by `item_count`: before that a menu in which nothing is
/// displayed wrapped for ever, so the key looked like a hung session.
/// The other half of the `99c5e32` left-key fix: the stored position has to stay a valid
/// index, so a root whose items are all private falls back to the first one.
#[tokio::test]
async fn the_left_key_falls_back_to_the_first_item_when_none_is_public() {
    let data = one_menu(&[(0, 0, 1, 0, "p1"), (1, 0, 1, 0, "p2"), (2, 0, 1, 0, "p3")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    let view = Levels {
        priv_mask: 1,
        ..Levels::default()
    };
    nav.draw(&mut term, &view).await;
    nav.control(&mut term, &view, KEY_END).await;
    assert_eq!(nav.cursor(), 2, "the items are displayed");

    assert_eq!(
        nav.control(&mut term, &view, KEY_LEFT).await,
        MenuFlow::NoRedraw
    );
    assert_eq!(
        nav.cursor(),
        0,
        "none of them is public, so the scan gives item 0"
    );
}

#[tokio::test]
async fn stepping_through_a_menu_with_nothing_displayed_stops() {
    let data = one_menu(&[
        (0, 0, 0, 9, "hidden1"),
        (1, 0, 0, 9, "hidden2"),
        (2, 0, 0, 9, "hidden3"),
    ]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, client) = pair();
    let keyed = push_key_later(client, b"\r");
    let mut nav = MenuNav::new(&set);
    assert_eq!(nav.draw(&mut term, &Levels::default()).await, Draw::Empty);
    let _client = keyed.await.unwrap();
    assert_eq!(nav.cursor(), 0);

    for key in [KEY_DOWN, KEY_UP, KEY_PGDN, KEY_PGUP] {
        assert_eq!(
            nav.control(&mut term, &Levels::default(), key).await,
            MenuFlow::NoRedraw,
            "the {key} walk over hidden items returns"
        );
        assert!(
            nav.cursor() < 3,
            "the {key} walk still lands on an item of this menu"
        );
    }
}

#[tokio::test]
async fn a_letter_key_jumps_to_the_item_with_that_initial() {
    let data = one_menu(&[(4, 1, 0, 0, "Goodbye"), (0, 0, 0, 0, "agree")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;

    nav.control(&mut term, &Levels::default(), u32::from(b'a'))
        .await;
    assert_eq!(nav.cursor(), 1, "the match is case insensitive");
    nav.control(&mut term, &Levels::default(), u32::from(b'z'))
        .await;
    assert_eq!(
        nav.cursor(),
        1,
        "an unmatched letter leaves the cursor alone"
    );

    // A name opening with a multibyte character has no ASCII initial to match, as in C.
    let wide = one_menu(&[(4, 1, 0, 0, "好")]);
    let set = MenuSet::parse(wide.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    nav.control(&mut term, &Levels::default(), u32::from(b'h'))
        .await;
    assert_eq!(nav.cursor(), 0, "the wide name was not matched");
}

/// C's `99c5e32` guard rejects the key before `isalnum` reads past the ctype tables; the
/// port never reaches `is_ascii_alphanumeric` with a byte it cannot name.
#[tokio::test]
async fn only_a_printable_ascii_key_selects_a_hot_key() {
    let data = one_menu(&[(0, 0, 0, 0, "a"), (1, 0, 0, 0, "b")]);
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;

    for key in [KEY_F1, KEY_INS, KEY_NULL, KEY_CONTROL] {
        assert_eq!(
            nav.control(&mut term, &Levels::default(), key).await,
            MenuFlow::NoRedraw
        );
        assert_eq!(nav.cursor(), 0, "the {key} code is not a hot key");
    }
    nav.control(&mut term, &Levels::default(), u32::from(b'b'))
        .await;
    assert_eq!(nav.cursor(), 1, "a printable letter still is");
}

#[tokio::test]
async fn the_cursor_is_two_columns_left_of_its_item() {
    let set = sample();
    let (mut term, mut client) = pair();
    let mut nav = MenuNav::new(&set);
    assert_eq!(nav.draw(&mut term, &Levels::default()).await, Draw::Show);
    term.flush().await.unwrap();
    let out = String::from_utf8_lossy(&client.take_available()).into_owned();
    // The `!BOARD` item sits at 4, 5, so its `>` goes to row 4 column 3.
    assert!(out.contains("\x1b[4;3H>"), "{out:?}");
}

#[tokio::test]
async fn the_title_and_the_screen_body_are_drawn_with_the_items() {
    let set = sample();
    let (mut term, mut client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    term.flush().await.unwrap();
    let out = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(out.contains("welcome"), "{out}");
    assert!(out.contains("(B) boards"), "{out}");
    assert!(out.contains("(G) leave"), "{out}");
    // `title 0, 0` is the standard top row, which prints the site name beside the title.
    assert!(out.contains("top"), "{out}");
    assert!(out.contains("test bbs"), "{out}");
}

#[tokio::test]
async fn a_menu_with_nothing_to_show_says_so_and_waits() {
    let data = one_menu(&[(4, 1, 128, 0, "admin")]);
    let data = data.replace("screen 0, 0, S_ONE", "screen 3, 2, S_ONE");
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, client) = pair();
    let keyed = push_key_later(client, b"\r");

    let mut nav = MenuNav::new(&set);
    assert_eq!(nav.draw(&mut term, &Levels::default()).await, Draw::Empty);
    let mut client = keyed.await.unwrap();
    term.flush().await.unwrap();
    let out = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(out.contains("没有可选项"), "{out}");
    assert!(out.contains("\x1b[3;2H"), "{out:?}");
}

#[tokio::test]
async fn an_empty_submenu_backs_out_a_level() {
    let data = concat!(
        "%S_ONE\n%\n%menu TOP\nscreen 0, 0, S_ONE\n!EMPTY 4, 1, 0, 0, \"e\", \"(e)\"\n%\n",
        "%menu EMPTY\nscreen 0, 0, S_ONE\n%\n"
    );
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, client) = pair();
    // Entering EMPTY draws "没有可选项", which waits for a key, and then pops back.
    let keyed = push_key_later(client, b"\r");
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    assert_eq!(
        nav.control(&mut term, &Levels::default(), CR).await,
        MenuFlow::Redraw
    );
    let _client = keyed.await.unwrap();
    assert_eq!(nav.depth(), 0);
}

#[test]
fn a_menu_set_with_no_menus_has_no_root() {
    let set = MenuSet::default();
    let nav = MenuNav::new(&set);
    assert!(nav.current_menu().is_none());
}

#[test]
fn the_action_names_are_the_bbs_cmd_table() {
    assert_eq!(Action::from_name("RunMBEM"), Some(Action::RunMbem));
    assert_eq!(
        Action::from_name("runmbem"),
        None,
        "the trie is case sensitive"
    );
    assert_eq!(Action::ListSection.name(), "LIST_SECTION");
    for (name, action) in Action::TABLE {
        assert_eq!(Action::from_name(name), Some(action));
        assert_eq!(action.name(), name);
    }
}

#[test]
fn the_menu_return_codes_are_the_ones_bbs_center_switches_on() {
    assert_eq!(MenuReturn::from_code(0), MenuReturn::Ok);
    assert_eq!(MenuReturn::from_code(3), MenuReturn::ExitMenu);
    assert_eq!(MenuReturn::from_code(0xff), MenuReturn::UnknownCmd);
    assert_eq!(MenuReturn::from_code(0xfe), MenuReturn::ExitBbs);
    assert_eq!(MenuReturn::from_code(1234), MenuReturn::NoRedraw);
}

/// `test_menu.c` #7: every config fragment the reference program loads, with the verdict it
/// gives. `build_conf` wraps a body in the screen block and the one menu the rest of the file
/// assumes, so most of the fragments here are bodies.
#[test]
fn the_parser_answers_the_reference_grammar_table() {
    let build_conf = |text: &str| format!("%S\nscreen text\n%\n%menu MENU1\n{text}%\n");
    let item = |fields: &str| format!("screen 0, 0, S\n!.. {fields}\n");

    let mut cases: Vec<(String, bool)> = Vec::new();
    for (text, ok) in [
        (item("0, 0, 0, 0, \"N\", \"t\""), true),
        ("screen 0, 0, S\n".to_string(), true),
        (String::new(), false),
        (
            "screen 0, 0, NOPE\n!.. 0, 0, 0, 0, \"N\", \"t\"\n".to_string(),
            false,
        ),
        (
            "screen 0, 0, S\n!NOSUCHMENU 0, 0, 0, 0, \"N\", \"t\"\n".to_string(),
            false,
        ),
        (
            "screen 0, 0, S\n@NOHANDLER 0, 0, 0, 0, \"N\", \"t\"\n".to_string(),
            false,
        ),
        (
            "screen 0, 0, S\n!.. 0, 0, 0, 0, \"N\", \"t\\\n".to_string(),
            false,
        ),
        (
            "screen 0, 0, S\nuse_filter\n!.. 0, 0, 0, 0, \"N\", \"t\"\n".to_string(),
            false,
        ),
        ("screen 0, 0, S\nuse_filter yes\n".to_string(), false),
        (
            "screen 0, 0, S\n!SU!B 0, 0, 0, 0, \"N\", \"t\"\n".to_string(),
            false,
        ),
        ("screen 0, 0, S\ntitle 0, 0\n".to_string(), false),
        ("screen 0, 0, S\ntitle 0, 0, T\n".to_string(), false),
        (
            "screen 0, 0, S\ntitle 0, 0, \"T\" junk\n".to_string(),
            false,
        ),
        ("title 0, 0, \"T\"\nscreen 0\n".to_string(), false),
        ("screen 0, x, S\n".to_string(), false),
        ("screen 0, 0, S\npage 1, 2\n".to_string(), false),
        ("screen 0, 0, S\npage 1, 2, x\n".to_string(), false),
        (item("0, 0, 0, 0, \"N\""), false),
        (item("0, 0, 0, \"N\", \"t\""), false),
        (item("x, 0, 0, 0, \"N\", \"t\""), false),
        (item("-1, 0, 0, 0, \"N\", \"t\""), false),
        (item("0, 0, 0, 0, 0, \"t\""), false),
        (item("0, 0, 0, 0, \"N t\", \"t\""), false),
        (item("0, 0, 0, 0, \"N, \"t\""), false),
        (item("0, 0, 0, 0, \"N\", t"), false),
        (item("0, 0, 0, 0, \"N\", \"t\" junk"), false),
    ] {
        cases.push((build_conf(&text), ok));
    }

    // The whole-file cases: the order of the blocks, the `%` markers on their own, and a
    // file that ends while a block is still open.
    for (text, ok) in [
        (
            "%menu MENU1\nscreen 0, 0, S\n!.. 0, 0, 0, 0, \"N\", \"t\"\n%\n%S\nscreen text\n%\n",
            true,
        ),
        ("bogus 1, 2\n%menu MENU1\nscreen 0, 0, S\n%\n", false),
        ("%menu\n%\n", false),
        ("%menu B$D\n%\n", false),
        ("%menu MENU1 EXTRA\n%\n", false),
        (
            "%S\nscreen text\n%\n%menu MENU1\nscreen 0, 0, S\n!.. 0, 0, 0, 0, \"N\", \"t\"\n",
            true,
        ),
        (
            "%menu MENU1\nscreen 0, 0, S\n!.. 0, 0, 0, 0, \"N\", \"t\"\n%\n%S\nfirst\nsecond\n",
            true,
        ),
    ] {
        cases.push((text.to_string(), ok));
    }

    // The name and text fields stop at the size of the buffer they fill.
    for (fill, ch, ok) in [
        (MAX_MENU_NAME_LENGTH - 1, 'M', true),
        (MAX_MENU_NAME_LENGTH, 'M', false),
        (MAX_MENUITEM_NAME_LENGTH - 1, 'n', true),
        (MAX_MENUITEM_NAME_LENGTH, 'n', false),
        (MAX_MENUITEM_TEXT_LENGTH - 1, 't', true),
        (MAX_MENUITEM_TEXT_LENGTH, 't', false),
    ] {
        let long = ch.to_string().repeat(fill);
        let conf = match ch {
            // A menu name and the action that points at it, both at the field's own limit:
            // the menu the action asks for is there, so only its size can refuse the line.
            'M' if ok => format!(
                "%S\nscreen text\n%\n%menu {long}\nscreen 0, 0, S\n%\n%menu MENU1\nscreen 0, 0, S\n!{long} 0, 0, 0, 0, \"N\", \"t\"\n%\n"
            ),
            'M' => format!(
                "%S\nscreen text\n%\n%menu MENU1\nscreen 0, 0, S\n!{long}M 0, 0, 0, 0, \"N\", \"t\"\n%\n"
            ),
            'n' => build_conf(&item(&format!("0, 0, 0, 0, \"{long}\", \"t\""))),
            _ => build_conf(&item(&format!("0, 0, 0, 0, \"N\", \"{long}\""))),
        };
        cases.push((conf, ok));
    }

    let wrong: Vec<&(String, bool)> = cases
        .iter()
        .filter(|(conf, ok)| MenuSet::parse(conf.as_bytes()).is_ok() != *ok)
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} fragments answer the other way: {wrong:?}",
        wrong.len(),
        cases.len()
    );
}

/// `test_menu.c` #8: the pools the reference file is capped at, and the screen buffer that
/// holds every body at once.
#[test]
fn the_pools_stop_where_the_reference_caps_them() {
    let grid = |menus: usize, items: usize| {
        let mut data = String::from("%S\nscreen text\n%\n");
        for menu in 0..menus {
            data.push_str(&format!("%menu M{menu}\nscreen 0, 0, S\n"));
            for item in 0..items {
                data.push_str(&format!("!.. {}, 0, 0, 0, \"N\", \"t\"\n", item % 20));
            }
            data.push_str("%\n");
        }
        data
    };

    for (menus, items, ok) in [
        (1, MAX_ITEMS_PER_MENU, true),
        (1, MAX_ITEMS_PER_MENU + 1, false),
        (MAX_MENUITEMS / MAX_ITEMS_PER_MENU, MAX_ITEMS_PER_MENU, true),
        (
            MAX_MENUITEMS / MAX_ITEMS_PER_MENU + 1,
            MAX_ITEMS_PER_MENU,
            false,
        ),
        (MAX_MENUS, 0, true),
        (MAX_MENUS + 1, 0, false),
    ] {
        assert_eq!(
            MenuSet::parse(grid(menus, items).as_bytes()).is_ok(),
            ok,
            "{menus} menus of {items} items"
        );
    }

    for screens in [MAX_MENUS, MAX_MENUS + 1] {
        let data: String = (0..screens)
            .map(|i| format!("%S{i:03}\nscreen text\n%\n"))
            .collect();
        assert_eq!(
            MenuSet::parse(data.as_bytes()).is_ok(),
            screens == MAX_MENUS,
            "{screens} screen records"
        );
    }
}

#[test]
fn a_deeper_nest_than_the_path_holds_refuses_the_jump() {
    // C counts the level off `MAX_MENU_DEPTH` too (`menu.c:1268-1273`); the port refuses the
    // depth it cannot record the same way.
    let mut data = String::from("%S_ONE\n%\n");
    for depth in 0..=MAX_MENU_DEPTH {
        data.push_str(&format!("%menu M{depth}\nscreen 0, 0, S_ONE\n"));
        if depth < MAX_MENU_DEPTH {
            data.push_str(&format!("!M{} 0, 0, 0, 0, \"n\", \"(n)\"\n", depth + 1));
        }
        data.push_str("@EXITBBS 0, 0, 0, 0, \"x\", \"(x)\"\n%\n");
    }
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    assert_eq!(set.menus.len(), MAX_MENU_DEPTH + 1);

    let mut nav = MenuNav::new(&set);
    for _ in 0..MAX_MENU_DEPTH - 1 {
        let dest = nav.current_item().unwrap().action_menu_id.unwrap();
        assert!(nav.push(dest));
    }
    assert_eq!(nav.depth(), MAX_MENU_DEPTH - 1);
    assert!(!nav.push(1));
}

/// The same limit driven through `menu_control` with C's ring of menus (`test_menu.c` #9):
/// every menu points at the next, so only the path arrays stop the walk, and the levels it
/// refuses draw nothing.
#[tokio::test]
async fn a_jump_past_the_depth_limit_answers_without_redrawing() {
    let ring = MAX_MENU_DEPTH + 5;
    let mut data = String::from("%S_ONE\n%\n");
    for menu in 0..ring {
        data.push_str(&format!(
            "%menu M{menu}\nscreen 0, 0, S_ONE\n!M{} 0, 0, 0, 0, \"n\", \"next\"\n%\n",
            (menu + 1) % ring
        ));
    }
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, mut client) = pair();
    let mut nav = MenuNav::new(&set);
    let view = Levels::default();

    for _ in 0..ring {
        assert_eq!(
            nav.control(&mut term, &view, CR).await,
            MenuFlow::NoRedraw,
            "the walk answers the key it was given"
        );
    }
    assert_eq!(
        nav.depth(),
        MAX_MENU_DEPTH - 1,
        "the last level the path can record is the one it stops on"
    );
    assert_eq!(nav.current_menu().unwrap().name, "M49");

    term.flush().await.unwrap();
    client.take_available();
    for _ in 0..3 {
        assert_eq!(nav.control(&mut term, &view, CR).await, MenuFlow::NoRedraw);
    }
    term.flush().await.unwrap();
    assert!(
        client.take_available().is_empty(),
        "a level the path cannot hold draws nothing"
    );
}

#[tokio::test]
async fn stepping_redraws_only_when_the_page_changes() {
    let mut data = String::from("%S_ONE\n%\n%menu M\nscreen 0, 0, S_ONE\npage 4, 1, 2\n");
    for n in 0..3 {
        data.push_str(&format!("@EXITBBS 4, 1, 0, 0, \"i{n}\", \"(i{n})\"\n"));
    }
    data.push_str("%\n");
    let set = MenuSet::parse(data.as_bytes()).unwrap();
    let (mut term, mut client) = pair();
    let mut nav = MenuNav::new(&set);
    nav.draw(&mut term, &Levels::default()).await;
    term.flush().await.unwrap();
    client.take_available();

    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    term.flush().await.unwrap();
    let within_page = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(
        within_page.contains('>') && !within_page.contains("(i0)"),
        "{within_page}"
    );

    nav.control(&mut term, &Levels::default(), KEY_DOWN).await;
    term.flush().await.unwrap();
    let across_page = String::from_utf8_lossy(&client.take_available()).into_owned();
    assert!(across_page.contains("(i2)"), "{across_page}");
}

/// The daemon's own menus, so a grammar change cannot pass quietly: `LBBS_REF_MENU=<C
/// tree>/var/menu_merged.conf cargo test --lib -- --ignored`.
#[test]
#[ignore = "needs the C tree's generated menu config"]
fn parses_the_reference_menu_conf() {
    let path = std::env::var("LBBS_REF_MENU").expect("set LBBS_REF_MENU to var/menu_merged.conf");
    let data = std::fs::read(&path).unwrap();
    let set = MenuSet::parse(&data).expect("the real menu config must parse");

    assert!(set.menus.len() > 20, "{} menus", set.menus.len());
    assert!(set.items.len() > 100, "{} items", set.items.len());
    assert_eq!(set.screens.len(), 12);
    assert_eq!(set.menu_by_name("TOPMENU").unwrap(), 0);

    // The post-pass resolved every name, or the parse would have failed.
    for menu in &set.menus {
        assert!(set.screen_of(menu).is_some(), "{}", menu.name);
    }
    for item in &set.items {
        if item.submenu {
            assert!(
                item.action_menu_id.is_some() || item.action == "..",
                "{}",
                item.action
            );
        } else {
            assert!(item.action_cmd.is_some(), "{}", item.action);
        }
    }

    let top = set.menu(set.menu_by_name("TOPMENU").unwrap()).unwrap();
    let egroup = set.item(top, 0).unwrap();
    assert_eq!(egroup.name, "EGroup");
    assert!(egroup.submenu);
    assert_eq!(
        set.menu(egroup.action_menu_id.unwrap()).unwrap().name,
        "M_EGROUP"
    );

    let board = set.menu(set.menu_by_name("M_BOARD").unwrap()).unwrap();
    assert!(board.page_item_limit > 0);
    let mut nav = MenuNav::new(&set);
    nav.set_cursor(0);
    assert!(nav.layout(board, &Public));
    assert!(nav.display.iter().any(|d| *d));
}

/// The second set the daemon loads, which `main.c:355` also refuses to start without:
/// `LBBS_REF_TOP10=<C tree>/var/bbs_top_menu.conf`. The generated file as shipped is a
/// title, a screen, and no items at all, which is why `show_top10_menu`
/// (`menu_proc.c:294`) only enters its loop when `display_menu` answers 0.
#[tokio::test]
#[ignore = "needs the C tree's generated top-ten menu config"]
async fn parses_the_reference_top10_menu_conf() {
    let path =
        std::env::var("LBBS_REF_TOP10").expect("set LBBS_REF_TOP10 to var/bbs_top_menu.conf");
    let set = MenuSet::parse(&std::fs::read(&path).unwrap())
        .expect("the real top-ten menu config must parse");
    assert_eq!(set.menus.len(), 1);
    assert_eq!(set.screens.len(), 1);

    let root = set.menu(set.menu_by_name("M_TOP10").unwrap()).unwrap();
    assert!(root.title.show);
    assert!(!root.title.text.is_empty());
    assert_eq!(root.screen_name, "S_TOP10");
    assert!(set.screen_of(root).unwrap().buf.len() > 20);
    assert!(root.items.is_empty());

    let (mut term, client) = pair();
    let keyed = push_key_later(client, b"\r");
    let mut nav = MenuNav::new(&set);
    nav.set_allow_exit(true);
    assert_eq!(nav.draw(&mut term, &Public).await, Draw::Empty);
    let _client = keyed.await.unwrap();
}

/// Walking the real graph is what proves the layout and the path arithmetic on the shipped
/// 34 menus rather than on the fixtures.
#[tokio::test]
#[ignore = "needs the C tree's generated menu config"]
async fn walks_the_reference_menu_graph() {
    let path = std::env::var("LBBS_REF_MENU").expect("set LBBS_REF_MENU to var/menu_merged.conf");
    let set = MenuSet::parse(&std::fs::read(&path).unwrap()).unwrap();
    let (mut term, _client) = pair();
    let mut nav = MenuNav::new(&set);

    assert_eq!(nav.draw(&mut term, &Public).await, Draw::Show);
    let root_items = nav.current_menu().unwrap().items.len();

    // Every submenu of the root opens, lays out, and comes back to the same place.
    for pos in 0..root_items {
        nav.set_cursor(pos);
        let item = nav.current_item().unwrap();
        let Some(dest) = item.action_menu_id else {
            continue;
        };
        assert!(nav.push(dest));
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.cursor(), 0);
        assert_eq!(nav.draw(&mut term, &Public).await, Draw::Show);
        assert!(nav.pop());
        assert_eq!(nav.depth(), 0);
        // The repaint `KEY_LEFT` asks for must find the root menu on the item it was left
        // on, which is the one thing the shipped tree cannot show on a single level.
        assert_eq!(nav.draw(&mut term, &Public).await, Draw::Show);
        assert_eq!(nav.cursor(), pos, "backing out of {}", set.menus[dest].name);
    }

    // The left key at the root parks on a public item instead of leaving.
    nav.set_cursor(0);
    nav.draw(&mut term, &Public).await;
    assert_eq!(
        nav.control(&mut term, &Public, KEY_LEFT).await,
        MenuFlow::NoRedraw
    );
    assert!(nav.display_at(nav.cursor()));
}
