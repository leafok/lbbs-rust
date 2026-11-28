use super::*;
use crate::term::{ByteIo, Pipe, Terminal};

/// The callers save into the article body limit; the tests only need one big enough for
/// what they assert.
const BUF: usize = 1 << 16;

fn doc(text: &str) -> EditorData {
    EditorData::load(text.as_bytes(), false)
}

fn saved(data: &EditorData) -> Vec<u8> {
    data.save(BUF).unwrap()
}

/// The editor over a byte pipe: the client end comes back so a test can feed keys and
/// read the painted screen.
fn editor_pipe() -> (Terminal, Session, Pipe) {
    let (client, server) = Pipe::new();
    let mut session = crate::session::offline_session();
    // The insert and delete paths call `online_update(None)`; a fresh action time keeps it
    // inside the refresh interval and therefore off the database.
    session.current_action_tm = session::now();
    (Terminal::new(ByteIo::Pipe(server)), session, client)
}

fn painted(client: &mut Pipe) -> String {
    String::from_utf8_lossy(&client.take_available()).into_owned()
}

/// What a test feeds the editor, with the exit key the flows behind [`editor_display`]
/// always press.
fn keys(client: &Pipe, input: &[u8]) {
    client.push(input);
    client.push(&[ctrl(b'W') as u8]);
}

/// The editor with a wall-clock guard: a key sequence that does not end it would otherwise
/// wait out [`MAX_USER_IDLE_TIME`].
async fn edit(term: &mut Terminal, session: &mut Session, data: &mut EditorData) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), editor_display(term, session, data))
        .await
        .expect("the editor stopped reading keys")
}

// ---- the document model ----

#[test]
fn a_carriage_return_costs_no_cells() {
    let data = doc("hello\r\nworld");
    assert_eq!(data.total(), 2);
    assert_eq!(data.display_line(0), b"hello\r\n");
    assert_eq!(data.display_line_length(0), 7);
    assert_eq!(data.display_line_width(0), 6);
    assert_eq!(data.display_line(1), b"world");
    assert_eq!(data.display_line_width(1), 5);
    // `split_line` counts the `'\r'` into the byte offset it returns without charging it a
    // cell, so the third column of the row is the `'\n'` two bytes further in: the row
    // reads `ab` in two cells and the newline is the third.
    assert_eq!(doc("ab\r\ncd").col_to_offset(0, 3), (3, 2));
}

#[test]
fn a_buffer_ending_in_a_newline_opens_a_row() {
    assert_eq!(doc("one\ntwo\n").total(), 3);
    assert_eq!(doc("one\ntwo").total(), 2);
    assert_eq!(doc("").total(), 1);
}

#[test]
fn tabs_load_as_spaces() {
    assert_eq!(doc("a\tb").display_line(0), b"a b");
}

#[test]
fn a_long_line_wraps_at_the_screen_width() {
    let data = doc(&"x".repeat(SCREEN_COLS + 5));
    assert_eq!(data.total(), 2);
    assert_eq!(data.display_line_length(0), SCREEN_COLS);
    assert_eq!(data.display_line_length(1), 5);
    assert_eq!(saved(&data), "x".repeat(SCREEN_COLS + 5).into_bytes());
}

#[test]
fn save_round_trips_and_leaves_a_byte_for_the_terminator() {
    assert_eq!(saved(&doc("one\ntwo\n")), b"one\ntwo\n");
    assert_eq!(doc("abc").save(3), None);
    assert_eq!(doc("abc").save(4), Some(b"abc".to_vec()));
}

#[test]
fn an_enter_breaks_the_line_at_the_cursor() {
    let mut data = doc("hello");
    let (mut line, mut offset) = (0i64, 2i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
        .unwrap();
    assert_eq!(data.display_line(0), b"he\n");
    assert_eq!(data.display_line(1), b"llo");
    // The cursor starts the next display line, which is what ends the inner key loop.
    assert_eq!((line, offset), (1, 0));
    assert_eq!(saved(&data), b"he\nllo");
}

/// An Enter on a row that is filled to the last cell. The break writes its line feed into the
/// row the cursor stands on (`:397-399`) and re-splits only the data line behind it (`:468`),
/// so the full row stays one row — the cursor lands at the head of the second one, not the
/// third. Measured against C's `editor_data_insert` for the same bytes.
#[test]
fn enter_on_a_row_filled_to_the_last_cell_lands_on_the_next_row() {
    let mut data = doc(&"a".repeat(SCREEN_COLS));
    assert_eq!(data.total(), 1);

    let (mut line, mut offset) = (0i64, SCREEN_COLS as i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
        .unwrap();

    assert_eq!(
        (line, offset),
        (1, 0),
        "the cursor starts the new data line"
    );
    assert_eq!(last_updated, 1);
    assert_eq!(data.total(), 2);
    assert_eq!(data.display_line_length(0), SCREEN_COLS + 1);
    assert_eq!(data.display_line_width(0), SCREEN_COLS);
    assert_eq!(
        data.display_line(0),
        format!("{}\n", "a".repeat(SCREEN_COLS)).into_bytes()
    );
    assert_eq!(data.display_line(1), b"");
    assert_eq!(
        saved(&data),
        format!("{}\n", "a".repeat(SCREEN_COLS)).into_bytes()
    );

    // The document those bytes reload into wraps differently: the line feed that leaves no
    // cell of its row behind gets a row of its own there.
    let reloaded = doc(&String::from_utf8_lossy(&saved(&data)));
    assert_eq!(reloaded.total(), 3);
}

/// The same break one row down, where the data line already spans two rows: `:433-435` glues
/// the line feed to the row under the cursor, which is the head's last one.
#[test]
fn enter_at_the_end_of_the_second_row_of_a_long_line_joins_that_row() {
    let mut data = doc(&"b".repeat(2 * SCREEN_COLS));
    assert_eq!(data.total(), 2);

    let (mut line, mut offset) = (1i64, SCREEN_COLS as i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
        .unwrap();

    assert_eq!((line, offset), (2, 0));
    assert_eq!(data.total(), 3);
    assert_eq!(data.display_line_length(0), SCREEN_COLS);
    assert_eq!(data.display_line_length(1), SCREEN_COLS + 1);
    assert_eq!(data.display_line_width(1), SCREEN_COLS);
    assert_eq!(data.display_line(2), b"");
}

/// A line feed that arrives as text instead of as an Enter goes through the insert that keeps
/// the line whole (`:453-468`), and that one re-splits — nothing is glued, and the cursor
/// still ends up inside the row the line feed was put on.
#[test]
fn a_line_feed_inserted_as_text_keeps_its_own_row_on_a_full_line() {
    let mut data = doc(&"a".repeat(SCREEN_COLS));
    let (mut line, mut offset) = (0i64, SCREEN_COLS as i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"\n", &mut last_updated)
        .unwrap();

    assert_eq!(data.total(), 2);
    assert_eq!(data.display_line_length(1), 1);
    assert_eq!((line, offset), (1, 1));
}

#[test]
fn typing_past_the_end_of_a_row_carries_the_cursor_onto_the_next() {
    // `:543-550`: an insert that fills a display line leaves the cursor one cell into the
    // row after it.
    let mut data = doc(&"x".repeat(SCREEN_COLS));
    assert_eq!(data.total(), 1);
    let (mut line, mut offset) = (0i64, SCREEN_COLS as i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"y", &mut last_updated)
        .unwrap();
    assert_eq!(data.total(), 2);
    assert_eq!((line, offset), (1, 1));
}

#[test]
fn a_deletion_at_the_end_of_a_line_joins_the_next_one() {
    let mut data = doc("ab\ncd");
    let (mut line, mut offset) = (0i64, 2i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, false, false),
        1
    );
    assert_eq!(data.total(), 1);
    assert_eq!(saved(&data), b"abcd");
    // And once the cursor is past the merged line's last byte there is nothing left.
    offset = 4;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, false, false),
        0
    );
}

/// `editor.c:692-694` (`6dd6846`) refuses a join that would leave the merged line without the
/// two bytes a split at its end keeps — the pair of checks C's `test_editor.c` #9 makes.
#[test]
fn a_join_that_would_leave_no_room_at_the_end_of_the_line_is_refused() {
    for fill in [
        MAX_EDITOR_DATA_LINE_LENGTH - 3,
        MAX_EDITOR_DATA_LINE_LENGTH - 4,
    ] {
        let text = format!("{}\nef", "g".repeat(fill));
        let mut data = doc(&text);
        let row = 12i64;
        assert_eq!(
            (data.total(), data.display_line_length(row)),
            (14, fill + 1 - 12 * SCREEN_COLS),
            "the full line takes thirteen rows and the short one a fourteenth"
        );

        let mut line = row;
        let mut offset = fill as i64 - 12 * SCREEN_COLS as i64;
        let mut last_updated = 0i64;
        let ret = data.delete(&mut line, &mut offset, &mut last_updated, false, false);
        if fill == MAX_EDITOR_DATA_LINE_LENGTH - 3 {
            assert_eq!(
                ret, 0,
                "the merged line would have no byte left for its terminator"
            );
            assert_eq!(
                (saved(&data), data.total()),
                (text.into_bytes(), 14),
                "the refused join changes nothing"
            );
        } else {
            assert_eq!(ret, 1, "one byte shorter and the lines do become one");
            assert_eq!(
                (saved(&data), data.total()),
                (format!("{}ef", "g".repeat(fill)).into_bytes(), 13),
                "the join drops the row the short line had"
            );
        }
    }
}

/// `editor.c:120-124` and `:692-694` (`6dd6846`) reserve the two bytes a split at the end of
/// a data line writes, so a line only breaks there once that room is gone.
#[test]
fn typing_at_the_end_of_a_line_breaks_it_only_when_no_room_is_left() {
    fn end(data: &EditorData) -> (i64, i64) {
        let row = data.total() as i64 - 1;
        (row, data.display_line_length(row) as i64)
    }

    for fill in 0..5 {
        let len = MAX_EDITOR_DATA_LINE_LENGTH - 3 + fill;
        let breaks = len >= MAX_EDITOR_DATA_LINE_LENGTH - 2;
        let text = "a".repeat(len);
        let sep = if breaks { "\n" } else { "" };
        let mut data = doc(&text);
        let (mut line, mut offset) = end(&data);
        let mut last_updated = 0i64;

        data.insert(&mut line, &mut offset, b"z", &mut last_updated)
            .unwrap();
        assert_eq!(
            saved(&data),
            format!("{text}{sep}z").into_bytes(),
            "one byte at the end of a {len} byte line"
        );

        let (mut line, mut offset) = end(&data);
        data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
            .unwrap();
        assert_eq!(
            saved(&data),
            format!("{text}{sep}z\n").into_bytes(),
            "a break at the end of a {len} byte line"
        );
    }
}

/// The fill sizes C's `test_wrapping` walks, with the display rows they take. C also counts
/// the 1024-byte blocks a long line fills; the port keeps the line in one buffer, so only
/// the rows are compared.
#[test]
fn a_loaded_line_wraps_into_the_rows_the_reference_counts() {
    for (fill, rows, tail) in [
        (80usize, 1usize, 80usize),
        (81, 2, 1),
        (200, 3, 40),
        (1022, 13, 62),
        (1023, 13, 63),
        (2000, 25, 80),
    ] {
        let data = doc(&"x".repeat(fill));
        assert_eq!(data.total(), rows, "{fill} characters take {rows} rows");
        assert_eq!(
            data.display_line_length((rows - 1) as i64),
            tail,
            "the last row of a {fill} character line"
        );
    }
}

/// C's `test_load_save` table: the display lines, lengths and widths one buffer loads into,
/// the data lines it splits into, and the round trip back out.
#[test]
fn a_buffer_loads_into_the_lines_and_widths_the_reference_measures() {
    let rows = |data: &EditorData| {
        (0..data.total() as i64)
            .map(|row| (data.display_line_length(row), data.display_line_width(row)))
            .collect::<Vec<_>>()
    };

    for (text, line_count, measured) in [
        ("", 1usize, vec![(0usize, 0usize)]),
        ("abc", 1, vec![(3, 3)]),
        ("abc\n", 2, vec![(4, 4), (0, 0)]),
        ("ab\ncd", 2, vec![(3, 3), (2, 2)]),
        ("one\ntwo\nthree", 3, vec![(4, 4), (4, 4), (5, 5)]),
        ("a\tb\n\tc", 2, vec![(4, 4), (2, 2)]),
        ("a\r\nb\rc", 2, vec![(3, 2), (3, 2)]),
    ] {
        let data = doc(text);
        assert_eq!(
            (data.lines.len(), data.total(), rows(&data)),
            (line_count, measured.len(), measured),
            "loading {text:?}"
        );
        let out = saved(&data);
        assert_eq!(
            out.as_slice(),
            text.replace('\t', " ").as_bytes(),
            "the round trip of {text:?}"
        );
    }

    let data = doc("a\tb\n\tc");
    assert_eq!(data.display_line(0), b"a b\n", "a tab loads as one space");
    assert_eq!(data.display_line(1), b" c");

    for (text, measured) in [
        ("中文", vec![(6usize, 4usize)]),
        ("a中b文c", vec![(9, 7)]),
        ("\x1b[32mred\x1b[m", vec![(11, 11)]),
    ] {
        let data = EditorData::load(text.as_bytes(), true);
        assert_eq!(rows(&data), measured, "the fixed width measure of {text:?}");
        assert_eq!(saved(&data), text.as_bytes());
    }

    let data = doc("Hello\nWorld");
    assert_eq!(data.save(12).unwrap(), b"Hello\nWorld");
    assert_eq!(data.save(11), None, "one byte short of the text");
    assert_eq!(data.save(5), None, "short of even the first line");
}

#[test]
fn a_deletion_cuts_a_multibyte_character_whole() {
    let mut data = doc("中x");
    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, false, false),
        3
    );
    assert_eq!(saved(&data), b"x");
}

#[test]
fn the_rest_of_a_row_deletes_as_one_block() {
    let mut data = doc("abcdef");
    let (mut line, mut offset) = (0i64, 2i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, true, false),
        4
    );
    assert_eq!(saved(&data), b"ab");
}

/// C's `test_editor.c` #8 with the `ae52b6a` `exclude_lf`: `Ctrl-K` takes the text of the row
/// and leaves its line feed, so the emptied row stays in the document (`editor.c:641-645`).
#[test]
fn ctrl_k_leaves_the_line_and_its_row() {
    let mut data = doc("one\ntwo\nthree");
    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, true, true),
        3,
        "the three characters go, the line feed stays"
    );
    assert_eq!(saved(&data), b"\ntwo\nthree");
    assert_eq!(data.total(), 3, "the emptied row is still a row");
    assert_eq!(data.display_line(0), b"\n");
}

/// `editor.c:346` (`ae52b6a`): a line that ends on a line feed is broken only when its text
/// plus the insert no longer fits with one byte free, not two — C's `test_editor.c` #13 types
/// in front of that line feed on a line already full to its reserve.
#[test]
fn a_terminated_line_holds_one_byte_more_than_an_unterminated_one() {
    let text = format!("{}\ntail", "a".repeat(1021));
    let mut data = doc(&text);
    assert_eq!(data.display_line_length(12), 62, "the tail row of the line");
    assert_eq!(data.display_line(12).last(), Some(&b'\n'));

    let (mut line, mut offset) = (12i64, 5i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"z", &mut last_updated)
        .unwrap();

    let mut want = text.clone();
    want.insert_str(960 + 5, "z");
    assert_eq!(
        saved(&data),
        want.as_bytes(),
        "the line only gained the character"
    );
}

/// `editor.c:385-393`, `:421-429`: the break a split writes at the end of a line that already
/// ends on a line feed is that line feed, so typing there costs the document no empty row.
#[test]
fn typing_behind_a_full_line_starts_the_next_without_a_blank_one() {
    let text = format!("{}\ntail", "a".repeat(1021));
    let mut data = doc(&text);
    let end = data.display_line_length(12) as i64;
    let (mut line, mut offset) = (12i64, end);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"q", &mut last_updated)
        .unwrap();

    let mut want = text.clone();
    want.insert_str(1022, "q");
    assert_eq!(saved(&data), want.as_bytes());
    assert!(
        !saved(&data).windows(2).any(|pair| pair == b"\n\n"),
        "the line feed the break would have written is the one the line ends on"
    );
}

/// `editor.c:388-392`: a carriage return asked for at that same place does want the empty
/// line, and it is the new data line's own line feed — a row of exactly one byte.
#[test]
fn a_break_behind_a_full_line_asks_for_the_empty_row() {
    let text = format!("{}\ntail", "a".repeat(1021));
    let mut data = doc(&text);
    let end = data.display_line_length(12) as i64;
    let (mut line, mut offset) = (12i64, end);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
        .unwrap();

    let mut want = text.clone();
    want.insert_str(1022, "\n");
    assert_eq!(
        saved(&data),
        want.as_bytes(),
        "the break leaves an empty line"
    );
    assert_eq!(
        data.display_line_length(line),
        1,
        "the empty row is the lone line feed"
    );
}

/// `editor.c:692-694`: joining a line whose text ends on a line feed keeps one free byte, not
/// two — the line feed comes along in the copy and is itself the terminator's neighbour.
#[test]
fn a_join_with_a_terminated_line_keeps_one_byte_of_reserve() {
    let text = format!("x\n{}\ntail", "a".repeat(1021));
    let mut data = doc(&text);
    let (mut line, mut offset) = (0i64, 1i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, true, false),
        1,
        "the row's line feed goes"
    );
    assert_eq!(
        saved(&data),
        format!("x{}\ntail", "a".repeat(1021)).as_bytes(),
        "the terminated line joins with one byte to spare"
    );
}

/// `editor.c:126-137` (`dd5b5d1`), the round trip C's `test_editor.c` #7 measures: a closed
/// line that spills its block comes back terminated where it spilled, and the keystrokes at
/// the end of that document still only append.
#[test]
fn an_over_long_closed_line_loads_terminated_at_the_block_boundary() {
    for count in 1..=4usize {
        let text = format!("{}\n", "d".repeat(1025)).repeat(count);
        let want = format!("{}\n{}\n", "d".repeat(960), "d".repeat(65)).repeat(count);
        let mut data = doc(&text);
        assert_eq!(
            saved(&data),
            want.as_bytes(),
            "the load of {count} over-long lines"
        );
        assert_eq!(
            data.lines.len(),
            count * 2 + 1,
            "two data lines apiece and the empty tail"
        );

        let row = data.total() as i64 - 1;
        let len = data.display_line_length(row);
        let (mut line, mut offset) = (row, len as i64);
        let mut last_updated = 0i64;
        data.insert(&mut line, &mut offset, b"tail", &mut last_updated)
            .unwrap();
        assert_eq!(
            saved(&data),
            format!("{want}tail").as_bytes(),
            "typing at the end of the capacity text"
        );

        let row = data.total() as i64 - 1;
        let len = data.display_line_length(row);
        let (mut line, mut offset) = (row, len as i64);
        data.insert(&mut line, &mut offset, b"\r", &mut last_updated)
            .unwrap();
        assert_eq!(
            saved(&data),
            format!("{want}tail\n").as_bytes(),
            "the break at the end of the capacity text"
        );
    }
}

/// `editor.c:103-114` and `:126-137` (`dd5b5d1`), from C's `test_editor.c` #13: three block
/// boundaries in one closed line load as three data lines, and typing inside the first one
/// only adds the character.
#[test]
fn a_closed_line_that_spills_two_blocks_loads_as_three_data_lines() {
    let text = format!("{}\ntail", "a".repeat(2000));
    let want = format!(
        "{}\n{}\n{}\ntail",
        "a".repeat(960),
        "a".repeat(960),
        "a".repeat(80)
    );
    let mut data = doc(&text);
    assert_eq!(
        saved(&data),
        want.as_bytes(),
        "the load of a line over two blocks"
    );
    assert_eq!(
        data.lines.len(),
        4,
        "one data line per block the reference packs"
    );

    let (mut line, mut offset) = (1i64, 10i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"x", &mut last_updated)
        .unwrap();

    let mut typed = want;
    typed.insert_str(90, "x");
    assert_eq!(
        saved(&data),
        typed.as_bytes(),
        "typing inside the first block only adds the character"
    );
    assert_eq!(
        (line, offset),
        (1, 11),
        "the cursor sits behind the character it typed"
    );
}

/// The same load with wide characters, which C measures in bytes: the boundary comes at a
/// display row, so the injected line feed never falls inside a character.
#[test]
fn a_wide_line_is_broken_between_characters_at_its_block_boundary() {
    let text = "一".repeat(400) + "\n";
    let want = format!("{}\n{}\n", "一".repeat(320), "一".repeat(80));
    let data = doc(&text);
    assert_eq!(
        saved(&data),
        want.as_bytes(),
        "the load of a wide line over one block"
    );
    assert_eq!(data.lines.len(), 3, "two blocks and the empty tail");
}

/// `editor.c:103-114` and `:130` (`dd5b5d1`), the other half of C's `test_editor.c` #13: a
/// buffer whose last line has no line feed of its own is loaded with its block boundaries
/// byte-free, so it saves back exactly as it came in and the character at a boundary still
/// belongs to the block behind it.
#[test]
fn an_unclosed_line_crosses_blocks_without_gaining_a_line_feed() {
    let text = "a".repeat(2000);
    let data = doc(&text);
    assert_eq!(
        saved(&data),
        text.as_bytes(),
        "the unclosed tail of the buffer keeps its boundaries empty"
    );

    let mut data = doc(&text);
    let (mut line, mut offset) = (11i64, 80i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, false, false),
        1,
        "the delete at the end of a block takes the character behind the boundary"
    );

    let mut want = text;
    want.remove(960);
    assert_eq!(
        (saved(&data), line, offset),
        (want.into_bytes(), 11, 80),
        "the port sits at the end of the row where C moves the cursor to the next block"
    );
}

#[test]
fn an_escape_stays_text_and_shows_as_a_star() {
    assert_eq!(
        escape_display(b"a\x1bb"),
        b"a\x1b[32m*\x1b[mb".to_vec(),
        "the ESC byte is replaced, its surroundings are untouched"
    );
}

#[test]
fn the_document_refuses_to_grow_past_the_display_line_limit() {
    let mut text = "a\n".repeat(MAX_EDITOR_DATA_LINES - 1);
    text.push('a');
    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.insert(&mut line, &mut offset, b"\r", &mut last_updated),
        Err(OverLines)
    );
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
}

/// The same cap as `editor.c:100`, where the display array is the size of the limit: a
/// body of short lines fills it and the rest of the document is not editable.
#[test]
fn loading_stops_counting_display_lines_at_the_limit() {
    let over = MAX_EDITOR_DATA_LINES + 4_000;
    let text = "a\n".repeat(over);
    let data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    // `:171-178`: the cap cut the array off in front of the row that would have held the
    // last `'\n'`, so that newline is the array's last byte and gets trimmed with it.
    assert_eq!(data.display_line(data.total() as i64 - 1), b"a");
    assert_eq!(data.display_line(data.total() as i64 - 2), b"a\n");
    assert_eq!(
        data.save(2 * MAX_EDITOR_DATA_LINES).unwrap().len(),
        2 * MAX_EDITOR_DATA_LINES - 1
    );
    assert_eq!(
        data.locate(data.total() as i64 - 1).0 + 1,
        MAX_EDITOR_DATA_LINES,
        "the last display line is the last data line C had room for"
    );
}

/// `:472-493`, `:553-564`: an edit into a document that already fills the display array
/// never adds a row — the text that needs the row the array does not have is dropped, and
/// what C measured byte for byte on the reference article is what these tests pin.
#[test]
fn a_full_document_loses_its_overflow_when_a_row_grows() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    let text = format!(
        "{}{}",
        "a\n".repeat(MAX_EDITOR_DATA_LINES - 1),
        "b".repeat(80)
    );
    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);

    let (mut line, mut offset) = (MAX_EDITOR_DATA_LINES as i64 - 1, 0i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"c", &mut last_updated)
        .unwrap();

    assert_eq!((line, offset), (MAX_EDITOR_DATA_LINES as i64 - 1, 1));
    assert_eq!(last_updated, MAX_EDITOR_DATA_LINES as i64);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    assert_eq!(
        data.save(BIG).unwrap(),
        format!(
            "{}c{}",
            "a\n".repeat(MAX_EDITOR_DATA_LINES - 1),
            "b".repeat(78)
        )
        .into_bytes(),
        "the row the array had no room for is lost, terminator with it"
    );
}

/// `:553-564`: the same cut runs on an edit that is nowhere near the end of the document:
/// a full array trims its last row to [`SCREEN_COLS`] - 1 cells whatever happened above it.
#[test]
fn a_full_document_loses_the_tail_of_its_last_row() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    let text = format!(
        "{}{}",
        "a\n".repeat(MAX_EDITOR_DATA_LINES - 2),
        "b".repeat(160)
    );
    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);

    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"c", &mut last_updated)
        .unwrap();

    assert_eq!(
        (line, offset),
        (0, 1),
        "the edit is on its own row, but the document still lost the tail of the last one"
    );
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    assert_eq!(
        data.save(BIG).unwrap(),
        format!(
            "c{}{}",
            "a\n".repeat(MAX_EDITOR_DATA_LINES - 2),
            "b".repeat(159)
        )
        .into_bytes()
    );
}

/// `:553-564` (`4358c9b`), C's `test_editor.c` #14 (a): the reserve cut walks by whole
/// codepoints, so a two-column character at the screen edge comes off entire and the document
/// loses it for good — the shape of the reference article that started this round.
#[test]
fn a_wide_character_at_the_screen_edge_is_cut_whole() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    const CAR: &[u8] = "车".as_bytes();
    let text = format!(
        "{}{}车",
        "\n".repeat(MAX_EDITOR_DATA_LINES - 1),
        "a".repeat(SCREEN_COLS - 2)
    );
    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    let last = data.total() as i64 - 1;
    assert_eq!(
        data.display_line_length(last),
        SCREEN_COLS + 1,
        "the wide tail takes three bytes for the last two columns"
    );
    assert_eq!(data.display_line_width(last), SCREEN_COLS);

    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"x", &mut last_updated)
        .unwrap();

    assert_eq!(
        data.display_line_length(last),
        SCREEN_COLS - 2,
        "the cut drops the whole wide character, not one byte of it"
    );
    assert_eq!(data.display_line_width(last), SCREEN_COLS - 2);
    let out = data.save(BIG).unwrap();
    assert!(
        !out.windows(CAR.len()).any(|w| w == CAR),
        "the wide character is gone from the saved document too"
    );

    let again = EditorData::load(&out, false);
    let last = again.total() as i64 - 1;
    assert!(
        !again
            .display_line(last)
            .windows(CAR.len())
            .any(|w| w == CAR),
        "the reloaded last row is still missing the wide character"
    );
}

/// C's `test_editor.c` #14 (b): a wide character with room for it inside [`SCREEN_COLS`] - 1
/// columns survives the cut, which then costs the single column of ASCII behind it and stops
/// on the codepoint boundary rather than mid-sequence.
#[test]
fn a_wide_character_before_its_ascii_tail_survives_the_cut() {
    let tail = format!("{}车", "a".repeat(SCREEN_COLS - 3));
    let text = format!("{}{}z", "\n".repeat(MAX_EDITOR_DATA_LINES - 1), tail);
    let mut data = doc(&text);
    let last = data.total() as i64 - 1;
    assert_eq!(data.display_line_length(last), SCREEN_COLS + 1);
    assert_eq!(data.display_line_width(last), SCREEN_COLS);

    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"y", &mut last_updated)
        .unwrap();

    assert_eq!(
        data.display_line_length(last),
        SCREEN_COLS,
        "the cut preserves the wide character and drops the ascii tail"
    );
    assert_eq!(data.display_line_width(last), SCREEN_COLS - 1);
    let row = data.display_line(last);
    assert_eq!(row[row.len() - 1], 0xa6, "the row ends on a byte");
    assert_eq!(
        row[row.len() - 3],
        0xe8,
        "the wide character begins three bytes from the end"
    );
}

/// C's `test_editor.c` #14 (c): with two wide characters at the tail the cut stops between
/// them, so the first stays whole and the second is what goes.
#[test]
fn two_wide_characters_are_cut_on_their_boundary() {
    let tail = format!("{}车车", "a".repeat(SCREEN_COLS - 4));
    let text = format!("{}{}", "\n".repeat(MAX_EDITOR_DATA_LINES - 1), tail);
    let mut data = doc(&text);
    let last = data.total() as i64 - 1;
    assert_eq!(
        data.display_line_length(last),
        SCREEN_COLS + 2,
        "two wide characters take six bytes for the last four columns"
    );
    assert_eq!(data.display_line_width(last), SCREEN_COLS);

    let (mut line, mut offset) = (0i64, 0i64);
    let mut last_updated = 0i64;
    data.insert(&mut line, &mut offset, b"q", &mut last_updated)
        .unwrap();

    assert_eq!(
        data.display_line_length(last),
        SCREEN_COLS - 1,
        "the cut stops on the codepoint boundary between the two"
    );
    assert_eq!(data.display_line_width(last), SCREEN_COLS - 2);
}

/// The same document pasted into three times over, at the top of it, inside a wrapped row
/// and with double-width text: the reference stays at [`MAX_EDITOR_DATA_LINES`] rows and
/// pays for the added text at its own end, in the byte counts `editor_data_insert` leaves.
#[test]
fn pasting_into_a_full_document_keeps_it_at_the_limit() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    let text = format!(
        "{}{}",
        "a\n".repeat(MAX_EDITOR_DATA_LINES - 2),
        "b".repeat(160)
    );

    let mut data = doc(&text);
    let (mut line, mut offset, mut last_updated) = (0i64, 0i64, 0i64);
    for _ in 0..4 {
        (line, offset, last_updated) = (0, 0, 0);
        data.insert(&mut line, &mut offset, b"0123456789", &mut last_updated)
            .unwrap();
    }
    assert_eq!(
        (line, offset, last_updated, data.total()),
        (0, 10, 0, MAX_EDITOR_DATA_LINES)
    );
    assert_eq!(data.save(BIG).unwrap().len(), 131_267);

    let mut data = doc(&text);
    for _ in 0..2 {
        (line, offset, last_updated) = (MAX_EDITOR_DATA_LINES as i64 - 2, 5, 0);
        data.insert(
            &mut line,
            &mut offset,
            b"0123456789abcdef",
            &mut last_updated,
        )
        .unwrap();
    }
    assert_eq!(
        (line, offset, last_updated, data.total()),
        (
            MAX_EDITOR_DATA_LINES as i64 - 2,
            21,
            MAX_EDITOR_DATA_LINES as i64,
            MAX_EDITOR_DATA_LINES
        )
    );
    let saved = data.save(BIG).unwrap();
    assert_eq!(saved.len(), 131_227);
    assert_eq!(
        &saved[2 * (MAX_EDITOR_DATA_LINES - 2)..2 * (MAX_EDITOR_DATA_LINES - 2) + 37],
        b"bbbbb0123456789abcdef0123456789abcdef",
        "both pastes land where they were typed, the text past the array's last row is gone"
    );

    let mut data = doc(&text);
    (line, offset, last_updated) = (MAX_EDITOR_DATA_LINES as i64 - 1, 5, 0);
    data.insert(&mut line, &mut offset, "中文".as_bytes(), &mut last_updated)
        .unwrap();
    (line, offset, last_updated) = (MAX_EDITOR_DATA_LINES as i64 - 1, 5, 0);
    data.insert(&mut line, &mut offset, "中".as_bytes(), &mut last_updated)
        .unwrap();
    assert_eq!((line, offset), (MAX_EDITOR_DATA_LINES as i64 - 1, 8));
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    assert_eq!(data.save(BIG).unwrap().len(), 131_230);
}

/// `:715`: a deletion re-splits with the rows its line already had, so the document gets
/// no longer than the array C could show.
#[test]
fn a_deletion_at_the_limit_cuts_the_document_back() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    let text = format!(
        "{}{}",
        "a\n".repeat(MAX_EDITOR_DATA_LINES - 2),
        "b".repeat(160)
    );
    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);

    let (mut line, mut offset) = (MAX_EDITOR_DATA_LINES as i64 - 2, 0i64);
    let mut last_updated = 0i64;
    assert_eq!(
        data.delete(&mut line, &mut offset, &mut last_updated, true, false),
        80,
        "the whole of the wrapped row goes"
    );

    assert_eq!((line, offset), (MAX_EDITOR_DATA_LINES as i64 - 2, 0));
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES - 1);
    assert_eq!(
        data.save(BIG).unwrap(),
        format!(
            "{}{}",
            "a\n".repeat(MAX_EDITOR_DATA_LINES - 2),
            "b".repeat(80)
        )
        .into_bytes()
    );
}

/// Testing #10 of the reference (`5da3fc6`): a line feed is worth a column of the row it ends
/// to the guard at `:552-564` but not to the loader, so a document whose last row exactly
/// fills the screen keeps all 80 columns until an edit asks for a row the array does not
/// have. Then the reserve comes off the end of the document, wherever the keystroke was.
#[test]
fn the_last_row_of_a_full_document_keeps_a_column_of_reserve() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    const LAST: i64 = MAX_EDITOR_DATA_LINES as i64 - 1;
    let text = format!(
        "{}{}",
        "\n".repeat(MAX_EDITOR_DATA_LINES - 1),
        "x".repeat(SCREEN_COLS)
    );
    let cut = "x".repeat(SCREEN_COLS - 1);

    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES);
    assert_eq!(
        data.display_line_length(LAST),
        SCREEN_COLS,
        "the screen wide last row of a full document loads whole"
    );
    assert_eq!(data.display_line_width(LAST), SCREEN_COLS);
    assert_eq!(
        data.save(BIG).unwrap(),
        text.as_bytes(),
        "a screen wide last row saves as it loaded"
    );

    let (mut line, mut offset, mut last_updated) = (0i64, 0i64, 0i64);
    data.insert(&mut line, &mut offset, b"y", &mut last_updated)
        .unwrap();
    assert_eq!(
        data.display_line_length(LAST),
        SCREEN_COLS - 1,
        "an edit at the top cuts the last row one column short"
    );
    assert_eq!(data.display_line_width(LAST), SCREEN_COLS - 1);
    assert_eq!(
        data.display_line(LAST),
        cut.as_bytes(),
        "the character lost is the last one of the document"
    );
    assert_eq!(
        data.save(BIG).unwrap(),
        format!("y{}{}", "\n".repeat(MAX_EDITOR_DATA_LINES - 1), cut).into_bytes()
    );

    // Typed at the screen edge of that last row, the keystroke is the one the guard drops:
    // the cursor is pulled back onto the reserve column and the row keeps the 79th `x`.
    let mut data = doc(&text);
    let (mut line, mut offset, mut last_updated) = (LAST, SCREEN_COLS as i64, 0i64);
    data.insert(&mut line, &mut offset, b"y", &mut last_updated)
        .unwrap();
    assert_eq!(
        (line, offset),
        (LAST, SCREEN_COLS as i64 - 1),
        "the cursor of the dropped character is pulled back one column"
    );
    assert_eq!(data.display_line_length(LAST), SCREEN_COLS - 1);
    assert_eq!(
        data.display_line(LAST),
        cut.as_bytes(),
        "the keystroke at the screen edge costs the document its last character"
    );

    // `:488-491`: the reserve column is not text either. The keystroke that asks for it is
    // paid for by the character it replaces, so the row never grows past 79 columns.
    let (mut line, mut offset, mut last_updated) = (LAST, SCREEN_COLS as i64 - 1, 0i64);
    data.insert(&mut line, &mut offset, b"z", &mut last_updated)
        .unwrap();
    assert_eq!((line, offset), (LAST, SCREEN_COLS as i64 - 1));
    assert_eq!(data.display_line_length(LAST), SCREEN_COLS - 1);
    assert_eq!(
        data.display_line(LAST),
        cut.as_bytes(),
        "the reserve column keeps the character it had"
    );
}

/// The other half of testing #10: one display line short of the array the overflow has
/// somewhere to go, so the same keystrokes cost the document nothing and the 80 columns of a
/// row stay a row of their own.
#[test]
fn a_row_short_of_the_limit_spills_onto_the_free_display_line() {
    const BIG: usize = 4 * MAX_EDITOR_DATA_LINES;
    const LAST: i64 = MAX_EDITOR_DATA_LINES as i64 - 2;
    let text = format!(
        "{}{}",
        "\n".repeat(MAX_EDITOR_DATA_LINES - 2),
        "x".repeat(SCREEN_COLS)
    );

    let mut data = doc(&text);
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES - 1);
    assert_eq!(data.display_line_length(LAST), SCREEN_COLS);

    let (mut line, mut offset, mut last_updated) = (0i64, 0i64, 0i64);
    data.insert(&mut line, &mut offset, b"y", &mut last_updated)
        .unwrap();
    assert_eq!(
        data.display_line_length(LAST),
        SCREEN_COLS,
        "an edit at the top costs the short model no column"
    );
    assert_eq!(data.total(), MAX_EDITOR_DATA_LINES - 1);

    // Typing at the screen edge of a row that is not the last of a full array grows the
    // logical line and re-wraps it: the character lands on a display line of its own without
    // a break of its own, and the document keeps every column.
    let (mut line, mut offset, mut last_updated) = (LAST, SCREEN_COLS as i64, 0i64);
    data.insert(&mut line, &mut offset, b"w", &mut last_updated)
        .unwrap();
    assert_eq!(
        data.total(),
        MAX_EDITOR_DATA_LINES,
        "the wrap takes the free row"
    );
    assert_eq!(data.display_line_length(LAST), SCREEN_COLS);
    assert_eq!(data.display_line_width(LAST), SCREEN_COLS);
    assert_eq!(data.display_line(LAST + 1), b"w");
    assert_eq!(
        data.save(BIG).unwrap(),
        format!(
            "y{}{}w",
            "\n".repeat(MAX_EDITOR_DATA_LINES - 2),
            "x".repeat(SCREEN_COLS)
        )
        .into_bytes(),
        "the wrap keeps every character of the document"
    );
}

// ---- the screen half ----

#[tokio::test]
async fn typed_text_reaches_the_document() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, b"hi");

    let mut data = doc("");
    assert_eq!(edit(&mut term, &mut session, &mut data).await, ctrl(b'W'));
    assert_eq!(saved(&data), b"hi");
}

#[tokio::test]
async fn enter_after_a_row_that_is_exactly_full_starts_the_next_line() {
    let (mut term, mut session, client) = editor_pipe();
    // Rows that fill the screen to the last cell: their `'\n'` has no cell left to share the
    // row with, which is the one case where C's incremental wrap disagrees with the wrap it
    // loads with. Text typed after such an Enter used to land inside the row above it, and a
    // pasted article came out of the editor with its lines merged.
    let paste: String = (0..3)
        .map(|i| format!("{}{}\n", "z".repeat(SCREEN_COLS - 1), i))
        .collect();
    keys(&client, paste.as_bytes());

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), paste.as_bytes());
    // Each filled row keeps the line feed the Enter wrote inside it, the way the break leaves
    // it, and the last Enter opens the blank line the cursor is on. Reading the same bytes
    // back is what splits those line feeds into rows of their own.
    assert_eq!(data.total(), 4);
}

/// The same break on the screen: the row under the cursor is the second text row, not the
/// third, and the status row counts the two rows the document has become.
#[tokio::test]
async fn enter_on_a_full_row_puts_the_cursor_on_the_next_screen_row() {
    let (mut term, mut session, mut client) = editor_pipe();
    keys(&client, format!("{}\r", "a".repeat(SCREEN_COLS)).as_bytes());

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(data.total(), 2);
    let screen = painted(&mut client);
    // The row is painted with the line feed the break left in it, and the cursor goes to the
    // head of the row after it.
    assert!(
        screen.contains(&format!("{}\n", "a".repeat(SCREEN_COLS))),
        "{screen}"
    );
    assert!(screen.ends_with("\x1b[2;1H"), "{screen}");
}

/// An edit re-wraps only the data line it touched, which has to give the same display
/// rows as splitting the whole document again — including the edits that add or remove a
/// data line, where the wrap array and the data lines are moved separately.
#[test]
fn editing_rewraps_the_same_way_as_a_full_recount() {
    let mut state = 0x2545F49144A1C6E3u64;
    let mut rand = move |limit: u64| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) % limit
    };

    let mut data = doc("one\ntwo\nthree\n");
    let mut display_line = 0i64;
    let mut offset = 0i64;
    let mut last_updated = 0i64;

    for op in 0..2000u32 {
        if rand(4) == 0 {
            data.delete(
                &mut display_line,
                &mut offset,
                &mut last_updated,
                rand(3) == 0,
                false,
            );
        } else {
            let bytes: &[u8] = match rand(5) {
                0 => b"x",
                1 => b" ",
                2 => "中".as_bytes(),
                3 => b"\n",
                _ => b"\x1b",
            };
            let _ = data.insert(&mut display_line, &mut offset, bytes, &mut last_updated);
        }

        if rand(3) == 0 {
            display_line = rand(data.total() as u64) as i64;
            offset = rand(30) as i64;
        } else {
            // Most of the time the cursor stays at the end of the last row, where the lines
            // fill up and break and the joins happen.
            display_line = data.total() as i64 - 1;
            offset = data.display_line_length(display_line) as i64;
        }

        let mut whole = data.clone();
        whole.recount();
        assert_eq!(whole.total, data.total, "display lines after edit {op}");
        assert_eq!(whole.segments, data.segments, "wrap after edit {op}");
    }
}

#[tokio::test]
async fn an_enter_breaks_the_row_and_repaints_it() {
    let (mut term, mut session, mut client) = editor_pipe();
    // The `"\r\n"` pair is one Enter, as on the wire.
    keys(&client, b"hi\rthere\r");

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"hi\nthere\n");
    // An Enter ends the inner key loop, so the rows are painted again up to the status row,
    // which is the only place the port flushes. The text typed after the last Enter is not
    // on the screen yet — like C, `Ctrl-W` returns without repainting.
    let screen = painted(&mut client);
    assert!(screen.contains("hi"), "{screen}");
    assert!(screen.contains("there"), "{screen}");
}

#[tokio::test]
async fn a_tab_fills_the_rest_of_its_stop() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, b"\tX");

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    let mut expected = vec![b' '; TAB_SIZE];
    expected.push(b'X');
    assert_eq!(saved(&data), expected);
}

#[tokio::test]
async fn the_status_row_describes_the_cursor_and_the_document() {
    let (mut term, mut session, mut client) = editor_pipe();
    keys(&client, b"\x1b[4~");

    let mut data = doc("one\ntwo\n");
    edit(&mut term, &mut session, &mut data).await;
    let screen = painted(&mut client);
    // `[row;col] 第line/total行 [mode]` — the last display line of a newline-terminated
    // document is the empty row the cursor ends on.
    assert!(
        screen.contains(
            "\x1b[1;44;33m[\x1b[32m3\x1b[33m;\x1b[32m1\x1b[33m] \
                             第\x1b[32m3\x1b[33m/\x1b[32m3\x1b[33m行 [\x1b[32m插入\x1b[33m]"
        ),
        "{screen}"
    );
}

#[tokio::test]
async fn backspace_at_the_start_of_a_row_joins_it_to_the_one_above() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, b"\x1b[B\x08");

    let mut data = doc("ab\ncd");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"abcd");
}

#[tokio::test]
async fn backspace_at_the_very_top_does_nothing() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, b"a\x08\x08");

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"");
}

#[tokio::test]
async fn ctrl_k_takes_the_rest_of_the_row() {
    let (mut term, mut session, client) = editor_pipe();
    // `abc`, one column back onto the `c`, then the block delete.
    keys(&client, b"abc\x1b[D\x0b");

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"ab");
}

#[tokio::test]
async fn insert_switch_toggles_overwrite() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, b"\x1b[2~ac");

    let mut data = doc("xyz");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"acz");
}

#[tokio::test]
async fn a_wide_character_takes_its_two_columns() {
    let (mut term, mut session, client) = editor_pipe();
    keys(&client, "中".as_bytes());

    let mut data = doc("");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), "中".as_bytes());
}

#[tokio::test]
async fn a_wide_character_deletes_whole() {
    let (mut term, mut session, client) = editor_pipe();
    // Right onto the last column of `中`, then one backspace: the character goes as one.
    keys(&client, b"\x1b[C\x08");

    let mut data = doc("中x");
    edit(&mut term, &mut session, &mut data).await;
    assert_eq!(saved(&data), b"x");
}

#[tokio::test]
async fn a_dropped_connection_ends_the_edit() {
    let (mut term, mut session, client) = editor_pipe();
    client.push(b"x");
    // Closing the client end is the end of stream the terminal reads.
    drop(client);

    let mut data = doc("");
    assert_eq!(edit(&mut term, &mut session, &mut data).await, KEY_NULL);
    // Whatever was typed before the hang-up stays in the document, as in C.
    assert_eq!(saved(&data), b"x");
}

/// Filling the first data line to SCREEN_COLS pushes the cursor onto the second display row;
/// the view must scroll so the data line below the filled one stays visible on screen.
#[tokio::test]
async fn filling_a_line_to_full_keeps_the_next_data_line_visible() {
    let (mut term, mut session, mut client) = editor_pipe();
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[B\x1b[A\x1b[C\x1b[C\x1b[C");
    for _ in 0..77 {
        input.push(b'a');
    }
    keys(&client, &input);

    let mut data = doc("abc\nkkk");
    edit(&mut term, &mut session, &mut data).await;

    let screen = painted(&mut client);
    assert!(
        screen.contains('k'),
        "second data line must remain visible: {screen}"
    );
}

/// The reported article-472411 keystrokes: down to the second data line, `Ctrl-Y` until nothing is
/// left below it, then `k`, `j`, `h`, one more `Ctrl-Y` and `b`. Each `Ctrl-Y` re-splits only the data
/// line it edited, from the row it edited (`editor.c:714-734`), so a row the cursor sits on survives an
/// emptied deletion as a zero-width row and the typing below it never reaches the first line: the digits
/// keep their 80 bytes and `b` is the whole of the second data line.
#[tokio::test]
async fn ctrl_y_then_typing_below_a_wrapped_first_line() {
    let (mut term, mut session, client) = editor_pipe();
    // Article 472411 content
    let content = "01234567890123456789012345678901234567890123456789012345678901234567890123456789\nabcdefg\n1234567890\n--\n※ 作者已于 2026-11-23 18:43:33 (UTC +0800) 修改本文※\n";

    let mut input = Vec::new();
    // Two down arrows: the first data line fills two display rows, so this is the second data line
    input.extend_from_slice(b"\x1b[B\x1b[B");
    // Delete all remaining lines (Ctrl-Y 4 times)
    input.extend_from_slice(&[0x19; 4]);
    // 'k' lands at the second data line's first column, 'jh' rides after it, the next
    // `Ctrl-Y` empties that row again and 'b' lands at its first column.
    input.push(b'k');
    input.extend_from_slice(b"jh");
    input.push(0x19);
    input.push(b'b');
    keys(&client, &input);

    let mut data = doc(content);
    edit(&mut term, &mut session, &mut data).await;

    let saved = String::from_utf8_lossy(&saved(&data)).into_owned();

    // The first data line is untouched: all 80 digits, and nothing typed joined itself to it.
    let first_line = saved.lines().next().unwrap_or("");
    assert_eq!(first_line.len(), 80, "first line: {first_line:?}");
    assert!(
        first_line.chars().all(|c| c.is_ascii_digit()),
        "first line is all digits: {first_line:?}"
    );

    let second_line = saved.lines().nth(1).unwrap_or("");
    assert_eq!(second_line, "b", "second line: {second_line:?}");
}

/// Regression for the article-472411 corruption: the first data line is exactly
/// SCREEN_COLS+'\n' (81 bytes), so it fills two display rows (row 0 = the 80 digits,
/// row 1 = the '\n'). Ctrl-Y from row 1 pulls each following line up into row 0; when
/// the last pull takes the empty trailing line, C keeps a zero-width row after the now
/// full-width line (`editor.c:714-734`) so `cursor_line()` stays a real row. A re-wrap
/// from the head drops that row, the cursor runs off the end, and the next keystroke is
/// located back onto row 0 — replacing the first digit and showing '9' where 'k' was typed.
#[tokio::test]
async fn typing_after_ctrl_y_all_below_wrapped_first_line() {
    let (mut term, mut session, client) = editor_pipe();
    let content = "01234567890123456789012345678901234567890123456789012345678901234567890123456789\nabcdefg\n1234567890\n--\n※ 作者已于 2026-11-23 18:43:33 (UTC +0800) 修改本文※\n";
    let mut input = Vec::new();
    // 1 down arrow → display row 1 = the '\n' segment of data line 0
    input.extend_from_slice(b"\x1b[B");
    // Five `Ctrl-Y`s: each pull takes the next data line up into row 0, until only the
    // 80 digits and C's empty trailing row are left.
    input.extend_from_slice(&[0x19; 5]);
    // 'k' lands on the row the cursor sits on, so it rides onto the digits.
    input.push(b'k');
    keys(&client, &input);
    let mut data = doc(content);
    edit(&mut term, &mut session, &mut data).await;
    let saved = String::from_utf8_lossy(&saved(&data)).into_owned();

    // First data line: 80 digits + 'k' = 81 bytes (k is appended, not inserted at pos 0)
    let first = saved.lines().next().unwrap_or("");
    assert_eq!(
        first.len(),
        81,
        "first data line should be 80 digits + 'k' = 81 bytes, got: {:?}",
        first
    );
    assert!(
        first.ends_with('k'),
        "'k' should be at end of first data line, not inserted at pos 0: {:?}",
        first
    );
    assert!(
        first.starts_with('0'),
        "first data line should start with '0' (not replaced by 'k'): {:?}",
        first
    );
}

/// Regression: the same keystrokes, then `Ctrl-Y` deletes the typed `k` and `b` lands on the
/// empty row the deletion left behind — the reported symptom was '8' showing there.
#[tokio::test]
async fn typing_after_repeated_ctrl_y_at_wrapped_first_line_end() {
    let (mut term, mut session, client) = editor_pipe();
    let content = "01234567890123456789012345678901234567890123456789012345678901234567890123456789\nabcdefg\n1234567890\n--\n※ 作者已于 2026-11-23 18:43:33 (UTC +0800) 修改本文※\n";
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[B");
    input.extend_from_slice(&[0x19; 5]);
    input.push(b'k');
    // `Ctrl-Y` again empties that row: 'k' was the only text on it
    input.push(0x19);
    input.push(b'b');
    keys(&client, &input);
    let mut data = doc(content);
    edit(&mut term, &mut session, &mut data).await;
    let saved = String::from_utf8_lossy(&saved(&data)).into_owned();

    // First line: 80 digits + 'b' = 81 bytes ('b' appended, not replacing first digit)
    let first = saved.lines().next().unwrap_or("");
    assert_eq!(
        first.len(),
        81,
        "first line should be 80 digits + 'b' = 81 bytes, got len {}: {:?}",
        first.len(),
        first
    );
    assert!(
        first.ends_with('b'),
        "'b' should be at end of first line: {:?}",
        first
    );
    assert!(
        first.starts_with("0123456789"),
        "first line should start with digits (not replaced): {:?}",
        first
    );
}

/// `Ctrl-Y` deletes a blank line: "a\n\nb\nc\n" has a blank '\n' row at display row 1, and
/// one `Ctrl-Y` there collapses it exactly as C does, leaving "a\nb\nc\n".
#[tokio::test]
async fn ctrl_y_removes_a_blank_line() {
    let (mut term, mut session, client) = editor_pipe();
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[B"); // down onto the blank row
    input.push(0x19); // Ctrl-Y
    keys(&client, &input);
    let mut data = doc("a\n\nb\nc\n");
    edit(&mut term, &mut session, &mut data).await;
    let saved = String::from_utf8_lossy(&saved(&data)).into_owned();
    assert_eq!(saved, "a\nb\nc\n", "the blank line is gone: {:?}", saved);
}

/// On the last content line, `Ctrl-Y` removes the text and — matching C — keeps a zero-width
/// trailing row (`editor.c:714-734`), so the next keystroke lands on it and appends ("a\nb\nk")
/// instead of corrupting the document; a second `Ctrl-Y` there is a no-op.
#[tokio::test]
async fn ctrl_y_on_last_line_keeps_empty_row_then_appends() {
    // Just the delete: the last line's text goes, an empty row survives.
    let (mut term, mut session, client) = editor_pipe();
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[B\x1b[B"); // down to "c\n" (row 2)
    input.push(0x19);
    keys(&client, &input);
    let mut data = doc("a\nb\nc\n");
    edit(&mut term, &mut session, &mut data).await;
    let text = String::from_utf8_lossy(&saved(&data)).into_owned();
    assert_eq!(
        text, "a\nb\n",
        "the last line's text is deleted: {:?}",
        text
    );
    assert_eq!(
        data.total(),
        3,
        "C's empty trailing row survives: total {}",
        data.total()
    );

    // Delete then type: 'k' lands on that empty row and appends.
    let (mut term, mut session, client) = editor_pipe();
    let mut input = Vec::new();
    input.extend_from_slice(b"\x1b[B\x1b[B");
    input.push(0x19); // Ctrl-Y removes 'c'
    input.push(0x19); // Ctrl-Y on the empty row is a no-op
    input.push(b'k');
    keys(&client, &input);
    let mut data = doc("a\nb\nc\n");
    edit(&mut term, &mut session, &mut data).await;
    let text = String::from_utf8_lossy(&saved(&data)).into_owned();
    assert_eq!(
        text, "a\nb\nk",
        "typing after the empty row appends: {:?}",
        text
    );
}

// ---- the painted screen ----

/// The bytes the editor wrote, applied to a grid: the screen the user sees rather than the
/// escape sequences that produced it. Wide enough for the whole status line.
fn screen(bytes: &[u8]) -> Vec<String> {
    const ROWS: usize = SCREEN_ROWS;
    const COLS: usize = 240;
    let blank = || vec![b' '; COLS];
    let mut grid = vec![blank(); ROWS];
    let (mut r, mut c) = (0usize, 0usize);
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                let mut j = i + 2;
                let mut params: Vec<u8> = Vec::new();
                while j < bytes.len()
                    && !((bytes[j] as char).is_ascii_alphabetic() || bytes[j] == b'~')
                {
                    params.push(bytes[j]);
                    j += 1;
                }
                let f = if j < bytes.len() { bytes[j] } else { b' ' };
                let nums: Vec<i64> = String::from_utf8_lossy(&params)
                    .split(';')
                    .map(|s| s.trim_start_matches('?').parse().unwrap_or(0))
                    .collect();
                match f {
                    b'H' | b'f' => {
                        let rr = *nums.first().unwrap_or(&1);
                        let cc = *nums.get(1).unwrap_or(&1);
                        r = (rr.max(1) as usize - 1).min(ROWS - 1);
                        c = (cc.max(1) as usize - 1).min(COLS - 1);
                    }
                    b'K' => {
                        for x in c.min(COLS - 1)..COLS {
                            grid[r.min(ROWS - 1)][x] = b' ';
                        }
                    }
                    b'J' if nums.first() == Some(&2) => {
                        for row in grid.iter_mut() {
                            *row = blank();
                        }
                    }
                    _ => {}
                }
                i = j + 1;
            } else {
                i += 1;
            }
            continue;
        }
        match b {
            b'\n' => {
                if r + 1 >= ROWS {
                    for rr in 0..ROWS - 1 {
                        grid[rr] = grid[rr + 1].clone();
                    }
                    grid[ROWS - 1] = blank();
                } else {
                    r += 1;
                }
                i += 1;
            }
            b'\r' => {
                c = 0;
                i += 1;
            }
            // The bell and the backspace the status line never uses move nothing.
            7 => i += 1,
            8 => {
                c = c.saturating_sub(1);
                i += 1;
            }
            _ => {
                if r < ROWS && c < COLS {
                    grid[r][c] = b;
                }
                c = (c + 1).min(COLS - 1);
                i += 1;
            }
        }
    }
    grid.iter()
        .map(|row| String::from_utf8_lossy(row).trim_end().to_string())
        .collect()
}

/// The editor in its own task over a byte pipe, so a test can hand the keys over in pieces and
/// read the screen back when the edit is over.
async fn spawn_edit(content: &str) -> (Pipe, tokio::task::JoinHandle<EditorData>) {
    let (client, server) = Pipe::new();
    let content = content.to_string();
    let task = tokio::spawn(async move {
        let mut term = Terminal::new(ByteIo::Pipe(server));
        let mut session = crate::session::offline_session();
        // A fresh action time keeps the paths' `online_update(None)` inside the refresh
        // interval and therefore off the database.
        session.current_action_tm = session::now();
        let mut data = doc(&content);
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            editor_display(&mut term, &mut session, &mut data),
        )
        .await;
        data
    });
    (client, task)
}

/// One burst of keys — every byte is already buffered when the editor next reads, which is
/// what a fast typist sends — and the exit that follows it. The screen and the document the
/// edit left behind come back.
async fn edit_burst(content: &str, input: &[u8]) -> (Vec<String>, EditorData) {
    let (mut client, task) = spawn_edit(content).await;
    let mut keys = input.to_vec();
    keys.push(ctrl(b'W') as u8);
    client.push(&keys);
    let data = task.await.expect("the burst ended the edit");
    (screen(&client.take_available()), data)
}

/// `[row;col] 第line/total行 [mode]` — the window the editor claims to be showing.
fn status_window(rows: &[String]) -> (i64, i64) {
    let status = rows.last().expect("the status row");
    let close = status.find(']').expect("the cursor field");
    let open = status[..close].rfind('[').expect("the cursor field");
    let row_pos: i64 = status[open + 1..close]
        .split(';')
        .next()
        .expect("the row")
        .parse()
        .expect("a row number");
    let rest = &status[close..];
    let mark = rest.find('第').expect("the line field") + '第'.len_utf8();
    let line = &rest[mark..];
    let cursor: i64 = line[..line.find('/').expect("the total")]
        .trim()
        .parse()
        .expect("a line number");
    (row_pos, cursor)
}

/// The text rows that do not show the document row the status window points at: the screen is
/// behind the text, which is the whole complaint about rows that vanish.
fn rows_behind_document(rows: &[String], data: &EditorData) -> Vec<String> {
    let (row_pos, cursor) = status_window(rows);
    let top = cursor - row_pos;
    let mut bad = Vec::new();
    for (i, shown) in rows[..SCREEN_ROWS - 1].iter().enumerate() {
        let index = top + i as i64;
        let want = if index < 0 || index >= data.total() as i64 {
            String::new()
        } else {
            let mut text = String::from_utf8_lossy(data.display_line(index)).into_owned();
            while text.ends_with('\n') || text.ends_with('\r') {
                text.pop();
            }
            text
        };
        if shown.trim_end() != want.trim_end() {
            bad.push(format!(
                "row {}: screen {:?} != document {:?}",
                i + 1,
                shown.trim_end(),
                want.trim_end()
            ));
        }
    }
    bad
}

/// A line of `n` letters, so a wrapped document is spelled out rather than pasted.
fn letters(n: usize) -> String {
    (0..n)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect::<String>()
}

/// Regression for the rows that disappear while the text stays intact: the keys arrive as one
/// batch, so `Ctrl-Y` blanks the rows below it and the `Ctrl-K` that follows narrows the paint
/// band to its own row (`editor.c:947`, and the batched `continue` at `:1093-1105`). C lets
/// the narrower band win and leaves the rows blank until the file is reopened; the port widens
/// it back over everything the batch blanked.
#[tokio::test]
async fn a_batched_delete_and_kill_repaints_the_rows_it_blanked() {
    let content = format!(
        "a\n{}\n{}\nabc\n{}\n{}\n",
        letters(5),
        letters(78),
        letters(17),
        letters(80)
    );
    // `Ctrl-Y` on the second row, then `Ctrl-K` before the screen is repainted.
    let (rows, data) = edit_burst(&content, &[0x19, 0x0b]).await;
    assert!(
        rows_behind_document(&rows, &data).is_empty(),
        "every row the batch blanked came back: {rows:#?}"
    );
}

/// Regression for the same symptom from the typing side: a character that pushes its line over
/// the row boundary shoves every row below it down one, so the redraw has to reach as far as the
/// document used to end (`editor.c:523`, `:541`). Stopping at the last row of the line that was
/// edited leaves the rows below showing the text one row too high.
#[tokio::test]
async fn a_row_created_by_typing_repaints_what_it_pushed_down() {
    let content = format!("{}\n{}\n{}\n", letters(79), letters(60), letters(60));
    let (rows, data) = edit_burst(&content, b"z").await;
    assert_eq!(data.total(), 5, "the line split into a second row");
    assert!(
        rows_behind_document(&rows, &data).is_empty(),
        "the rows the new one pushed down were redrawn: {rows:#?}"
    );
}

/// Regression for the typed character that never shows: an insert leaves its own row waiting
/// for the paint pass, and `Ctrl-W` in the same batch used to end the edit right there
/// (`:1117-1120`), so the screen kept the text as it was before the keystroke. The port paints
/// the pending row before it returns.
#[tokio::test]
async fn the_exit_key_waits_for_the_row_just_typed() {
    let (rows, data) = edit_burst("abcdef\nghijkl\n", b"z").await;
    assert_eq!(data.display_line(0), b"zabcdef\n");
    assert_eq!(rows[0], "zabcdef", "the typed letter is on the screen");
    assert!(
        rows_behind_document(&rows, &data).is_empty(),
        "the whole screen caught up: {rows:#?}"
    );
}

/// Regression for the Enter or Delete that takes two presses: a sequence split across two
/// reads used to answer an escape key and throw the rest of the buffer away, which ate both the
/// key and the Enter typed behind it. `igetch` (`io.c:430-487`) consumes the buffer the same
/// way; the port waits briefly for the rest instead.
#[tokio::test]
async fn a_key_split_across_two_reads_still_edits() {
    let (mut client, task) = spawn_edit("abc\ndef\n").await;
    client.push(b"\x1b[3");
    tokio::time::sleep(Duration::from_millis(20)).await;
    client.push(b"~\r");
    tokio::time::sleep(Duration::from_millis(20)).await;
    client.push(&[ctrl(b'W') as u8]);
    let data = task.await.expect("the edit stopped");

    assert_eq!(
        saved(&data),
        b"\nbc\ndef\n",
        "one Delete and one Enter, not an escape and a lost Enter"
    );
    let rows = screen(&client.take_available());
    assert_eq!(rows[0], "", "the row the Enter opened");
    assert_eq!(rows[1], "bc");
}

/// `ESC[H` and `ESC[F`, the xterm-style Home and End, are editing keys: C's tables
/// (`io.c:802-822`) leave them unmapped, so they fall through as text and the editor paints a
/// star. The port reads them as the keys every terminal client sends.
#[tokio::test]
async fn home_and_end_move_the_cursor_rather_than_insert_text() {
    let (rows, data) = edit_burst("one\ntwo\nthree\n", b"\x1b[F").await;
    assert!(
        !rows.iter().any(|row| row.contains('*')),
        "no escape painted as a star: {rows:#?}"
    );
    assert_eq!(
        saved(&data),
        b"one\ntwo\nthree\n",
        "the keys edited nothing"
    );
    // The last display row of a newline-terminated document is the empty one after it.
    assert_eq!(status_window(&rows).1, data.total() as i64);

    let (rows, data) = edit_burst("one\ntwo\nthree\n", b"\x1b[H").await;
    assert_eq!(status_window(&rows).1, 1);
    assert_eq!(saved(&data), b"one\ntwo\nthree\n");
    assert!(rows_behind_document(&rows, &data).is_empty());
}
