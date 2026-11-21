/* SPDX-License-Identifier: GPL-3.0-or-later */
//! The body of one interactive session, ported from `src/bbs_main.c`.
//!
//! C runs this in a forked child that inherits the daemon's read-only shared memory, so the
//! whole session is a straight-line program: charset, welcome, login, [`bbs_center`], logout.
//! Here it is one task per connection, which changes two things and nothing else:
//!
//! * The per-child `static`s of `show_active_board()` become [`ActiveBoard`], owned by
//!   [`bbs_center`]. Two sessions therefore scroll their own copy of the panel, which is what
//!   two children did.
//! * The signal handlers of `bbs_main()` disappear. `SIGUSR1` was the parent's order to reopen
//!   the logs and `SIGHUP` in a child only kept it alive across a reload; a task sharing one
//!   [`crate::log::LogSink`] needs neither. Exiting is
//!   [`crate::state::AppState::is_exiting`], which the key loop polls the way
//!   `while (!SYS_server_exit && loop)` did.
//!
//! The menu tree is taken once, at [`bbs_center`], and held for the session — the same
//! snapshot a child attached to at `fork`. A reload reaches the next connection.

use std::pin::Pin;
use std::time::Duration;

use crate::bbsnet;
use crate::browse;
use crate::data_file_mgr;
use crate::favorlist;
use crate::login::{self, Login};
use crate::menu::{Action, Draw, MenuFlow, MenuNav, MenuReturn};
use crate::paths::{
    DATA_ACTIVE_BOARD, DATA_COPYRIGHT, DATA_EULA, DATA_LICENSE, DATA_VERSION, DATA_WELCOME,
    MAX_USER_COUNT, MAX_USER_IDLE_TIME, VAR_BBS_TOP,
};
use crate::screen;
use crate::session::{self, Session};
use crate::term::Charset;
use crate::term::Terminal;
use crate::term::key::{CR, KEY_NULL, KEY_TIMEOUT};
use crate::userlist;

/// `ACTIVE_BOARD_HEIGHT` (`screen.c:1007`'s window): the rows of `data/active_board.txt` the
/// center menu keeps open below the header.
pub const ACTIVE_BOARD_HEIGHT: usize = 8;

/// `bbs_center`'s `igetch(100)`: a tenth of a second, the resolution of the panel refresh.
pub const MENU_KEY_TIMEOUT: Duration = Duration::from_millis(100);

/// `BBS_max_user_idle_time` in the loop's own units: the seconds a keyless session may idle.
const IDLE_LIMIT: i64 = MAX_USER_IDLE_TIME as i64;

/// The letters the two prompts accept, uppercase because the C switch ran `toupper` first.
const KEY_UPPER_U: u32 = b'U' as u32;
const KEY_UPPER_G: u32 = b'G' as u32;
const KEY_UPPER_V: u32 = b'V' as u32;
const KEY_UPPER_F: u32 = b'F' as u32;
const KEY_UPPER_Y: u32 = b'Y' as u32;

/// The `toupper()` of the C switch: only the ASCII letters take part in the two prompts.
fn toupper(key: u32) -> u32 {
    if ((b'a' as u32)..=(b'z' as u32)).contains(&key) {
        key - (b'a' - b'A') as u32
    } else {
        key
    }
}

/// `bbs_info` (`bbs_main.c:50-56`).
pub async fn bbs_info(term: &mut Terminal, session: &Session) {
    term.print(&format!(
        "\r\n欢迎光临 \x1b[1;33m{} \x1b[32m[{}] \x1b[37m({})\x1b[m\r\n",
        session.config.bbs_name,
        session.config.bbs_server,
        crate::daemon::app_info()
    ));
    let _ = term.flush().await;
}

/// `bbs_welcome` (`bbs_main.c:58-104`): the logo file, then the counts line.
///
/// The online list is the loader's snapshot, which does not contain this connection yet; the
/// `+1` for the caller and the conditional `+1` of the anonymous count are C's.
pub async fn bbs_welcome(term: &mut Terminal, session: &mut Session) {
    let (online, anonymous) = session.state.online().counts();
    let online = online + anonymous + 1;
    let anonymous = anonymous + i32::from(session.is_anonymous());
    let total = session.state.roster().count();
    let login_count = session.state.login_count();
    let (max_client, _) = session.state.limits();

    session.display_file(term, DATA_WELCOME, 2).await;

    term.print(&format!(
        "\r\x1b[1;35m欢迎光临\x1b[33m 【 {} 】 \x1b[35mBBS\r\n\
         \x1b[32m目前上站人数 [\x1b[36m{}/{}\x1b[32m] \
         匿名游客[\x1b[36m{}\x1b[32m] \
         注册用户数[\x1b[36m{}/{}\x1b[32m]\r\n\
         从 [\x1b[36m{}\x1b[32m] 起，累计访问人次: [\x1b[36m{}\x1b[32m]\x1b[m\r\n",
        session.config.bbs_name,
        online,
        max_client,
        anonymous,
        total,
        MAX_USER_COUNT,
        session.config.start_dt,
        login_count,
    ));
    let _ = term.flush().await;
}

/// `bbs_charset_select` (`bbs_main.c:236-295`): five seconds for the charset, five for the
/// wide-character rule. A timeout, Enter or the default letter leaves UTF-8 variable-width in
/// place, exactly as C's `break` out of the two loops did.
///
/// `false` is C's `-1`: the pipe broke or the daemon is stopping, and the session must not
/// continue into a login prompt nobody will read.
pub async fn bbs_charset_select(term: &mut Terminal) -> bool {
    const CHARSET_PROMPT: &str =
        "\rChoose character set in 5 seconds, (U)UTF-8, (G)GBK [U]: ";
    const WIDTH_PROMPT: &str = "\r请在5秒内选择宽字符显示规则, (V)变宽, (F)定宽? [V]: ";

    loop {
        match toupper(screen::press_any_key_ex(term, CHARSET_PROMPT, 5).await) {
            KEY_NULL => return false,
            KEY_TIMEOUT | CR | KEY_UPPER_U => break,
            KEY_UPPER_G => {
                term.set_charset(Charset::Gbk);
                break;
            }
            _ => continue,
        }
    }

    loop {
        match toupper(screen::press_any_key_ex(term, WIDTH_PROMPT, 5).await) {
            KEY_NULL => return false,
            KEY_TIMEOUT | CR | KEY_UPPER_V => {
                term.set_fixed_width(false);
                break;
            }
            KEY_UPPER_F => {
                term.set_fixed_width(true);
                break;
            }
            _ => continue,
        }
    }

    true
}

/// The rotating panel `show_active_board()` keeps in `static`s.
#[derive(Default)]
pub struct ActiveBoard {
    /// `p_data` + `p_line_offsets`: the preloaded file, split into rows.
    lines: Vec<String>,
    /// `line_current`.
    current: usize,
    /// `line_last`: the row the next ten-second tick continues from.
    last: usize,
    /// `t_last_show`.
    last_show: i64,
}

impl ActiveBoard {
    /// `get_file_shm_readonly(DATA_ACTIVE_BOARD)`, from the published table — so the panel a
    /// session sees is the one that was current when its task started, as in a forked child.
    ///
    /// `None` covers both C failures: the file missing from `data/`, which makes
    /// `show_active_board()` return `KEY_NULL`, and an empty file.
    pub fn load(session: &Session) -> Option<ActiveBoard> {
        let data = session.state.data_file(DATA_ACTIVE_BOARD)?;
        let mut lines: Vec<String> = String::from_utf8_lossy(&data)
            .split('\n')
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect();
        while lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return None;
        }
        Some(ActiveBoard::default().with_lines(lines))
    }

    fn with_lines(mut self, lines: Vec<String>) -> Self {
        self.lines = lines;
        self
    }

    /// `show_active_board` (`screen.c:1007-1056`). The return is the original's `KEY_NULL` for
    /// a file it could not map, which its caller ignores.
    pub fn show(&mut self, term: &mut Terminal) {
        if self.lines.is_empty() {
            return;
        }

        let at = session::now();
        if at - self.last_show >= 10 {
            self.last = self.current;
            self.last_show = at;
        } else {
            self.current = self.last;
        }

        screen::clrline(term, 2, 2 + ACTIVE_BOARD_HEIGHT);

        // One line per row, then wrap — which is why a file shorter than the panel leaves the
        // remaining rows cleared and stops the rotation early, as C's `break` did.
        for row in 0..ACTIVE_BOARD_HEIGHT {
            let Some(line) = self.lines.get(self.current) else {
                break;
            };
            term.moveto(3 + row, 0);
            term.print(line);
            self.current += 1;
            if self.current >= self.lines.len() {
                self.current = 0;
                break;
            }
        }
    }
}

/// `bbs_center` (`bbs_main.c:153-234`), the menu loop a user lives in.
pub async fn bbs_center(term: &mut Terminal, session: &mut Session) {
    let menus = session.state.menus();
    let mut nav = MenuNav::new(&menus);
    let mut board = ActiveBoard::load(session);
    let mut last_action = session::now();
    session.last_access_tm = last_action;

    redraw(term, session, &mut nav, board.as_mut()).await;

    while !session.state.is_exiting() {
        let key = term.read_key(MENU_KEY_TIMEOUT).await;
        if key == KEY_NULL {
            tracing::debug!("KEY_NULL");
            break;
        }
        if key != KEY_TIMEOUT {
            session.last_access_tm = session::now();
            // tracing::debug!("Debug: BBS_last_access_tm is updated");
        }

        // `bbs_menu.choose_step == 0` is the root menu: only there does the panel tick.
        if nav.depth() == 1 && session::now() - last_action >= 10 {
            last_action = session::now();
            // `bbs_center` ignores the return, so a `data/active_board.txt` that cannot be
            // read leaves the panel rows empty rather than ending the session.
            if let Some(board) = board.as_mut() {
                board.show(term);
            }
            screen::show_bottom(term, "", &session.username, session.login_tm);
            nav.draw_cursor(term, true);
            let _ = term.flush().await;
        }

        if let Err(e) = session.online_update(Some("MENU")).await {
            tracing::error!("user_online_update(MENU) error: {e}");
        }

        if key == KEY_TIMEOUT {
            if session::now() - session.last_access_tm >= IDLE_LIMIT {
                tracing::debug!("User input timeout");
                break;
            }
            continue;
        }

        match nav.control(term, session, key).await {
            MenuFlow::ExitBbs | MenuFlow::ExitMenu => break,
            MenuFlow::Redraw => {
                last_action = session::now();
                redraw(term, session, &mut nav, board.as_mut()).await;
            }
            MenuFlow::NoRedraw => {}
            MenuFlow::Run(action, param) => {
                match bbs_do_choice(term, session, action, &param, false).await {
                    MenuReturn::ExitBbs | MenuReturn::ExitMenu => break,
                    // `bbs_main.c:216-223`: the handler that finished a file or a list asks for
                    // the whole screen, menu included.
                    MenuReturn::Redraw => {
                        last_action = session::now();
                        redraw(term, session, &mut nav, board.as_mut()).await;
                    }
                    _ => {}
                }
            }
        }
        let _ = term.flush().await;
    }
}

/// The `REDRAW` branch (`bbs_main.c:220-226`): header, panel, footer, menu.
async fn redraw(
    term: &mut Terminal,
    session: &mut Session,
    nav: &mut MenuNav<'_>,
    board: Option<&mut ActiveBoard>,
) {
    screen::clearscr(term);
    screen::show_top(term, "", &session.config.bbs_name, "");
    if let Some(board) = board {
        board.show(term);
    }
    screen::show_bottom(term, "", &session.username, session.login_tm);
    nav.draw(term, session).await;
    let _ = term.flush().await;
}

/// The `bbs_cmd_list` dispatch (`bbs_cmd.c:19-40`, looked up by `get_cmd_handler` at `:77-92`)
/// over the handlers the port has.
///
/// C handed the handler's `menu_return_t` straight back out of `menu_control`, so the center
/// loop repaints the menu when the handler asks for it — which is how the screen comes back
/// from EULA, a board list or the top-ten menu with the menu on the item it was left on. Every
/// handler answers `REDRAW` except `EXITBBS`, the top-ten guard and the paths that bail out
/// before drawing anything. `nested` is the `static int show_top10` guard of
/// `show_top10_menu`: a `TOP10` item inside the top-ten menu does nothing there and answers
/// `NOREDRAW`.
///
/// The future is boxed because the top-ten handler calls back into this function: C had the
/// same recursion through a per-child `static int`, and the two frames are sized in the port
/// only by handing one of them to the heap.
pub fn bbs_do_choice<'a>(
    term: &'a mut Terminal,
    session: &'a mut Session,
    action: Action,
    param: &'a str,
    nested: bool,
) -> Pin<Box<dyn Future<Output = MenuReturn> + Send + 'a>> {
    Box::pin(async move {
    match action {
        Action::ExitBbs => MenuReturn::ExitBbs,
        // `menu_proc.c:128-140`: EULA and License are read with `eof_exit` 0, so the pager
        // waits for `q` or Esc at the end instead of taking any key. The two below it are 1.
        Action::Eula => {
            session.display_file(term, DATA_EULA, 0).await;
            MenuReturn::Redraw
        }
        Action::License => {
            session.display_file(term, DATA_LICENSE, 0).await;
            MenuReturn::Redraw
        }
        Action::Copyright => {
            session.display_file(term, DATA_COPYRIGHT, 1).await;
            MenuReturn::Redraw
        }
        Action::Version => {
            session.display_file(term, DATA_VERSION, 1).await;
            MenuReturn::Redraw
        }
        Action::ShowTop10Menu => {
            if nested {
                return MenuReturn::NoRedraw;
            }
            show_top10_menu(term, session).await;
            MenuReturn::Redraw
        }
        Action::ReloadConf => {
            // `reload_bbs_conf`'s `kill(getppid(), SIGHUP)`: the daemon owns the reload, so a
            // session only asks for it.
            session.state.request_reload();
            MenuReturn::Redraw
        }
        Action::ListSection => {
            browse::list_section(term, session, param).await;
            MenuReturn::Redraw
        }
        Action::LocateArticle => {
            // A parameter that is not `sname|aid` is `menu_proc.c:350`'s `NOREDRAW`: the top-ten
            // entry leaves the screen exactly as the list before it left it.
            if !browse::locate_article_menu(term, session, param).await {
                return MenuReturn::NoRedraw;
            }
            MenuReturn::Redraw
        }
        // `menu_proc.c:259-276`: the parameter is a board name. Only a board the pool does not
        // have answers `NOREDRAW`; a digest menu that failed to open still redraws.
        Action::ListExSection => {
            if !browse::list_ex_section(term, session, param).await {
                return MenuReturn::NoRedraw;
            }
            MenuReturn::Redraw
        }
        // `menu_proc.c:218-257`: an unknown `AID` or a missing cache is `NOREDRAW`, so the digest
        // menu stays on screen exactly as the item left it.
        Action::ViewExArticle => {
            if !browse::view_ex_article(term, session, param).await {
                return MenuReturn::NoRedraw;
            }
            MenuReturn::Redraw
        }
        Action::Shutdown => {
            // `shutdown_bbs` (`menu_proc.c:176-193`): a cleared screen, one character asked for
            // with a default of `N`, and the order to exit only when what stands in that
            // character after Enter is a `Y`. Enter alone leaves the `N` there, Esc empties the
            // answer, and a timeout keeps it — and C ignores `get_data`'s return code, so all
            // three leave the system running. Nothing is printed for the answer either way:
            // the `REDRAW` the handler returns repaints the menu over the question.
            //
            // The answer cell is already full when the question is drawn (`char buf[2]`, shown
            // `max_display_len` 1), so `get_data`'s printable arm rings the bell at anything
            // typed over the default: the way to answer yes is to erase the `N` first, which is
            // what the confirmation has always looked like from a terminal.
            screen::clearscr(term);
            let answer = screen::get_data(
                term,
                1,
                1,
                "真的要关闭系统吗[y/N]? ",
                b"N",
                2,
                1,
                MAX_USER_IDLE_TIME,
            )
            .await
            .unwrap_or_default();

            if answer
                .first()
                .is_some_and(|c| toupper(u32::from(*c)) == KEY_UPPER_Y)
            {
                tracing::info!("Notify main process to exit by [{}]", session.username);
                // `kill(getppid(), SIGTERM)`: the daemon owns the drain, so a session only
                // asks for it and then keeps serving until the loop notices.
                session.state.exit();
            }
            MenuReturn::Redraw
        }
        // `menu_proc.c:382-400`: the two rosters are the same call with the other flag, and
        // neither looks at the menu parameter. C turns a negative return into a `log_error`
        // and still redraws, which [`userlist::user_list_display`] does in place.
        Action::ListUser => {
            userlist::user_list_display(term, session, false).await;
            MenuReturn::Redraw
        }
        Action::ListOnlineUser => {
            userlist::user_list_display(term, session, true).await;
            MenuReturn::Redraw
        }
        // `menu_proc.c:53-121`: the parameter carries the entry name after `@mod:`, so the item
        // picks its own screen and every answer — including a shuttle that never connected —
        // redraws the menu it came from.
        Action::RunMbem => {
            bbsnet::exec_mbem(term, session, param).await;
            MenuReturn::Redraw
        }
        // `menu_proc.c:372-380`: the menu parameter is ignored, and C's `log_error` of a negative
        // return is where [`favorlist::article_favor_display`] already reports each failed query
        // before it gives the list up. Either way the menu comes back.
        Action::FavorTopic => {
            favorlist::article_favor_display(term, session).await;
            MenuReturn::Redraw
        }
        // `bbs_cmd.c:28` calls `data_file_mgr.c:55-273`, which ignores its menu parameter and
        // answers `REDRAW` whichever way it leaves: the number that was not a row, the file that
        // could not be opened, the key that timed out, the cancel, the shutdown, or the save.
        Action::DataMgr => {
            data_file_mgr::data_file_mgr(term, session).await;
            MenuReturn::Redraw
        }
        // The write and profile handlers arrive with their own tasks; until then an item that
        // resolves to one reports itself instead of looking like a hung key.
        _ => {
            tracing::error!("Action {}({param}) is not implemented", action.name());
            MenuReturn::Redraw
        }
    }
    })
}

/// `show_top10_menu` (`menu_proc.c:278-350`): a nested loop over the second tree, which the
/// daemon loaded with `allow_exit` precisely so this loop has a way out.
///
/// Visible to [`crate::browse`] because `H` inside a board list opens it, as C's
/// `SHOW_TOP10` arm does, and the board screen is redrawn when it returns.
pub(crate) async fn show_top10_menu(term: &mut Terminal, session: &mut Session) {
    let menus = session.state.top10();
    let mut nav = MenuNav::new(&menus);
    nav.set_allow_exit(true);

    screen::clearscr(term);
    screen::show_top(term, "", &session.config.bbs_name, "");
    screen::show_bottom(term, "", &session.username, session.login_tm);
    // `menu_proc.c:294`: `if (display_menu(&top10_menu) == 0)`. A menu with no selectable item
    // answers -1 after printing 没有可选项 and taking one key press, and C then returns without
    // opening the read loop — so that one press is the whole visit.
    if nav.draw(term, session).await != Draw::Show {
        return;
    }

    while !session.state.is_exiting() {
        let key = term.read_key(MENU_KEY_TIMEOUT).await;

        if let Err(e) = session.online_update(Some("TOP10_MENU")).await {
            tracing::error!("user_online_update(TOP10_MENU) error: {e}");
        }

        match key {
            KEY_NULL => break,
            KEY_TIMEOUT => continue,
            _ => {}
        }

        match nav.control(term, session, key).await {
            MenuFlow::ExitBbs | MenuFlow::ExitMenu => break,
            // `menu_proc.c:326-331`: the nested loop repaints itself — header excluded, which
            // is what `show_top10_menu`'s `clearscr()`/`show_bottom("")`/`display_menu()` did —
            // and stays open, because a handler's `REDRAW` reaches this switch and not the one
            // in `bbs_center`.
            MenuFlow::Redraw => {
                screen::clearscr(term);
                screen::show_bottom(term, "", &session.username, session.login_tm);
                nav.draw(term, session).await;
            }
            MenuFlow::NoRedraw => {}
            MenuFlow::Run(action, param) => {
                match bbs_do_choice(term, session, action, &param, true).await {
                    MenuReturn::ExitBbs | MenuReturn::ExitMenu => break,
                    MenuReturn::Redraw => {
                        screen::clearscr(term);
                        screen::show_bottom(term, "", &session.username, session.login_tm);
                        nav.draw(term, session).await;
                    }
                    _ => {}
                }
            }
        }
        let _ = term.flush().await;
    }
}

/// `bbs_main` (`bbs_main.c:297-482`) from its `set_input_echo(0)` on: everything a transport owes
/// a connection once its terminal and [`Session`] exist.
///
/// What C does around this belongs elsewhere: the signal handlers and the `mprotect` calls to
/// the process, the editor memory pool to the write path, and the per-board reading positions
/// (`section_aid_locations`) to the browse path. The per-user read and star tables are here,
/// loaded after the agreement check and flushed after the goodbye file, as in C.
pub async fn bbs_main(term: &mut Terminal, session: &mut Session) {
    // C sends this on both transports; an SSH client has no use for the option, and reads it as
    // the six bytes C's client would have read.
    screen::set_input_echo(term, false).await;

    if !bbs_charset_select(term).await {
        return;
    }

    bbs_info(term, session).await;
    bbs_welcome(term, session).await;

    if session.is_ssh {
        // `bbs_main.c:376-380` and `screen.c:99-108`: the transport authenticated the account, so
        // the login prompt is replaced by a greeting — and C hands that greeting to
        // `press_any_key_ex`, which parks the cursor on the bottom row and clears it first.
        // Printing it at wherever the pager left the cursor instead lets that clear erase it.
        let msg = format!(
            "\x1b[1m{} 欢迎使用ssh方式访问 \x1b[1;33m按任意键继续...\x1b[m",
            session.username
        );
        if screen::press_any_key_ex(term, &msg, 60).await == KEY_NULL {
            return;
        }
    } else if login::login(term, session).await == Login::Out {
        return;
    }

    session.log_login();

    if session.update_eula {
        // C's `user_update_agreement()` falls through to `cleanup`, ending the connection so
        // the agreement is read from the start.
        login::update_agreement(term, session).await;
        return;
    }

    session.load_user_logs().await;

    screen::clearscr(term);
    session.display_file(term, VAR_BBS_TOP, 1).await;

    bbs_center(term, session).await;

    session.logout(term).await;
    session.save_user_logs().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{ByteIo, Pipe};

    /// Run the shutdown item with `answer` typed a quarter second in, which is past the
    /// `reset_input` (`igetch_reset`) that opens the question and so cannot be discarded by it.
    async fn shutdown(answer: &[u8]) -> (MenuReturn, bool, Vec<u8>) {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        let (ret, ()) = tokio::join!(
            bbs_do_choice(&mut term, &mut session, Action::Shutdown, "", false),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(answer);
            }
        );

        (ret, session.state.is_exiting(), client.take_available())
    }

    /// `clearscr` then `moveto(1, 1)` then the question with the `N` it starts out holding.
    const PROMPT: &str = "\x1b[1;1H真的要关闭系统吗[y/N]? N";

    fn screen_of(painted: &[u8]) -> String {
        String::from_utf8_lossy(painted).into_owned()
    }

    #[tokio::test]
    async fn shutdown_asks_before_it_orders_the_exit() {
        let (ret, exiting, painted) = shutdown(b"\r").await;
        assert_eq!(ret, MenuReturn::Redraw);
        assert!(
            screen_of(&painted).starts_with("\x1b[2J"),
            "`clearscr` comes first: {}",
            screen_of(&painted)
        );
        assert!(screen_of(&painted).contains(PROMPT), "{}", screen_of(&painted));
        assert!(!exiting, "Enter keeps the default answer, which is `N`");
    }

    /// The answer is `char buf[2]` shown one cell wide and the cell already holds the `N`, so
    /// `get_data`'s printable arm (`screen.c:596`) rings the bell instead of taking the key:
    /// typing over the default is not a way to close the system. Erasing it first is, and
    /// `io.c:1079-1083` folds Ctrl-H and DEL onto the one erase key C then tests for.
    #[tokio::test]
    async fn shutdown_takes_the_y_only_after_the_default_is_deleted() {
        for answer in [b"y\r".as_slice(), b"Y\r".as_slice()] {
            let (_, exiting, painted) = shutdown(answer).await;
            assert!(
                !exiting,
                "{:?} must leave it running: {}",
                String::from_utf8_lossy(answer),
                screen_of(&painted)
            );
            assert!(
                screen_of(&painted).contains('\u{7}'),
                "{:?} must be refused with a bell",
                String::from_utf8_lossy(answer)
            );
        }

        for answer in [b"\x7fy\r".as_slice(), b"\x7fY\r".as_slice(), b"\x08y\r".as_slice()] {
            let (_, exiting, painted) = shutdown(answer).await;
            assert!(
                exiting,
                "{:?} must close the system: {}",
                String::from_utf8_lossy(answer),
                screen_of(&painted)
            );
        }
    }

    /// An erased answer is an empty one — `buf[0]` is the terminator `get_data` left there,
    /// which is neither a `Y` nor anything else.
    #[tokio::test]
    async fn shutdown_with_the_answer_erased_leaves_the_system_running() {
        for answer in [b"\x7f\r".as_slice(), b"\x7fn\r".as_slice()] {
            let (ret, exiting, _) = shutdown(answer).await;
            assert_eq!(ret, MenuReturn::Redraw);
            assert!(!exiting, "{:?} must leave it running", String::from_utf8_lossy(answer));
        }
    }

    /// The two roster items reach the page with the flag that titles it — which is all the
    /// menu has to say about them — and both answer with `REDRAW`. The tables are empty, so
    /// what identifies the variant is the column header the flag picks.
    #[tokio::test]
    async fn the_roster_items_open_the_two_pages() {
        for (action, header) in [
            (Action::ListUser, "上次登陆距今"),
            (Action::ListOnlineUser, "在线时长"),
        ] {
            let (mut client, server) = Pipe::new();
            let mut term = Terminal::new(ByteIo::Pipe(server));
            let mut session = crate::session::offline_session();

            let (ret, ()) = tokio::join!(
                bbs_do_choice(&mut term, &mut session, action, "", false),
                async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    client.push(b"\x1b");
                }
            );

            assert_eq!(ret, MenuReturn::Redraw);
            let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
            assert!(
                screen.contains(header),
                "{} must draw the {header} column: {screen}",
                action.name()
            );
        }
    }

    /// `@FAVOR_TOPIC` (`menu_proc.c:372-380`) passes its parameter to the screen, which ignores
    /// it, and redraws the menu whatever the list does on the way out. With nothing starred the
    /// page has only its header and key line to show.
    #[tokio::test]
    async fn the_topic_item_opens_the_star_list() {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        let (ret, ()) = tokio::join!(
            bbs_do_choice(&mut term, &mut session, Action::FavorTopic, "Topic", false),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(b"\x1b");
            }
        );

        assert_eq!(ret, MenuReturn::Redraw);
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(screen.contains("[主题收藏]"), "{screen}");
        assert!(screen.contains("移除[\x1b[1;32m-\x1b[0;37m"), "{screen}");
    }

    /// `@DATAMGR` ignores its menu parameter, so the dispatch only has to reach the list and read
    /// the number back: an answer that is not one of the eight rows opens nothing, and the return
    /// is the `REDRAW` C gives on every one of its ways out.
    #[tokio::test]
    async fn the_data_item_draws_the_list_it_asks_a_number_for() {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();
        session.root = std::env::temp_dir().join(format!("lbbs-datamgr-menu-{}", std::process::id()));

        let (ret, ()) = tokio::join!(
            bbs_do_choice(&mut term, &mut session, Action::DataMgr, "", false),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(b"9\r");
            }
        );

        assert_eq!(ret, MenuReturn::Redraw);
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(screen.contains("数据文件列表: "), "{screen}");
        assert!(screen.contains(" 1. 进站欢迎"), "{screen}");
        assert!(screen.contains(" 8. 穿梭配置"), "{screen}");
        assert!(!screen.contains("再编辑"), "{screen}");
    }

    /// `@RunMBEM` answers with the entry named after the `#` in its own parameter, so the shuttle
    /// item needs no menu id and no privilege beyond the item's. With no stations configured the
    /// frame still draws, the pager still complains about the empty selection, and the key that
    /// leaves it hands the menu back.
    #[tokio::test]
    async fn the_mbem_item_opens_the_shuttle() {
        let root = std::env::temp_dir().join(format!("lbbs-runmbem-{}", std::process::id()));
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::write(root.join("conf/bbsnet.conf"), "# 暂无站点\n").unwrap();

        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();
        session.root = root.clone();

        let (ret, ()) = tokio::join!(
            bbs_do_choice(
                &mut term,
                &mut session,
                Action::RunMbem,
                "N@mod:lib/libbbs.so#bbs_net",
                false
            ),
            async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                client.push(b"\r\x03");
            }
        );

        assert_eq!(ret, MenuReturn::Redraw);
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(screen.contains(" [\x1b[1;32mCtrl+C\x1b[m]退出"), "{screen}");
        assert!(screen.contains("没有可选项"), "{screen}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A session takes the echo away from the client before it asks anything. The `*` mask at the
    /// login prompt only hides what this side prints; without the negotiation a telnet client
    /// still echoes what it typed itself, and the password walks across the screen in clear.
    #[tokio::test]
    async fn a_session_negotiates_the_echo_before_it_asks_anything() {
        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();

        // The client stays mute, so the session is still waiting on the charset question when the
        // deadline cuts it off; nothing past that prompt is reached, database included.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            bbs_main(&mut term, &mut session),
        )
        .await;

        let wire = client.take_available();
        let body = wire
            .strip_prefix(&[0xff, 0xfb, 0x01, 0xff, 0xfb, 0x03][..])
            .unwrap_or_else(|| panic!("the session must open with the negotiation: {wire:?}"));
        // UTF-8 is the default at this point, so the prompt that follows is still plain text.
        assert!(
            String::from_utf8_lossy(body).contains("Choose character set"),
            "the negotiation must come first: {body:?}"
        );
    }

    /// The shipped `var/bbs_top_menu.conf` is a title, a screen body, and no items at all. C
    /// enters `show_top10_menu`'s read loop only when `display_menu` returns 0, so such a menu
    /// costs the single press `press_any_key` already asked for; the loop would ask for a second.
    #[tokio::test]
    async fn an_empty_top10_menu_ends_with_the_press_it_asks_for() {
        let set = crate::menu::MenuSet::parse(
            b"%menu M_TOP10\ntitle 0, 0, \"Top\"\nscreen 2, 0, S_TOP10\n%\n%S_TOP10\nten\n%\n",
        )
        .unwrap();
        let mut session = crate::session::offline_session();
        session.state.publish_top10(set);
        let (client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));

        let keyed = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(b"\r");
            client
        });

        // A menu still stuck in its read loop never returns: the press it takes is the only one
        // this test offers.
        let visited =
            tokio::time::timeout(Duration::from_secs(2), show_top10_menu(&mut term, &mut session))
                .await;
        assert!(visited.is_ok(), "the empty menu must end after one press");

        let mut client = keyed.await.unwrap();
        let painted = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(painted.contains("没有可选项"), "{painted}");
    }

    /// What C's `kill(0, SIGTERM)` achieved: the flag was raised, every child's key wait answered
    /// at once, and each session ran its own exit path before the parent's 15 s wait was up. A
    /// session that ignores the flag is only ever abandoned by the drain, so the daemon reports
    /// "Give up waiting for N sessions" where C reported the children gone.
    #[tokio::test]
    async fn a_session_waits_on_the_client_not_on_the_shutdown() {
        let (client, server) = Pipe::new();
        let mut session = crate::session::offline_session();
        let state = session.state.clone();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        term.set_exit_watch(state.exit_watch());

        let parked = tokio::spawn(async move {
            bbs_main(&mut term, &mut session).await;
        });
        // The client stays mute, so the session is inside a key wait when the notify lands.
        tokio::time::sleep(Duration::from_millis(250)).await;
        state.exit();

        let ended = tokio::time::timeout(Duration::from_secs(2), parked).await;
        assert!(ended.is_ok(), "the session must end on the notify");
        drop(client);
    }

    /// `bbs_main.c:378-379` hands the greeting to `press_any_key_ex`, and `screen.c:101-104` moves
    /// to the bottom row and clears that row *before* printing. Printing the greeting at whatever
    /// cursor position the welcome left, and then clearing the row for an empty prompt, is what
    /// made the line never reach the client.
    #[tokio::test]
    async fn the_ssh_greeting_is_the_bottom_row_prompt() {
        let (mut client, server) = Pipe::new();
        let mut session = crate::session::offline_session();
        session.is_ssh = true;
        let mut term = Terminal::new(ByteIo::Pipe(server));

        let (ran, ()) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(3), bbs_main(&mut term, &mut session)),
            async {
                // The charset rule, the width rule, then the greeting: every key has to land
                // after the `igetch_reset` opening the wait it answers.
                for _ in 0..3 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    client.push(b"\r");
                }
            }
        );
        assert!(ran.is_err(), "the session continues into its menu after the greeting");

        let wire = String::from_utf8_lossy(&client.take_available()).into_owned();
        assert!(
            wire.contains(
                "\x1b[24;0H\x1b[K\u{1b}[1m 欢迎使用ssh方式访问 \u{1b}[1;33m按任意键继续...\u{1b}[m"
            ),
            "the greeting must come from the prompt that clears the row: {wire:?}"
        );
    }
}
