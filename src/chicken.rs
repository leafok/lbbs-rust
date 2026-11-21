/* SPDX-License-Identifier: GPL-3.0-or-later */
//! 电子小鸡: the pocket chicken one user raises in a file under their own uid, reached from the
//! top menu's `【 电子小鸡 】` item.
//!
//! Port of `utils/game/chicken.c` — `chicken_main` (`:60-84`), `load_chicken` (`:86-123`),
//! `save_chicken` (`:125-135`), `create_a_egg` (`:137-188`), `show_chicken` (`:190-386`),
//! `select_menu` (`:388-638`), `death` (`:640-667`), `pressany` (`:669-680`), `guess`
//! (`:682-751`), `win_c` (`:753-762`), `tie` (`:764-772`), `lose` (`:774-783`), `situ`
//! (`:785-801`) and `sell` (`:803-857`). The menu reaches it through `exec_mbem`
//! (`src/menu_proc.c:107-110`) on the parameter `C@mod:game/so/libgame.so#chicken_main`
//! (`conf/menu.conf:238`), and the 糖糖 it spends is [`crate::money`].
//!
//! The record is one line of fourteen numbers and a name in `var/chicken/<UID>`, and the shared
//! `var/chicken/log` says who hatched, who sold and who lost. Age is never stored as an age:
//! `load_chicken` rebuilds it from the birth day and month the file does keep (`:119-120`), so the
//! picture on the screen is a function of the calendar rather than of the number in the file.
//!
//! # Deviations from C
//!
//! * The record lives in a [`Chicken`] handed from function to function, not in the globals of
//!   `:36-44`. Of those the file only reads and writes fourteen; `angery`, `sick`, `gold`, `x`,
//!   `ran`, `q_mon`, `p_mon`, `bank`, `birth`, `cfp`, `buf` and `buf1` are named and never used at
//!   all, and `agetmp` (`:93`) and `last` (`:104`, `:109`) are written and never read, so none of
//!   them has a field here.
//! * `chictime` (`:39`) *is* read — `show_chicken` (`:192-200`) thins and dirties the chicken past
//!   200 of them — but no line in `chicken.c` ever assigns it, so the branch is dead in C and is
//!   kept dead here with a test on both sides of it.
//! * `fopen(fname, "r+")` (`:98`) is asked for read *and* write, so a record the session cannot
//!   write to is treated as absent and hatched over. The port asks for the same two modes.
//! * `fscanf` (`:111-112`) matches each `%d` and stops at the first token a number will not start
//!   in; [`Chicken::parse`] splits on whitespace and reads every field with
//!   [`crate::config::c_atoi`], which answers `0` where C would have left the rest of the line
//!   unread. The two agree on every line `save_chicken` and `create_a_egg` wrote, and the count C
//!   tested (`:113-116`) is the count reported here.
//! * `save_chicken`'s `fopen(fname, "r+")` (`:129`) seeks to the front of the file and does not
//!   truncate, so a shortened name leaves its own tail behind — visible in the reference tree's own
//!   `var/chicken/12` — and `%s` stops at the first space, so the tail is never read back. The port
//!   writes the same way, and C's NULL `FILE` handover on a vanished file is a logged error here.
//! * `create_a_egg`'s naming loop (`:151-166`) is left by a name, by `SYS_server_exit`, or by
//!   nothing at all: a `get_data` that times out leaves `chicken_name` empty and asks again, one
//!   `bbs_delay` at a time. The port takes the timeout as the reader having gone and writes the egg
//!   with the empty name it would have written on shutdown — the same record either way. The buffer
//!   the loop re-asks with is `name_tmp` (`:146`), which the previous round left masked, and which
//!   starts out holding `宝宝`: a nameless Enter names the chicken 宝宝, and because `get_data`
//!   (`src/screen.c:326`) starts counting at that prefill and appends to it, a typed name rides on
//!   it too — `宝宝` then `新鸡` is accepted as `宝宝新鸡`.
//! * `srand((unsigned) time(NULL))` and `rand() % 3` (`:697-698`) are glibc's additive-feedback
//!   generator, whose answer for a given second is a property of libc rather than of this game.
//!   [`opponent`] mixes the same one-second seed into one of the same three hands: a second still
//!   picks one hand for everyone and all three still come up, but which one is not glibc's answer.
//! * `check_badwords` (`:155`, `:616`) returns a negative when the filter cannot be consulted, and
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

/// `CHICKEN_NAME_LEN` (`:27-28`), and the `+ 1` on every buffer that holds one.
const CHICKEN_NAME_LEN: usize = 40;

/// `DATA_FILE` (`:30`), the directory [`setuserfile`] puts the uid under — [`crate::paths::VAR_CHICKEN_DIR`],
/// which is the directory the daemon creates at startup.
const DATA_FILE: &str = crate::paths::VAR_CHICKEN_DIR;

/// `LOG_FILE` (`:31`), the one file every player's hatchings, sales and deaths share.
const LOG_FILE: &str = "var/chicken/log";

/// The `current_action` the game signs the online row with (`:74`).
pub const ACTION: &str = "CHICKEN";

/// `cstate` (`:33-34`): ten slots and seven words. The three that were never initialised print
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

/// The name `create_a_egg` seeds its prompt with (`:146`), so an Enter with nothing typed hatches
/// a chicken called 宝宝.
const EGG_NAME: &str = "宝宝";

/// The one-byte answers `:404` and `:825` read, and the three hands of `:692`.
const KEY_ROCK: u32 = b'2' as u32;
const KEY_PAPER: u32 = b'3' as u32;
const KEY_YES: u8 = b'y';
const KEY_SCISSORS: u32 = b'1' as u32;

/// The pictures of the `switch (age)` of `:225-353` and the 大怪鸟 of `:357-371`.
const PICTURE_0: &str = "  ●●●●\r\n●  ●●  ●\r\n●●●●●●\r\n●●    ●●\r\n●●●●●●\r\n  ●●●●   ";
const PICTURE_2: &str = "    ●●●●●●\r\n  ●            ●\r\n●    ●    ●    ●\r\n●                ●\r\n●      ●●      ●\r\n●                ●\r\n●                ●\r\n  ●            ●\r\n    ●●●●●●   ";
const PICTURE_4: &str = "      ●●●●●●\r\n    ●            ●\r\n  ●  ●        ●  ●\r\n  ●                ●\r\n  ●      ●●      ●\r\n●●●    ●●      ●●\r\n  ●                ●\r\n  ●                ●\r\n    ●  ●●●●  ●\r\n      ●      ●  ●\r\n                ●    ";
const PICTURE_6: &str = "   ●    ●●●●●●\r\n●  ●●  ●        ●\r\n●              ●    ●\r\n  ●●●                ●\r\n●                      ●\r\n●  ●●                ●\r\n  ●  ●                ●\r\n      ●                ●\r\n        ●            ●\r\n          ●●●●●●        ";
const PICTURE_8: &str = "    ●●          ●●\r\n  ●●●●      ●●●●\r\n  ●●●●●●●●●●●\r\n  ●                  ●\r\n  ●    ●      ●    ●\r\n●                      ●\r\n●        ●●●        ●\r\n  ●                  ●\r\n●    ●          ●  ●\r\n  ●●            ●●●\r\n  ●                  ●\r\n    ●              ●\r\n      ●  ●●●  ●\r\n      ●  ●    ●\r\n        ●               ";
const PICTURE_11: &str = "        ●●●●●●\r\n      ●    ●    ●●\r\n    ●  ●      ●  ●●\r\n  ●●              ●●●\r\n●              ●    ●●\r\n●●●●●●●●      ●●\r\n  ●                  ●●\r\n    ●        ●  ●    ●\r\n    ●        ●  ●    ●\r\n    ●          ●      ●\r\n      ●              ●\r\n        ●  ●●●  ●\r\n        ●  ●  ●  ●\r\n          ●      ●             ";
const PICTURE_13: &str = "              ●●●●\r\n      ●●●●●●●●\r\n    ●●●●●●●●●●\r\n  ●●●●●●●●●●●●\r\n  ●    ●●●●●●●●●\r\n●●    ●            ●●\r\n●●●●                ●\r\n  ●                    ●\r\n    ●●            ●●\r\n  ●    ●●●●●●  ●\r\n  ●                  ●\r\n    ●                  ●\r\n      ●                ●\r\n    ●●●            ●●●        ";
const PICTURE_15: &str = "  ●    ●●●●●●\r\n●  ●●  ●        ●\r\n●              ●    ●\r\n  ●●●                ●\r\n●                      ●\r\n●  ●●                ●\r\n  ●  ●                ●\r\n      ●        ●  ●    ●\r\n      ●          ●      ●\r\n      ●                  ●\r\n        ●              ●\r\n        ●  ●●  ●●●\r\n        ●  ●●  ●\r\n          ●    ●             ";
const PICTURE_MONSTER: &str = "          ●●●●●●●\r\n        ●          ●●●\r\n      ●    ●    ●  ●●●\r\n  ●●●●●●●        ●●\r\n  ●          ●          ●\r\n  ●●●●●●●          ●            \x1b[1;5;31m我是大怪鸟\x1b[m\r\n  ●        ●            ●\r\n  ●●●●●●            ●\r\n  ●                    ●\r\n  ●                    ●\r\n    ●                ●\r\n●●  ●            ●\r\n●      ●●●●●●  ●●\r\n  ●                      ●\r\n●●●    我是大怪鸟       ●●● ";

/// The meal (`:434-442`), the snack (`:482-488`), the cleaned-out coop (`:508-517`) and the
/// 大补丸 (`:564-568`).
const SCENE_EAT: &str = "       □□□□□□\r\n         ∵∴ □  □\r\n              □  □                             □□□□  □          \r\n              □  □     □              □      □□□□□□□            \r\n         Ｃｏｋｅ □    _□□□□□□□□□_    □□□□□□□□                  \r\n             □   □     ％％％％％％％％％       □—∩∩—□          \r\n            □    □     □□□□□□□□□       │Mcdonald│      　　　 　\r\n           □     □     ※※※※※※※※※　     □————□          \r\n       □□□□□□      □□□□□□□□□     □□□□□□□□ ";
const SCENE_SNACK: &str = "             □\r\n       \x1b[33;1m□\x1b[m□○\r\n       \x1b[37;42m■■\x1b[m\r\n       \x1b[32m□□\x1b[m\r\n       \x1b[32;40;1m□□\x1b[m\r\n       \x1b[31m □ \x1b[m\r\n      \x1b[31m □□\x1b[m   \x1b[32;1m水果酒冰淇淋苏打\x1b[m   嗯!好喝!   ";
const SCENE_CLEAN: &str = "\x1b[1;36m                              □□□□□\x1b[m\r\n\x1b[1;33m                             『\x1b[37m□□□□\x1b[m\r\n\x1b[1;37m                               □□□□\x1b[m\r\n\x1b[1;37m             □□□□□□□□\x1b[32m◎\x1b[37m□□□□\x1b[m\r\n\x1b[37m             □□□□□□□□□\x1b[1;37m□□□□\x1b[m\r\n\x1b[37m             □□□□□□□□□\x1b[1;33m □\x1b[m\r\n\x1b[36m                  □□□□□□\x1b[1;33m□□\x1b[m\r\n\x1b[1;36m                  □□□□□□\x1b[m\r\n  \x1b[1;37m                □□□□□□\x1b[m\r\n                  耶耶耶...便便拉光光...                              ";
const SCENE_PILL: &str = "\r\n               □□□□\r\n               □□□□\r\n               □□□□\r\n                          偷吃禁药大补丸.....";

/// The three verdicts of `guess` (`:758`, `:768`, `:780`).
const VERDICT_WIN: &str = "判定:小鸡输了....    >_<~~~~~\r\n\r\n                                 ";
const VERDICT_TIE: &str =
    "判定:平手                    -_-\r\n\r\n                                              ";
const VERDICT_LOSE: &str =
    "小鸡赢罗                      ∩∩\r\n                               □       ";

/// The record as `var/chicken/<UID>` holds it: the fourteen numbers of the `fprintf` of
/// `:130-131` in that order, then the name. C declares them all `int` (`:36-44`), so `i32`.
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
    /// `chictime` (`:39`): read by `show_chicken`, assigned by nothing — see the header note.
    chictime: i32,
}

impl Chicken {
    /// `fprintf(fp, "%d %d … %s ", …)` of `:130-131` and `:173-174`, trailing space included.
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

    /// The `fscanf` of `:111-112`, and the fifteen the return of `:113` is compared against.
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

/// `setuserfile(fname, sizeof(fname), DATA_FILE)` (`:67`), which is `snprintf("%s/%d", …, uid)`
/// (`bbs.c:161-166`): each record lives in the shared directory under its uid.
fn setuserfile(session: &Session) -> PathBuf {
    session
        .root
        .join(DATA_FILE)
        .join(session.privs.uid.to_string())
}

/// `chicken_main` (`:60-84`). The `-1`, `-2` and `0` of `:64`, `:71` and `:83` are returned to
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

/// `load_chicken` (`:86-123`), keeping the record in place the way the C globals are refilled.
/// `None` is the `-1` of `:99-103`, which only a `create_a_egg` that could not write produces —
/// and which `chicken_main` (`:69-72`) turns into its `-2`.
async fn load_chicken(term: &mut Terminal, session: &mut Session, path: &Path) -> Option<Chicken> {
    // `:94-96`: the clock is read before the naming loop can spend a minute inside `create_a_egg`,
    // and it is this fixed moment the age is computed from however long the name took.
    let now = Local::now();

    // `:98`: `r+` wants to write as well as read, so an unwritable file counts as no file.
    let existing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok();

    if existing.is_none() {
        let mut egg = Chicken::default();
        if create_a_egg(term, session, path, &mut egg).await < 0 {
            return None;
        }
    }
    drop(existing);

    // `:111-116`: the reopened `fp` is read whether or not an egg was just written, and a short
    // line is the `Error in chicken data` of `:113-116`.
    let (mut chicken, fields) = Chicken::parse(&std::fs::read(path).unwrap_or_default());
    if fields != 15 {
        tracing::error!("Error in chicken data");
    }

    // `:119-120`: the stored age is dropped and rebuilt from the birth day and month. `+12` then
    // `% 12` counts whole months since the birthday, so a chicken of a year and a day is one day
    // old again, and in the days before its birthday inside the month the answer is negative —
    // which draws no picture at all (`picture` has no case for it) and makes `[1]` fatten it by
    // `5 - age`.
    chicken.age =
        (now.month0() as i32 + 1 + 12 - chicken.month) % 12 * 31 + (now.day() as i32 - chicken.day);

    Some(chicken)
}

/// `save_chicken` (`:125-135`): the front of the existing file is overwritten and the tail of a
/// longer previous line is left exactly where it was — see the header note.
fn save_chicken(path: &Path, chicken: &Chicken) {
    let text = chicken.line();
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(mut file) => {
            if let Err(err) = file.write_all(text.as_bytes()) {
                tracing::error!("fwrite({}) error: {err}", path.display());
            }
        }
        // C hands a NULL `FILE` to `fprintf`; the record it meant to write is lost either way.
        Err(err) => tracing::error!("fopen({}) error: {err}", path.display()),
    }
}

/// `create_a_egg` (`:137-188`): ask for a name until one passes the badword filter, then write the
/// egg and sign the shared log. A negative is the error `load_chicken` (`:100-103`) leaves on.
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

    // `:151-166`: an empty name *is* the loop condition, so a rejected or absent answer asks again.
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
        // `check_badwords` masks `name_tmp` in place (`:155`), so the next round of the loop opens
        // with the `*`-masked text already on its line, and the `ret > 0` of `:159-162` is the
        // `continue` that shows it.
        initial = masked;
        if hit || typed.is_empty() {
            continue;
        }
        chicken.name = typed;
    }

    // `:173-174`: the egg weighs twice the hour it was laid at, and starts with ten meals and five
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

    // `fopen(fname, "w")` (`:168`) truncates, which is the one write in this file that does.
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

/// `show_chicken` (`:190-386`): the numbers, the picture the calendar age picks, the smell
/// warning, and the nine keys.
async fn show_chicken(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &Wallet,
) {
    // `:192-200`: the ageing `chictime` gates, which nothing in C ever raises.
    if chicken.chictime >= 200 {
        chicken.weight -= 5;
        chicken.clean += 3;
        if chicken.tiredstrong > 2 {
            chicken.tiredstrong -= 2;
        }
    }
    if chicken.weight < 0 {
        // `:201-204` comes before the screen: a chicken already too thin does not get one.
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
        // The `switch` of `:225-353` has no `default`, and no `case` for a negative age: C leaves
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
            // `:377-378`: the death happens mid-warning, and the rows below it still print.
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

/// The eight `case` groups of `:225-353`, in the ages they cover.
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

/// `select_menu` (`:388-638`): the help bar, the question, and one action per answer.
async fn select_menu(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &mut Wallet,
) -> i32 {
    // `:392-395`: `ptime` is read once, before the loop, which is the second the `猜拳` bonus of
    // `:525` is taken from however many actions follow.
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
            // `:404-407`: a timeout leaves the menu with no save beyond the one `chicken_main` does.
            return 0;
        };
        let answer = typed.first().copied().unwrap_or(b'\0');

        // `:409-415`: the access time, then the heartbeat that keeps `CHICKEN` as the action.
        session.last_access_tm = session::now();
        if let Err(err) = session.online_update(None).await {
            tracing::error!("user_online_update(NULL) error: {err}");
        }

        if chicken.tiredstrong > 20 {
            // `:417-423`: the rest is painted and then the action runs anyway.
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

/// The `switch (inbuf[0])` of `:424-630`. The return is C's `loop`: only `q` of `:627-629` stops
/// the redraw of `:632-635`.
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
            // `:449-456`: a young chicken gains by how young it is.
            chicken.weight += if chicken.age < 5 { 5 - chicken.age } else { 1 };

            if chicken.weight > 100 {
                too_heavy(term, "太重了啦..肥鸡~~你想撑死鸡啊？....哇咧○●××").await;
            }
            if chicken.weight > 150 {
                too_heavy(term, "鸡撑晕了~~").await;
            }
            if chicken.weight > 200 {
                // `:469-472`: no message, just the two rows `death` prints.
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
                // `:534-539`: `<= 0` covers `-1`, `-2` and the impossible `0` of `:126-129`.
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
            // `:609-612`: the rename starts from the name the record already has.
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
                // `:620-624`: a hit leaves the old name in place — unlike the naming loop, which asks
                // again.
                chicken.name = typed;
            }
        }
        b'q' => return false,
        // `:630`: any other byte is no action at all, and the screen is redrawn.
        _ => {}
    }

    true
}

/// The two `moveto(9, 30)` warnings of `:457-468` and `:494-499`, which share everything but words.
async fn too_heavy(term: &mut Terminal, message: &str) {
    term.moveto(9, 30);
    term.print(message);
    screen::press_any_key(term).await;
}

/// `pressany` (`:669-680`): the status line over the menu, one key, and the line cleared again.
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

/// `iflush` (`:445`, `:674`, `:678`, `:689`): the flush C wants before it waits for a key.
async fn iflush(term: &mut Terminal) {
    if let Err(err) = term.flush().await {
        tracing::error!("io_flush() error: {err}");
    }
}

/// `guess` (`:682-751`). `-1` is the error input of `:692-695`, which `:524` throws away.
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
            // `:721-726`.
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

/// `srand((unsigned) time(NULL))` and `rand() % 3` (`:697-698`): one hand per second, three hands
/// in all. See the header note on why the mixing is the port's own.
fn opponent(seed: u64) -> i32 {
    let mut state = seed | 1;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (state % 3) as i32
}

/// `win_c` (`:753-762`): the player's win, which is the chicken's loss.
async fn win_c(term: &mut Terminal, chicken: &mut Chicken) {
    chicken.winn += 1;
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_WIN);
}

/// `tie` (`:764-772`): C's `tie(void)` moves no counter, so it takes no record.
async fn tie(term: &mut Terminal) {
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_TIE);
}

/// `lose` (`:774-783`): the chicken wins, and is happier for it.
async fn lose(term: &mut Terminal, chicken: &mut Chicken) {
    chicken.losee += 1;
    chicken.happy += 2;
    screen::clrtobot(term, 20);
    term.moveto(20, 0);
    term.print(VERDICT_LOSE);
}

/// `situ` (`:785-801`): the record, printed from both sides, and one of the two comments on it.
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

/// `death` (`:640-667`): the log line, two rows of text, the record deleted, and a new egg hatched
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

    // `:663-664`: the file goes, and `load_chicken` hatches a new one over the top of it.
    let _ = std::fs::remove_file(path);
    if let Some(fresh) = load_chicken(term, session, path).await {
        *chicken = fresh;
    }
}

/// `sell` (`:803-857`). The return codes of `-1` (`:826-832`), `-2` (`:834-840`, `:842-846`) and
/// `0` are all thrown away by `:589`.
async fn sell(
    term: &mut Terminal,
    session: &mut Session,
    path: &Path,
    chicken: &mut Chicken,
    wallet: &mut Wallet,
) -> i32 {
    let now = Local::now();

    // `:816-820`: happiness, satisfaction, weight, age, and a bonus decided by the second the
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
        // `:829-832`: `N`, an empty answer and `Yes` all refuse; only a lowercase `y` sells.
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

/// The sum of `:816-820`, in the order C adds it.
fn price(happy: i32, satis: i32, weight: i32, age: i32, second: i32) -> i32 {
    let mut sel = 0;
    sel += happy * 10;
    sel += satis * 7;
    sel += (second % 9) * 10;
    sel += weight;
    sel += age * 10;
    sel
}

/// The `fopen(LOG_FILE, "a")` and `fprintf` of `:177-185`, `:650-658` and `:842-850`: one
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
    /// the line is the one `fprintf` of `:130-131` would have written: space separated, one space
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
            "the field order of `:130-131`"
        );
        let (back, fields) = Chicken::parse(line.as_bytes());
        assert_eq!(back, chicken);
        assert_eq!(fields, 15, "the count `:113` compares against");
    }

    /// `%s` reads one token, so a record with anything after the name has it ignored — which is what
    /// makes the tail `save_chicken` leaves behind invisible.
    #[test]
    fn a_sixteenth_field_is_not_part_of_the_name() {
        let line = "1 2 3 4 5 6 7 8 9 10 11 12 13 14 小鸡 TAIL-OF-A-LONGER-NAME ".as_bytes();
        let (chicken, fields) = Chicken::parse(line);
        assert_eq!(chicken.name, "小鸡".as_bytes());
        assert_eq!(fields, 15, "fifteen were matched and the rest left unread");

        // A line with too few fields is the `Error in chicken data` of `:113-116`, and the numbers it
        // does not carry read as the `0` `c_atoi` answers.
        let (short, fields) = Chicken::parse(b"5 6 7 ");
        assert_eq!(fields, 3);
        assert_eq!((short.weight, short.day, short.month), (5, 6, 7));
        assert_eq!((short.satis, short.age, short.food), (0, 0, 0));
        assert!(short.name.is_empty());
    }

    /// The eight `case` groups of `:225-353`, and the two ages that draw nothing: a negative one,
    /// which the `age <= 16` test of `:224` lets through into a `switch` with no matching `case`, and
    /// anything above sixteen, which takes the 大怪鸟 of `:357` instead.
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
            "no `case` matches a chicken whose birthday is still to come"
        );
    }

    /// The `+12` then `% 12` of `:119` counts whole months since the birthday and forgets the years,
    /// and it counts days by simple subtraction, so a birthday that has not come yet this month ages
    /// the chicken *backwards*.
    /// Hatched today, aged by the calendar: see the note on `:119-120` in the header.
    async fn age_of(root: &Path) -> Chicken {
        let (loaded, _) = page!(root, [], |term, session, path| load_chicken(
            &mut term,
            &mut session,
            &path
        ));
        loaded.expect("the record is there")
    }

    #[tokio::test]
    async fn the_age_is_the_calendar_subtraction_of_load_chicken() {
        let now = Local::now();
        let month = now.month0() as i32 + 1;

        // Hatched today: `tm_mon + 1 + 12 - month` is 12, and `% 12` makes it nothing.
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

        // Born on the first of this month, a year ago or yesterday: the same number, because the
        // modulo cannot tell a year from nothing.
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

        // Born on the 28th, looked at before the 28th: the day subtraction goes negative and stays
        // there, which is the age `picture` has no case for and `[1]` fattens by `5 - age`.
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
        assert_eq!(chicken.age, now.day() as i32 - 28);
        if now.day() < 28 {
            assert!(
                chicken.age < 0,
                "the birthday has not come yet: {:?}",
                chicken.age
            );
            assert_eq!(
                picture(chicken.age),
                None,
                "and the screen is left without a picture"
            );
        }
    }

    /// `save_chicken` opens `"r+"` and does not truncate (`:129`), so a chicken renamed shorter keeps
    /// the tail of the longer name in its file — the reason the reference tree's own records end in
    /// debris. `%s` stops at the first space, so the debris is never read back.
    #[tokio::test]
    async fn a_shorter_record_leaves_the_tail_of_the_longer_one() {
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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.starts_with("0 0 0 0 0 0 0 0 0 0 0 0 4 0 短 "),
            "{text:?}"
        );
        assert!(
            text.contains("长长"),
            "the tail of the previous name is still in the file: {text:?}"
        );

        let (back, fields) = Chicken::parse(&bytes);
        assert_eq!(
            back.name,
            "短".as_bytes(),
            "and none of it reaches the name"
        );
        assert_eq!(fields, 15);
    }

    /// `chictime` (`:39`) is read by `show_chicken` (`:192-200`) and assigned by no line of
    /// `chicken.c`, so the thinning and dirtying it gates cannot happen: nothing that reaches the
    /// field can raise it past 200.
    #[test]
    fn no_record_ever_carries_a_chictime() {
        let (chicken, _) = Chicken::parse("50 1 1 0 0 0 0 0 0 0 0 0 10 5 鸡 ".as_bytes());
        assert_eq!(chicken.chictime, 0, "the fifteen fields do not include one");
        assert_eq!(Chicken::default().chictime, 0);
        assert_eq!(hen("新").chictime, 0);
    }

    /// The sum of `:816-820`, including the `(tm_sec % 9) * 10` that makes a sale worth a different
    /// price at each second of the minute.
    #[test]
    fn a_sale_is_worth_what_c_adds_up() {
        assert_eq!(price(2, 3, 40, 7, 8), 20 + 21 + 80 + 40 + 70);
        assert_eq!(price(0, 0, 0, 0, 0), 0);
        assert_eq!(price(0, 0, 0, 0, 9), 0, "the tenth second wraps to nothing");
        assert_eq!(price(0, 0, 0, 0, 17), 80, "% 9 and not / 9");
        assert_eq!(price(1, 1, 1, 1, 1), 10 + 7 + 10 + 1 + 10);
    }

    /// `srand(time(NULL))` and `rand() % 3` (`:697-698`): the chicken has one hand per second, and it
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

    /// `guess` (`:682-751`): the outcome table of `:717-747`, driven by a seed chosen for the hand it
    /// gives the chicken rather than by the clock.
    #[tokio::test]
    async fn the_guess_table_moves_the_record_the_way_c_writes_it() {
        let root = root("guess");
        let scissors = (0..200u64).find(|s| opponent(*s) == 0).unwrap();
        let rock = (0..200u64).find(|s| opponent(*s) == 1).unwrap();
        let cloth = (0..200u64).find(|s| opponent(*s) == 2).unwrap();

        // 石头 against the chicken's 石头 is the `tie()` of `:733`, and `tie` of `:764-772` takes no
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

        // 布 against 布 ties the same way (`:744`), and 石头 against 剪刀 is the `win_c()` of `:731`,
        // which counts the hand for the player at `:753-762`.
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

        // 剪刀 against 石头 is the `lose()` of `:724`, which counts against the player and pays the
        // chicken two happiness (`:774-783`).
        let mut chicken = hen("猜拳");
        page!(&root, [b"1", b" "], |term, session, path| guess(
            &mut term,
            &mut chicken,
            rock
        ));
        assert_eq!(chicken.losee, 1, "the chicken won");
        assert_eq!(chicken.happy, 2, "and is happier for it");
    }

    /// An answer that is not one of the three hands leaves at `:692-695`: no chicken hand is drawn,
    /// no verdict, no counter.
    #[tokio::test]
    async fn a_hand_that_is_not_one_of_three_is_not_a_hand() {
        let root = root("guess-bad");
        let mut chicken = hen("猜拳");
        let (ret, screen) = page!(&root, [b"z"], |term, session, path| async {
            let ret = guess(&mut term, &mut chicken, 7).await;
            (ret, ())
        });
        assert_eq!(ret.0, -1, "the error input of `:692-695`");
        assert!(screen.contains("[1]-剪刀 [2]-石头 [3]-布: "), "{screen}");
        assert!(!screen.contains("小鸡:"), "{screen}");
        assert_eq!((chicken.winn, chicken.losee), (0, 0));
    }

    /// `create_a_egg` (`:137-188`) with nothing typed: `name_tmp` starts as `宝宝` (`:146`) and
    /// `get_data` counts the buffer it leaves behind, so Enter alone is a name. The egg weighs twice
    /// the hour it was laid in and starts with ten meals and five snacks (`:173-174`).
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
            "the stamp of `:181-185` is the hour the page opened, not the minute the name landed: {log}"
        );
    }

    /// A name the filter knows is masked (`:155-162`) and asked again — and because
    /// `check_badwords` masks the buffer the prompt is prefilled from, the second round opens with
    /// the masked text already on the line, so one more Enter lets it in.
    #[tokio::test]
    async fn a_bad_name_comes_back_masked_and_is_accepted() {
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
        let script: &[&[u8]] = &["笨蛋\r".as_bytes(), b"\r"];
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(
                create_a_egg(&mut term, &mut session, &path, &mut chicken),
                feed(&mut client, script)
            );
        })
        .await
        .expect("the second Enter is the second round of the loop");
        let _ = term.flush().await;
        let screen = String::from_utf8_lossy(&client.take_available()).into_owned();

        assert!(
            screen.contains("笨"),
            "the first round echoes what was typed: {screen}"
        );
        assert!(screen.contains("蛋"), "{screen}");
        assert!(
            screen.contains("帮小鸡取个好名字: 宝宝***蛋"),
            "the second round opens on the masked buffer, `宝宝` and all: {screen}"
        );
        assert_eq!(
            chicken.name,
            "宝宝***蛋".as_bytes(),
            "the masked buffer is the one `:163` copies"
        );
        assert!(
            log_lines(&root).contains("宝宝***蛋"),
            "and reaches the log that way"
        );
    }

    /// A filter file for the naming tests, in the one-pattern-per-line format [`crate::bwf`] reads.
    fn write_badwords(name: &str, body: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lbbs-chicken-bwf-{name}-{}", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Clearing the line with `ESC` is not an answer: the loop of `:151-166` asks again, and the
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

    /// `[1]` with an empty store (`:426-430`) is `pressany(5)` and nothing else: `没食物啦..` over the
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

    /// A meal is the arithmetic of `:446-456`: one food, one fatigue, one satisfaction, and weight by
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
            "the scene of `:434-442`: {screen}"
        );
        assert!(screen.contains("我在吃饭"), "pressany(0): {screen}");
        assert_eq!(
            (chicken.food, chicken.tiredstrong, chicken.satis),
            (2, 1, 2)
        );
        assert_eq!(chicken.weight, 13, "10 + (5 - 2)");

        // Past five it is the single gram of `:455`.
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

    /// Two hundred and one grams is the `death()` of `:469-472`, which the chicken reaches through
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

    /// `[3]` is the cleaned coop of `:505-522`: five fatigue for nothing, and `clean` back to zero.
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

    /// `[6]` asks the database for twenty 糖糖 before it fills the store (`:531-544`). With nothing
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

    /// `[t]` is the same test for fifty 糖糖 (`:594-607`): a database that cannot answer leaves the
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
            "`tiredstrong = 0` of `:605` is only for the payer"
        );
    }

    /// `[9]` under five is the refusal of `:580-586`, and the negative ages `load_chicken` can
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

    /// `[9]` at or over five opens the price line (`:818-820`) and asks `真的要卖掉小鸡?[y/N]`. A deposit
    /// the database refuses (`:834-840`) prints `无法存钱，放弃交易！` and keeps the chicken: no log
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
    /// empty Enter, or `Yes` (`:825-832`) — leaves before the money is asked, so no refusal line
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

    /// `[k]` is `death()` (`:591-593` → `:640-667`): the log line, `呜...小鸡挂了....`,
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

    /// `[c]` renames from the name the record already has (`:609-612`), and unlike the naming loop a
    /// hit leaves the old name in place: `:620-624` only copies when the count is nothing.
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

    /// `[5]` is `situ()` (`:785-801`): the record printed from both sides, and `pressany(4)` when the
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

    /// Any other byte at the question (`:630`) is no action at all, and `q` is the only answer that
    /// stops the loop without redrawing (`:627-629`).
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
            "the help bar of `:398`: {screen}"
        );
        assert!(
            screen.contains("重量:20"),
            "the unknown answer redrew the status line: {screen}"
        );
        assert_eq!(chicken, before, "and changed nothing");
    }

    /// A chicken past twenty fatigue is told so (`:417-423`) before whatever it was asked to do runs —
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

    /// `show_chicken` (`:190-386`): the numbers of `:205-222`, with 糖糖 right-aligned in eight like
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

    /// The smell warning of `:374-383` is the only place `show_chicken` stops for a key: past ten
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

        // Fifteen is `:377-378`: the death is announced over the warning, mid-draw — and the
        // `press_any_key()` of `:381` still waits for the reader afterwards, because the branch was
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

    /// `weight < 0` (`:201-204`) is the other way into `death`, and it is checked before anything is
    /// drawn: the screen gets no status line at all, only the two rows of `:660-661` and a new egg.
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
            "the status line of `:205` never ran: {screen}"
        );
        assert_eq!(chicken.name, "宝宝新鸡".as_bytes());
    }

    /// The `switch` of `:225-353` covers nothing above sixteen, and `:357-371` is the 大怪鸟 that
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
