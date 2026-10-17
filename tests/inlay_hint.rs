//! `textDocument/inlayHint` through the protocol.
//!
//! What the labels say is `src/render/hint.rs`'s question. Here: which lines the
//! range covers, where the hints land, and how the answer adapts to a client
//! that does or does not resolve tooltips.

mod harness;

use harness::{hint_labels, TestClient, Workspace};
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

const HEARTBEAT: &str =
    "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

/// Keep protocol tests independent of the configured defaults.
fn hints_on() -> Value {
    json!({
        "inlayHints": {
            "tagNames": { "enabled": true },
            "enumValues": { "enabled": true },
        }
    })
}

fn open_with(settings: Value, caps: Value, text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": caps,
        "initializationOptions": { "fix": settings },
    }));
    client.did_open(URI, text, 1);
    client
}

fn plain_caps() -> Value {
    json!({ "general": { "positionEncodings": ["utf-8"] } })
}

fn over(client: &mut TestClient, from: (u32, u32), to: (u32, u32)) -> Vec<Value> {
    client
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": URI },
                "range": {
                    "start": { "line": from.0, "character": from.1 },
                    "end": { "line": to.0, "character": to.1 },
                },
            }),
        )
        .expect("answered")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_tag_and_its_value_are_labelled_where_they_end_by_default() {
    let mut client = open_with(json!({}), plain_caps(), HEARTBEAT);
    let hints = over(&mut client, (0, 0), (0, 400));

    let tag_end = HEARTBEAT.find("49=").unwrap() as u32 + 2;
    let named = hints
        .iter()
        .find(|hint| hint["position"]["character"] == tag_end)
        .expect("a hint after SenderCompID(49)");
    assert_eq!(named["label"], "(SenderCompID)");
    assert_eq!(named["kind"], 1, "Parameter");

    let value_end = HEARTBEAT.find("35=0").unwrap() as u32 + 4;
    let typed = hints
        .iter()
        .find(|hint| hint["position"]["character"] == value_end && hint["kind"] == 2)
        .expect("a hint after the value of 35");
    assert_eq!(typed["label"], "(Heartbeat)");
    client.shutdown();
}

/// The two formats are two settings. One shared string would pass every test
/// that changed only one of them, so this changes both and asserts both.
#[test]
fn each_label_is_written_by_its_own_format() {
    let mut client = open_with(
        json!({
            "inlayHints": {
                "tagNames": { "enabled": true, "format": "<{}>" },
                "enumValues": { "enabled": true, "format": " is {}" },
            }
        }),
        plain_caps(),
        HEARTBEAT,
    );
    let hints = over(&mut client, (0, 0), (0, 400));

    let tag_end = HEARTBEAT.find("49=").unwrap() as u32 + 2;
    let named = hints
        .iter()
        .find(|hint| hint["position"]["character"] == tag_end)
        .expect("a hint after SenderCompID(49)");
    assert_eq!(named["label"], "<SenderCompID>");

    let value_end = HEARTBEAT.find("35=0").unwrap() as u32 + 4;
    let typed = hints
        .iter()
        .find(|hint| hint["position"]["character"] == value_end && hint["kind"] == 2)
        .expect("a hint after the value of 35");
    assert_eq!(typed["label"], " is Heartbeat");
    client.shutdown();
}

/// An empty value suppresses the label and its literals, so a tag neither the
/// dictionary nor standard FIX knows is passed over while its neighbours are
/// drawn. The literals are the point: `<>` would be worse than nothing.
#[test]
fn a_tag_the_dictionary_does_not_define_draws_nothing() {
    let mut client = open_with(
        json!({ "inlayHints": {
            "tagNames": { "enabled": true, "format": "<{}>" },
            "enumValues": { "enabled": false },
        } }),
        plain_caps(),
        "8=FIX.4.4|35=0|9999=x|10=1|",
    );
    let hints = over(&mut client, (0, 0), (0, 400));
    assert_eq!(
        hint_labels(&hints),
        ["<BeginString>", "<MsgType>", "<CheckSum>"],
        "9999 is on the line and has no label"
    );
    client.shutdown();
}

/// A tag the dictionary lacks but standard FIX names is drawn with that name
/// and a `?`, inside the user's format, where a defined tag's label would go;
/// its eager tooltip is the hover of the same field, which explains the mark.
#[test]
fn a_tag_standard_fix_names_and_the_dictionary_lacks_is_marked() {
    const LINE: &str = "8=FIXT.1.1|35=D|1128=7|1328=x|10=1|";
    let mut client = open_with(
        json!({ "inlayHints": {
            "tagNames": { "enabled": true, "format": "<{}>" },
            "enumValues": { "enabled": false },
        } }),
        plain_caps(),
        LINE,
    );
    let hints = over(&mut client, (0, 0), (0, 400));
    assert_eq!(
        hint_labels(&hints),
        [
            "<BeginString>",
            "<MsgType>",
            "<ApplVerID>",
            "<RejectText?>",
            "<CheckSum>"
        ]
    );
    let tag_end = LINE.find("1328=").unwrap() as u32 + 4;
    let marked = hints
        .iter()
        .find(|hint| hint["label"] == "<RejectText?>")
        .expect("the marked hint");
    assert_eq!(marked["position"]["character"], tag_end);
    let tooltip = marked["tooltip"]["value"].as_str().expect("eager markdown");
    let hover = client.hover(URI, 0, tag_end - 1).expect("answered");
    assert_eq!(
        tooltip,
        hover["contents"]["value"].as_str().expect("markdown")
    );
    assert!(tooltip.starts_with("**RejectText** (1328)"), "{tooltip}");
    client.shutdown();
}

/// The same hint through `inlayHint/resolve`, when the client defers tooltips.
#[test]
fn a_marked_hints_tooltip_resolves_to_the_hover() {
    const LINE: &str = "8=FIXT.1.1|35=D|1128=7|1328=x|10=1|";
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "inlayHint": { "resolveSupport": { "properties": ["tooltip"] } } },
        },
        "initializationOptions": { "fix": { "inlayHints": { "tagNames": { "enabled": true } } } },
    }));
    client.did_open(URI, LINE, 1);
    let hints = over(&mut client, (0, 0), (0, 400));
    let marked = hints
        .iter()
        .find(|hint| hint["label"] == "(RejectText?)")
        .expect("the marked hint")
        .clone();
    assert!(marked.get("tooltip").is_none(), "deferred");
    let resolved = client
        .request("inlayHint/resolve", marked)
        .expect("answered");
    assert!(
        resolved["tooltip"]["value"]
            .as_str()
            .expect("markdown")
            .starts_with("**RejectText** (1328)"),
        "{resolved}"
    );
    client.shutdown();
}

/// With paths on, an undefined tag after a group's last member ends the entry
/// (a bundled dictionary lists no member it does not define), so it has no
/// path and the mark is what appears — not a digit leaf.
#[test]
fn a_marked_tag_inside_a_group_draws_the_mark_not_a_path() {
    const LINE: &str = "8=FIX.4.4|35=W|55=BTCUSD|268=1|269=0|270=65000.50|1328=x|10=1|";
    let mut client = open_with(
        json!({ "inlayHints": {
            "tagNames": { "enabled": true, "groupPaths": true, "format": "<{}>" },
            "enumValues": { "enabled": false },
        } }),
        plain_caps(),
        LINE,
    );
    let hints = over(&mut client, (0, 0), (0, 400));
    assert_eq!(
        hint_labels(&hints),
        [
            "<BeginString>",
            "<MsgType>",
            "<Symbol>",
            "<NoMDEntries>",
            "<NoMDEntries/1/MDEntryType>",
            "<NoMDEntries/1/MDEntryPx>",
            "<RejectText?>",
            "<CheckSum>",
        ]
    );
    client.shutdown();
}

#[test]
fn each_hint_kind_can_be_disabled_explicitly() {
    for (settings, tags_expected, values_expected) in [
        (json!({ "tagNames": { "enabled": false } }), false, true),
        (json!({ "enumValues": { "enabled": false } }), true, false),
        (
            json!({
                "tagNames": { "enabled": false },
                "enumValues": { "enabled": false },
            }),
            false,
            false,
        ),
    ] {
        let mut client = open_with(json!({ "inlayHints": settings }), plain_caps(), HEARTBEAT);
        let hints = over(&mut client, (0, 0), (0, 400));
        assert_eq!(
            hints.iter().any(|hint| hint["data"]["kind"] == "tag"),
            tags_expected
        );
        assert_eq!(
            hints.iter().any(|hint| hint["data"]["kind"] == "value"),
            values_expected
        );
        if !tags_expected && !values_expected {
            assert!(hints.is_empty(), "{hints:?}");
        }
        client.shutdown();
    }
}

/// The range's end is exclusive, and clipping is per character because a hint
/// is a point. A line-level reading would emit hints the client did not ask
/// about on the first and last lines.
#[test]
fn the_range_is_clipped_by_character_and_its_end_is_exclusive() {
    let mut client = open_with(hints_on(), plain_caps(), HEARTBEAT);
    let all = over(&mut client, (0, 0), (0, 400));
    assert!(all.len() > 2, "{all:#?}");

    let first = all[0]["position"]["character"].as_u64().unwrap() as u32;
    let after_first = over(&mut client, (0, first + 1), (0, 400));
    assert_eq!(
        after_first.len(),
        all.len() - 1,
        "a start past the first hint drops exactly it"
    );

    let ends_at_first = over(&mut client, (0, 0), (0, first));
    assert!(
        ends_at_first.is_empty(),
        "the end is exclusive, so a hint exactly at it is outside: {ends_at_first:#?}"
    );
    client.shutdown();
}

/// A range ending at column zero does not reach that line at all.
#[test]
fn a_range_ending_at_column_zero_excludes_that_line() {
    let text = format!("{HEARTBEAT}\n{HEARTBEAT}");
    let mut client = open_with(hints_on(), plain_caps(), &text);
    let hints = over(&mut client, (0, 0), (1, 0));
    assert!(
        hints.iter().all(|hint| hint["position"]["line"] == 0),
        "{hints:#?}"
    );
    client.shutdown();
}

/// The summary rides the same channel when asked to, at column zero.
#[test]
fn the_summary_is_its_own_hint_when_the_channel_is_on() {
    let settings = json!({ "messageSummary": { "channels": { "inlayHints": true } } });
    let mut client = open_with(settings, plain_caps(), HEARTBEAT);
    let hints = over(&mut client, (0, 0), (0, 400));
    let summary = hints
        .iter()
        .find(|hint| hint["data"]["kind"] == "summary")
        .expect("a summary hint");
    assert_eq!(summary["position"]["character"], 0);
    assert_eq!(summary["paddingRight"], true);
    assert!(!hint_labels(std::slice::from_ref(summary))[0].is_empty());
    client.shutdown();
}

/// `resolveSupport` is a property list. A client that resolves hints but not
/// their tooltips must be given them up front, or its popups are empty.
#[test]
fn tooltips_are_eager_unless_the_client_names_them() {
    let mut eager = open_with(hints_on(), plain_caps(), HEARTBEAT);
    let hints = over(&mut eager, (0, 0), (0, 400));
    assert_every_field_hint_has_a_tooltip(&hints, "no client support at all means eager");
    eager.shutdown();

    let mut partial = TestClient::start();
    partial.initialize_with(json!({
        "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "inlayHint": { "resolveSupport": { "properties": ["label"] } } },
        },
        "initializationOptions": { "fix": hints_on() },
    }));
    partial.did_open(URI, HEARTBEAT, 1);
    let hints = over(&mut partial, (0, 0), (0, 400));
    assert_every_field_hint_has_a_tooltip(&hints, "resolves labels but not tooltips: still eager");
    partial.shutdown();
}

/// Every one of them, not merely one: a client that cannot resolve gets an empty
/// popup for each hint that was left bare, so `any` would pass a regression that
/// filled the first and dropped the other thousands. The summary hint is the
/// exception by design — it describes a line, not a field, and has no hover.
fn assert_every_field_hint_has_a_tooltip(hints: &[Value], what: &str) {
    let fields: Vec<&Value> = hints
        .iter()
        .filter(|hint| hint["data"]["kind"] != "summary")
        .collect();
    assert!(fields.len() > 1, "{what}: nothing to check");
    assert!(
        fields.iter().all(|hint| hint.get("tooltip").is_some()),
        "{what}: {} of {} carry no tooltip",
        fields.iter().filter(|h| h.get("tooltip").is_none()).count(),
        fields.len()
    );
}

#[test]
fn a_client_that_resolves_tooltips_is_given_them_lazily() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "inlayHint": { "resolveSupport": { "properties": ["tooltip"] } } },
        },
        "initializationOptions": { "fix": hints_on() },
    }));
    client.did_open(URI, HEARTBEAT, 1);
    let hints = over(&mut client, (0, 0), (0, 400));
    assert!(
        hints.iter().all(|hint| hint.get("tooltip").is_none()),
        "deferred"
    );

    let resolved = client
        .request("inlayHint/resolve", hints[0].clone())
        .expect("answered");
    assert!(
        resolved["tooltip"]["value"]
            .as_str()
            .expect("markdown")
            .starts_with("**"),
        "{resolved}"
    );
    client.shutdown();
}

/// A hint held across an edit is about text that has moved. Versions increase
/// strictly, so the version is what makes the offset safe to trust at all.
#[test]
fn a_resolve_against_a_moved_document_adds_nothing() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "inlayHint": { "resolveSupport": { "properties": ["tooltip"] } } },
        },
        "initializationOptions": { "fix": hints_on() },
    }));
    client.did_open(URI, HEARTBEAT, 1);
    let held = over(&mut client, (0, 0), (0, 400))[0].clone();

    client.did_change_full(URI, HEARTBEAT, 2);
    let resolved = client.request("inlayHint/resolve", held).expect("answered");
    assert!(resolved.get("tooltip").is_none(), "{resolved}");
    client.shutdown();
}

/// Past the cap, `[]` and a reason — a client that asked for the whole buffer
/// otherwise gets nothing back and nothing to act on.
#[test]
fn an_oversized_range_is_answered_with_nothing_and_says_why() {
    let settings = json!({
        "inlayHints": { "tagNames": { "enabled": true } },
        "limits": { "rangeLines": 2 },
    });
    let mut client = open_with(settings, plain_caps(), &[HEARTBEAT; 5].join("\n"));
    assert_eq!(over(&mut client, (0, 0), (4, 400)), Vec::<Value>::new());

    let logged = client.wait_for_log("inlay hints over");
    assert!(
        logged["message"]
            .as_str()
            .expect("text")
            .contains("smaller range"),
        "{logged}"
    );
    client.shutdown();
}

#[test]
fn a_closed_document_answers_with_nothing() {
    let mut client = open_with(hints_on(), plain_caps(), HEARTBEAT);
    client.did_close(URI);
    assert_eq!(over(&mut client, (0, 0), (0, 400)), Vec::<Value>::new());
    client.shutdown();
}

#[test]
fn malformed_params_are_an_error() {
    let mut client = open_with(hints_on(), plain_caps(), HEARTBEAT);
    let error = client
        .request(
            "textDocument/inlayHint",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// A configuration change that alters what the annotation channels would say
/// asks the client to re-request them — otherwise a template edit, a toggle or
/// a new dictionary sits stale until the user happens to scroll.
///
/// One case per field of the comparison, so the list is exhaustive by
/// construction rather than by whichever setting came to mind: what a channel
/// draws, what its payload carries, and how the line is interpreted at all.
#[test]
fn a_change_that_alters_the_channels_asks_for_a_refresh() {
    let cases = [
        (
            json!({ "inlayHints": { "tagNames": { "enabled": false } } }),
            "what the channel draws",
        ),
        (
            // The switch is untouched: a format is the other half of what the
            // channel draws, and a comparison that only watched switches would
            // leave every visible label stale.
            json!({ "inlayHints": { "tagNames": { "format": "[{}]" } } }),
            "how the channel writes what it draws",
        ),
        (
            json!({ "documentLink": { "url": "https://fix.internal/{version}/t{tag}" } }),
            "the reference link an eager tooltip embeds",
        ),
        (
            // An application default changes FIXT interpretation.
            json!({ "defaultApplVerID": "FIX.5.0SP2" }),
            "how the line is interpreted",
        ),
    ];

    for (settings, what) in cases {
        let changes_context = settings.get("defaultApplVerID").is_some();
        let mut client = TestClient::start();
        client.initialize_caps(refresh_caps());
        client.answer_base_pull();
        client.did_open(
            URI,
            if changes_context {
                "8=FIXT.1.1|35=D|54=1|"
            } else {
                HEARTBEAT
            },
            1,
        );
        client.drain_server_methods(std::time::Duration::from_millis(20));

        client.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": null }),
        );
        let (id, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration", "{what}");
        client.reply(id, json!([settings]));

        // Bounded rather than "receive until two": a regression that sends
        // neither would otherwise hang the suite instead of failing it. A
        // configuration change now pulls only global settings.
        let mut asked: Vec<String> = client
            .drain_server_methods(std::time::Duration::from_millis(500))
            .into_iter()
            .filter(|method| method.ends_with("/refresh"))
            .collect();
        asked.sort();
        assert_eq!(
            asked,
            if changes_context {
                vec!["workspace/codeLens/refresh", "workspace/inlayHint/refresh"]
            } else {
                vec!["workspace/inlayHint/refresh"]
            },
            "{what}: only affected channels, each asked once"
        );
        client.shutdown();
    }
}

/// An eager tooltip names the dictionary. Moving `default` between two names
/// for one file changes no dictionary and no interpretation, only that name.
#[test]
fn a_new_name_for_the_same_dictionary_refreshes_the_hints() {
    let workspace = Workspace::new("inlay-rename");
    let path = workspace.write(
        "desk.xml",
        "<fix type='FIX' major='4' minor='4' servicepack='0'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
         <field number='55' name='Symbol' type='STRING'/></fields></fix>",
    );
    let named = |default: &str| {
        json!({
            "allowDictionaryPath": true,
            "dictionaries": [
                { "path": path, "name": "alpha", "default": default == "alpha" },
                { "path": path, "name": "beta", "default": default == "beta" },
            ],
        })
    };
    let mut client = TestClient::start();
    client.initialize_caps(refresh_caps());
    let (id, _, _) = client.recv_server_request();
    client.reply(id, json!([named("alpha")]));
    client.did_open(URI, HEARTBEAT, 1);
    client.drain_server_methods(std::time::Duration::from_millis(20));

    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, _, _) = client.recv_server_request();
    client.reply(id, json!([named("beta")]));
    let asked: Vec<String> = client
        .drain_server_methods(std::time::Duration::from_millis(500))
        .into_iter()
        .filter(|method| method.ends_with("/refresh"))
        .collect();
    assert_eq!(asked, ["workspace/inlayHint/refresh"]);
    client.shutdown();
}

/// Two ways to be asked nothing, and they are different reasons: the client
/// cannot take a refresh, or nothing it draws would change.
///
/// Drained with `drain_server_methods`, not `drain_notifications`: a refresh is
/// a *request*, and the notification drain discards every request — so it would
/// report "no refresh" whether or not one was sent.
#[test]
fn nothing_is_asked_without_support_or_without_a_change() {
    let cases = [
        // No refresh support at all.
        (
            TestClient::caps_with_pull(),
            json!({ "inlayHints": { "tagNames": { "enabled": true } } }),
            "no support",
        ),
        // Support, and a real configuration change — but one no annotation
        // channel reads, so neither would say anything new.
        (
            refresh_caps(),
            json!({ "diagnostics": { "rules": { "integrity": false } } }),
            "nothing an annotation says",
        ),
    ];

    for (caps, settings, what) in cases {
        let mut client = TestClient::start();
        client.initialize_caps(caps);
        client.answer_base_pull();
        client.did_open(URI, HEARTBEAT, 1);
        client.drain_server_methods(std::time::Duration::from_millis(20));
        client.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": null }),
        );
        let (id, method, _) = client.recv_server_request();
        assert_eq!(method, "workspace/configuration", "{what}");
        client.reply(id, json!([settings]));

        let seen = client.drain_server_methods(std::time::Duration::from_millis(200));
        assert!(
            !seen.iter().any(|method| method.contains("refresh")),
            "{what}: {seen:?}"
        );
        client.shutdown();
    }
}

/// Pull, plus both refreshes advertised.
fn refresh_caps() -> Value {
    json!({
        "general": { "positionEncodings": ["utf-8"] },
        "workspace": {
            "configuration": true,
            "inlayHint": { "refreshSupport": true },
            "codeLens": { "refreshSupport": true },
        },
    })
}

/// Everything a client can hand back that is not the hint it was given. None of
/// it is an error: a hint without a tooltip is still a hint, and an error would
/// make a client discard what it is already drawing.
#[test]
fn a_resolve_it_cannot_trust_returns_the_hint_unadorned() {
    let mut client = resolving_client(HEARTBEAT);
    let hint = over(&mut client, (0, 0), (0, 400))[0].clone();
    assert!(hint["data"]["offset"].is_number(), "{hint}");

    let mut without_data = hint.clone();
    without_data
        .as_object_mut()
        .expect("an object")
        .remove("data");
    let mut malformed = hint.clone();
    malformed["data"] = json!("not an object");
    let mut wrong_line = hint.clone();
    wrong_line["data"]["line"] = json!(99);
    let mut wrong_offset = hint.clone();
    wrong_offset["data"]["offset"] = json!(9999);
    let mut no_document = hint.clone();
    no_document["data"]["uri"] = json!("");

    for (what, sent) in [
        ("no data", without_data),
        ("data that is not an object", malformed),
        ("a line the document does not have", wrong_line),
        ("an offset no field starts at", wrong_offset),
        ("a uri that does not parse", no_document),
    ] {
        let back = client
            .request("inlayHint/resolve", sent)
            .unwrap_or_else(|code| panic!("{what}: refused with {code}"));
        assert!(back.get("tooltip").is_none(), "{what}: {back}");
    }

    // And a closed document, which is the same answer for a different reason.
    client.did_close(URI);
    let back = client.request("inlayHint/resolve", hint).expect("answered");
    assert!(back.get("tooltip").is_none(), "{back}");
    client.shutdown();
}

/// A tag can appear many times on one line, so `data` carries the field's own
/// byte offset rather than its tag. Resolving by tag would give every entry of
/// a repeating group the first entry's answer.
#[test]
fn repeated_tags_resolve_to_their_own_field() {
    const GROUPS: &str = "8=FIX.4.4|9=124|35=W|34=3|49=EXCHANGE|56=CLIENT|\
52=20251026-09:05:00.000|55=BTCUSD|268=2|269=0|270=65000.50|271=1.5|269=1|\
270=65001.00|271=2.0|10=145|";

    let mut client = resolving_client(GROUPS);
    let repeated: Vec<Value> = over(&mut client, (0, 0), (0, 400))
        .into_iter()
        .filter(|hint| hint["data"]["tag"] == 269 && hint["data"]["kind"] == "tag")
        .collect();
    assert_eq!(repeated.len(), 2, "the group has two entries");
    assert_ne!(
        repeated[0]["data"]["offset"], repeated[1]["data"]["offset"],
        "and they are told apart by where they start"
    );

    let mut tooltip = |hint: Value| -> String {
        client.request("inlayHint/resolve", hint).expect("answered")["tooltip"]["value"]
            .as_str()
            .expect("markdown")
            .to_owned()
    };
    let first = tooltip(repeated[0].clone());
    let second = tooltip(repeated[1].clone());
    assert!(first.contains("NoMDEntries/1/"), "{first}");
    assert!(second.contains("NoMDEntries/2/"), "{second}");
    client.shutdown();
}

/// A client that resolves tooltips, which is what makes an unresolved hint
/// worth testing at all.
fn resolving_client(text: &str) -> TestClient {
    resolving_client_with(text, hints_on())
}

/// As `resolving_client`, with `fix` as the whole settings object.
fn resolving_client_with(text: &str, fix: Value) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "inlayHint": { "resolveSupport": { "properties": ["tooltip"] } } },
        },
        "initializationOptions": { "fix": fix },
    }));
    client.did_open(URI, text, 1);
    client
}

/// The tooltip is hover's markdown, so a custom tag says what hover says.
#[test]
fn a_custom_tags_tooltip_says_it_is_not_standard() {
    let workspace = Workspace::new("hint-desk");
    let mut fix = workspace.desk_dictionary(
        "<fix type='FIX' major='4' minor='4' servicepack='0'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
         <field number='20001' name='DeskMode' type='INT'/></fields></fix>",
    );
    fix["inlayHints"] = hints_on()["inlayHints"].take();
    let mut client = resolving_client_with("8=FIX.4.4|35=0|20001=1|", fix);

    let hint = over(&mut client, (0, 0), (0, 400))
        .into_iter()
        .find(|hint| hint["data"]["tag"] == 20001 && hint["data"]["kind"] == "tag")
        .expect("the custom tag is labelled");
    let tooltip = client.request("inlayHint/resolve", hint).expect("answered")["tooltip"]["value"]
        .as_str()
        .expect("markdown")
        .to_owned();
    assert!(
        tooltip.contains("*Custom field, introduced in \"desk\"*"),
        "{tooltip}"
    );
    assert!(!tooltip.contains("[FIX Reference"), "{tooltip}");
    client.shutdown();
}

/// Only an *edit* must advance the version, and a `didOpen` for an already open
/// URI replaces the document — so a hint held across that reincarnation can name
/// a byte where a different field now starts. The tag travels with the offset to
/// catch it; without that check the new field's tooltip arrives under the old
/// label.
#[test]
fn a_hint_held_across_a_reopen_at_the_same_version_is_not_resolved() {
    // Same prefix, so byte 15 starts MsgType(35) in one and
    // SenderCompID(49) in the other.
    const REORDERED: &str =
        "8=FIX.4.4|9=57|49=CLIENT1|35=0|34=1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
    assert_eq!(&HEARTBEAT[15..17], "35");
    assert_eq!(&REORDERED[15..17], "49");

    let mut client = resolving_client(HEARTBEAT);
    let held = over(&mut client, (0, 0), (0, 400))
        .into_iter()
        .find(|hint| hint["data"]["offset"] == 15)
        .expect("a hint on the field at byte 15");
    assert_eq!(held["data"]["tag"], 35);

    // Not a change: the same version, which is what makes this the case the
    // version check cannot see.
    client.did_open(URI, REORDERED, 1);
    let back = client.request("inlayHint/resolve", held).expect("answered");
    assert!(
        back.get("tooltip").is_none(),
        "a tooltip here would describe SenderCompID(49) under a MsgType(35) label: {back}"
    );
    client.shutdown();
}

/// A hint's `character` is a column in the negotiated encoding, not a byte
/// offset — the invariant `CLAUDE.md` states, and the one every other test here
/// is blind to because they are all ASCII, where the two are equal.
///
/// Under `utf-16` a non-ASCII value makes them differ by construction: the text
/// below is six Cyrillic characters, twelve bytes. A handler that passed the
/// byte offset through would put every later hint on this line six columns too
/// far right, and would do it silently.
#[test]
fn a_column_is_not_a_byte_offset_when_the_line_is_not_ascii() {
    const TEXT: &str = "8=FIX.4.4|9=57|35=0|58=ПРИВЕТ|49=CLIENT1|10=171|";

    // No `positionEncodings`, so the negotiation lands on utf-16.
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {},
        "initializationOptions": { "fix": hints_on() },
    }));
    client.did_open(URI, TEXT, 1);

    let hint = over(&mut client, (0, 0), (0, 400))
        .into_iter()
        .find(|hint| hint["data"]["tag"] == 49 && hint["data"]["kind"] == "tag")
        .expect("a tag hint for 49");

    // Where the tag's digits end, counted as the client counts.
    let prefix = &TEXT[..TEXT.find("49=").expect("the tag") + 2];
    let column = prefix.encode_utf16().count();
    assert_eq!(hint["position"]["character"], column, "{hint}");
    assert_ne!(
        column,
        prefix.len(),
        "the fixture must make bytes and columns differ, or it proves nothing"
    );
    client.shutdown();
}

#[test]
fn fixt_value_hints_and_summary_use_the_application_message() {
    for code in ["7", "8", "9"] {
        let line = format!("8=FIXT.1.1|35=D|1128={code}|54=1|38=2|55=SYNTHETIC|40=2|59=0|44=10|");
        let mut client = open_with(
            json!({
                "inlayHints": { "enumValues": { "enabled": true } },
                "messageSummary": { "channels": { "inlayHints": true }, "template": "{35:name} ({35})" },
            }),
            plain_caps(),
            &line,
        );
        let hints = over(&mut client, (0, 0), (0, 400));
        let names = hint_labels(&hints);
        assert!(names.contains(&"(NewOrderSingle)".to_owned()), "{names:?}");
        assert!(names.contains(&"(Buy)".to_owned()), "{names:?}");
        assert!(
            names.iter().any(|name| name.contains("NewOrderSingle (D)")),
            "{names:?}"
        );
        client.shutdown();
    }
}

#[test]
fn unresolved_application_does_not_inherit_a_transport_msg_type_enum() {
    const LINE: &str = "8=FIXT.1.1|35=D|54=1|";
    let mut client = open_with(
        json!({
            "inlayHints": {
                "tagNames": { "enabled": false },
                "enumValues": { "enabled": true },
            },
            "messageSummary": {"channels": {"inlayHints": true}, "template": "{35:name|unresolved}"},
        }),
        plain_caps(),
        LINE,
    );
    let hints = over(&mut client, (0, 0), (0, 400));
    let names = hint_labels(&hints);
    assert!(
        !names
            .iter()
            .any(|name| name.contains("NewOrderSingle") || name.contains("Buy")),
        "{names:?}"
    );
    assert_eq!(names, ["D"]);
    client.shutdown();
}
