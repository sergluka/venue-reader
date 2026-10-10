//! `textDocument/selectionRange` through the protocol.
//!
//! What every case here is really checking is one rule: **each level must
//! contain the position it was asked about**. That is what decides whether a
//! cursor gets three levels or two.

mod harness;

use harness::TestClient;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

/// `8=FIX.4.4|` is bytes 0..10, so `35=0` is tag at 15, `=` at 17, value at 18.
const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

fn open(text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open(URI, text, 1);
    client
}

fn at(client: &mut TestClient, positions: &[(u32, u32)]) -> Vec<Value> {
    let positions: Vec<Value> = positions
        .iter()
        .map(|(line, character)| json!({ "line": line, "character": character }))
        .collect();
    client
        .request(
            "textDocument/selectionRange",
            json!({ "textDocument": { "uri": URI }, "positions": positions }),
        )
        .expect("answered")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The levels of one answer, outermost last, as `(start, end)` columns.
fn levels(mut node: &Value) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    loop {
        out.push((
            node["range"]["start"]["character"].as_u64().expect("start"),
            node["range"]["end"]["character"].as_u64().expect("end"),
        ));
        match node.get("parent") {
            Some(parent) => node = parent,
            None => return out,
        }
    }
}

/// A cursor in the value gets all three: the value it is in, the field that
/// value belongs to, then the line.
#[test]
fn a_cursor_in_a_value_expands_value_field_line() {
    let mut client = open(HEARTBEAT);
    let found = at(&mut client, &[(0, 18)]);
    assert_eq!(
        levels(&found[0]),
        vec![(18, 19), (15, 19), (0, 79)],
        "`0`, then `35=0`, then the line"
    );
    client.shutdown();
}

/// **The case that makes this two levels and not three.** `field_at` matches
/// the whole of `tag=value`, so a cursor on the tag is inside the field but not
/// inside the value — and a value range there would exclude the position it was
/// asked about, which the protocol forbids.
#[test]
fn a_cursor_on_the_tag_or_the_equals_starts_at_the_field() {
    let mut client = open(HEARTBEAT);
    for (character, what) in [
        (15, "the first tag digit"),
        (16, "the second"),
        (17, "the `=`"),
    ] {
        let found = at(&mut client, &[(0, character)]);
        assert_eq!(
            levels(&found[0]),
            vec![(15, 19), (0, 79)],
            "{what}: no value level, because the cursor is not in the value"
        );
    }
    client.shutdown();
}

/// Every level contains the position, over every column of a whole line rather
/// than three hand-picked ones.
#[test]
fn every_level_contains_the_position_and_its_parent() {
    let mut client = open(HEARTBEAT);
    let columns: Vec<(u32, u32)> = (0..HEARTBEAT.len() as u32).map(|c| (0, c)).collect();
    let found = at(&mut client, &columns);
    assert_eq!(found.len(), columns.len());

    for (nth, node) in found.iter().enumerate() {
        let column = nth as u64;
        let chain = levels(node);
        for (from, to) in &chain {
            assert!(
                *from <= column && column < *to || (*from == column && *from == *to),
                "column {column}: level ({from}, {to}) does not contain it"
            );
        }
        for pair in chain.windows(2) {
            let (inner, outer) = (pair[0], pair[1]);
            assert!(
                outer.0 <= inner.0 && inner.1 <= outer.1 && inner != outer,
                "column {column}: {inner:?} is not strictly inside {outer:?}"
            );
        }
    }
    client.shutdown();
}

/// A separator is inside no field: the line is all there is to expand to.
#[test]
fn a_cursor_on_a_separator_gets_the_line_only() {
    let mut client = open(HEARTBEAT);
    let found = at(&mut client, &[(0, 9)]);
    assert_eq!(levels(&found[0]), vec![(0, 79)]);
    client.shutdown();
}

/// A comment has no fields, and a user can still expand to it.
#[test]
fn a_line_that_is_not_a_message_still_answers() {
    let mut client = open("# a comment");
    let found = at(&mut client, &[(0, 3)]);
    assert_eq!(levels(&found[0]), vec![(0, 11)]);
    client.shutdown();
}

/// **A cursor addresses the character to its right**, as it does for hover, and
/// that decides the empty-value case rather than a rule of its own: `35=|` has
/// nothing between the `=` and the separator, so a cursor there is on the
/// separator and gets the line. On the `=` itself it is still in the field.
///
/// Worth pinning because the alternative reading — a cursor is *between*
/// characters, so column 13 is inside the field that ends at 13 — is equally
/// defensible and would make hover and this disagree about the same click.
#[test]
fn an_empty_value_is_addressed_by_its_equals_not_past_it() {
    let mut client = open("8=FIX.4.4|35=|10=171|");
    assert_eq!(
        levels(&at(&mut client, &[(0, 12)])[0]),
        vec![(10, 13), (0, 21)],
        "on the `=`: the field, with no value level to enter"
    );
    assert_eq!(
        levels(&at(&mut client, &[(0, 13)])[0]),
        vec![(0, 21)],
        "one past it is the separator, which is inside no field"
    );
    client.shutdown();
}

/// Field and line coincide when the field is the whole line, and a level that
/// expands nothing is a keypress with no movement.
#[test]
fn a_field_that_is_the_whole_line_collapses_into_it() {
    let mut client = open("8=FIX.4.4");
    let found = at(&mut client, &[(0, 3)]);
    assert_eq!(
        levels(&found[0]),
        vec![(2, 9), (0, 9)],
        "value then line: the field *is* the line, so it is not repeated"
    );
    client.shutdown();
}

/// The response is positional: one range per position, in the order asked,
/// however the positions arrive.
#[test]
fn duplicates_and_disorder_keep_their_slots() {
    let mut client = open(&format!("{HEARTBEAT}\n{HEARTBEAT}"));
    let asked = [(1, 18), (0, 18), (1, 18), (0, 9)];
    let found = at(&mut client, &asked);
    assert_eq!(found.len(), asked.len());

    assert_eq!(found[0], found[2], "the same position, the same answer");
    for (nth, (line, _)) in asked.iter().enumerate() {
        assert_eq!(found[nth]["range"]["start"]["line"], *line, "slot {nth}");
    }
    assert_eq!(levels(&found[3]), vec![(0, 79)], "a separator on line 0");
    client.shutdown();
}

/// Past the end of a line clamps to it; past the end of the document is an
/// empty range at the position, which keeps the array aligned without claiming
/// there is text there.
#[test]
fn positions_past_the_text_still_get_a_slot() {
    let mut client = open(HEARTBEAT);
    let found = at(&mut client, &[(0, 9999), (7, 3)]);
    assert_eq!(found.len(), 2);
    assert_eq!(levels(&found[0]), vec![(0, 79)], "clamped to its line");
    assert_eq!(
        found[1],
        json!({ "range": {
            "start": { "line": 7, "character": 3 },
            "end": { "line": 7, "character": 3 },
        }}),
        "past the document: empty, at itself, no parent"
    );
    client.shutdown();
}

/// Past the bound every position still gets an answer — the response cannot be
/// truncated — but the cheapest correct one.
#[test]
fn past_the_line_bound_every_position_gets_its_line() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "limits": { "rangeLines": 2 } } },
    }));
    client.did_open(URI, &[HEARTBEAT; 4].join("\n"), 1);

    let found = at(&mut client, &[(0, 18), (1, 18), (2, 18)]);
    assert_eq!(found.len(), 3);
    for (nth, node) in found.iter().enumerate() {
        assert_eq!(levels(node), vec![(0, 79)], "slot {nth}: the line only");
    }

    let logged = client.wait_for_log("rangeLines");
    let text = logged["message"].as_str().expect("text");
    assert!(text.contains("whole lines only"), "{text}");
    client.shutdown();
}

#[test]
fn a_closed_document_answers_with_null() {
    let mut client = open(HEARTBEAT);
    client.did_close(URI);
    let answer = client
        .request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": URI },
                "positions": [{ "line": 0, "character": 0 }],
            }),
        )
        .expect("answered");
    assert_eq!(answer, Value::Null, "nothing to be inside");
    client.shutdown();
}

#[test]
fn malformed_params_are_an_error() {
    let mut client = open(HEARTBEAT);
    let error = client
        .request("textDocument/selectionRange", json!({}))
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// A column is not a byte offset, and only a non-ASCII line can tell.
#[test]
fn the_levels_are_columns_not_byte_offsets() {
    const TEXT: &str = "8=FIX.4.4|58=ПРИВЕТ|49=CLIENT1|";

    let mut client = TestClient::start();
    client.initialize_with(json!({ "capabilities": {} }));
    client.did_open(URI, TEXT, 1);

    // The cursor in `CLIENT1`, addressed as the client would address it.
    let prefix = &TEXT[..TEXT.find("CLIENT1").expect("the value")];
    let column = prefix.encode_utf16().count() as u32;
    assert_ne!(
        column as usize,
        prefix.len(),
        "or the fixture proves nothing"
    );

    let found = at(&mut client, &[(0, column)]);
    let chain = levels(&found[0]);
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].0, column as u64, "the value starts at the cursor");
    assert_eq!(
        chain[2],
        (0, TEXT.encode_utf16().count() as u64),
        "and the line ends in columns too"
    );
    client.shutdown();
}

/// Two parties, the second carrying two sub-IDs — a group, sibling entries and
/// one nested group, which is every ancestor pair a FIX line can offer.
const PARTIES: &str = concat!(
    "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1",
    "|448=B|447=D|452=2|802=2|523=S1|803=1|523=S2|803=2|54=1"
);

/// Byte offset of a substring, and of the byte `after` bytes into it.
fn at_of(text: &str, needle: &str, after: u32) -> u32 {
    text.find(needle).expect("in the line") as u32 + after
}

/// A field inside a repeating group expands through its own entry, then the
/// whole group, before it reaches the line.
#[test]
fn a_field_in_a_group_expands_through_its_entry_then_the_group() {
    let mut client = open(PARTIES);
    let cursor = at_of(PARTIES, "448=A", 4);
    let found = at(&mut client, &[(0, cursor)]);

    let entry = (at_of(PARTIES, "448=A", 0), at_of(PARTIES, "452=1", 5));
    let group = (at_of(PARTIES, "453=2", 0), at_of(PARTIES, "803=2", 5));
    assert_eq!(
        levels(&found[0]),
        vec![
            (cursor as u64, cursor as u64 + 1),
            (entry.0 as u64, entry.0 as u64 + 5),
            (entry.0 as u64, entry.1 as u64),
            (group.0 as u64, group.1 as u64),
            (0, PARTIES.len() as u64),
        ],
        "value, field, entry, group, line"
    );
    client.shutdown();
}

/// A cursor in the innermost group climbs *every* ancestor pair — its own
/// entry, its own group, then the party entry and the party group.
#[test]
fn a_nested_group_climbs_every_ancestor_pair() {
    let mut client = open(PARTIES);
    let cursor = at_of(PARTIES, "523=S1", 4);
    let found = at(&mut client, &[(0, cursor)]);
    let widths: Vec<u64> = levels(&found[0])
        .iter()
        .map(|(start, end)| end - start)
        .collect();
    assert_eq!(
        widths.len(),
        7,
        "value, field, sub entry, sub group, party entry, party group, line"
    );
    assert!(
        widths.windows(2).all(|pair| pair[0] < pair[1]),
        "each strictly wider than the last: {widths:?}"
    );
    client.shutdown();
}

/// **A count field is not inside the group it opens.** Without a level of its
/// own, a cursor on `453=2` would expand straight to the line, past the very
/// group that tag announces.
#[test]
fn a_count_tag_expands_through_the_group_it_opens() {
    let mut client = open(PARTIES);
    let cursor = at_of(PARTIES, "453=2", 4);
    let found = at(&mut client, &[(0, cursor)]);

    let count = at_of(PARTIES, "453=2", 0) as u64;
    let group_end = at_of(PARTIES, "803=2", 5) as u64;
    assert_eq!(
        levels(&found[0]),
        vec![
            (cursor as u64, cursor as u64 + 1),
            (count, count + 5),
            (count, group_end),
            (0, PARTIES.len() as u64),
        ],
        "value, the count field, the group it opens, line"
    );
    client.shutdown();
}

/// The same shape under utf-16, where a column is not a byte offset: the group
/// levels are converted like every other, in one walk of the line.
#[test]
fn group_levels_are_columns_under_utf16() {
    let text = PARTIES.replace("448=A", "448=привет");
    let mut client = TestClient::start();
    client.initialize(&["utf-16"]);
    client.did_open(URI, &text, 1);

    let column = |byte: u32| text[..byte as usize].encode_utf16().count() as u64;
    let cursor = column(at_of(&text, "448=привет", 4));
    let found = at(&mut client, &[(0, cursor as u32)]);

    let entry_start = column(at_of(&text, "448=привет", 0));
    let group_end = column(at_of(&text, "803=2", 5));
    let levels = levels(&found[0]);
    assert_eq!(
        levels[1],
        (entry_start, cursor + 6),
        "the field, in columns"
    );
    assert_eq!(
        levels[3],
        (column(at_of(&text, "453=2", 0)), group_end),
        "the group, in columns rather than bytes"
    );
    assert!(
        group_end < at_of(&text, "803=2", 5) as u64,
        "and columns really are behind bytes here"
    );
    client.shutdown();
}

/// NoHops(627) is a header group: a hop's value expands through its entry and
/// the group under FIX.4.4 and under `8=FIXT.1.1`.
#[test]
fn a_header_group_field_expands_through_its_entry_then_the_group() {
    for line in [
        "8=FIX.4.4|35=0|627=2|628=A|630=1|628=B|630=2|112=T",
        "8=FIXT.1.1|35=0|627=2|628=A|630=1|628=B|630=2|112=T",
    ] {
        let mut client = open(line);
        let cursor = at_of(line, "628=B", 4);
        let found = at(&mut client, &[(0, cursor)]);
        let entry = (at_of(line, "628=B", 0), at_of(line, "630=2", 5));
        let group = (at_of(line, "627=2", 0), at_of(line, "630=2", 5));
        assert_eq!(
            levels(&found[0]),
            vec![
                (cursor as u64, cursor as u64 + 1),
                (entry.0 as u64, entry.0 as u64 + 5),
                (entry.0 as u64, entry.1 as u64),
                (group.0 as u64, group.1 as u64),
                (0, line.len() as u64),
            ],
            "{line}: value, field, entry, group, line"
        );
        client.shutdown();
    }
}
