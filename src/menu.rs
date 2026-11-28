/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The menu subsystem: the `menu.conf` grammar, the layout pass, and the cursor navigation.
//!
//! Port of `src/menu.c` (`load_menu`, `display_menu_cursor`, `display_menu_current_page`,
//! `display_menu`, `menu_control`) plus the action table of `src/bbs_cmd.c`.
//!
//! The C daemon builds the menus once at startup in a POSIX shared-memory segment,
//! `mprotect`s that segment read-only, and forks. Every child walks the same tables with
//! scratch arrays that live in its private copy of `MENU_SET` — the id path, the cursor, the
//! resolved rows and page numbers. That split is why there are two types here: [`MenuSet`] is
//! parsed once, never changes, and belongs behind an `Arc`, while [`MenuNav`] is one session's
//! walk over it.
//!
//! Parsing is all-or-nothing, as in the original: a syntax error, an overlong field, a
//! depleted screen buffer, or a name the post-pass cannot resolve aborts the whole file,
//! because `main.c:350` treats a failed `load_menu` as a reason not to start.
//!
//! Two of the guards C added to this layer are structural here instead of code: a menu id
//! reaches its entry through [`MenuSet::menu`] and a menu name through the name dictionary, so
//! neither lookup can return anything but a valid menu, and a reload replaces the `Arc` holding
//! the [`MenuSet`] rather than reusing the table the old one was built in.

use std::collections::HashMap;
use std::fmt;

use tracing::{debug, error, warn};

use crate::paths::LINE_BUFFER_LEN;
use crate::screen;
use crate::term::Terminal;
use crate::term::key::{
    CR, KEY_DOWN, KEY_END, KEY_ESC, KEY_HOME, KEY_LEFT, KEY_PGDN, KEY_PGUP, KEY_RIGHT, KEY_UP,
};

/// `menu_constant_t` from `include/menu.h`. Each is a buffer size, so the longest accepted
/// value is one byte short of it: C compares the field against `sizeof(field) - 1` and rejects
/// the line rather than truncating it.
pub const MAX_MENU_NAME_LENGTH: usize = 30;
pub const MAX_ITEMS_PER_MENU: usize = 256;
pub const MAX_MENUITEM_NAME_LENGTH: usize = 256;
pub const MAX_MENUITEM_TEXT_LENGTH: usize = 200;
pub const MAX_MENUITEM_ACTION_LENGTH: usize = 30;
pub const MAX_MENUTITLE_TEXT_LENGTH: usize = 100;
pub const MAX_MENU_SCR_NAME_LENGTH: usize = 20;
pub const MAX_MENU_SCR_BUF_LENGTH: usize = 2000;
pub const MAX_MENUS: usize = 256;
pub const MAX_MENUITEMS: usize = 5120;
pub const MAX_MENU_DEPTH: usize = 50;

/// The one bump-allocated screen buffer of `MENU_SET`, shared by every screen body: a screen
/// may run past `MAX_MENU_SCR_BUF_LENGTH` as long as the total holds.
const SCREEN_BUF_CAPACITY: usize = MAX_MENU_SCR_BUF_LENGTH * MAX_MENUS;

/// `MENU_CONF_DELIM_WITH_SPACE` and `MENU_CONF_DELIM_WITHOUT_SPACE` (`menu.c:50`). The second
/// set is what lets a quoted text field keep spaces and commas.
const DELIM_WITH_SPACE: [u8; 5] = [b' ', b',', b'\t', b'\r', b'\n'];
const DELIM_WITHOUT_SPACE: [u8; 2] = [b'\r', b'\n'];

/// `MENU_ID`, `MENU_ITEM_ID` and `MENU_SCREEN_ID`: indices into the pools. The item arrays and
/// the tries speak in these, so they stay plain numbers.
pub type MenuId = usize;
pub type MenuItemId = usize;
pub type MenuScreenId = usize;

/// `bbs_cmd_list` (`bbs_cmd.c:19`), the twenty action names `menu.conf` may bind an `@` item
/// to. Matching is case-sensitive — the trie indexes raw bytes, and `RunMBEM` is the only
/// mixed-case name — and an unbound name fails the load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ListSection,
    RunMbem,
    ExitBbs,
    Eula,
    License,
    Copyright,
    Version,
    ReloadConf,
    DataMgr,
    Shutdown,
    FavorSectionFilter,
    ViewExArticle,
    ListExSection,
    ShowTop10Menu,
    LocateArticle,
    FavorTopic,
    ListUser,
    ListOnlineUser,
    EditIntro,
    EditSign,
}

/// The table in source order, so [`Action::name`] and [`Action::from_name`] cannot drift apart.
impl Action {
    const TABLE: [(&'static str, Action); 20] = [
        ("LIST_SECTION", Action::ListSection),
        ("RunMBEM", Action::RunMbem),
        ("EXITBBS", Action::ExitBbs),
        ("EULA", Action::Eula),
        ("LICENSE", Action::License),
        ("COPYRIGHT", Action::Copyright),
        ("VERSION", Action::Version),
        ("RELOADCONF", Action::ReloadConf),
        ("DATAMGR", Action::DataMgr),
        ("SHUTDOWN", Action::Shutdown),
        ("M_FAVOR_SECTION", Action::FavorSectionFilter),
        ("VIEW_EX_ARTICLE", Action::ViewExArticle),
        ("LIST_EX_SECTION", Action::ListExSection),
        ("TOP10", Action::ShowTop10Menu),
        ("LOCATE_ARTICLE", Action::LocateArticle),
        ("FAVOR_TOPIC", Action::FavorTopic),
        ("LIST_USER", Action::ListUser),
        ("LIST_ONLINE_USER", Action::ListOnlineUser),
        ("EDIT_INTRO", Action::EditIntro),
        ("EDIT_SIGN", Action::EditSign),
    ];

    /// `get_cmd_handler`. A `use_filter` menu is looked up by *menu* name, which is why
    /// `M_FAVOR_SECTION` serves both as an action name and as a filter.
    pub fn from_name(name: &str) -> Option<Action> {
        Action::TABLE
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, a)| *a)
    }

    pub fn name(self) -> &'static str {
        Action::TABLE
            .iter()
            .find(|(_, a)| *a == self)
            .map(|(n, _)| *n)
            .unwrap_or("<unknown>")
    }
}

/// `menu_return_t` from `include/bbs_cmd.h`. Handlers return these to the session loop, whose
/// switch is `bbs_center` (`bbs_main.c:210`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuReturn {
    Ok,
    Redraw,
    NoRedraw,
    ExitMenu,
    UnknownCmd,
    ExitBbs,
}

impl MenuReturn {
    /// The enum's values are `0..=3`, `0xff` and `0xfe`; `bbs_center`'s `default:` arm treats
    /// anything else like `NOREDRAW`.
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => MenuReturn::Ok,
            1 => MenuReturn::Redraw,
            2 => MenuReturn::NoRedraw,
            3 => MenuReturn::ExitMenu,
            0xff => MenuReturn::UnknownCmd,
            0xfe => MenuReturn::ExitBbs,
            _ => MenuReturn::NoRedraw,
        }
    }
}

/// `display_menu`'s result: `0`, or the `-1` it returns after the "没有可选项" pause. C's third
/// answer — a `menu_return_t` from a `get_menu_by_id` that returned NULL — has no analogue,
/// because the id path here only ever holds ids this same immutable [`MenuSet`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Draw {
    Show,
    Empty,
}

/// What one `menu_control` step asks of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuFlow {
    /// Clear and repaint: the caller redraws the header, the footer, and the current menu.
    Redraw,
    /// The menu drew what it needed; leave the screen alone.
    NoRedraw,
    /// Backed out of a menu set that allows exit.
    ExitMenu,
    /// Leave the BBS.
    ExitBbs,
    /// An `@` item: its handler and the item name C passes it as `void *param`. The handler
    /// bodies arrive with the browse and write tasks.
    Run(Action, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuError {
    /// A line the parser refused; C logs it with `fin_line` and returns -1.
    Syntax { line: usize, message: &'static str },
    /// A pool or the screen buffer ran out, which C reports as -3 without a line number.
    Exhausted(String),
    /// A name the post-pass could not resolve: a screen, a submenu, or a handler.
    Unresolved(String),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MenuError::Syntax { line, message } => {
                write!(f, "{message} in menu config line {line}")
            }
            MenuError::Exhausted(message) | MenuError::Unresolved(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for MenuError {}

#[derive(Debug, Clone)]
pub struct MenuItem {
    /// The *requested* position. A 0 means "where the layout pen is", which is what lets a menu
    /// write `0, 0` down a column of items; [`MenuNav`] resolves it into the real row.
    pub row: i16,
    pub col: i16,
    pub action: String,
    /// The menu a `!` item opens. `None` only for the parent marker `..`.
    pub action_menu_id: Option<MenuId>,
    /// The command an `@` item runs.
    pub action_cmd: Option<Action>,
    pub submenu: bool,
    /// `priv` and `level` are the `checkpriv`/`checklevel2` requirements — except in a
    /// `use_filter` menu, where the filter reads them as its own parameter, which is how the
    /// generated favorite menus carry a section id here.
    pub priv_mask: i32,
    pub level: i32,
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct MenuTitle {
    pub show: bool,
    pub row: i16,
    pub col: i16,
    pub text: String,
}

/// A `%SCREENNAME` body: every line as it was read, each preceded by `CTRL_SEQ_CLR_LINE` and
/// with a `\r` inserted before a bare `\n`. C closes the run with a NUL and prints it with
/// `%s`, so the bytes kept here are exactly the string that reached the screen.
#[derive(Debug, Clone)]
pub struct MenuScreen {
    pub name: String,
    pub buf: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub name: String,
    pub title: MenuTitle,
    pub screen_name: String,
    /// Resolved from `screen_name` by the post-pass, which fails the load on an unknown name.
    pub screen_id: Option<MenuScreenId>,
    pub screen_show: bool,
    pub screen_row: i16,
    pub screen_col: i16,
    pub items: Vec<MenuItemId>,
    /// `page r, c, limit`: after `limit` visible items the layout restarts at `r, c` on the
    /// next page. A `limit` of 0 puts the whole menu on one page.
    pub page_row: i16,
    pub page_col: i16,
    pub page_item_limit: i16,
    pub use_filter: bool,
    /// The filter `use_filter` resolves to, found under this menu's own name.
    pub filter: Option<Action>,
}

impl Menu {
    fn new(name: String) -> Self {
        Self {
            name,
            title: MenuTitle::default(),
            screen_name: String::new(),
            screen_id: None,
            screen_show: false,
            screen_row: 0,
            screen_col: 0,
            items: Vec::new(),
            page_row: 0,
            page_col: 0,
            page_item_limit: 0,
            use_filter: false,
            filter: None,
        }
    }
}

/// The parsed tables: one per config file, shared by every session and swapped whole on a
/// reload, which is what `net_server.c:847` does with `unload_menu` and `load_menu`.
#[derive(Debug, Clone, Default)]
pub struct MenuSet {
    pub menus: Vec<Menu>,
    pub items: Vec<MenuItem>,
    pub screens: Vec<MenuScreen>,
    name_dict: HashMap<String, MenuId>,
    screen_dict: HashMap<String, MenuScreenId>,
}

impl MenuSet {
    /// `load_menu`, from the bytes of `var/menu_merged.conf` or `var/bbs_top_menu.conf`.
    pub fn parse(data: &[u8]) -> Result<MenuSet, MenuError> {
        let mut parser = Parser::default();
        parser.run(data)?;
        parser.resolve()?;
        Ok(parser.set)
    }

    pub fn menu(&self, id: MenuId) -> Option<&Menu> {
        self.menus.get(id)
    }

    pub fn menu_by_name(&self, name: &str) -> Option<MenuId> {
        self.name_dict.get(name).copied()
    }

    pub fn item(&self, menu: &Menu, pos: usize) -> Option<&MenuItem> {
        let id = *menu.items.get(pos)?;
        self.items.get(id)
    }

    pub fn screen_of(&self, menu: &Menu) -> Option<&MenuScreen> {
        self.screens.get(menu.screen_id?)
    }

    /// The pool `bbs_net.c:95-117` builds with `calloc` instead of reading a config file: one
    /// menu with no title and no `%SCREEN` body, and every item at its own row and column.
    /// C sizes the item pool to the maximum the screen can hold and counts up what it reads;
    /// here the items are exactly the ones the reader kept, which is the same list.
    pub fn hand_built(menu_name: &str, items: Vec<MenuItem>) -> MenuSet {
        let mut menu = Menu::new(menu_name.to_string());
        menu.items = (0..items.len()).collect();
        let mut set = MenuSet {
            menus: vec![menu],
            items,
            screens: Vec::new(),
            name_dict: HashMap::new(),
            screen_dict: HashMap::new(),
        };
        set.name_dict.insert(menu_name.to_string(), 0);
        set
    }
}

/// `checkpriv(&BBS_priv, 0, priv) && checklevel2(&BBS_priv, level)` (`user_priv.h:72-85`):
/// every bit the item asks for must be set in the user's privilege word and the item's level
/// must not exceed the user's. An item asking for neither is public.
pub fn is_permitted(user_priv: i32, user_level: i32, item: &MenuItem) -> bool {
    (user_priv & item.priv_mask) == item.priv_mask && user_level >= item.level
}

/// What a session hands the menu layer: the two visibility tests and the site name that a
/// `title 0, 0` row prints beside its text.
pub trait MenuView {
    fn permits(&self, item: &MenuItem) -> bool;
    /// A `use_filter` menu replaces [`MenuView::permits`] with this; C showed an item only when
    /// the handler returned 0.
    fn filter_allows(&self, filter: Action, item: &MenuItem) -> bool;
    fn bbs_name(&self) -> &str;
}

/// Everything visible, for callers that have no privilege model to consult yet, such as the
/// top-ten menu, which `main.c:360` loads with `allow_exit` and no filtering.
pub struct Public;

impl MenuView for Public {
    fn permits(&self, _item: &MenuItem) -> bool {
        true
    }

    fn filter_allows(&self, _filter: Action, _item: &MenuItem) -> bool {
        true
    }

    fn bbs_name(&self) -> &str {
        "LBBS"
    }
}

/// One session's position in a [`MenuSet`]: the id path from the root, the cursor in the
/// current menu, and the layout arrays `display_menu` rebuilds on every draw.
pub struct MenuNav<'a> {
    set: &'a MenuSet,
    /// `menu_id_path` and `menu_item_pos`, one entry per level.
    path: Vec<MenuId>,
    pos: Vec<usize>,
    allow_exit: bool,
    display: Vec<bool>,
    r_row: Vec<i16>,
    r_col: Vec<i16>,
    page_id: Vec<i16>,
}

impl<'a> MenuNav<'a> {
    pub fn new(set: &'a MenuSet) -> Self {
        // `menu_id_path[0] = 0`: the first `%menu` in the file is the root menu.
        Self {
            set,
            path: vec![0],
            pos: vec![0],
            allow_exit: false,
            display: Vec::new(),
            r_row: Vec::new(),
            r_col: Vec::new(),
            page_id: Vec::new(),
        }
    }

    pub fn set_allow_exit(&mut self, allow: bool) {
        self.allow_exit = allow;
    }

    /// `choose_step`: the depth below the root menu.
    pub fn depth(&self) -> usize {
        self.path.len() - 1
    }

    pub fn path(&self) -> &[MenuId] {
        &self.path
    }

    pub fn cursor(&self) -> usize {
        self.pos[self.depth()]
    }

    pub fn current_menu(&self) -> Option<&'a Menu> {
        self.set.menu(*self.path.last()?)
    }

    pub fn current_item(&self) -> Option<&'a MenuItem> {
        let menu = self.current_menu()?;
        self.set.item(menu, self.cursor())
    }

    fn set_cursor(&mut self, pos: usize) {
        let step = self.depth();
        self.pos[step] = pos;
    }

    fn item_count(&self) -> usize {
        self.current_menu().map(|m| m.items.len()).unwrap_or(0)
    }

    fn display_at(&self, pos: usize) -> bool {
        self.display.get(pos).copied().unwrap_or(false)
    }

    fn page_at(&self, pos: usize) -> i16 {
        self.page_id.get(pos).copied().unwrap_or(0)
    }

    fn hidden(&self, menu: &Menu, item: &MenuItem, view: &dyn MenuView) -> bool {
        if menu.use_filter {
            // The filter is found under the menu's name, so a menu that asks for one always
            // has it: the post-pass refuses the load otherwise.
            return !match menu.filter {
                Some(action) => view.filter_allows(action, item),
                None => true,
            };
        }
        !view.permits(item)
    }

    /// `display_menu`'s layout pass: resolve each item's requested position into a real row and
    /// column, drop the items this user may not see, split the rest into pages, and report
    /// whether anything at all is selectable.
    fn layout(&mut self, menu: &Menu, view: &dyn MenuView) -> bool {
        let count = menu.items.len();
        self.display.resize(count, false);
        self.r_row.resize(count, 0);
        self.r_col.resize(count, 0);
        self.page_id.resize(count, 0);

        let mut row: i16 = 0;
        let mut col: i16 = 0;
        let mut page_id: i16 = 0;
        let mut page_item_count: i16 = 0;
        let mut selectable = false;

        // The saved position keeps its place when the layout can still show it, which is what
        // lets a parent menu hold the item you entered a submenu from after you back out of it
        // (`menu.c:1128-1134`). Only a position that has since been hidden moves, and the loop
        // below gives it the first visible item.
        let cursor = self.cursor();
        if cursor > 0 && cursor < count {
            if let Some(item) = self.set.item(menu, cursor) {
                if !self.hidden(menu, item, view) {
                    selectable = true;
                }
            }
        }

        for pos in 0..count {
            let Some(item) = self.set.item(menu, pos) else {
                continue;
            };
            // An invisible item still moves the pen, as it does in C.
            if item.row != 0 {
                row = item.row;
            }
            if item.col != 0 {
                col = item.col;
            }
            self.page_id[pos] = page_id;

            if self.hidden(menu, item, view) {
                self.display[pos] = false;
                self.r_row[pos] = 0;
                self.r_col[pos] = 0;
                continue;
            }

            self.display[pos] = true;
            if !selectable {
                self.set_cursor(pos);
                selectable = true;
            }
            self.r_row[pos] = row;
            self.r_col[pos] = col;
            row = row.wrapping_add(1);

            page_item_count += 1;
            if menu.page_item_limit > 0 && page_item_count >= menu.page_item_limit {
                page_id += 1;
                page_item_count = 0;
                row = menu.page_row;
                col = menu.page_col;
            }
        }

        selectable
    }

    /// `display_menu`: lay the menu out, draw it, and park the cursor.
    pub async fn draw(&mut self, term: &mut Terminal, view: &(dyn MenuView + Sync)) -> Draw {
        let Some(menu) = self.current_menu() else {
            return Draw::Empty;
        };
        if menu.items.is_empty() {
            self.say_nothing_selectable(term).await;
            return Draw::Empty;
        }
        if !self.layout(menu, view) {
            self.say_nothing_selectable(term).await;
            return Draw::Empty;
        }
        self.redraw_page(term, view);
        self.draw_cursor(term, true);
        Draw::Show
    }

    async fn say_nothing_selectable(&self, term: &mut Terminal) {
        let Some(menu) = self.current_menu() else {
            return;
        };
        term.moveto_signed(menu.screen_row as i32, menu.screen_col as i32);
        term.clrtoeol();
        term.print("没有可选项");
        screen::press_any_key(term).await;
    }

    /// `display_menu_current_page`: clear the item area, then the title, the screen body, and
    /// the items sharing the cursor's page.
    fn redraw_page(&self, term: &mut Terminal, view: &dyn MenuView) {
        let Some(menu) = self.current_menu() else {
            return;
        };

        clrline_rows(
            term,
            menu.page_row,
            menu.page_row
                .wrapping_add(menu.page_item_limit)
                .wrapping_sub(1),
        );

        if menu.title.show {
            if menu.title.row == 0 && menu.title.col == 0 {
                screen::show_top(term, &menu.title.text, view.bbs_name(), "");
            } else {
                term.moveto_signed(menu.title.row as i32, menu.title.col as i32);
                term.print(&menu.title.text);
            }
        }

        if menu.screen_show {
            if let Some(screen) = self.set.screen_of(menu) {
                term.moveto_signed(menu.screen_row as i32, menu.screen_col as i32);
                term.print_bytes(&screen.buf);
            }
        }

        let page_id = self.page_at(self.cursor());
        let count = menu.items.len();
        let mut pos = self.cursor();
        // Walk back to the first item of this page, then forward past its last.
        while pos > 0 && self.page_at(pos - 1) == page_id {
            pos -= 1;
        }
        while pos < count && self.page_at(pos) == page_id {
            if self.display_at(pos) {
                if let Some(item) = self.set.item(menu, pos) {
                    let row = self.r_row.get(pos).copied().unwrap_or(0);
                    let col = self.r_col.get(pos).copied().unwrap_or(0);
                    term.moveto_signed(row as i32, col as i32);
                    term.print(&item.text);
                }
            }
            pos += 1;
        }
    }

    /// `display_menu_cursor`: the `>` two columns left of the item, or a space to erase it.
    pub fn draw_cursor(&self, term: &mut Terminal, show: bool) {
        let pos = self.cursor();
        let (Some(row), Some(col)) = (self.r_row.get(pos), self.r_col.get(pos)) else {
            return;
        };
        term.moveto_signed(*row as i32, *col as i32 - 2);
        term.print(if show { ">" } else { " " });
    }

    /// `menu_control`.
    pub async fn control(
        &mut self,
        term: &mut Terminal,
        view: &(dyn MenuView + Sync),
        key: u32,
    ) -> MenuFlow {
        if self.set.menus.is_empty() {
            error!("Empty menu set");
            return MenuFlow::ExitBbs;
        }
        let Some(menu) = self.current_menu() else {
            return MenuFlow::ExitBbs;
        };
        if menu.items.is_empty() {
            debug!("Empty menu ({})", menu.name);
            return self.pop_or_exit_bbs();
        }

        if self.cursor() >= menu.items.len() {
            self.set_cursor(0);
            return MenuFlow::Redraw;
        }

        match key {
            CR | KEY_RIGHT => {
                let Some(item) = self.current_item() else {
                    self.set_cursor(0);
                    return MenuFlow::Redraw;
                };
                if !item.submenu {
                    let Some(action) = item.action_cmd else {
                        return MenuFlow::NoRedraw;
                    };
                    return MenuFlow::Run(action, item.name.clone());
                }
                if item.action == ".." {
                    return self.back(term).await;
                }
                let Some(dest) = item.action_menu_id else {
                    return MenuFlow::NoRedraw;
                };
                if !self.push(dest) {
                    // `menu.c:1268-1273` (`99c5e32`): the path arrays keep one entry per level, so
                    // C refuses the level, names it in the error log, and leaves the screen as it
                    // was. Before that commit it wrote past the end of `menu_id_path`.
                    let name = self.current_menu().map_or("", |menu| menu.name.as_str());
                    error!("Menu [{name}] is nested past {MAX_MENU_DEPTH} levels");
                    return MenuFlow::NoRedraw;
                }
                if self.draw(term, view).await == Draw::Empty {
                    return self.back(term).await;
                }
            }
            KEY_ESC | KEY_LEFT => return self.back(term).await,
            KEY_PGUP => self.step(term, view, -1, true),
            KEY_UP => self.step(term, view, -1, false),
            KEY_PGDN => self.step(term, view, 1, true),
            KEY_DOWN => self.step(term, view, 1, false),
            KEY_HOME => self.jump(term, view, true),
            KEY_END => self.jump(term, view, false),
            _ => {
                // `menu.c:1466-1468` (`99c5e32`) now tests `key >= 32 && key <= 126` before
                // `isalnum`, because the extended key codes are past the range the ctype tables
                // cover. A Rust `u8` ASCII check is the same gate, and the arrow and function
                // codes were already handled above.
                if key < 0x80 && (key as u8).is_ascii_alphanumeric() {
                    self.hotkey(term, view, key as u8);
                }
            }
        }

        MenuFlow::NoRedraw
    }

    fn push(&mut self, menu_id: MenuId) -> bool {
        if self.path.len() >= MAX_MENU_DEPTH {
            return false;
        }
        self.path.push(menu_id);
        self.pos.push(0);
        true
    }

    /// `choose_step--`, which is the whole of the way back: the level being returned to keeps
    /// the item it was left on in [`Self::pos`], and the next `layout` reads it back out.
    fn pop(&mut self) -> bool {
        if self.depth() == 0 {
            return false;
        }
        self.path.pop();
        self.pos.pop();
        true
    }

    fn pop_or_exit_bbs(&mut self) -> MenuFlow {
        if self.pop() {
            MenuFlow::Redraw
        } else {
            MenuFlow::ExitBbs
        }
    }

    /// `KEY_LEFT` and `KEY_ESC`: up a level, out of the menu set when `allow_exit` is set, and
    /// otherwise a jump to the last public item of the root — which is how the main menu keeps
    /// the left arrow from quitting the BBS.
    async fn back(&mut self, term: &mut Terminal) -> MenuFlow {
        if self.pop() {
            return MenuFlow::Redraw;
        }
        if self.allow_exit {
            return MenuFlow::ExitMenu;
        }
        self.draw_cursor(term, false);
        let Some(menu) = self.current_menu() else {
            return MenuFlow::NoRedraw;
        };
        let mut found = None;
        let mut pos = menu.items.len() as i32 - 1;
        while pos >= 0 {
            // This scan is the one place C tests `priv` and `level` directly instead of
            // consulting the layout, so a filtered menu's items are judged on their numbers.
            if let Some(item) = self.set.item(menu, pos as usize) {
                if self.display_at(pos as usize) && item.priv_mask == 0 && item.level == 0 {
                    found = Some(pos as usize);
                    break;
                }
            }
            pos -= 1;
        }
        // `menu.c:1324-1329` (`99c5e32`): with nothing both public and displayed the stored
        // position falls back to item 0, which keeps it a valid index into this menu's items.
        self.set_cursor(found.unwrap_or(0));
        self.draw_cursor(term, true);
        MenuFlow::NoRedraw
    }

    /// `KEY_UP`/`KEY_DOWN`, and the page keys that fall through to them owing a page change:
    /// keep walking while a page change is still owed or the item under the cursor is hidden.
    fn step(
        &mut self,
        term: &mut Terminal,
        view: &dyn MenuView,
        delta: i32,
        mut page_change: bool,
    ) {
        self.draw_cursor(term, false);
        let count = self.item_count() as i32;
        if count <= 0 {
            return;
        }
        let page_id = self.page_at(self.cursor());
        let mut pos = self.cursor() as i32;
        // `menu.c:1339-1341` and `:1377-1379` (`99c5e32`) count the steps off `item_count`: one
        // full turn of the items is enough, because a menu with nothing displayed would otherwise
        // wrap around for ever. The cursor lands on the last position scanned.
        for _ in 0..count {
            pos += delta;
            if pos < 0 {
                pos = count - 1;
                page_change = false;
            } else if pos >= count {
                pos = 0;
                page_change = false;
            }
            if page_change && self.page_at(pos as usize) != page_id {
                page_change = false;
            }
            if !page_change && self.display_at(pos as usize) {
                break;
            }
        }
        self.set_cursor(pos as usize);
        if self.page_at(pos as usize) != page_id {
            self.redraw_page(term, view);
        }
        self.draw_cursor(term, true);
    }

    /// `KEY_HOME` and `KEY_END`: the outermost displayed item, with the scan stopping one short
    /// of the far end so that item is the fallback.
    fn jump(&mut self, term: &mut Terminal, view: &dyn MenuView, to_first: bool) {
        self.draw_cursor(term, false);
        let count = self.item_count();
        if count == 0 {
            return;
        }
        let page_id = self.page_at(self.cursor());
        let mut pos: usize = if to_first { 0 } else { count - 1 };
        while (to_first && pos + 1 < count) || (!to_first && pos > 0) {
            if self.display_at(pos) {
                break;
            }
            pos = if to_first { pos + 1 } else { pos - 1 };
        }
        self.set_cursor(pos);
        if self.page_at(pos) != page_id {
            self.redraw_page(term, view);
        }
        self.draw_cursor(term, true);
    }

    /// The first item whose name begins with the pressed letter, compared case-insensitively
    /// one byte at a time — a name opening with a multibyte character has no ASCII initial to
    /// match, as in the original.
    fn hotkey(&mut self, term: &mut Terminal, view: &dyn MenuView, key: u8) {
        let Some(menu) = self.current_menu() else {
            return;
        };
        let page_id = self.page_at(self.cursor());
        let count = menu.items.len();
        for pos in 0..count {
            let Some(item) = self.set.item(menu, pos) else {
                continue;
            };
            let Some(head) = item.name.as_bytes().first() else {
                continue;
            };
            if key.to_ascii_uppercase() == head.to_ascii_uppercase() && self.display_at(pos) {
                self.draw_cursor(term, false);
                self.set_cursor(pos);
                if self.page_at(pos) != page_id {
                    self.redraw_page(term, view);
                }
                self.draw_cursor(term, true);
                return;
            }
        }
    }
}

/// `clrline` with the original's signed arguments: an inverted range clears nothing.
fn clrline_rows(term: &mut Terminal, begin: i16, end: i16) {
    if end < begin {
        return;
    }
    screen::clrline(term, begin.max(0) as usize, end.max(0) as usize);
}

struct Parser {
    set: MenuSet,
    /// `p_menu_screen_buf_free - p_menu_screen_buf`.
    screen_buf_used: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            set: MenuSet::default(),
            screen_buf_used: 0,
        }
    }
}

/// One `fgets` line as the original read them: a line longer than the buffer reaches the parser
/// in `LINE_BUFFER_LEN - 1` byte pieces, each counted as its own `fin_line`.
fn fgets_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let end = data[at..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| at + i + 1)
            .unwrap_or(data.len());
        let mut piece = at;
        while piece < end {
            let next = (piece + LINE_BUFFER_LEN - 1).min(end);
            lines.push(&data[piece..next]);
            piece = next;
        }
        at = end;
    }
    lines
}

/// What `strtok_r` answers, with the span it came from: the quoted fields compare their closing
/// quote against the end of the token.
#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    bytes: &'a [u8],
    start: usize,
    /// The delimiter that ended the token, or the end of the line when there was none.
    end: usize,
}

/// The `saveptr` of one line.
struct Cursor<'a> {
    line: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(line: &'a [u8]) -> Self {
        Self { line, pos: 0 }
    }

    fn next(&mut self, delims: &[u8]) -> Option<Tok<'a>> {
        let mut start = self.pos.min(self.line.len());
        while start < self.line.len() && delims.contains(&self.line[start]) {
            start += 1;
        }
        if start >= self.line.len() {
            self.pos = self.line.len();
            return None;
        }
        let mut end = start;
        while end < self.line.len() && !delims.contains(&self.line[end]) {
            end += 1;
        }
        self.pos = (end + 1).min(self.line.len());
        Some(Tok {
            bytes: &self.line[start..end],
            start,
            end,
        })
    }

    /// `strtok_r(q + 1, …)`: the trailing-content check resumes just past the quote that closed
    /// the text field, not where the last token ended.
    fn resume_at(&mut self, pos: usize) {
        self.pos = pos;
    }

    fn line(&self) -> &'a [u8] {
        self.line
    }
}

/// The quoted-field scan of `load_menu`. A backslash is deleted where it stands so the byte
/// after it — a quote included — stays in the field, which is how the real config writes an
/// item's own hotkey into its text. A field with no closing quote before `limit` is C's fatal
/// `*q != '\"'`.
fn unquote(line: &[u8], open: usize, limit: usize) -> Option<(Vec<u8>, usize)> {
    let stop = limit.min(line.len());
    let mut content = Vec::new();
    let mut i = open + 1;
    while i < stop {
        match line[i] {
            b'"' => return Some((content, i)),
            b'\\' if i + 1 < stop => {
                content.push(line[i + 1]);
                i += 2;
            }
            // A backslash closing the field: C walks past its own NUL here, so the port keeps
            // the byte and lets the missing quote fail the line.
            b'\\' => {
                content.push(b'\\');
                i += 1;
            }
            b => {
                content.push(b);
                i += 1;
            }
        }
    }
    None
}

/// `isalnum(*q) || *q == '_' || *q == '-'` in the C locale: a byte at or above 0x80 never
/// passes, so a name written in Chinese is a parse error rather than a name.
fn is_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn all_identifier(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|b| is_identifier(*b))
}

/// `while (isdigit(*q)) q++; if (*q != '\0')` and then `atoi`, whose result C casts to
/// `int16_t` for a coordinate — so a row number past 32767 wraps instead of failing.
fn digits(cur: &mut Cursor, field: &'static str, line_no: usize) -> Result<i32, MenuError> {
    let syntax = MenuError::Syntax {
        line: line_no,
        message: field,
    };
    let tok = cur.next(&DELIM_WITH_SPACE).ok_or(syntax.clone())?;
    if !tok.bytes.iter().all(|b| b.is_ascii_digit()) {
        return Err(syntax);
    }
    let mut value: i32 = 0;
    for b in tok.bytes {
        value = value.wrapping_mul(10).wrapping_add(i32::from(b - b'0'));
    }
    Ok(value)
}

fn coordinate(cur: &mut Cursor, field: &'static str, line_no: usize) -> Result<i16, MenuError> {
    digits(cur, field, line_no).map(|v| v as i16)
}

/// The `Unknown extra content` check that closes most directives.
fn no_more_tokens(cur: &mut Cursor, line_no: usize) -> Result<(), MenuError> {
    if cur.next(&DELIM_WITH_SPACE).is_some() {
        return Err(MenuError::Syntax {
            line: line_no,
            message: "Unknown extra content",
        });
    }
    Ok(())
}

/// The rest of the line, quoted: C takes the text field with the `"\r\n"` delimiter set, so
/// everything up to the end of the line — spaces and commas intact — is searched for the quote
/// that opens it.
fn quoted_tail(
    cur: &mut Cursor,
    field: &'static str,
    cap: usize,
    line_no: usize,
) -> Result<(Vec<u8>, usize), MenuError> {
    let syntax = MenuError::Syntax {
        line: line_no,
        message: field,
    };
    let tok = cur.next(&DELIM_WITHOUT_SPACE).ok_or(syntax.clone())?;
    let line = cur.line();
    let open = match tok.bytes.iter().position(|b| *b == b'"') {
        Some(i) => tok.start + i,
        None => return Err(syntax),
    };
    let (bytes, quote) = unquote(line, open, tok.end).ok_or(syntax)?;
    if bytes.len() > cap - 1 {
        return Err(MenuError::Syntax {
            line: line_no,
            message: if cap == MAX_MENUTITLE_TEXT_LENGTH {
                "Too longer menu title text"
            } else {
                "Too longer menu item text"
            },
        });
    }
    Ok((bytes, quote))
}

/// `strncpy(field, p, sizeof(field) - 1)`: identifiers go into fixed buffers with no length
/// check, so an overlong one is truncated — and a truncated screen reference then fails to
/// resolve, which is how C reports it.
fn truncated(bytes: &[u8], cap: usize) -> String {
    utf8(&bytes[..bytes.len().min(cap)])
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The file cursor all three block loops share.
struct Lines<'a> {
    items: Vec<&'a [u8]>,
    next: usize,
}

impl<'a> Lines<'a> {
    /// The next line, with the `fin_line` number a message about it carries.
    fn take(&mut self) -> Option<(usize, &'a [u8])> {
        let line = *self.items.get(self.next)?;
        self.next += 1;
        Some((self.next, line))
    }
}

impl Parser {
    fn run(&mut self, data: &[u8]) -> Result<(), MenuError> {
        let mut lines = Lines {
            items: fgets_lines(data),
            next: 0,
        };

        while let Some((line_no, line)) = lines.take() {
            let mut cur = Cursor::new(line);
            let Some(first) = cur.next(&DELIM_WITH_SPACE) else {
                continue;
            };
            match first.bytes[0] {
                // A comment, or a line of nothing but delimiters.
                b'#' => continue,
                b'%' => {
                    if first.bytes[1..] == *b"menu" {
                        self.menu_block(&mut lines, &mut cur, line_no)?;
                    } else {
                        self.screen_block(&mut lines, &first, &cur, line_no)?;
                    }
                }
                // The top level takes nothing else. Inside a menu, an unrecognized keyword is
                // ignored; out here a `!` or a `title` is a fatal prefix.
                _ => {
                    return Err(MenuError::Syntax {
                        line: line_no,
                        message: "Error",
                    });
                }
            }
        }

        Ok(())
    }

    /// `%menu NAME`, whose header the caller already recognized, then its item lines up to the
    /// bare `%`.
    fn menu_block(
        &mut self,
        lines: &mut Lines,
        cur: &mut Cursor,
        line_no: usize,
    ) -> Result<(), MenuError> {
        if self.set.menus.len() >= MAX_MENUS {
            return Err(MenuError::Exhausted(format!(
                "Menu count ({}) exceed limit ({MAX_MENUS})",
                self.set.menus.len()
            )));
        }
        let menu_id = self.set.menus.len();

        let name_tok = cur.next(&DELIM_WITH_SPACE).ok_or(MenuError::Syntax {
            line: line_no,
            message: "Error menu name",
        })?;
        if !all_identifier(name_tok.bytes) {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Error menu name",
            });
        }
        if name_tok.bytes.len() > MAX_MENU_NAME_LENGTH - 1 {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Too longer menu name",
            });
        }
        no_more_tokens(cur, line_no)?;
        let name = utf8(name_tok.bytes);

        if self.set.name_dict.contains_key(&name) {
            // `trie_dict_set` refuses the second name and C only logs it, leaving two menus with
            // one name and only the first reachable through it.
            warn!("Error set menu dict [{}]", name);
        } else {
            self.set.name_dict.insert(name.clone(), menu_id);
        }
        self.set.menus.push(Menu::new(name));

        while let Some((line_no, line)) = lines.take() {
            let mut cur = Cursor::new(line);
            let Some(first) = cur.next(&DELIM_WITH_SPACE) else {
                continue;
            };
            match first.bytes[0] {
                b'#' => continue,
                // Any `%…` line ends the menu; the real files write a bare `%`.
                b'%' => return Ok(()),
                b'!' | b'@' => self.menu_item(menu_id, &first, &mut cur, line_no)?,
                _ => match first.bytes {
                    b"title" => self.menu_title(menu_id, &mut cur, line_no)?,
                    b"screen" => self.menu_screen(menu_id, &mut cur, line_no)?,
                    b"page" => self.menu_page(menu_id, &mut cur, line_no)?,
                    b"use_filter" => {
                        self.set.menus[menu_id].use_filter = true;
                        no_more_tokens(&mut cur, line_no)?;
                    }
                    // C ignores anything else between a menu's markers.
                    _ => {}
                },
            }
        }

        // End of file with no closing `%`: the outer loop stops on the same condition.
        Ok(())
    }

    /// `!ACTION row, col, priv, level, "NAME", "TEXT"`, and the `@` form of the same line.
    fn menu_item(
        &mut self,
        menu_id: MenuId,
        first: &Tok<'_>,
        cur: &mut Cursor,
        line_no: usize,
    ) -> Result<(), MenuError> {
        if self.set.menus[menu_id].items.len() >= MAX_ITEMS_PER_MENU {
            return Err(MenuError::Exhausted(format!(
                "Menuitem count per menu ({}) exceed limit ({MAX_ITEMS_PER_MENU})",
                self.set.menus[menu_id].items.len()
            )));
        }
        if self.set.items.len() >= MAX_MENUITEMS {
            return Err(MenuError::Exhausted(format!(
                "Menuitem count ({}) exceed limit ({MAX_MENUITEMS})",
                self.set.items.len()
            )));
        }

        let submenu = first.bytes[0] == b'!';
        // `..` is the parent marker: it skips the identifier test, as in C, and gets no id.
        let action = &first.bytes[1..];
        if action != b".." && !all_identifier(action) {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Error menu item action",
            });
        }
        if action.len() > MAX_MENUITEM_ACTION_LENGTH - 1 {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Too longer menu action",
            });
        }

        let row = coordinate(cur, "Error menu item row", line_no)?;
        let col = coordinate(cur, "Error menu item col", line_no)?;
        let priv_mask = digits(cur, "Error menu item priv", line_no)?;
        let level = digits(cur, "Error menu item level", line_no)?;

        // The name is a space-free quoted field: its closing quote has to end the token, so a
        // name containing a space fails the line.
        let syntax = MenuError::Syntax {
            line: line_no,
            message: "Error menu item name",
        };
        let name_tok = cur.next(&DELIM_WITH_SPACE).ok_or(syntax.clone())?;
        let line = cur.line();
        if name_tok.bytes.first() != Some(&b'"') {
            return Err(syntax);
        }
        let (name_bytes, name_quote) =
            unquote(line, name_tok.start, name_tok.end).ok_or(syntax.clone())?;
        if name_quote + 1 != name_tok.end {
            return Err(syntax);
        }
        if name_bytes.len() > MAX_MENUITEM_NAME_LENGTH - 1 {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Too longer menu name",
            });
        }

        let (text_bytes, text_quote) = quoted_tail(
            cur,
            "Error menu item text",
            MAX_MENUITEM_TEXT_LENGTH,
            line_no,
        )?;
        cur.resume_at(text_quote + 1);
        no_more_tokens(cur, line_no)?;

        let item_id = self.set.items.len();
        self.set.items.push(MenuItem {
            row,
            col,
            action: utf8(action),
            action_menu_id: None,
            action_cmd: None,
            submenu,
            priv_mask,
            level,
            name: utf8(&name_bytes),
            text: utf8(&text_bytes),
        });
        self.set.menus[menu_id].items.push(item_id);
        Ok(())
    }

    /// `title row, col, "text"`, where `(0, 0)` asks for the standard top row instead.
    fn menu_title(
        &mut self,
        menu_id: MenuId,
        cur: &mut Cursor,
        line_no: usize,
    ) -> Result<(), MenuError> {
        let title = &mut self.set.menus[menu_id].title;
        title.show = true;
        title.row = coordinate(cur, "Error menu title row", line_no)?;
        title.col = coordinate(cur, "Error menu title col", line_no)?;

        let (text, quote) = quoted_tail(
            cur,
            "Error menu title text",
            MAX_MENUTITLE_TEXT_LENGTH,
            line_no,
        )?;
        title.text = utf8(&text);
        cur.resume_at(quote + 1);
        no_more_tokens(cur, line_no)
    }

    fn menu_screen(
        &mut self,
        menu_id: MenuId,
        cur: &mut Cursor,
        line_no: usize,
    ) -> Result<(), MenuError> {
        let menu = &mut self.set.menus[menu_id];
        menu.screen_show = true;
        menu.screen_row = coordinate(cur, "Error menu screen row", line_no)?;
        menu.screen_col = coordinate(cur, "Error menu screen col", line_no)?;

        let name_tok = cur.next(&DELIM_WITH_SPACE).ok_or(MenuError::Syntax {
            line: line_no,
            message: "Error menu screen name",
        })?;
        if !all_identifier(name_tok.bytes) {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Error menu screen name",
            });
        }
        menu.screen_name = truncated(name_tok.bytes, MAX_MENU_SCR_NAME_LENGTH - 1);
        no_more_tokens(cur, line_no)
    }

    fn menu_page(
        &mut self,
        menu_id: MenuId,
        cur: &mut Cursor,
        line_no: usize,
    ) -> Result<(), MenuError> {
        let menu = &mut self.set.menus[menu_id];
        menu.page_row = coordinate(cur, "Error menu page row", line_no)?;
        menu.page_col = coordinate(cur, "Error menu page col", line_no)?;
        menu.page_item_limit = coordinate(cur, "Error menu page item limit", line_no)?;
        no_more_tokens(cur, line_no)
    }

    /// `%SCREENNAME` and the body up to the next `%…`. Every byte between the markers, comments
    /// and blanks included, is screen content: the body is copied from the raw line and only
    /// the terminator test tokenizes.
    fn screen_block(
        &mut self,
        lines: &mut Lines,
        header: &Tok<'_>,
        cur: &Cursor<'_>,
        line_no: usize,
    ) -> Result<(), MenuError> {
        // `screen_id` is looked up by `get_menu_screen_by_id`, which until 37dfc47 guarded it with
        // `menu_item_count` and strode the pool by `sizeof(MENU_ITEM)`; a `Vec` cannot do either.
        if self.set.screens.len() >= MAX_MENUS {
            return Err(MenuError::Exhausted(format!(
                "Menu screen count ({}) exceed limit ({MAX_MENUS})",
                self.set.screens.len()
            )));
        }
        let name_bytes = &header.bytes[1..];
        if !all_identifier(name_bytes) {
            return Err(MenuError::Syntax {
                line: line_no,
                message: "Error menu screen name",
            });
        }
        // The header is `%NAME` alone: C checks the rest of the line with the *second* strtok,
        // which can only be NULL there.
        let mut rest = Cursor::new(cur.line());
        rest.resume_at(cur.pos);
        no_more_tokens(&mut rest, line_no)?;

        let name = truncated(name_bytes, MAX_MENU_SCR_NAME_LENGTH - 1);
        let screen_id = self.set.screens.len();
        if self.set.screen_dict.contains_key(&name) {
            warn!("Error set menu screen dict [{}]", name);
        } else {
            self.set.screen_dict.insert(name.clone(), screen_id);
        }
        self.set.screens.push(MenuScreen {
            name: name.clone(),
            buf: Vec::new(),
        });

        // The `q` the original compares against: the last byte of the buffer stays spare.
        let limit = SCREEN_BUF_CAPACITY - 1;
        let mut terminated = false;

        while let Some((_, line)) = lines.take() {
            let mut cur = Cursor::new(line);
            if let Some(first) = cur.next(&DELIM_WITH_SPACE) {
                if first.bytes[0] == b'%' {
                    if self.screen_buf_used + 1 > limit {
                        return Err(MenuError::Exhausted(
                            "Menu screen buffer depleted".to_string(),
                        ));
                    }
                    // C writes the string's NUL here; the port's `buf` needs none, but the
                    // capacity is charged for it so the depletion point matches.
                    self.screen_buf_used += 1;
                    terminated = true;
                    break;
                }
            }

            if self.screen_buf_used + screen::CTRL_SEQ_CLR_LINE.len() > limit {
                return Err(MenuError::Exhausted(
                    "Menu screen buffer depleted".to_string(),
                ));
            }
            self.set.screens[screen_id]
                .buf
                .extend_from_slice(screen::CTRL_SEQ_CLR_LINE.as_bytes());
            self.screen_buf_used += screen::CTRL_SEQ_CLR_LINE.len();

            for (i, b) in line.iter().enumerate() {
                if self.screen_buf_used + 2 > limit {
                    return Err(MenuError::Exhausted(
                        "Menu screen buffer depleted".to_string(),
                    ));
                }
                // A bare `\n` is paired up; `i > 0` is C's `p > buffer`, so a file opening with
                // a newline keeps its bare one.
                if *b == b'\n' && i > 0 && line[i - 1] != b'\r' {
                    self.set.screens[screen_id].buf.push(b'\r');
                    self.screen_buf_used += 1;
                }
                self.set.screens[screen_id].buf.push(*b);
                self.screen_buf_used += 1;
            }
        }

        if !terminated {
            error!("End of menu screen [{name}] not found");
        }
        Ok(())
    }

    /// The two passes after the file is read, which turn names into ids and refuse the load on
    /// anything they cannot find.
    fn resolve(&mut self) -> Result<(), MenuError> {
        let screen_dict = &self.set.screen_dict;
        for menu in self.set.menus.iter_mut() {
            let id = screen_dict.get(&menu.screen_name).copied().ok_or_else(|| {
                MenuError::Unresolved(format!("Undefined menu screen [{}]", menu.screen_name))
            })?;
            menu.screen_id = Some(id);
            if menu.use_filter {
                menu.filter = Some(Action::from_name(&menu.name).ok_or_else(|| {
                    MenuError::Unresolved(format!("Undefined menu filter handler [{}]", menu.name))
                })?);
            }
        }

        let name_dict = &self.set.name_dict;
        for item in self.set.items.iter_mut() {
            if !item.submenu {
                item.action_cmd = Some(Action::from_name(&item.action).ok_or_else(|| {
                    MenuError::Unresolved(format!(
                        "Undefined menu action cmd handler [{}]",
                        item.action
                    ))
                })?);
            } else if item.action != ".." {
                item.action_menu_id = Some(*name_dict.get(&item.action).ok_or_else(|| {
                    MenuError::Unresolved(format!("Undefined sub menu id [{}]", item.action))
                })?);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../tests/test_menu.rs"]
mod tests;
