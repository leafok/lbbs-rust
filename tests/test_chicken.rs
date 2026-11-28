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
    String::from_utf8_lossy(&std::fs::read(root.join(LOG_FILE)).unwrap_or_default()).into_owned()
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
            let on_disk = String::from_utf8_lossy(&std::fs::read(&record).unwrap()).into_owned();
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
    let path = std::env::temp_dir().join(format!("lbbs-chicken-bwf-{name}-{}", std::process::id()));
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
            String::from_utf8_lossy(&std::fs::read(record_path(&root)).unwrap()).contains("不肯"),
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
        screen.contains("[1]-吃饭     [2]-吃零食   [3]-清理鸡舍   [4]-跟小鸡猜拳  [5]-目前战绩"),
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
