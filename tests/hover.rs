//! `textDocument/hover` through the protocol.
//!
//! What the markdown says is `render::hover`'s question. Here the question is
//! which field the cursor is on, what range comes back, and how the answer
//! adapts to what the client said it can read.

mod harness;

use harness::{TestClient, Workspace};
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

fn open(text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.did_open(URI, text, 1);
    client
}

fn at(client: &mut TestClient, character: u32) -> Value {
    client.hover(URI, 0, character).expect("answered")
}

/// The tag and its value are one field, so the cursor may sit anywhere in
/// `49=CLIENT1` and get the same answer about the same range.
#[test]
fn a_hover_anywhere_in_a_field_answers_about_that_field() {
    let mut client = open(HEARTBEAT);
    let start = HEARTBEAT.find("49=CLIENT1").expect("the field is there") as u32;
    let end = start + "49=CLIENT1".len() as u32;

    let on_tag = at(&mut client, start);
    let on_value = at(&mut client, start + 3);
    let last_byte = at(&mut client, end - 1);
    assert_eq!(on_tag, on_value);
    assert_eq!(on_tag, last_byte);

    assert!(on_tag["contents"]["value"]
        .as_str()
        .expect("markdown")
        .starts_with("**SenderCompID** (49)"));
    assert_eq!(
        on_tag["range"]["start"],
        json!({ "line": 0, "character": start })
    );
    assert_eq!(
        on_tag["range"]["end"],
        json!({ "line": 0, "character": end })
    );
    client.shutdown();
}

/// A separator belongs to no field. Answering about the neighbour would put a
/// popup on screen about something the cursor is not on.
/// The label the dictionary join restores has to survive the whole way out:
/// FIX44.xml gives PartySubIDType(803) no values, so before the join this read
/// `Value: \`1\`` with nothing after it.
#[test]
fn a_restored_enum_label_reaches_the_client() {
    const PARTIES: &str = "8=FIX.4.4|9=57|35=D|453=1|448=A|447=D|452=1|802=1|523=S1|803=1|10=171|";
    let mut client = open(PARTIES);
    let at_803 = PARTIES.find("803=1").expect("the field is there") as u32;
    let answer = at(&mut client, at_803);
    let markdown = answer["contents"]["value"].as_str().expect("markdown");
    assert!(
        markdown.starts_with("**PartySubIDType** (803)"),
        "{markdown}"
    );
    assert!(markdown.contains("Value: `1` — **Firm**"), "{markdown}");
}

#[test]
fn a_hover_between_fields_answers_nothing() {
    let mut client = open(HEARTBEAT);
    let separator = HEARTBEAT.find("|9=").expect("a separator") as u32;
    assert_eq!(at(&mut client, separator), Value::Null);
    assert_eq!(
        at(&mut client, HEARTBEAT.len() as u32),
        Value::Null,
        "and past the end of the line"
    );
    client.shutdown();
}

#[test]
fn a_line_that_is_not_a_message_answers_nothing() {
    let mut client = open("# just a comment");
    assert_eq!(at(&mut client, 3), Value::Null);
    client.shutdown();
}

#[test]
fn a_line_past_the_document_answers_nothing() {
    let mut client = open(HEARTBEAT);
    assert_eq!(client.hover(URI, 99, 0).expect("answered"), Value::Null);
    client.shutdown();
}

/// Columns leave the crate in the negotiated encoding. A log prefix with a
/// character outside ASCII moves every column after it, and the byte offset the
/// cursor arrives as has to be read the same way.
#[test]
fn positions_are_converted_under_the_negotiated_encoding() {
    let prefixed = format!("— {HEARTBEAT}");
    let mut client = TestClient::start();
    client.initialize(&["utf-16"]);
    client.did_open(URI, &prefixed, 1);

    // The em dash is three utf-8 bytes and one utf-16 unit, so with the space
    // the prefix is four bytes and two units: every column after it is two
    // behind its byte offset.
    let byte = prefixed.find("49=CLIENT1").expect("the field") as u32;
    let character = byte - 2;
    let answer = client.hover(URI, 0, character).expect("answered");
    assert!(answer["contents"]["value"]
        .as_str()
        .expect("markdown")
        .starts_with("**SenderCompID** (49)"));
    assert_eq!(
        answer["range"]["start"]["character"], character,
        "the range comes back in the same units the request arrived in"
    );
    assert_ne!(
        answer["range"]["start"]["character"], byte,
        "and they are not the byte offsets"
    );
    client.shutdown();
}

/// `contentFormat` is an ordered preference; the answer carries the kind the
/// client asked for first, with the same text either way.
#[test]
fn the_answer_carries_the_kind_the_client_asked_for() {
    for (formats, expected) in [
        (json!(["markdown"]), "markdown"),
        (json!(["plaintext"]), "plaintext"),
        (json!(["plaintext", "markdown"]), "plaintext"),
    ] {
        let mut client = TestClient::start();
        client.initialize_caps(json!({
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "hover": { "contentFormat": formats } },
        }));
        client.did_open(URI, HEARTBEAT, 1);
        let answer = at(&mut client, HEARTBEAT.find("49=").unwrap() as u32);
        assert_eq!(answer["contents"]["kind"], expected, "{formats}");
        assert!(answer["contents"]["value"]
            .as_str()
            .expect("text")
            .starts_with("**SenderCompID** (49)"));
        client.shutdown();
    }
}

/// `*.enabled` gates the response, not the capability: the provider is still
/// advertised, so turning hover back on needs no reconnect.
#[test]
fn hover_can_be_turned_off_without_withdrawing_the_provider() {
    let mut client = TestClient::start();
    let advertised = client.initialize_with(json!({
        "capabilities": { "general": { "positionEncodings": ["utf-8"] } },
        "initializationOptions": { "fix": { "hover": { "enabled": false } } },
    }));
    assert_eq!(
        advertised["capabilities"]["hoverProvider"], true,
        "still advertised"
    );
    client.did_open(URI, HEARTBEAT, 1);
    assert_eq!(
        at(&mut client, HEARTBEAT.find("49=").unwrap() as u32),
        Value::Null
    );
    client.shutdown();
}

#[test]
fn a_closed_document_answers_nothing() {
    let mut client = open(HEARTBEAT);
    client.did_close(URI);
    assert_eq!(at(&mut client, 0), Value::Null);
    client.shutdown();
}

/// Params that do not parse are the client's error. `null` would be a claim
/// about the document — that there is nothing under the cursor — where the
/// problem is that we were not told where the cursor is.
#[test]
fn malformed_params_are_an_error() {
    let mut client = open(HEARTBEAT);
    let error = client
        .request(
            "textDocument/hover",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// A desk's own FIX.4.4 dictionary: one standard tag, one FIX.5.0 tag, one of
/// its own. Written by the test, so the shared fixtures stay as they are.
const DESK_44: &str = "<fix type='FIX' major='4' minor='4' servicepack='0'><fields>\
<field number='55' name='Symbol' type='STRING'/>\
<field number='1328' name='RejectText' type='STRING'/>\
<field number='20001' name='DeskMode' type='INT'><value enum='1' description='ALPHA'/></field>\
</fields></fix>";

const DESK_LINE: &str = "8=FIX.4.4|35=0|55=X|1328=x|20001=1|";

/// The desk dictionary bound as the default for FIX.4.4, plus `extra` settings.
/// The workspace is returned so the file outlives the session.
fn open_desk(extra: Value) -> (TestClient, Workspace) {
    let workspace = Workspace::new("hover-desk");
    let mut fix = workspace.desk_dictionary(DESK_44);
    for (key, value) in extra.as_object().expect("settings object") {
        fix[key] = value.clone();
    }
    let mut client = workspace.client(fix);
    client.did_open(URI, DESK_LINE, 1);
    (client, workspace)
}

fn markdown_at(client: &mut TestClient, field: &str) -> String {
    let start = DESK_LINE.find(field).expect("the field is there") as u32;
    at(client, start)["contents"]["value"]
        .as_str()
        .expect("markdown")
        .to_owned()
}

/// Under a custom dictionary hover says which tags standard FIX does not
/// define for the line's version, and links only the ones it does.
#[test]
fn a_custom_dictionary_is_measured_against_standard_fix() {
    let (mut client, _workspace) = open_desk(json!({}));

    let custom = markdown_at(&mut client, "20001=");
    assert!(
        custom.contains("*Custom field, introduced in \"desk\"*"),
        "{custom}"
    );
    assert!(custom.contains("Value: `1` — **ALPHA**"), "{custom}");
    assert!(!custom.contains("[FIX Reference"), "{custom}");

    let later = markdown_at(&mut client, "1328=");
    assert!(
        later.contains("*Not in FIX.4.4, introduced in FIX.5.0SP1*"),
        "{later}"
    );
    assert!(!later.contains("[FIX Reference"), "{later}");

    let standard = markdown_at(&mut client, "55=");
    assert!(standard.contains("*Standard FIX.4.4*"), "{standard}");
    assert!(
        standard.ends_with("(https://www.onixs.biz/fix-dictionary/4.4/tagNum_55.html)"),
        "{standard}"
    );
    client.shutdown();
}

/// A template without the site's version segment is the desk's own browser,
/// which knows the desk's own tags.
#[test]
fn a_flat_reference_url_still_links_a_custom_tag() {
    let (mut client, _workspace) =
        open_desk(json!({ "documentLink": { "url": "https://fix.internal/t{tag}" } }));
    let custom = markdown_at(&mut client, "20001=");
    assert!(
        custom.contains("*Custom field, introduced in \"desk\"*"),
        "{custom}"
    );
    assert!(
        custom.ends_with("[FIX Reference: fix.internal](https://fix.internal/t20001)"),
        "{custom}"
    );
    client.shutdown();
}
