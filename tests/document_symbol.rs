//! `textDocument/documentSymbol` through the protocol.
//!
//! What the in-crate tests cannot see: the wire shape a client actually
//! receives, and the `window/logMessage` the server sends *before* the task
//! runs — which is where every limit in this server is decided.

mod harness;

use harness::TestClient;
use serde_json::{json, Value};

const URI: &str = "file:///a.fix";

/// Two parties, the first carrying a nested sub-ID group.
const PARTIES: &str = concat!(
    "8=FIX.4.4|35=D|453=2|448=A|447=D|452=1|802=1|523=S1|803=1",
    "|448=B|447=D|452=2|54=1"
);

fn nesting_client() -> Value {
    json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": { "documentSymbol": {
            "hierarchicalDocumentSymbolSupport": true,
            "symbolKind": { "valueSet": (1..=26).collect::<Vec<i32>>() },
        }},
    })
}

fn open_with(capabilities: Value, text: &str) -> TestClient {
    let mut client = TestClient::start();
    client.initialize_caps(capabilities);
    client.did_open(URI, text, 1);
    client
}

fn outline(client: &mut TestClient) -> Vec<Value> {
    client
        .request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": URI } }),
        )
        .expect("answered")
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The tree as one readable block, so a change of shape fails as a shape.
fn shape(nodes: &[Value]) -> String {
    fn walk(nodes: &[Value], depth: usize, out: &mut Vec<String>) {
        for node in nodes {
            out.push(format!(
                "{}{} [{}]",
                "  ".repeat(depth),
                node["name"].as_str().expect("a name"),
                node["kind"],
            ));
            walk(
                node["children"].as_array().unwrap_or(&Vec::new()),
                depth + 1,
                out,
            );
        }
    }
    let mut out = Vec::new();
    walk(nodes, 0, &mut out);
    out.join("\n")
}

#[test]
fn a_client_that_takes_nesting_gets_the_tree() {
    let mut client = open_with(nesting_client(), PARTIES);
    assert_eq!(
        shape(&outline(&mut client)),
        "\
# → NewOrderSingle (D) [19]
  BeginString (8) = FIX.4.4 [8]
  MsgType (35) = D (NewOrderSingle) [8]
  NoPartyIDs (453) = 2 [18]
    [1] [23]
      PartyID (448) = A [8]
      PartyIDSource (447) = D (Proprietary) [8]
      PartyRole (452) = 1 (ExecutingFirm) [8]
      NoPartySubIDs (802) = 1 [18]
        [1] [23]
          PartySubID (523) = S1 [8]
          PartySubIDType (803) = 1 (Firm) [8]
    [2] [23]
      PartyID (448) = B [8]
      PartyIDSource (447) = D (Proprietary) [8]
      PartyRole (452) = 2 (BrokerOfCredit) [8]
  Side (54) = 1 (Buy) [8]"
    );
    client.shutdown();
}

/// `Object` (19) and `Struct` (23) are outside the set a client understands
/// without listing one, so a client that lists nothing gets `Class` and
/// `Array` — and the tree is otherwise identical.
#[test]
fn a_client_that_lists_no_kinds_gets_the_legacy_ones() {
    let mut client = open_with(
        json!({
            "general": { "positionEncodings": ["utf-8"] },
            "textDocument": { "documentSymbol": {
                "hierarchicalDocumentSymbolSupport": true,
            }},
        }),
        PARTIES,
    );
    let shape = shape(&outline(&mut client));
    assert!(shape.starts_with("# → NewOrderSingle (D) [5]"), "{shape}");
    assert!(shape.contains("    [1] [18]"), "an entry as Array: {shape}");
    assert!(
        !shape.contains("[19]") && !shape.contains("[23]"),
        "{shape}"
    );
    client.shutdown();
}

/// The outline is only offered nested, so a client that takes no nesting is
/// not offered the provider at all — the way the lens provider is withheld from
/// a client that resolves no command. `initialize` is the only moment it can be
/// told, because it will never send a request.
#[test]
fn a_client_without_nesting_is_offered_no_provider() {
    let mut client = TestClient::start();
    let result = client.initialize_caps(json!({ "general": { "positionEncodings": ["utf-8"] } }));
    assert!(
        result["capabilities"]["documentSymbolProvider"].is_null(),
        "withheld: {}",
        result["capabilities"]
    );

    let said = client.drain_notifications(std::time::Duration::from_millis(50));
    let logged = said
        .iter()
        .filter(|(method, _)| method == "window/logMessage")
        .find_map(|(_, params)| params["message"].as_str())
        .expect("the limitation is named");
    assert!(
        logged.contains("hierarchicalDocumentSymbolSupport"),
        "{logged}"
    );
    assert!(logged.contains("no document-symbol provider"), "{logged}");
    client.shutdown();
}

/// And a client that takes nesting is offered it.
#[test]
fn a_client_that_takes_nesting_is_offered_the_provider() {
    let mut client = TestClient::start();
    let result = client.initialize_caps(nesting_client());
    assert_eq!(result["capabilities"]["documentSymbolProvider"], true);
    client.shutdown();
}

/// Past the budget: a deterministic prefix, one marker that counts itself, and
/// one log naming the setting.
#[test]
fn past_the_budget_the_answer_says_what_it_left_out() {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": nesting_client(),
        "initializationOptions": { "fix": { "limits": { "documentSymbolItems": 2 } } },
    }));
    let heartbeat = "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|10=171|";
    client.did_open(URI, &[heartbeat; 3].join("\n"), 1);

    let found = outline(&mut client);
    assert_eq!(found.len(), 2, "one message and the marker: {found:?}");
    assert_eq!(found[1]["name"], "… 2 more messages");
    assert_eq!(
        found[1]["range"]["start"]["line"], 1,
        "anchored on the first message it stands for"
    );

    let said = client.drain_notifications(std::time::Duration::from_millis(50));
    let logged = said
        .iter()
        .find(|(method, _)| method == "window/logMessage")
        .map(|(_, params)| params["message"].as_str().unwrap_or_default().to_owned())
        .expect("the cap is reported");
    assert!(logged.contains("documentSymbolItems"), "{logged}");
    client.shutdown();
}

/// A column is not a byte offset, and an outline produces many of both.
#[test]
fn ranges_are_columns_under_utf16() {
    let text = PARTIES.replace("448=A", "448=привет");
    let mut client = TestClient::start();
    client.initialize_caps(json!({
        "general": { "positionEncodings": ["utf-16"] },
        "textDocument": { "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }},
    }));
    client.did_open(URI, &text, 1);

    let found = outline(&mut client);
    let end = found[0]["range"]["end"]["character"]
        .as_u64()
        .expect("an end");
    assert_eq!(
        end,
        text.encode_utf16().count() as u64,
        "the message spans the line in columns, not bytes"
    );
    assert!(end < text.len() as u64, "and columns are behind bytes here");
    client.shutdown();
}

#[test]
fn a_closed_document_answers_with_nothing() {
    let mut client = open_with(nesting_client(), PARTIES);
    client.did_close(URI);
    assert_eq!(outline(&mut client), Vec::<Value>::new());
    client.shutdown();
}

#[test]
fn malformed_params_are_an_error() {
    let mut client = open_with(nesting_client(), PARTIES);
    let error = client
        .request("textDocument/documentSymbol", json!({ "textDocument": 7 }))
        .expect_err("refused");
    assert_eq!(error, harness::INVALID_PARAMS);
    client.shutdown();
}

/// The nesting bound, which needs a dictionary rather than a line: nesting
/// depth is a property of the dictionary, so no input can exceed what its own
/// declares. `deep-groups.xml` declares 32, one past the codec's cap and eight
/// past the outline's.
mod deep {
    use super::*;

    const DEEP_XML: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/xml/custom/synthetic/deep-groups.xml"
    );

    /// `8=FIX.4.4|35=ZZ|55=X|` then 32 nested `NoLevelN=1|LeafN=vN`.
    fn deep_line() -> String {
        let mut line = String::from("8=FIX.4.4|35=ZZ|55=X");
        for level in 0..32 {
            line.push_str(&format!("|{}=1|{}=v{level}", 20000 + level, 20100 + level));
        }
        line
    }

    fn open_deep() -> (TestClient, String) {
        let text = deep_line();
        let mut client = TestClient::start();
        client.initialize_with(json!({
            "capabilities": nesting_client(),
            "initializationOptions": { "fix": { "allowDictionaryPath": true, "dictionaries": [{
                "path": DEEP_XML, "name": "deep", "default": true,
            }]}},
        }));
        client.did_open(URI, &text, 1);
        (client, text)
    }

    fn depth_of(node: &Value) -> usize {
        1 + node["children"]
            .as_array()
            .map(|kids| kids.iter().map(depth_of).max().unwrap_or(0))
            .unwrap_or(0)
    }

    fn names(node: &Value, out: &mut Vec<String>) {
        out.push(node["name"].as_str().expect("a name").to_owned());
        for child in node["children"].as_array().unwrap_or(&Vec::new()) {
            names(child, out);
        }
    }

    /// Every field of the line appears exactly once, however deep the
    /// dictionary nests: past the bound a field attaches to the last
    /// representable ancestor rather than to an entry that was never emitted.
    #[test]
    fn no_field_is_lost_or_repeated_past_the_nesting_bound() {
        let (mut client, _) = open_deep();
        let found = outline(&mut client);
        let mut all = Vec::new();
        names(&found[0], &mut all);

        for level in 0..32 {
            let leaf = format!("Leaf{level} ({}) = v{level}", 20100 + level);
            assert_eq!(
                all.iter().filter(|name| **name == leaf).count(),
                1,
                "{leaf} appears once"
            );
            let count = format!("NoLevel{level} ({}) = 1", 20000 + level);
            assert_eq!(
                all.iter().filter(|name| **name == count).count(),
                1,
                "{count} appears once"
            );
        }
        client.shutdown();
    }

    /// And the tree stops where the outline says it does, not where the
    /// dictionary does.
    #[test]
    fn the_tree_stops_at_the_outlines_bound() {
        let (mut client, _) = open_deep();
        let found = outline(&mut client);
        // message + 24 × (group + entry), and the deepest entry's fields.
        assert_eq!(depth_of(&found[0]), 1 + 24 * 2 + 1);
        client.shutdown();
    }

    /// The bound exists because of a limit no in-process test can reach: the
    /// harness passes decoded values and never serialises, while a real client
    /// parses JSON with a default nesting limit of 128.
    #[test]
    fn the_answer_survives_a_real_json_round_trip() {
        let (mut client, _) = open_deep();
        let found = outline(&mut client);
        let text = serde_json::to_string(&Value::Array(found.clone())).expect("serialises");
        let back: Value = serde_json::from_str(&text).expect("a client can parse it back");
        assert_eq!(back, Value::Array(found));
        client.shutdown();
    }
}

/// NoHops(627) is a header group: the outline nests its entries under FIX.4.4
/// and under `8=FIXT.1.1`.
#[test]
fn a_header_group_nests_in_the_outline() {
    for (line, begin) in [
        (
            "8=FIX.4.4|35=0|627=2|628=A|630=1|628=B|630=2|112=T",
            "FIX.4.4",
        ),
        (
            "8=FIXT.1.1|35=0|627=2|628=A|630=1|628=B|630=2|112=T",
            "FIXT.1.1",
        ),
    ] {
        let mut client = open_with(nesting_client(), line);
        assert_eq!(
            shape(&outline(&mut client)),
            format!(
                "\
# → Heartbeat (0) [19]
  BeginString (8) = {begin} [8]
  MsgType (35) = 0 (Heartbeat) [8]
  NoHops (627) = 2 [18]
    [1] [23]
      HopCompID (628) = A [8]
      HopRefID (630) = 1 [8]
    [2] [23]
      HopCompID (628) = B [8]
      HopRefID (630) = 2 [8]
  TestReqID (112) = T [8]"
            )
        );
        client.shutdown();
    }
}

#[test]
fn fixt_outline_names_groups_and_values_from_their_own_sources() {
    const LINE: &str = "8=FIXT.1.1|35=D|1128=9|453=1|448=P|447=D|452=1|802=1|523=S|803=1|54=1|";
    let mut client = open_with(nesting_client(), LINE);
    let tree = shape(&outline(&mut client));
    for expected in [
        "NewOrderSingle (D)",
        "BeginString (8) = FIXT.1.1",
        "MsgType (35) = D (NewOrderSingle)",
        "NoPartyIDs (453) = 1",
        "NoPartySubIDs (802) = 1",
        "PartySubIDType (803) = 1 (Firm)",
        "Side (54) = 1 (Buy)",
    ] {
        assert!(tree.contains(expected), "{tree}");
    }
    client.shutdown();
}
