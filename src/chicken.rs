/* SPDX-License-Identifier: GPL-3.0-or-later */
//! 电子小鸡: the pocket chicken one user raises in a file under their own uid, reached from the
//! top menu's `【 电子小鸡 】` item.
//!
//! Port of `utils/game/chicken.c` — `chicken_main` (`:61-85`), `load_chicken` (`:87-135`),
//! `save_chicken` (`:137-163`), `create_a_egg` (`:165-217`), `show_chicken` (`:219-415`),
//! `select_menu` (`:417-667`), `death` (`:669-696`), `pressany` (`:698-709`), `guess`
//! (`:711-780`), `win_c` (`:782-791`), `tie` (`:793-801`), `lose` (`:803-812`), `situ`
//! (`:814-830`) and `sell` (`:832-886`). The menu reaches it through `exec_mbem`
//! (`src/menu_proc.c:107-110`) on the parameter `C@mod:game/so/libgame.so#chicken_main`
//! (`conf/menu.conf:238`), and the 糖糖 it spends is [`crate::money`].
//!
//! The record is one line of fourteen numbers and a name in `var/chicken/<UID>`, and the shared
//! `var/chicken/log` says who hatched, who sold and who lost. Age is never stored as an age:
//! `load_chicken` rebuilds it from the birth day and month the file does keep (`:125-132`), so the
//! picture on the screen is a function of the calendar rather than of the number in the file.
//!
//! # Deviations from C
//!
//! * The record lives in a [`Chicken`] handed from function to function, not in the globals of
//!   `:37-45`. Of those the file only reads and writes fourteen; `angery`, `sick`, `gold`, `x`,
//!   `ran`, `q_mon`, `p_mon`, `bank`, `birth`, `cfp`, `buf` and `buf1` are named and never used at
//!   all, and `agetmp` (`:94`) and `last` (`:105`, `:115`) are written and never read, so none of
//!   them has a field here.
//! * `chictime` (`:40`) *is* read — `show_chicken` (`:221-229`) thins and dirties the chicken past
//!   200 of them — but no line in `chicken.c` ever assigns it, so the branch is dead in C and is
//!   kept dead here with a test on both sides of it.
//! * `fopen(fname, "r+")` (`:99`) is asked for read *and* write, so a record the session cannot
//!   write to is treated as absent and hatched over. The port asks for the same two modes.
//! * The egg that was just laid is opened a second time to be read back (`:106`), and an open the
//!   kernel refuses there is the `Error!!cannot open file` of `:107-111` and a `-1` out of
//!   `load_chicken`: the game is refused rather than started on the zeroes an unread record parses
//!   to. A record that was already there is read through the `"r+"` of `:99`, which is never NULL,
//!   so for it the only failure left is the short line of `:119-122`.
//! * `fscanf` (`:117-118`) matches each `%d` and stops at the first token a number will not start
//!   in; [`Chicken::parse`] splits on whitespace and reads every field with
//!   [`crate::config::c_atoi`], which answers `0` where C would have left the rest of the line
//!   unread. The two agree on every line `save_chicken` and `create_a_egg` wrote, and the count C
//!   tested (`:119-122`) is the count reported here.
//! * `save_chicken` (`:137-163`) opens `"r+"`, writes the line and then cuts the file to the
//!   length it just wrote, so a chicken renamed shorter takes the tail of the longer name with it.
//!   `%s` stops at the first space either way. An open that fails is C's `Error!!cannot open
//!   file` and no write at all (`:143-147`).
//! * `create_a_egg`'s naming loop (`:179-195`) is left by a name, by `SYS_server_exit`, or by
//!   nothing at all: a `get_data` that times out leaves `chicken_name` empty and asks again, one
//!   `bbs_delay` at a time. The port takes the timeout as the reader having gone and writes the egg
//!   with the empty name it would have written on shutdown — the same record either way. The buffer
//!   the loop re-asks with is `name_tmp` (`:172`, seeded at `:175`), which starts out holding `宝宝`: a nameless
//!   Enter names the chicken 宝宝, and because `get_data` (`src/screen.c:326`) starts counting at
//!   that prefill and appends to it, a typed name rides on it too — `宝宝` then `新鸡` is accepted
//!   as `宝宝新鸡`. A name the filter masked is emptied first (`:189`), so the second round opens
//!   blank rather than offering the `***` back as an answer.
//! * `srand((unsigned) time(NULL))` and `rand() % 3` (`:726-727`) are glibc's additive-feedback
//!   generator, whose answer for a given second is a property of libc rather than of this game.
//!   [`opponent`] mixes the same one-second seed into one of the same three hands: a second still
//!   picks one hand for everyone and all three still come up, but which one is not glibc's answer.
//! * `check_badwords` (`:183`, `:645`) returns a negative when the filter cannot be consulted, and
//!   the two callers treat it differently: `create_a_egg` falls through to `strncpy` with the
//!   half-masked name, `select_menu` leaves the old one alone and only `log_error`s.
//!   [`crate::bwf::Badwords::mask`] has no such failure, the same swap documented in
//!   [`crate::user_info_update`].
//! * The name reaches the screen raw. C prints `chicken_name` with `prints` and never runs it
//!   through `lml_render`, so an escape sequence typed into the name goes to the terminal — kept.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, Timelike};

use crate::config::c_atoi;
use crate::money::{self, Wallet};
use crate::paths::MAX_USER_IDLE_TIME;
use crate::screen;
use crate::session::{self, Session};
use crate::term::Terminal;

/// `CHICKEN_NAME_LEN` (`:28-29`), and the `+ 1` on every buffer that holds one.
const CHICKEN_NAME_LEN: usize = 40;

/// `DATA_FILE` (`:31`), the directory [`setuserfile`] puts the uid under — [`crate::paths::VAR_CHICKEN_DIR`],
/// which is the directory the daemon creates at startup.
const DATA_FILE: &str = crate::paths::VAR_CHICKEN_DIR;

/// `LOG_FILE` (`:32`), the one file every player's hatchings, sales and deaths share.
const LOG_FILE: &str = "var/chicken/log";

/// The `current_action` the game signs the online row with (`:75`).
pub const ACTION: &str = "CHICKEN";

/// `cstate` (`:34-35`): ten slots and seven words. The three that were never initialised print
/// what glibc's `printf` prints for a NULL, and no caller of `pressany` reaches them.
const CSTATE: [&str; 10] = [
    "我在吃饭",
    "偷吃零食",
    "拉便便",
    "笨蛋..输给鸡?",
    "哈..赢小鸡也没多光荣",
    "没食物啦..",
    "疲劳全消!",
    "(null)",
    "(null)",
    "(null)",
];

/// The name `create_a_egg` seeds its prompt with (`:175`), so an Enter with nothing typed hatches
/// a chicken called 宝宝.
const EGG_NAME: &str = "宝宝";

/// The one-byte answers `:433` and `:854` read, and the three hands of `:721`.
const KEY_ROCK: u32 = b'2' as u32;
const KEY_PAPER: u32 = b'3' as u32;
const KEY_YES: u8 = b'y';
const KEY_SCISSORS: u32 = b'1' as u32;

/// The pictures of the `switch (age)` of `:254-382` and the 大怪鸟 of `:386-400`.
const PICTURE_0: &str = "  ●●●●\r\n●  ●●  ●\r\n●●●●●●\r\n●●    ●●\r\n●●●●●●\r\n  ●●●●   ";
const PICTURE_2: &str = "    ●●●●●●\r\n  ●            ●\r\n●    ●    ●    ●\r\n●                ●\r\n●      ●●      ●\r\n●                ●\r\n●                ●\r\n  ●            ●\r\n    ●●●●●●   ";
const PICTURE_4: &str = "      ●●●●●●\r\n    ●            ●\r\n  ●  ●        ●  ●\r\n  ●                ●\r\n  ●      ●●      ●\r\n●●●    ●●      ●●\r\n  ●                ●\r\n  ●                ●\r\n    ●  ●●●●  ●\r\n      ●      ●  ●\r\n                ●    ";
const PICTURE_6: &str = "   ●    ●●●●●●\r\n●  ●●  ●        ●\r\n●              ●    ●\r\n  ●●●                ●\r\n●                      ●\r\n●  ●●                ●\r\n  ●  ●                ●\r\n      ●                ●\r\n        ●            ●\r\n          ●●●●●●        ";
const PICTURE_8: &str = "    ●●          ●●\r\n  ●●●●      ●●●●\r\n  ●●●●●●●●●●●\r\n  ●                  ●\r\n  ●    ●      ●    ●\r\n●                      ●\r\n●        ●●●        ●\r\n  ●                  ●\r\n●    ●          ●  ●\r\n  ●●            ●●●\r\n  ●                  ●\r\n    ●              ●\r\n      ●  ●●●  ●\r\n      ●  ●    ●\r\n        ●               ";
const PICTURE_11: &str = "        ●●●●●●\r\n      ●    ●    ●●\r\n    ●  ●      ●  ●●\r\n  ●●              ●●●\r\n●              ●    ●●\r\n●●●●●●●●      ●●\r\n  ●                  ●●\r\n    ●        ●  ●    ●\r\n    ●        ●  ●    ●\r\n    ●          ●      ●\r\n      ●              ●\r\n        ●  ●●●  ●\r\n        ●  ●  ●  ●\r\n          ●      ●             ";
const PICTURE_13: &str = "              ●●●●\r\n      ●●●●●●●●\r\n    ●●●●●●●●●●\r\n  ●●●●●●●●●●●●\r\n  ●    ●●●●●●●●●\r\n●●    ●            ●●\r\n●●●●                ●\r\n  ●                    ●\r\n    ●●            ●●\r\n  ●    ●●●●●●  ●\r\n  ●                  ●\r\n    ●                  ●\r\n      ●                ●\r\n    ●●●            ●●●        ";
const PICTURE_15: &str = "  ●    ●●●●●●\r\n●  ●●  ●        ●\r\n●              ●    ●\r\n  ●●●                ●\r\n●                      ●\r\n●  ●●                ●\r\n  ●  ●                ●\r\n      ●        ●  ●    ●\r\n      ●          ●      ●\r\n      ●                  ●\r\n        ●              ●\r\n        ●  ●●  ●●●\r\n        ●  ●●  ●\r\n          ●    ●             ";
const PICTURE_MONSTER: &str = "          ●●●●●●●\r\n        ●          ●●●\r\n      ●    ●    ●  ●●●\r\n  ●●●●●●●        ●●\r\n  ●          ●          ●\r\n  ●●●●●●●          ●            \x1b[1;5;31m我是大怪鸟\x1b[m\r\n  ●        ●            ●\r\n  ●●●●●●            ●\r\n  ●                    ●\r\n  ●                    ●\r\n    ●                ●\r\n●●  ●            ●\r\n●      ●●●●●●  ●●\r\n  ●                      ●\r\n●●●    我是大怪鸟       ●●● ";

/// The meal (`:463-471`), the snack (`:511-517`), the cleaned-out coop (`:537-546`) and the
/// 大补丸 (`:593-597`).
const SCENE_EAT: &str = "       □□□□□□\r\n         ∵∴ □  □\r\n              □  □                             □□□□  □          \r\n              □  □     □              □      □□□□□□□            \r\n         Ｃｏｋｅ □    _□□□□□□□□□_    □□□□□□□□                  \r\n             □   □     ％％％％％％％％％       □—∩∩—□          \r\n            □    □     □□□□□□□□□       │Mcdonald│      　　　 　\r\n           □     □     ※※※※※※※※※　     □————□          \r\n       □□□□□□      □□□□□□□□□     □□□□□□□□ ";
const SCENE_SNACK: &str = "             □\r\n       \x1b[33;1m□\x1b[m□○\r\n       \x1b[37;42m■■\x1b[m\r\n       \x1b[32m□□\x1b[m\r\n       \x1b[32;40;1m□□\x1b[m\r\n       \x1b[31m □ \x1b[m\r\n      \x1b[31m □□\x1b[m   \x1b[32;1m水果酒冰淇淋苏打\x1b[m   嗯!好喝!   ";
const SCENE_CLEAN: &str = "\x1b[1;36m                              □□□□□\x1b[m\r\n\x1b[1;33m                             『\x1b[37m□□□□\x1b[m\r\n\x1b[1;37m                               □□□□\x1b[m\r\n\x1b[1;37m             □□□□□□□□\x1b[32m◎\x1b[37m□□□□\x1b[m\r\n\x1b[37m             □□□□□□□□□\x1b[1;37m□□□□\x1b[m\r\n\x1b[37m             □□□□□□□□□\x1b[1;33m □\x1b[m\r\n\x1b[36m                  □□□□□□\x1b[1;33m□□\x1b[m\r\n\x1b[1;36m                  □□□□□□\x1b[m\r\n  \x1b[1;37m                □□□□□□\x1b[m\r\n                  耶耶耶...便便拉光光...                              ";
const SCENE_PILL: &str = "\r\n               □□□□\r\n               □□□□\r\n               □□□□\r\n                          偷吃禁药大补丸.....";

/// The three verdicts of `guess` (`:787`, `:797`, `:809`).
const VERDICT_WIN: &str = "判定:小鸡输了....    >_<~~~~~\r\n\r\n                                 ";
const VERDICT_TIE: &str =
    "判定:平手                    -_-\r\n\r\n                                              ";
const VERDICT_LOSE: &str =
    "小鸡赢罗                      ∩∩\r\n                               □       ";

/// The record as `var/chicken/<UID>` holds it: the fourteen numbers of the `fprintf` of
/// `:149-150` in that order, then the name. C declares them all `int` (`:37-45`), so `i32`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Chicken {
    weight: i32,
    day: i32,
    month: i32,
    satis: i32,
    age: i32,
    oo: i32,
    happy: i32,
    clean: i32,
    tiredstrong: i32,
    play: i32,
    winn: i32,
    losee: i32,
    food: i32,
    zfood: i32,
    name: Vec<u8>,
    /// `chictime` (`:40`): read by `show_chicken`, assigned by nothing — see the header note.
    chictime: i32,
}

impl Chicken {
    /// `fprintf(fp, "%d %d … %s ", …)` of `:149-150` and `:202-203`, trailing space included.
    fn line(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} ",
            self.weight,
            self.day,
            self.month,
            self.satis,
            self.age,
            self.oo,
            self.happy,
            self.clean,
            self.tiredstrong,
            self.play,
            self.winn,
            self.losee,
            self.food,
            self.zfood,
            String::from_utf8_lossy(&self.name)
        )
    }

    /// The `fscanf` of `:117-118`, and the fifteen the return of `:119` is compared against.
    /// Fields past the fifteenth are not read, as they are not read in C either.
    fn parse(bytes: &[u8]) -> (Self, usize) {
        let text = String::from_utf8_lossy(bytes);
        let fields: Vec<&str> = text.split_ascii_whitespace().take(15).collect();

        let numbers =
            |index: usize| -> i32 { c_atoi(fields.get(index).copied().unwrap_or("")) as i32 };
        let chicken = Self {
            weight: numbers(0),
            day: numbers(1),
            month: numbers(2),
            satis: numbers(3),
            age: numbers(4),
            oo: numbers(5),
            happy: numbers(6),
            clean: numbers(7),
            tiredstrong: numbers(8),
            play: numbers(9),
            winn: numbers(10),
            losee: numbers(11),
            food: numbers(12),
            zfood: numbers(13),
            name: fields
                .get(14)
                .map(|field| field.as_bytes().to_vec())
                .unwrap_or_default(),
            chictime: 0,
        };
        (chicken, fields.len())
    }
}

/// `setuserfile(fname, sizeof(fname), DATA_FILE)` (`:68`), which is `snprintf("%s/%d", …, uid)`
/// (`bbs.c:161-165`): each record lives in the shared directory under its uid.
fn setuserfile(session: &Session) -> PathBuf {
    session
        .root
        .join(DATA_FILE)
        .join(session.privs.uid.to_string())
}

/// `chicken_main` (`:61-85`). The `-1`, `-2` and `0` of `:65`, `:72` and `:84` are returned to
/// nobody: `menu_proc.c:107-110` calls the entry and redraws the menu whatever it answers.
pub async fn chicken_main(term: &mut Terminal, session: &mut Session) -> i32 {
    let mut wallet = Wallet::default();
    if money::money_refresh(session, &mut wallet).await < 0 {
        return -1;
    }

    let path = setuserfile(session);

    let Some(mut chicken) = load_chicken(term, session, &path).await else {
        return -2;
    };

    if let Err(err) = session.online_update(Some(ACTION)).await {
        tracing::error!("user_online_update(CHICKEN) error: {err}");
    }

    show_chicken(term, session, &path, &mut chicken, &wallet).await;
    select_menu(term, session, &path, &mut chicken, &mut wallet).await;
    save_chicken(&path, &chicken);

    0
}

/// `load_chicken` (`:87-135`), keeping the record in place the way the C globals are refilled.
/// `None` is one of the two `-1`s of this function — the `create_a_egg` that could not write
/// (`:100-104`) and the egg that could not be read back (`:107-111`) — and `chicken_main`
/// (`:70-73`) turns either into its `-2`.
async fn load_chicken(term: &mut Terminal, session: &mut Session, path: &Path) -> Option<Chicken> {
    // `:95-97`: the clock is read before the naming loop can spend a minute inside `create_a_egg`,
    // and it is this fixed moment the age is computed from however long the name took.
    let now = Local::now();

    // `:99`: `r+` wants to write as well as read, so an unwritable file counts as no file.
    let existing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok();

    let laid_an_egg = existing.is_none();
    if laid_an_egg {
        let mut egg = Chicken::default();
        if create_a_egg(term, session, path, &mut egg).await < 0 {
            return None;
        }
    }
    drop(existing);

    // `:107-111`: C reaches the egg through a handle it opens here, so an egg it cannot open is
    // reported and refused rather than parsed. A record that was already there is read through the
    // `"r+"` of `:99`, and that handle is never NULL, so its only failure is the short line below.
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(err) if laid_an_egg => {
            tracing::error!("Error!!cannot open file '{}': {err}", path.display());
            return None;
        }
        Err(_) => Vec::new(),
    };

    // `:117-122`: the reopened `fp` is read whether or not an egg was just written, and a short
    // line is the `Error in chicken data` of `:119-122`.
    let (mut chicken, fields) = Chicken::parse(&raw);
    if fields != 15 {
        tracing::error!("Error in chicken data");
    }

    // `:125-132`: the stored age is dropped and rebuilt from the birth day and month. The month
    // span is no longer taken modulo twelve, so a chicken of a year and a day is a year and a day
    // old, and the only negative left is the days before this year's birthday — `:129-132` adds
    // the year back, where the old formula aged the chicken backwards into a picture the `switch`
    // of `:254-382` has no `case` for.
    chicken.age = (now.month0() as i32 + 1 - chicken.month) * 31 + (now.day() as i32 - chicken.day);
    if chicken.age < 0 {
        chicken.age += 12 * 31;
    }

    Some(chicken)
}

/// `save_chicken` (`:137-163`): the front of the existing file is overwritten and the record is
/// then cut to the length just written, so a shortened name takes its tail with it.
fn save_chicken(path: &Path, chicken: &Chicken) {
    let text = chicken.line();
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(mut file) => {
            if let Err(err) = file.write_all(text.as_bytes()) {
                tracing::error!("fwrite({}) error: {err}", path.display());
            }
            // `:151-158`: `fflush` then `ftell` then `ftruncate` — the length the write reached.
            let len = u64::try_from(text.as_bytes().len()).unwrap_or(u64::MAX);
            if let Err(err) = file.set_len(len) {
                tracing::error!("ftruncate({}) error: {err}", path.display());
            }
        }
        // `:143-147`: the record the daemon could not open is one it does not write.
        Err(err) => tracing::error!("Error!!cannot open file '{}': {err}", path.display()),
    }
}

/// `create_a_egg` (`:165-217`): ask for a name until one passes the badword filter, then write the
/// egg and sign the shared log. A negative is the error `load_chicken` (`:101-104`) leaves on.
async fn create_a_egg(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
) -> i32 {
    let now = Local::now();

    screen::clrtobot(term, 2);

    let mut initial: Vec<u8> = EGG_NAME.as_bytes().to_vec();
    chicken.name.clear();

    // `:179-195`: an empty name *is* the loop condition, so a rejected or absent answer asks again.
    while !session.state.is_exiting() && chicken.name.is_empty() {
        let Some(typed) = screen::get_data(
            term,
            2,
            1,
            "帮小鸡取个好名字: ",
            &initial,
            CHICKEN_NAME_LEN + 1,
            CHICKEN_NAME_LEN / 2,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            // The timed-out prompt C would have re-asked once a second; see the header note.
            break;
        };

        let mut masked = typed.clone();
        let hit = !masked.is_empty() && session.state.badwords().mask(&mut masked, b'*') > 0;
        // `:187-191`: a masked answer is emptied instead of handed back, so the next round opens
        // on a blank line — `name_tmp[0] = '\0'` before the `continue`.
        initial = if hit { Vec::new() } else { masked };
        if hit || typed.is_empty() {
            continue;
        }
        chicken.name = typed;
    }

    // `:202-203`: the egg weighs twice the hour it was laid at, and starts with ten meals and five
    // snacks.
    chicken.weight = now.hour() as i32 * 2;
    chicken.day = now.day() as i32;
    chicken.month = now.month0() as i32 + 1;
    chicken.satis = 0;
    chicken.age = 0;
    chicken.oo = 0;
    chicken.happy = 0;
    chicken.clean = 0;
    chicken.tiredstrong = 0;
    chicken.play = 0;
    chicken.winn = 0;
    chicken.losee = 0;
    chicken.food = 10;
    chicken.zfood = 5;

    // `fopen(fname, "w")` (`:197`) truncates, which is the one write in this file that does.
    if let Err(err) = std::fs::write(path, chicken.line()) {
        tracing::error!("Error!!cannot open file '{}': {err}", path.display());
        return -2;
    }

    if append_log(
        session,
        &format!(
            "\x1b[32m{}\x1b[m 在 \x1b[34;43m[{}/{}  {}:{:02}]\x1b[m  养了一只叫 \x1b[33m{}\x1b[m 的小鸡\r\n",
            session.username,
            i64::from(now.month0()) + 1,
            now.day(),
            now.hour(),
            now.minute(),
            String::from_utf8_lossy(&chicken.name)
        ),
    )
    .is_err()
    {
        tracing::error!("Error!!cannot open file '{LOG_FILE}'!");
        return -2;
    }

    0
}

/// `show_chicken` (`:219-415`): the numbers, the picture the calendar age picks, the smell
/// warning, and the nine keys.
async fn show_chicken(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &Wallet,
) {
    // `:221-229`: the ageing `chictime` gates, which nothing in C ever raises.
    if chicken.chictime >= 200 {
        chicken.weight -= 5;
        chicken.clean += 3;
        if chicken.tiredstrong > 2 {
            chicken.tiredstrong -= 2;
        }
    }
    if chicken.weight < 0 {
        // `:230-233` comes before the screen: a chicken already too thin does not get one.
        death(term, session, path, chicken).await;
    }

    screen::clrtobot(term, 1);
    term.print(&format!(
        "\x1b[33mName:{}\x1b[m  \x1b[45mAge :{}岁\x1b[m  重量:{}  食物:{}  零食:{}  疲劳:{}  便便:{}\r\n  生日:{}月{}日  糖糖:{:>8}  快乐度:{}  满意度:{}",
        String::from_utf8_lossy(&chicken.name),
        chicken.age,
        chicken.weight,
        chicken.food,
        chicken.zfood,
        chicken.tiredstrong,
        chicken.clean,
        chicken.month,
        chicken.day,
        wallet.balance(),
        chicken.happy,
        chicken.satis
    ));

    term.moveto(3, 0);
    if chicken.age <= 16 {
        // The `switch` of `:254-382` has no `default`, and no `case` for a negative age: C leaves
        // the picture out entirely.
        if let Some(picture) = picture(chicken.age) {
            term.print(picture);
        }
    } else {
        term.print(PICTURE_MONSTER);
    }

    if chicken.clean > 10 {
        term.moveto(10, 30);
        term.print("便便好多..臭臭...");
        if chicken.clean > 15 {
            // `:406-407`: the death happens mid-warning, and the rows below it still print.
            death(term, session, path, chicken).await;
        }
        screen::press_any_key(term).await;
    }

    term.moveto(17, 0);
    term.print(
        "\x1b[32m[1]-吃饭     [2]-吃零食   [3]-清理鸡舍   [4]-跟小鸡猜拳  [5]-目前战绩\x1b[m",
    );
    term.print("\r\n\x1b[32m[6]-买鸡饲料$20  [7]-买零食$30  [8]-吃大补丸  [9]-卖鸡喔 \x1b[m");
}

/// The eight `case` groups of `:254-382`, in the ages they cover.
fn picture(age: i32) -> Option<&'static str> {
    match age {
        0 | 1 => Some(PICTURE_0),
        2 | 3 => Some(PICTURE_2),
        4 | 5 => Some(PICTURE_4),
        6 | 7 => Some(PICTURE_6),
        8 | 9 | 10 => Some(PICTURE_8),
        11 | 12 => Some(PICTURE_11),
        13 | 14 => Some(PICTURE_13),
        15 | 16 => Some(PICTURE_15),
        _ => None,
    }
}

/// `select_menu` (`:417-667`): the help bar, the question, and one action per answer.
async fn select_menu(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &mut Wallet,
) -> i32 {
    // `:421-424`: `ptime` is read once, before the loop, which is the second the `猜拳` bonus of
    // `:554` is taken from however many actions follow.
    let opened = Local::now();
    let mut loop_open = true;

    while !session.state.is_exiting() && loop_open {
        term.moveto(23, 0);
        term.print("\x1b[0;46;31m  使用帮助  \x1b[0;47;34m c 改名字   k 杀鸡   t 消除非疲劳($50)   q 退出     \x1b[m");

        let Some(typed) = screen::get_data(
            term,
            22,
            1,
            "要做些什么呢?: ",
            b"",
            2,
            1,
            MAX_USER_IDLE_TIME,
        )
        .await
        else {
            // `:433-436`: a timeout leaves the menu with no save beyond the one `chicken_main` does.
            return 0;
        };
        let answer = typed.first().copied().unwrap_or(b'\0');

        // `:438-444`: the access time, then the heartbeat that keeps `CHICKEN` as the action.
        session.last_access_tm = session::now();
        if let Err(err) = session.online_update(None).await {
            tracing::error!("user_online_update(NULL) error: {err}");
        }

        if chicken.tiredstrong > 20 {
            // `:446-452`: the rest is painted and then the action runs anyway.
            screen::clearscr(term);
            term.moveto(15, 30);
            term.print("呜~~~小鸡会累坏的...要先去休息一下..");
            term.print("\r\n\r\n休    养     中");
        }

        loop_open = handle_key(
            term,
            session,
            path,
            chicken,
            wallet,
            answer,
            opened.second() as i32,
        )
        .await;

        if loop_open {
            show_chicken(term, session, path, chicken, wallet).await;
        }
    }

    0
}

/// The `switch (inbuf[0])` of `:453-659`. The return is C's `loop`: only `q` of `:656-658` stops
/// the redraw of `:661-664`.
async fn handle_key(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &mut Wallet,
    answer: u8,
    opened_second: i32,
) -> bool {
    match answer {
        b'1' => {
            if chicken.food <= 0 {
                pressany(term, 5).await;
                return true;
            }
            screen::clrtobot(term, 10);
            term.moveto(10, 0);
            term.print(SCENE_EAT);
            pressany(term, 0).await;
            iflush(term).await;

            chicken.food -= 1;
            chicken.tiredstrong += 1;
            chicken.satis += 1;
            // `:478-485`: a young chicken gains by how young it is.
            chicken.weight += if chicken.age < 5 { 5 - chicken.age } else { 1 };

            if chicken.weight > 100 {
                too_heavy(term, "太重了啦..肥鸡~~你想撑死鸡啊？....哇咧○●××").await;
            }
            if chicken.weight > 150 {
                too_heavy(term, "鸡撑晕了~~").await;
            }
            if chicken.weight > 200 {
                // `:498-501`: no message, just the two rows `death` prints.
                death(term, session, path, chicken).await;
            }
        }
        b'2' => {
            if chicken.zfood <= 0 {
                pressany(term, 5).await;
                return true;
            }
            screen::clrtobot(term, 10);
            term.moveto(10, 0);
            term.print(SCENE_SNACK);
            pressany(term, 1).await;

            chicken.zfood -= 1;
            chicken.tiredstrong += 1;
            chicken.happy += 1;
            chicken.weight += 2;
            if chicken.weight > 100 {
                too_heavy(term, "太重了啦..肥鸡~~").await;
            }
            if chicken.weight > 200 {
                death(term, session, path, chicken).await;
            }
        }
        b'3' => {
            screen::clrtobot(term, 10);
            term.moveto(10, 0);
            term.print(SCENE_CLEAN);
            pressany(term, 2).await;
            chicken.tiredstrong += 5;
            chicken.clean = 0;
        }
        b'4' => {
            guess(term, chicken, session::now() as u64).await;
            chicken.satis += opened_second % 2;
            chicken.tiredstrong += 1;
        }
        b'5' => {
            situ(term, chicken).await;
        }
        b'6' => {
            screen::clrtobot(term, 20);
            term.moveto(20, 0);
            if money::money_withdraw(session, wallet, 20).await <= 0 {
                // `:563-568`: `<= 0` covers `-1`, `-2` and the impossible `0` of `:138-142`.
                term.print("糖果不足!!");
                screen::press_any_key(term).await;
                return true;
            }
            chicken.food += 5;
            term.print(&format!("\r\n食物有 \x1b[33;41m {} \x1b[m份", chicken.food));
            term.print(&format!(
                "   剩下 \x1b[33;41m {} \x1b[m糖",
                wallet.balance()
            ));
            screen::press_any_key(term).await;
        }
        b'7' => {
            screen::clrtobot(term, 20);
            term.moveto(20, 0);
            if money::money_withdraw(session, wallet, 30).await <= 0 {
                term.print("糖果不足!!");
                screen::press_any_key(term).await;
                return true;
            }
            chicken.zfood += 5;
            term.print(&format!(
                "\r\n零食有 \x1b[33;41m {} \x1b[m份",
                chicken.zfood
            ));
            term.print(&format!("  剩下 \x1b[33;41m {} \x1b[m糖", wallet.balance()));
            screen::press_any_key(term).await;
        }
        b'8' => {
            if chicken.oo > 0 {
                screen::clrtobot(term, 10);
                term.moveto(10, 0);
                term.print(SCENE_PILL);
                chicken.tiredstrong = 0;
                chicken.happy += 3;
                chicken.oo -= 1;
                pressany(term, 6).await;
                return true;
            }
            screen::clrtobot(term, 20);
            term.moveto(20, 4);
            term.print("没大补丸啦!!");
            screen::press_any_key(term).await;
        }
        b'9' => {
            if chicken.age < 5 {
                screen::clrtobot(term, 20);
                term.moveto(20, 4);
                term.print("太小了...不得贩售未成年小鸡.....");
                screen::press_any_key(term).await;
                return true;
            }
            sell(term, session, path, chicken, wallet).await;
        }
        b'k' => {
            death(term, session, path, chicken).await;
        }
        b't' => {
            if money::money_withdraw(session, wallet, 50).await <= 0 {
                screen::clrtobot(term, 20);
                term.moveto(20, 4);
                term.print("糖果不足!!");
                screen::press_any_key(term).await;
            } else {
                chicken.tiredstrong = 0;
            }
        }
        b'c' => {
            // `:638-641`: the rename starts from the name the record already has.
            screen::clrline(term, 22, 22);
            let Some(typed) = screen::get_data(
                term,
                22,
                1,
                "帮小鸡取个好名字: ",
                &chicken.name,
                CHICKEN_NAME_LEN + 1,
                CHICKEN_NAME_LEN / 2,
                MAX_USER_IDLE_TIME,
            )
            .await
            else {
                return true;
            };
            if typed.is_empty() {
                return true;
            }
            let mut masked = typed.clone();
            if session.state.badwords().mask(&mut masked, b'*') == 0 {
                // `:649-653`: a hit leaves the old name in place — unlike the naming loop, which asks
                // again.
                chicken.name = typed;
            }
        }
        b'q' => return false,
        // `:659`: any other byte is no action at all, and the screen is redrawn.
        _ => {}
    }

    true
}

/// The two `moveto(9, 30)` warnings of `:486-497` and `:523-528`, which share everything but words.
async fn too_heavy(term: &mut Terminal, message: &str) {
    term.moveto(9, 30);
    term.print(message);
    screen::press_any_key(term).await;
}

/// `pressany` (`:698-709`): the status line over the menu, one key, and the line cleared again.
async fn pressany(term: &mut Terminal, index: usize) -> u32 {
    term.moveto(23, 0);
    term.print(&format!(
        "\x1b[33;46;1m                           \x1b[34m{}\x1b[37m                         \x1b[0m",
        CSTATE[index]
    ));
    iflush(term).await;
    let ch = screen::press_any_key_no_prompt(term, 60).await;
    term.moveto(23, 0);
    term.clrtoeol();
    iflush(term).await;
    ch
}

/// `iflush` (`:474`, `:703`, `:707`, `:718`): the flush C wants before it waits for a key.
async fn iflush(term: &mut Terminal) {
    if let Err(err) = term.flush().await {
        tracing::error!("io_flush() error: {err}");
    }
}

/// `guess` (`:711-780`). `-1` is the error input of `:721-724`, which `:553` throws away.
async fn guess(term: &mut Terminal, chicken: &mut Chicken, seed: u64) -> i32 {
    term.moveto(23, 0);
    term.print("[1]-剪刀 [2]-石头 [3]-布: ");
    term.clrtoeol();
    iflush(term).await;

    let ch = screen::press_any_key_no_prompt(term, 60).await;
    if ch != KEY_SCISSORS && ch != KEY_ROCK && ch != KEY_PAPER {
        return -1;
    }

    let com = opponent(seed);
    term.moveto(21, 35);
    term.print(match com {
        0 => "小鸡:剪刀",
        1 => "小鸡:石头",
        _ => "小鸡:布",
    });
    term.clrtoeol();

    term.moveto(19, 0);
    match ch {
        KEY_SCISSORS => {
            term.print("笨鸡---看我捡来的刀---");
            // `:750-755`.
            match com {
                0 => tie(term).await,
                1 => lose(term, chicken).await,
                _ => win_c(term, chicken).await,
            }
        }
        KEY_ROCK => {
            term.print("呆鸡---砸你一块石头!---");
            match com {
                0 => win_c(term, chicken).await,
                1 => tie(term).await,
                _ => lose(term, chicken).await,
            }
        }
        _ => {
            term.print("蠢鸡---送你一堆破布!---");
            match com {
                0 => lose(term, chicken).await,
                1 => win_c(term, chicken).await,
                _ => tie(term).await,
            }
        }
    }
    term.clrtoeol();

    screen::press_any_key(term).await;
    0
}

/// `srand((unsigned) time(NULL))` and `rand() % 3` (`:726-727`): one hand per second, three hands
/// in all. See the header note on why the mixing is the port's own.
fn opponent(seed: u64) -> i32 {
    let mut state = seed | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (state % 3) as i32
}

/// `win_c` (`:782-791`): the player's win, which is the chicken's loss.
async fn win_c(term: &mut Terminal, chicken: &mut Chicken) {
    chicken.winn += 1;
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_WIN);
}

/// `tie` (`:793-801`): C's `tie(void)` moves no counter, so it takes no record.
async fn tie(term: &mut Terminal) {
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_TIE);
}

/// `lose` (`:803-812`): the chicken wins, and is happier for it.
async fn lose(term: &mut Terminal, chicken: &mut Chicken) {
    chicken.losee += 1;
    chicken.happy += 2;
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_LOSE);
}

/// `situ` (`:814-830`): the record, printed from both sides, and one of the two comments on it.
async fn situ(term: &mut Terminal, chicken: &Chicken) {
    screen::clrtobot(term, 16);
    term.moveto(16, 0);
    term.print("           ");
    term.moveto(17, 0);
    term.print(&format!(
        "你:\x1b[44m {}胜 {}负\x1b[m                   ",
        chicken.winn, chicken.losee
    ));
    term.moveto(18, 0);
    term.print(&format!(
        "鸡:\x1b[44m {}胜 {}负\x1b[m                   ",
        chicken.losee, chicken.winn
    ));

    if chicken.winn >= chicken.losee {
        pressany(term, 4).await;
    } else {
        pressany(term, 3).await;
    }
}

/// `death` (`:669-696`): the log line, two rows of text, the record deleted, and a new egg hatched
/// in its place — the name asked in the middle of whatever action called this, because in C the
/// globals are simply overwritten.
async fn death(term: &mut Terminal, session: &mut Session, path: &Path, chicken: &mut Chicken) {
    let now = Local::now();

    screen::clearscr(term);
    screen::clrtobot(term, 5);

    if append_log(
        session,
        &format!(
            "\x1b[32m{}\x1b[m 在 \x1b[34;43m[{}/{}  {}:{:02}]\x1b[m  的小鸡 \x1b[33m{}  \x1b[36m挂了~~\x1b[m \r\n",
            session.username,
            i64::from(now.month0()) + 1,
            now.day(),
            now.hour(),
            now.minute(),
            String::from_utf8_lossy(&chicken.name)
        ),
    )
    .is_err()
    {
        tracing::error!("Error!!cannot open file '{LOG_FILE}'!");
        return;
    }

    term.print("呜...小鸡挂了....");
    term.print("\r\n笨史了...赶出系统...");
    screen::press_any_key(term).await;

    // `:692-693`: the file goes, and `load_chicken` hatches a new one over the top of it.
    let _ = std::fs::remove_file(path);
    if let Some(fresh) = load_chicken(term, session, path).await {
        *chicken = fresh;
    }
}

/// `sell` (`:832-886`). The return codes of `-1` (`:855-861`), `-2` (`:863-869`, `:871-875`) and
/// `0` are all thrown away by `:618`.
async fn sell(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &mut Wallet,
) -> i32 {
    let now = Local::now();

    // `:845-849`: happiness, satisfaction, weight, age, and a bonus decided by the second the
    // screen opened the question in.
    let sel = price(
        chicken.happy,
        chicken.satis,
        chicken.weight,
        chicken.age,
        now.second() as i32,
    );

    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(&format!("小鸡值\x1b[33;45m$$ {sel} \x1b[m糖糖"));

    let Some(typed) = screen::get_data(
        term,
        19,
        1,
        "真的要卖掉小鸡?[y/N]",
        b"",
        2,
        1,
        MAX_USER_IDLE_TIME,
    )
    .await
    else {
        return -1;
    };
    if typed.first().copied() != Some(KEY_YES) {
        // `:858-861`: `N`, an empty answer and `Yes` all refuse; only a lowercase `y` sells.
        return -1;
    }

    if money::money_deposit(session, wallet, sel).await <= 0 {
        tracing::error!("Cannot deposit money {sel}");
        term.moveto(21, 0);
        term.print("无法存钱，放弃交易！");
        return -2;
    }

    if append_log(
        session,
        &format!(
            "\x1b[32m{}\x1b[m 在 \x1b[34;43m[{}/{}  {}:{:02}]\x1b[m  把小鸡 \x1b[33m{}  \x1b[31m以 \x1b[37;44m{sel}\x1b[m \x1b[31m糖果卖了\x1b[m\r\n",
            session.username,
            i64::from(now.month0()) + 1,
            now.day(),
            now.hour(),
            now.minute(),
            String::from_utf8_lossy(&chicken.name)
        ),
    )
    .is_err()
    {
        tracing::error!("Error!!cannot open file '{LOG_FILE}'!");
        return -2;
    }

    screen::clearscr(term);
    let _ = std::fs::remove_file(path);
    if let Some(fresh) = load_chicken(term, session, path).await {
        *chicken = fresh;
    }

    0
}

/// The sum of `:845-849`, in the order C adds it.
fn price(happy: i32, satis: i32, weight: i32, age: i32, second: i32) -> i32 {
    let mut sel = 0;
    sel += happy * 10;
    sel += satis * 7;
    sel += (second % 9) * 10;
    sel += weight;
    sel += age * 10;
    sel
}

/// The `fopen(LOG_FILE, "a")` and `fprintf` of `:206-214`, `:679-687` and `:871-879`: one
/// appended line in the directory every player shares.
fn append_log(session: &Session, text: &str) -> std::io::Result<()> {
    let path = session.root.join(LOG_FILE);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{ByteIo, Pipe};
    use std::path::PathBuf;
    use std::time::Duration;

    /// A site root with the chicken directory the egg needs, and nothing else.
    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("lbbs-chicken-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::paths::VAR_CHICKEN_DIR)).unwrap();
        root
    }

    /// The offline session — no database behind it, so every money call answers `-1` — moved out to
    /// `root`.
    fn test_session(root: &Path) -> Session {
        let mut session = crate::session::offline_session();
        session.root = root.to_path_buf();
        session
    }

    /// Where this site keeps the test user's record.
    fn record_path(root: &Path) -> PathBuf {
        setuserfile(&test_session(root))
    }

    /// The shared log, which the site only grows.
    fn log_lines(root: &Path) -> String {
        String::from_utf8_lossy(&std::fs::read(root.join(LOG_FILE)).unwrap_or_default())
            .into_owned()
    }

    /// A record written the way `save_chicken` writes one, for the tests that start from a chicken
    /// rather than hatching one.
    fn write_record(root: &Path, chicken: &Chicken) {
        std::fs::write(record_path(root), chicken.line()).unwrap();
    }

    /// A chicken with a name and the fourteen numbers the tests that do not care about them leave
    /// at zero.
    fn hen(name: &str) -> Chicken {
        Chicken {
            name: name.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    /// The keys of a page arrive a quarter second after each other, because [`screen::get_data`] and
    /// [`screen::press_any_key`] both empty the input before they ask: a script fed at once is a
    /// script nobody was asking for.
    async fn feed(client: &mut Pipe, script: &[&[u8]]) {
        for step in script {
            tokio::time::sleep(Duration::from_millis(250)).await;
            client.push(step);
        }
    }

    /// Run `$body` — an async expression written against the `$t`, `$s` and `$p` the calling test
    /// names — against a script of keystrokes, and hand back what it returned and what it drew.
    /// The three names are captured rather than hard-coded because a `let` a macro writes is
    /// invisible to the expressions its caller passes in.
    macro_rules! page {
        [$root:expr, [$($step:expr),* $(,)?], |$t:ident, $s:ident, $p:ident| $body:expr] => {{
            let (mut client, server) = Pipe::new();
            #[allow(unused_mut, unused_variables)]
            let mut $t = Terminal::new(ByteIo::Pipe(server));
            #[allow(unused_mut, unused_variables)]
            let mut $s = test_session($root);
            #[allow(unused_variables)]
            let $p = setuserfile(&$s);
            let script: &[&[u8]] = &[$($step),*];
            let (out, _) = tokio::time::timeout(
                Duration::from_secs(20),
                async {
                    tokio::join!(
                        $body,
                        feed(&mut client, script)
                    )
                },
            )
            .await
            .expect("the page ends on its keys");
            let _ = $t.flush().await;
            (out, String::from_utf8_lossy(&client.take_available()).into_owned())
        }};
    }

    /// The fifteen fields `save_chicken` writes come back out of [`Chicken::parse`] unchanged, and
    /// the line is the one `fprintf` of `:149-150` would have written: space separated, one space
    /// after the name.
    #[test]
    fn the_record_round_trips_through_its_own_line() {
        let chicken = Chicken {
            weight: 37,
            day: 21,
            month: 11,
            satis: 4,
            age: 130,
            oo: 1,
            happy: 9,
            clean: 2,
            tiredstrong: 6,
            play: 3,
            winn: 12,
            losee: 7,
            food: 8,
            zfood: 2,
            name: "小花".as_bytes().to_vec(),
            chictime: 0,
        };

        let line = chicken.line();
        assert_eq!(
            line, "37 21 11 4 130 1 9 2 6 3 12 7 8 2 小花 ",
            "the field order of `:149-150`"
        );
        let (back, fields) = Chicken::parse(line.as_bytes());
        assert_eq!(back, chicken);
        assert_eq!(fields, 15, "the count `:119` compares against");
    }

    /// `%s` reads one token, so a record with anything after the name has it ignored — which is what
    /// makes the tail `save_chicken` leaves behind invisible.
    #[test]
    fn a_sixteenth_field_is_not_part_of_the_name() {
        let line = "1 2 3 4 5 6 7 8 9 10 11 12 13 14 小鸡 TAIL-OF-A-LONGER-NAME ".as_bytes();
        let (chicken, fields) = Chicken::parse(line);
        assert_eq!(chicken.name, "小鸡".as_bytes());
        assert_eq!(fields, 15, "fifteen were matched and the rest left unread");

        // A line with too few fields is the `Error in chicken data` of `:119-122`, and the numbers it
        // does not carry read as the `0` `c_atoi` answers.
        let (short, fields) = Chicken::parse(b"5 6 7 ");
        assert_eq!(fields, 3);
        assert_eq!((short.weight, short.day, short.month), (5, 6, 7));
        assert_eq!((short.satis, short.age, short.food), (0, 0, 0));
        assert!(short.name.is_empty());
    }

    /// The eight `case` groups of `:254-382`, the ages above sixteen that take the 大怪鸟 of `:386`
    /// instead, and the shape of a `switch` that has no `default` to catch either.
    #[test]
    fn the_picture_is_chosen_by_the_age_c_switches_on() {
        assert_eq!(picture(0), Some(PICTURE_0));
        assert_eq!(picture(1), Some(PICTURE_0));
        assert_eq!(picture(3), Some(PICTURE_2));
        assert_eq!(picture(5), Some(PICTURE_4));
        assert_eq!(picture(7), Some(PICTURE_6));
        assert_eq!(picture(8), Some(PICTURE_8));
        assert_eq!(
            picture(10),
            Some(PICTURE_8),
            "three ages share the eighth picture"
        );
        assert_eq!(picture(12), Some(PICTURE_11));
        assert_eq!(picture(14), Some(PICTURE_13));
        assert_eq!(picture(16), Some(PICTURE_15));
        assert_eq!(
            picture(17),
            None,
            "the caller takes the monster branch at 17"
        );
        assert_eq!(
            picture(-7),
            None,
            "the `switch` has no `default`, so an age no `case` covers draws nothing"
        );
    }

    /// A record on disk, aged by the calendar: see the note on `:125-132` in the header.
    async fn age_of(root: &Path) -> Chicken {
        let (loaded, _) = page!(root, [], |term, session, path| load_chicken(
            &mut term,
            &mut session,
            &path
        ));
        loaded.expect("the record is there")
    }

    /// `:125-132` counts whole months since the birthday without forgetting the years, and a span
    /// that is negative because this year's birthday has not come round gets the year back.
    #[tokio::test]
    async fn the_age_is_the_calendar_subtraction_of_load_chicken() {
        let now = Local::now();
        let month = now.month0() as i32 + 1;

        // Hatched today: the month span is nothing and so is the day subtraction.
        let root = root("age-today");
        write_record(
            &root,
            &Chicken {
                day: now.day() as i32,
                month,
                ..hen("今天")
            },
        );
        assert_eq!(age_of(&root).await.age, 0);
        assert_eq!(
            age_of(&root).await.name,
            "今天".as_bytes(),
            "the name is the fifteenth field"
        );

        // Born on the first of this month: only the days since the first count, whether that was
        // this year or last.
        let root = self::root("age-wrap");
        write_record(
            &root,
            &Chicken {
                day: 1,
                month,
                ..hen("周岁")
            },
        );
        assert_eq!(age_of(&root).await.age, now.day() as i32 - 1);

        // Born on the 28th, looked at before the 28th: the span is negative, and `:129-132` makes
        // it the year-and-a-bit that has actually passed rather than a subtraction below zero.
        let root = self::root("age-negative");
        write_record(
            &root,
            &Chicken {
                day: 28,
                month,
                ..hen("下月")
            },
        );
        let chicken = age_of(&root).await;
        if now.day() < 28 {
            assert_eq!(
                chicken.age,
                now.day() as i32 - 28 + 12 * 31,
                "the birthday has not come round: the year is added back instead of an age below zero"
            );
            assert_eq!(
                picture(chicken.age),
                None,
                "and an age past the last `case` of `:254-382` draws nothing"
            );
        } else {
            assert_eq!(chicken.age, now.day() as i32 - 28);
        }
    }

    /// `save_chicken` (`:151-158`) cuts the file to the length it has just written, so a chicken
    /// renamed shorter takes the tail of the longer name away with it. Before `:151-158` the debris
    /// stayed in the record, harmless only because `%s` stops at the first space.
    #[tokio::test]
    async fn a_shorter_record_cuts_the_tail_of_the_longer_one() {
        let root = root("tail");
        let long = Chicken {
            food: 3,
            name: "长长长长长长长长长长长长".as_bytes().to_vec(),
            ..Default::default()
        };
        write_record(&root, &long);

        let short = Chicken {
            food: 4,
            name: "短".as_bytes().to_vec(),
            ..Default::default()
        };
        save_chicken(&record_path(&root), &short);

        let bytes = std::fs::read(record_path(&root)).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&bytes).as_ref(),
            short.line(),
            "the file is the record and nothing more of the previous one"
        );

        let (back, fields) = Chicken::parse(&bytes);
        assert_eq!(back.name, "短".as_bytes());
        assert_eq!(fields, 15);
    }

    /// `:143-147`: the record `save_chicken` cannot open is the record it does not write. Before
    /// `c96f842` the `FILE *` went to `fprintf` unchecked, so this is the line the daemon used to
    /// take down with it.
    #[tokio::test]
    async fn a_record_that_cannot_be_opened_is_not_written() {
        let root = root("tail-absent");
        let path = record_path(&root);
        std::fs::remove_file(&path).ok();

        let (_common, error) = crate::log::captured(|| save_chicken(&path, &hen("不见")));

        assert!(
            error.contains(&format!("Error!!cannot open file '{}'", path.display())),
            "{error}"
        );
        assert!(!path.exists(), "and no record appears in its place");
    }

    /// `chictime` (`:40`) is read by `show_chicken` (`:221-229`) and assigned by no line of
    /// `chicken.c`, so the thinning and dirtying it gates cannot happen: nothing that reaches the
    /// field can raise it past 200.
    #[test]
    fn no_record_ever_carries_a_chictime() {
        let (chicken, _) = Chicken::parse("50 1 1 0 0 0 0 0 0 0 0 0 10 5 鸡 ".as_bytes());
        assert_eq!(chicken.chictime, 0, "the fifteen fields do not include one");
        assert_eq!(Chicken::default().chictime, 0);
        assert_eq!(hen("新").chictime, 0);
    }

    /// The sum of `:845-849`, including the `(tm_sec % 9) * 10` that makes a sale worth a different
    /// price at each second of the minute.
    #[test]
    fn a_sale_is_worth_what_c_adds_up() {
        assert_eq!(price(2, 3, 40, 7, 8), 20 + 21 + 80 + 40 + 70);
        assert_eq!(price(0, 0, 0, 0, 0), 0);
        assert_eq!(price(0, 0, 0, 0, 9), 0, "the tenth second wraps to nothing");
        assert_eq!(price(0, 0, 0, 0, 17), 80, "% 9 and not / 9");
        assert_eq!(price(1, 1, 1, 1, 1), 10 + 7 + 10 + 1 + 10);
    }

    /// `srand(time(NULL))` and `rand() % 3` (`:726-727`): the chicken has one hand per second, and it
    /// keeps it for every guess made inside that second.
    #[test]
    fn the_chicken_holds_one_hand_per_second() {
        let mut seen = [false; 3];
        for seed in 0..200u64 {
            let hand = opponent(seed);
            assert!((0..3).contains(&hand), "{seed} gives {hand}");
            seen[hand as usize] = true;
            assert_eq!(opponent(seed), hand, "a second does not change its mind");
        }
        assert_eq!(seen, [true, true, true], "and all three hands come up");
    }

    /// `guess` (`:711-780`): the outcome table of `:746-776`, driven by a seed chosen for the hand it
    /// gives the chicken rather than by the clock.
    #[tokio::test]
    async fn the_guess_table_moves_the_record_the_way_c_writes_it() {
        let root = root("guess");
        let scissors = (0..200u64).find(|s| opponent(*s) == 0).unwrap();
        let rock = (0..200u64).find(|s| opponent(*s) == 1).unwrap();
        let cloth = (0..200u64).find(|s| opponent(*s) == 2).unwrap();

        // 石头 against the chicken's 石头 is the `tie()` of `:762`, and `tie` of `:793-801` takes no
        // record: the verdict line is all that changes.
        let mut chicken = hen("猜拳");
        let (ret, screen) = page!(&root, [b"2", b" "], |term, session, path| async {
            let ret = guess(&mut term, &mut chicken, rock).await;
            (ret, ())
        });
        assert_eq!(ret.0, 0);
        assert!(screen.contains("呆鸡---砸你一块石头!---"), "{screen}");
        assert!(screen.contains("小鸡:石头"), "{screen}");
        assert!(screen.contains("判定:平手"), "{screen}");
        assert_eq!(
            (chicken.winn, chicken.losee, chicken.happy),
            (0, 0, 0),
            "a tie moves nothing"
        );

        // 布 against 布 ties the same way (`:773`), and 石头 against 剪刀 is the `win_c()` of `:760`,
        // which counts the hand for the player at `:782-791`.
        let mut chicken = hen("猜拳");
        page!(&root, [b"3", b" "], |term, session, path| guess(
            &mut term,
            &mut chicken,
            cloth
        ));
        assert_eq!((chicken.winn, chicken.losee, chicken.happy), (0, 0, 0));

        let mut chicken = hen("猜拳");
        page!(&root, [b"2", b" "], |term, session, path| guess(
            &mut term,
            &mut chicken,
            scissors
        ));
        assert_eq!((chicken.winn, chicken.losee), (1, 0), "the player won");

        // 剪刀 against 石头 is the `lose()` of `:753`, which counts against the player and pays the
        // chicken two happiness (`:803-812`).
        let mut chicken = hen("猜拳");
        page!(&root, [b"1", b" "], |term, session, path| guess(
            &mut term,
            &mut chicken,
            rock
        ));
        assert_eq!(chicken.losee, 1, "the chicken won");
        assert_eq!(chicken.happy, 2, "and is happier for it");
    }

    /// An answer that is not one of the three hands leaves at `:721-724`: no chicken hand is drawn,
    /// no verdict, no counter.
    #[tokio::test]
    async fn a_hand_that_is_not_one_of_three_is_not_a_hand() {
        let root = root("guess-bad");
        let mut chicken = hen("猜拳");
        let (ret, screen) = page!(&root, [b"z"], |term, session, path| async {
            let ret = guess(&mut term, &mut chicken, 7).await;
            (ret, ())
        });
        assert_eq!(ret.0, -1, "the error input of `:721-724`");
        assert!(screen.contains("[1]-剪刀 [2]-石头 [3]-布: "), "{screen}");
        assert!(!screen.contains("小鸡:"), "{screen}");
        assert_eq!((chicken.winn, chicken.losee), (0, 0));
    }

    /// `create_a_egg` (`:165-217`) with nothing typed: `name_tmp` starts as `宝宝` (`:175`) and
    /// `get_data` counts the buffer it leaves behind, so Enter alone is a name. The egg weighs twice
    /// the hour it was laid in and starts with ten meals and five snacks (`:202-203`).
    #[tokio::test]
    async fn an_egg_nobody_named_is_called_baby() {
        let root = root("egg-default");
        let now = Local::now();
        let mut chicken = Chicken::default();

        let (ret, screen) = page!(&root, [b"\r"], |term, session, path| async {
            let ret = create_a_egg(&mut term, &mut session, &path, &mut chicken).await;
            (ret, ())
        });
        assert_eq!(ret.0, 0);
        assert!(screen.contains("帮小鸡取个好名字: "), "{screen}");
        assert_eq!(chicken.name, "宝宝".as_bytes());
        assert_eq!(chicken.weight, now.hour() as i32 * 2);
        assert_eq!(
            (chicken.day, chicken.month),
            (now.day() as i32, now.month0() as i32 + 1)
        );
        assert_eq!((chicken.food, chicken.zfood), (10, 5));
        assert_eq!(
            (chicken.satis, chicken.age, chicken.oo, chicken.happy),
            (0, 0, 0, 0)
        );
        assert_eq!(
            (chicken.clean, chicken.tiredstrong, chicken.play),
            (0, 0, 0)
        );

        let on_disk = std::fs::read(record_path(&root)).unwrap();
        assert!(
            String::from_utf8_lossy(&on_disk).contains("宝宝"),
            "{on_disk:?}"
        );
        let log = log_lines(&root);
        assert!(
            log.contains("养了一只叫 \x1b[33m宝宝\x1b[m 的小鸡\r\n"),
            "{log}"
        );
        assert!(
            log.contains(&format!("[{}/{}  ", now.month0() as i32 + 1, now.day())),
            "the stamp of `:210-214` is the hour the page opened, not the minute the name landed: {log}"
        );
    }

    /// A name the filter knows is masked (`:183-191`) and asked again — and since `:189` empties the
    /// buffer the prompt is prefilled from, the masked text is never offered back as an answer: the
    /// second round opens blank, an Enter there is no name, and only a clean third answer counts.
    #[tokio::test]
    async fn a_masked_name_is_emptied_and_asked_again() {
        let root = root("egg-badword");
        let bwf = crate::bwf::Badwords::load(write_badwords("egg-badword", "笨\n")).unwrap();
        let mut session = test_session(&root);
        session.state.publish_badwords(bwf);
        let path = setuserfile(&session);
        std::fs::remove_file(&path).ok();
        let _ = std::fs::remove_file(root.join(LOG_FILE));

        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut chicken = Chicken::default();
        let script: &[&[u8]] = &["笨蛋\r".as_bytes(), b"\r", "花花\r".as_bytes()];
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(
                create_a_egg(&mut term, &mut session, &path, &mut chicken),
                feed(&mut client, script)
            );
        })
        .await
        .expect("the third answer is the third round of the loop");
        let _ = term.flush().await;
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();

        assert!(
            screen.contains("笨") && screen.contains("蛋"),
            "the first round echoes what was typed: {screen}"
        );
        assert!(
            !screen.contains('*'),
            "the mask of `:189` stays inside the buffer that is then emptied: {screen}"
        );
        assert_eq!(
            screen.matches("帮小鸡取个好名字: ").count(),
            3,
            "a masked name and an empty Enter each ask again: {screen}"
        );
        assert_eq!(
            screen.matches("帮小鸡取个好名字: 宝宝").count(),
            1,
            "and only the first of the three opens on the prefill: {screen}"
        );
        assert_eq!(
            chicken.name,
            "花花".as_bytes(),
            "the masked buffer is never the one `:192` copies"
        );
        let log = log_lines(&root);
        assert!(
            log.contains("花花"),
            "and the log sees the clean name: {log}"
        );
        assert!(
            !log.contains("笨") && !log.contains("宝宝"),
            "not the word the filter knew, nor the prefill it was typed onto: {log}"
        );
    }

    /// `:107-111`: an egg that cannot be opened again after it was laid is reported and the game
    /// refused. The record it leaves on disk stays the egg — unparsed, unsaved — and the
    /// `Error in chicken data` of `:119-122` is never reached because there is no handle to read
    /// through. A process the kernel never refuses — root — cannot answer this, and says so.
    #[test]
    fn an_egg_that_cannot_be_read_back_refuses_the_game() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let root = root("egg-blind");

        // `captured` puts the subscriber on this thread, so the hatching runs on a runtime that
        // never leaves it — and the session it needs has to wait for that runtime too.
        let mut outcome = None;
        let (_common, error) = crate::log::captured(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime that stays on the thread carrying the log");
            outcome = Some(rt.block_on(async {
                let record = record_path(&root);
                std::fs::write(&record, "0 1 1 0 0 0 0 0 0 0 0 0 0 0 旧鸡 ").unwrap();
                #[cfg(unix)]
                std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o200)).unwrap();
                if std::fs::File::open(&record).is_ok() {
                    return (record, true, None, String::new(), String::new());
                }
                let (laid, drew) = page!(&root, [b"\r"], |term, session, path| load_chicken(
                    &mut term,
                    &mut session,
                    &path
                ));
                #[cfg(unix)]
                std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
                let on_disk =
                    String::from_utf8_lossy(&std::fs::read(&record).unwrap()).into_owned();
                (record, false, laid, drew, on_disk)
            }));
        });
        let (record, readable, laid, drew, on_disk) = outcome.expect("the page ran");
        if readable {
            eprintln!(
                "{} is readable after all: the guard needs a reader the kernel refuses",
                record.display()
            );
            return;
        }

        assert!(
            drew.contains("帮小鸡取个好名字: "),
            "the egg was really laid: {drew}"
        );
        assert!(laid.is_none(), "and the record it left was refused");
        assert!(
            error.contains(&format!("Error!!cannot open file '{}'", record.display())),
            "{error}"
        );
        assert!(
            !error.contains("Error in chicken data"),
            "nothing was read, so nothing could be short: {error}"
        );
        assert!(
            on_disk.contains("宝宝"),
            "the file holds the egg the naming round wrote, neither the old record nor a saved chicken: {on_disk}"
        );
    }

    /// A filter file for the naming tests, in the one-pattern-per-line format [`crate::bwf`] reads.
    fn write_badwords(name: &str, body: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lbbs-chicken-bwf-{name}-{}", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Clearing the line with `ESC` is not an answer: the loop of `:179-195` asks again, and the
    /// name typed after it is the name the egg gets.
    #[tokio::test]
    async fn clearing_the_name_asks_again() {
        let root = root("egg-esc");
        let mut chicken = Chicken::default();
        let (ret, _) = page!(
            &root,
            [b"\x1b", "小黑\r".as_bytes()],
            |term, session, path| async {
                let ret = create_a_egg(&mut term, &mut session, &path, &mut chicken).await;
                (ret, ())
            }
        );
        assert_eq!(ret.0, 0);
        assert_eq!(chicken.name, "小黑".as_bytes());
        assert!(
            std::fs::read(record_path(&root))
                .unwrap()
                .windows(6)
                .any(|w| w == "小黑".as_bytes())
        );
    }

    /// `[1]` with an empty store (`:455-459`) is `pressany(5)` and nothing else: `没食物啦..` over the
    /// menu, one key, and the numbers untouched.
    #[tokio::test]
    async fn an_empty_food_store_is_the_only_answer() {
        let root = root("no-food");
        let mut chicken = hen("饿了");
        let mut wallet = Wallet::default();
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b'1',
                0,
            )
            .await
        });

        assert!(screen.contains("没食物啦.."), "{screen}");
        assert!(
            !screen.contains("Ｃｏｋｅ"),
            "the meal itself never prints: {screen}"
        );
        assert_eq!(
            (
                chicken.food,
                chicken.weight,
                chicken.satis,
                chicken.tiredstrong
            ),
            (0, 0, 0, 0)
        );
    }

    /// A meal is the arithmetic of `:475-485`: one food, one fatigue, one satisfaction, and weight by
    /// `5 - age` while the chicken is under five — which is why a chicken whose birthday has not come
    /// this month (`age` negative) gains more than five.
    #[tokio::test]
    async fn a_meal_fattens_a_young_chicken_by_its_age() {
        let root = root("eat");
        let mut chicken = Chicken {
            food: 3,
            weight: 10,
            age: 2,
            satis: 1,
            ..hen("小花")
        };
        let mut wallet = Wallet::default();
        let (again, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b'1',
                0,
            )
            .await
        });

        assert!(again, "the meal is not an exit");
        assert!(
            screen.contains("Ｃｏｋｅ"),
            "the scene of `:463-471`: {screen}"
        );
        assert!(screen.contains("我在吃饭"), "pressany(0): {screen}");
        assert_eq!(
            (chicken.food, chicken.tiredstrong, chicken.satis),
            (2, 1, 2)
        );
        assert_eq!(chicken.weight, 13, "10 + (5 - 2)");

        // Past five it is the single gram of `:484`.
        let mut grown = Chicken {
            food: 3,
            weight: 10,
            age: 40,
            ..hen("大鸡")
        };
        page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut grown,
                &mut Wallet::default(),
                b'1',
                0,
            )
            .await
        });
        assert_eq!(grown.weight, 11);

        // A negative age is a gift: 5 - (-7) is twelve grams off one meal.
        let mut backdated = Chicken {
            food: 3,
            weight: 10,
            age: -7,
            ..hen("倒")
        };
        page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut backdated,
                &mut Wallet::default(),
                b'1',
                0,
            )
            .await
        });
        assert_eq!(backdated.weight, 22);
    }

    /// Two hundred and one grams is the `death()` of `:498-501`, which the chicken reaches through
    /// both warnings: `太重了啦..肥鸡~~` at a hundred and one, `鸡撑晕了~~` at a hundred and fifty-one,
    /// and no message at all past two hundred — just `呜...小鸡挂了....` and a new egg to name.
    #[tokio::test]
    async fn a_chicken_stuffed_past_two_hundred_grams_dies() {
        let root = root("eat-to-death");
        let mut chicken = Chicken {
            food: 5,
            weight: 200,
            age: 10,
            clean: 0,
            ..hen("肥鸡")
        };
        let mut wallet = Wallet::default();
        page!(
            &root,
            [b" ", b" ", b" ", b" ", "新鸡\r".as_bytes()],
            |term, session, path| async {
                handle_key(
                    &mut term,
                    &mut session,
                    &path,
                    &mut chicken,
                    &mut wallet,
                    b'1',
                    0,
                )
                .await
            }
        );

        assert_eq!(
            chicken.name,
            "宝宝新鸡".as_bytes(),
            "the typed half of the name rides on the `宝宝` prefill"
        );
        assert_eq!(chicken.food, 10, "the new egg starts with a full store");
        let log = log_lines(&root);
        assert!(log.contains("的小鸡 \x1b[33m肥鸡  \x1b[36m挂了~~"), "{log}");
        assert!(log.contains("养了一只叫 \x1b[33m宝宝新鸡"), "{log}");
    }

    /// `[3]` is the cleaned coop of `:534-551`: five fatigue for nothing, and `clean` back to zero.
    #[tokio::test]
    async fn cleaning_the_coop_costs_five_fatigue() {
        let root = root("clean");
        let mut chicken = Chicken {
            clean: 9,
            tiredstrong: 1,
            ..hen("臭臭")
        };
        let mut wallet = Wallet::default();
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b'3',
                0,
            )
            .await
        });

        assert!(screen.contains("便便拉光光"), "{screen}");
        assert!(screen.contains("拉便便"), "pressany(2): {screen}");
        assert_eq!((chicken.clean, chicken.tiredstrong), (0, 6));
    }

    /// `[6]` asks the database for twenty 糖糖 before it fills the store (`:560-573`). With nothing
    /// behind it — the `-1` [`crate::money`] answers when the `BEGIN` fails — the screen says
    /// `糖果不足!!`, and `food` and the cached balance both stay where they were.
    #[tokio::test]
    async fn a_shop_the_database_cannot_answer_sells_nothing() {
        let root = root("buy");
        let mut chicken = Chicken {
            food: 5,
            zfood: 2,
            ..hen("缺粮")
        };
        let mut wallet = Wallet::with_balance(400);
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b'6',
                0,
            )
            .await
        });

        assert!(screen.contains("糖果不足!!"), "{screen}");
        assert!(
            !screen.contains("食物有"),
            "the purchase line is only for the success path: {screen}"
        );
        assert_eq!((chicken.food, chicken.zfood), (5, 2));
        assert_eq!(
            wallet.balance(),
            400,
            "the cache is only moved by a committed transaction"
        );
    }

    /// `[t]` is the same test for fifty 糖糖 (`:623-636`): a database that cannot answer leaves the
    /// fatigue exactly where the rest of the screen said it should go.
    #[tokio::test]
    async fn resting_costs_money_or_does_not_happen() {
        let root = root("rest");
        let mut chicken = Chicken {
            tiredstrong: 30,
            ..hen("累了")
        };
        let mut wallet = Wallet::default();
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b't',
                0,
            )
            .await
        });

        assert!(screen.contains("糖果不足!!"), "{screen}");
        assert_eq!(
            chicken.tiredstrong, 30,
            "`tiredstrong = 0` of `:634` is only for the payer"
        );
    }

    /// `[9]` under five is the refusal of `:609-615`, and the negative ages `load_chicken` can
    /// produce are under five too.
    #[tokio::test]
    async fn a_minor_cannot_be_sold() {
        let root = root("sell-minor");
        for age in [0, 4, -7] {
            let mut chicken = Chicken {
                age,
                ..hen("未成年")
            };
            let mut wallet = Wallet::default();
            let (_, screen) = page!(&root, [b" "], |term, session, path| async {
                handle_key(
                    &mut term,
                    &mut session,
                    &path,
                    &mut chicken,
                    &mut wallet,
                    b'9',
                    0,
                )
                .await
            });
            assert!(
                screen.contains("太小了...不得贩售未成年小鸡....."),
                "age {age}: {screen}"
            );
        }
    }

    /// `[9]` at or over five opens the price line (`:847-849`) and asks `真的要卖掉小鸡?[y/N]`. A deposit
    /// the database refuses (`:863-869`) prints `无法存钱，放弃交易！` and keeps the chicken: no log
    /// line, and the record still holds its name.
    #[tokio::test]
    async fn a_sale_the_database_refuses_keeps_the_chicken() {
        let root = root("sell-refused");
        let mut chicken = Chicken {
            happy: 2,
            satis: 3,
            weight: 40,
            age: 7,
            ..hen("肥鸡")
        };
        write_record(&root, &chicken);
        let mut wallet = Wallet::default();

        let (_, screen) = page!(&root, [b"y\r"], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut wallet,
                b'9',
                0,
            )
            .await
        });

        assert!(screen.contains("真的要卖掉小鸡?[y/N]"), "{screen}");
        assert!(screen.contains("小鸡值\x1b[33;45m$$"), "{screen}");
        assert!(screen.contains("无法存钱，放弃交易！"), "{screen}");
        assert!(
            String::from_utf8_lossy(&std::fs::read(record_path(&root)).unwrap()).contains("肥鸡"),
            "the record is the one that was there"
        );
        assert!(
            !root.join(LOG_FILE).exists(),
            "the sale was never signed: {}",
            log_lines(&root)
        );
    }

    /// Answering the sale question with anything but a lowercase `y` — the `N` the prompt offers, an
    /// empty Enter, or `Yes` (`:854-861`) — leaves before the money is asked, so no refusal line
    /// either.
    #[tokio::test]
    async fn only_a_lowercase_y_sells() {
        let root = root("sell-answered");
        for answer in [b"N\r".as_slice(), b"\r", b"Yes\r"] {
            let mut chicken = Chicken {
                age: 7,
                ..hen("不肯")
            };
            write_record(&root, &chicken);
            let mut wallet = Wallet::default();
            let (_, screen) = page!(&root, [answer], |term, session, path| async {
                handle_key(
                    &mut term,
                    &mut session,
                    &path,
                    &mut chicken,
                    &mut wallet,
                    b'9',
                    0,
                )
                .await
            });
            assert!(
                screen.contains("小鸡值\x1b[33;45m$$"),
                "{answer:?}: {screen}"
            );
            assert!(
                !screen.contains("无法存钱"),
                "{answer:?} never reaches the deposit: {screen}"
            );
            assert!(
                String::from_utf8_lossy(&std::fs::read(record_path(&root)).unwrap())
                    .contains("不肯"),
                "{answer:?} keeps the chicken"
            );
        }
    }

    /// `[k]` is the case arm of `:620-622`, which calls `death()` (`:669-696`): the log line, `呜...小鸡挂了....`,
    /// `笨史了...赶出系统...`, one key, the record unlinked and a new egg hatched in its place — the
    /// naming prompt arriving in the middle of the action that asked for the kill.
    #[tokio::test]
    async fn a_killed_chicken_hatches_a_new_egg() {
        let root = root("kill");
        let mut chicken = Chicken {
            food: 1,
            weight: 50,
            ..hen("旧鸡")
        };
        write_record(&root, &chicken);
        let mut wallet = Wallet::default();

        let (_, screen) = page!(
            &root,
            [b" ", "新鸡\r".as_bytes()],
            |term, session, path| async {
                handle_key(
                    &mut term,
                    &mut session,
                    &path,
                    &mut chicken,
                    &mut wallet,
                    b'k',
                    0,
                )
                .await
            }
        );

        assert!(screen.contains("呜...小鸡挂了...."), "{screen}");
        assert!(screen.contains("笨史了...赶出系统..."), "{screen}");
        assert_eq!(chicken.name, "宝宝新鸡".as_bytes());
        assert_eq!(
            (chicken.food, chicken.zfood, chicken.weight),
            (10, 5, Local::now().hour() as i32 * 2)
        );
        assert_eq!(chicken.satis, 0, "the numbers start over with the egg");

        let log = log_lines(&root);
        assert!(log.contains("的小鸡 \x1b[33m旧鸡  \x1b[36m挂了~~"), "{log}");
    }

    /// `[c]` renames from the name the record already has (`:638-641`), and unlike the naming loop a
    /// hit leaves the old name in place: `:649-653` only copies when the count is nothing.
    #[tokio::test]
    async fn a_bad_rename_keeps_the_name_it_had() {
        let root = root("rename");
        let bwf = crate::bwf::Badwords::load(write_badwords("rename", "笨\n")).unwrap();
        let mut chicken = Chicken {
            age: 7,
            ..hen("旧名")
        };
        write_record(&root, &chicken);
        let mut wallet = Wallet::default();

        let (mut client, server) = Pipe::new();
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = test_session(&root);
        session.state.publish_badwords(bwf);
        let path = setuserfile(&session);
        let script: &[&[u8]] = &["笨\r".as_bytes(), "好\r".as_bytes()];

        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(
                async {
                    handle_key(
                        &mut term,
                        &mut session,
                        &path,
                        &mut chicken,
                        &mut wallet,
                        b'c',
                        0,
                    )
                    .await;
                    handle_key(
                        &mut term,
                        &mut session,
                        &path,
                        &mut chicken,
                        &mut wallet,
                        b'c',
                        0,
                    )
                    .await;
                },
                feed(&mut client, script)
            );
        })
        .await
        .expect("two rounds of the rename prompt");
        let _ = term.flush().await;
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();

        assert!(
            screen.contains("帮小鸡取个好名字: 旧名"),
            "the prompt opens prefilled: {screen}"
        );
        assert!(
            screen.contains("笨"),
            "the first answer is the old name plus what was typed: {screen}"
        );
        assert_eq!(
            chicken.name,
            "旧名好".as_bytes(),
            "only the clean second answer lands"
        );
    }

    /// `[5]` is `situ()` (`:814-830`): the record printed from both sides, and `pressany(4)` when the
    /// player is ahead or level, `pressany(3)` when behind.
    #[tokio::test]
    async fn the_record_is_read_from_both_sides() {
        let root = root("situ");
        let mut ahead = Chicken {
            winn: 3,
            losee: 1,
            ..hen("战绩")
        };
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut ahead,
                &mut Wallet::default(),
                b'5',
                0,
            )
            .await
        });
        assert!(screen.contains("你:\x1b[44m 3胜 1负"), "{screen}");
        assert!(
            screen.contains("鸡:\x1b[44m 1胜 3负"),
            "the chicken's row is the same numbers reversed: {screen}"
        );
        assert!(
            screen.contains("哈..赢小鸡也没多光荣"),
            "pressany(4): {screen}"
        );

        let mut behind = Chicken {
            winn: 1,
            losee: 4,
            ..hen("战绩")
        };
        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            handle_key(
                &mut term,
                &mut session,
                &path,
                &mut behind,
                &mut Wallet::default(),
                b'5',
                0,
            )
            .await
        });
        assert!(screen.contains("笨蛋..输给鸡?"), "pressany(3): {screen}");
    }

    /// Any other byte at the question (`:659`) is no action at all, and `q` is the only answer that
    /// stops the loop without redrawing (`:656-658`).
    #[tokio::test]
    async fn an_unknown_answer_only_redraws_and_q_leaves() {
        let root = root("menu");
        let mut chicken = Chicken {
            weight: 20,
            day: 1,
            month: 1,
            food: 2,
            zfood: 2,
            ..hen("菜单")
        };
        write_record(&root, &chicken);
        let before = chicken.clone();

        let (_, screen) = page!(&root, [b"?\r", b"q\r"], |term, session, path| async {
            select_menu(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut Wallet::default(),
            )
            .await
        });

        assert!(screen.contains("要做些什么呢?: "), "{screen}");
        assert!(
            screen.contains("c 改名字   k 杀鸡   t 消除非疲劳($50)   q 退出"),
            "the help bar of `:427`: {screen}"
        );
        assert!(
            screen.contains("重量:20"),
            "the unknown answer redrew the status line: {screen}"
        );
        assert_eq!(chicken, before, "and changed nothing");
    }

    /// A chicken past twenty fatigue is told so (`:446-452`) before whatever it was asked to do runs —
    /// the rest screen does not replace the action, it prints over the top of the screen.
    #[tokio::test]
    async fn the_rest_screen_is_painted_before_the_answer_counts() {
        let root = root("menu-tired");
        let mut chicken = Chicken {
            tiredstrong: 21,
            clean: 9,
            weight: 20,
            ..hen("累了")
        };
        write_record(&root, &chicken);

        let (_, screen) = page!(&root, [b"3\r", b" ", b"q\r"], |term, session, path| async {
            select_menu(
                &mut term,
                &mut session,
                &path,
                &mut chicken,
                &mut Wallet::default(),
            )
            .await
        });

        assert!(
            screen.contains("呜~~~小鸡会累坏的...要先去休息一下.."),
            "{screen}"
        );
        assert!(screen.contains("休    养     中"), "{screen}");
        assert!(
            screen.contains("拉便便"),
            "the cleaning still ran underneath: {screen}"
        );
        assert_eq!(
            (chicken.clean, chicken.tiredstrong),
            (0, 26),
            "and its numbers still moved"
        );
    }

    /// `show_chicken` (`:219-415`): the numbers of `:234-251`, with 糖糖 right-aligned in eight like
    /// `%8d`, the picture `age` picks, and the two menu rows.
    #[tokio::test]
    async fn the_status_line_carries_the_wallet_and_the_record() {
        let root = root("show");
        let mut chicken = Chicken {
            weight: 37,
            day: 21,
            month: 11,
            satis: 4,
            age: 3,
            happy: 9,
            clean: 2,
            tiredstrong: 6,
            food: 8,
            zfood: 2,
            ..hen("小花")
        };
        let wallet = Wallet::with_balance(12345);

        let (_, screen) = page!(&root, [], |term, session, path| async {
            show_chicken(&mut term, &mut session, &path, &mut chicken, &wallet).await
        });

        assert!(screen.contains("\x1b[33mName:小花\x1b[m"), "{screen}");
        assert!(screen.contains("\x1b[45mAge :3岁\x1b[m"), "{screen}");
        assert!(
            screen.contains("重量:37  食物:8  零食:2  疲劳:6  便便:2"),
            "{screen}"
        );
        assert!(
            screen.contains("生日:11月21日  糖糖:   12345  快乐度:9  满意度:4"),
            "%8d: {screen}"
        );
        assert!(
            screen.contains(PICTURE_2),
            "age three is the second picture: {screen}"
        );
        assert!(
            screen
                .contains("[1]-吃饭     [2]-吃零食   [3]-清理鸡舍   [4]-跟小鸡猜拳  [5]-目前战绩"),
            "{screen}"
        );
        assert!(
            screen.contains("[6]-买鸡饲料$20  [7]-买零食$30  [8]-吃大补丸  [9]-卖鸡喔 "),
            "{screen}"
        );
        assert_eq!(
            chicken.weight, 37,
            "chictime is zero, so nothing thinned it"
        );
    }

    /// The smell warning of `:403-412` is the only place `show_chicken` stops for a key: past ten
    /// `便便` the row `便便好多..臭臭...` appears at `moveto(10, 30)` and the redraw waits for the
    /// reader.
    #[tokio::test]
    async fn a_dirty_coop_interrupts_the_redraw() {
        let root = root("show-dirty");
        let mut chicken = Chicken {
            clean: 12,
            weight: 20,
            age: 3,
            ..hen("臭臭")
        };
        let wallet = Wallet::default();

        let (_, screen) = page!(&root, [b" "], |term, session, path| async {
            show_chicken(&mut term, &mut session, &path, &mut chicken, &wallet).await
        });
        assert!(screen.contains("便便好多..臭臭..."), "{screen}");
        assert!(
            screen.contains("[9]-卖鸡喔"),
            "and the rows below it still came: {screen}"
        );

        // Fifteen is `:406-407`: the death is announced over the warning, mid-draw — and the
        // `press_any_key()` of `:410` still waits for the reader afterwards, because the branch was
        // taken on the old `clean` and the new egg's zero does not close it.
        let root = self::root("show-dead");
        let mut dying = Chicken {
            clean: 16,
            weight: 20,
            age: 3,
            ..hen("快臭死")
        };
        write_record(&root, &dying);
        let wallet = Wallet::default();
        let (_, screen) = page!(
            &root,
            [b" ", "新鸡\r".as_bytes(), b" "],
            |term, session, path| async {
                show_chicken(&mut term, &mut session, &path, &mut dying, &wallet).await
            }
        );
        assert!(screen.contains("便便好多..臭臭..."), "{screen}");
        assert_eq!(dying.name, "宝宝新鸡".as_bytes());
        assert!(
            log_lines(&root).contains("的小鸡 \x1b[33m快臭死  \x1b[36m挂了~~"),
            "the death was signed"
        );
    }

    /// `weight < 0` (`:230-233`) is the other way into `death`, and it is checked before anything is
    /// drawn: the screen gets no status line at all, only the two rows of `:689-690` and a new egg.
    #[tokio::test]
    async fn a_starved_chicken_dies_before_the_screen() {
        let root = root("show-thin");
        let mut chicken = Chicken {
            weight: -1,
            age: 3,
            ..hen("饿死")
        };
        write_record(&root, &chicken);
        let wallet = Wallet::default();

        let (_, screen) = page!(
            &root,
            [b" ", "新鸡\r".as_bytes()],
            |term, session, path| async {
                show_chicken(&mut term, &mut session, &path, &mut chicken, &wallet).await
            }
        );

        assert!(screen.contains("呜...小鸡挂了...."), "{screen}");
        assert!(
            !screen.contains("重量:-1"),
            "the status line of `:234` never ran: {screen}"
        );
        assert_eq!(chicken.name, "宝宝新鸡".as_bytes());
    }

    /// The `switch` of `:254-382` covers nothing above sixteen, and `:386-400` is the 大怪鸟 that
    /// answers for all of it: one picture for every age past sixteen, whatever the number.
    #[tokio::test]
    async fn anything_past_sixteen_is_the_same_monster() {
        let root = root("show-monster");
        let wallet = Wallet::default();
        for age in [17i32, 120, 4000] {
            let mut chicken = Chicken {
                age,
                weight: 20,
                ..hen("怪")
            };
            let (_, screen) = page!(&root, [], |term, session, path| async {
                show_chicken(&mut term, &mut session, &path, &mut chicken, &wallet).await
            });
            assert!(screen.contains(PICTURE_MONSTER), "age {age}: {screen}");
            assert!(screen.contains("我是大怪鸟"), "{screen}");
        }
    }
}
