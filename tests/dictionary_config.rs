mod harness;

use harness::{TestClient, Workspace};
use serde_json::{json, Value};

const WARNING: i64 = 2;

fn custom_root_uri() -> String {
    format!("file://{}/xml/custom/synthetic", env!("CARGO_MANIFEST_DIR"))
}

fn shows(notes: &[(String, Value)]) -> Vec<&Value> {
    notes
        .iter()
        .filter(|(m, _)| m == "window/showMessage")
        .map(|(_, p)| p)
        .collect()
}

fn logs_mentioning<'a>(notes: &'a [(String, Value)], needle: &str) -> Vec<&'a Value> {
    notes
        .iter()
        .filter(|(m, p)| {
            m == "window/logMessage" && p["message"].as_str().is_some_and(|s| s.contains(needle))
        })
        .map(|(_, p)| p)
        .collect()
}

/// Wait for a committed configuration before collecting its reports.
fn run_with_dicts(dictionaries: Value) -> Vec<(String, Value)> {
    let mut client = TestClient::start();
    client.initialize_with(json!({
        "capabilities": {},
        "rootUri": custom_root_uri(),
        "initializationOptions": { "fix": { "allowDictionaryPath": true, "dictionaries": dictionaries } }
    }));
    client.did_open("untitled:configured", "8=FIX.4.4|35=0|", 1);
    let (res, notes) = client.request_collecting("shutdown", Value::Null);
    assert!(res.is_ok());
    notes
}

#[test]
fn configured_missing_dictionary_is_fatal() {
    let fixture = Workspace::new("missing-dictionary");
    let path = fixture.0.join("x.xml");
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (id, _, _) = client.recv_server_request();
    client.reply(
        id,
        json!([{ "allowDictionaryPath": true, "dictionaries": [{ "path": path, "name": "gone" }] }]),
    );

    client.did_open_unready("untitled:configured", "8=FIX.4.4|35=0|", 1);
    assert!(
        matches!(client.wait_for_exit(), venue_reader::server::ServerExit::Fatal(message) if message.contains("x.xml"))
    );
}

#[test]
fn an_invalid_config_is_reported_once_not_on_every_pull() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let bad = json!([{ "dictionaries": "nonsense" }]);
    let (id, _, _) = client.recv_server_request();
    client.reply(id, bad.clone());
    // The same broken payload again, as a re-pull would deliver it.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    let (id, _, _) = client.recv_server_request();
    client.reply(id, bad);

    let (res, notes) = client.request_collecting(
        "textDocument/hover",
        TestClient::hover_params("file:///none.fix", 0, 0),
    );
    assert!(res.is_ok(), "server still answers");
    let shown = shows(&notes);
    assert_eq!(shown.len(), 1, "one warning for two pulls: {notes:?}");
    insta::assert_json_snapshot!("invalid_dictionaries", shown);
    client.shutdown();
}

#[test]
fn an_unchanged_path_permission_refusal_warns_once_not_on_every_toggle() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let broken =
        json!([{ "dictionaries": [{ "path": "/nonexistent/dir/x.xml", "name": "gone" }] }]);
    let (id, _, _) = client.recv_server_request();
    client.reply(id, broken.clone());
    client.did_open("untitled:initial", "8=FIX.4.4|35=0|", 1);
    // A client may re-pull on every toggle; the answer is byte-identical each time.
    for index in 0..3 {
        client.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": null }),
        );
        let (id, _, _) = client.recv_server_request();
        client.reply(id, broken.clone());
        client.did_open(&format!("untitled:toggle-{index}"), "8=FIX.4.4|35=0|", 1);
    }

    let (res, notes) = client.request_collecting("shutdown", Value::Null);
    assert!(res.is_ok());
    assert_eq!(
        shows(&notes).len(),
        1,
        "one warning for four applications: {notes:?}"
    );
    assert_eq!(shows(&notes)[0]["type"], WARNING);
    assert!(
        logs_mentioning(&notes, "/nonexistent/dir/x.xml").len() >= 4,
        "each global commit is logged; document-ready commits may log too"
    );
}

#[test]
fn relative_path_loads_through_initialize_root() {
    // A relative dictionary path resolves against rootUri: loads silently.
    let notes = run_with_dicts(json!([{ "path": "oe-fix44.xml", "name": "oe" }]));
    assert!(shows(&notes).is_empty(), "{notes:?}");
}

#[test]
fn reply_with_null_after_a_custom_config_drops_custom_dictionaries_silently() {
    let mut client = TestClient::start();
    client.initialize_with(
        json!({ "capabilities": TestClient::caps_with_pull(), "rootUri": custom_root_uri() }),
    );
    client.answer("workspace/configuration", json!([{ "allowDictionaryPath": true, "dictionaries": [{ "path": "oe-fix44.xml", "name": "oe" }] }]));
    client.did_open("untitled:custom", "8=FIX.4.4|35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:custom")["dictionaries"][0]["name"],
        "oe"
    );
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    client.answer_base_pull();
    client.did_open("untitled:reset", "8=FIX.4.4|35=0|", 1);
    assert_eq!(
        client.wait_ready("untitled:reset")["dictionaries"],
        json!([])
    );
    let (_, notes) = client.request_collecting("shutdown", Value::Null);
    assert!(shows(&notes).is_empty(), "{notes:?}");
}

/// `name` is required, so an entry without one is a *configuration* error, not
/// a dictionary that quietly does not load. The distinction matters: a user who
/// mistypes the key should be told, and the rest of their configuration should
/// not silently take effect around the hole.
#[test]
fn a_dictionary_entry_without_a_name_is_reported() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    let (id, _, _) = client.recv_server_request();
    client.reply(id, json!([{ "dictionaries": [{ "path": "/tmp/x.xml" }] }]));

    client.did_open_unready("file:///a.fix", "8=FIX.4.4|35=0|", 1);
    let (res, notes) = client.request_collecting(
        "textDocument/hover",
        TestClient::hover_params("file:///a.fix", 0, 0),
    );
    assert_eq!(
        res,
        Err(harness::REQUEST_FAILED),
        "the unconfigured document is not processed"
    );

    let shown = shows(&notes);
    insta::assert_json_snapshot!("missing_name", shown);
    client.shutdown();
}

#[test]
fn a_blank_name_rejects_initial_configuration_until_a_valid_update() {
    let mut client = TestClient::start();
    client.initialize_with(
        json!({ "capabilities": {}, "initializationOptions": { "fix": {
        "allowDictionaryPath": true, "dictionaries": [{ "path": "oe-fix44.xml", "name": "  " }],
    } } }),
    );
    client.did_open_unready("untitled:invalid", "8=FIX.4.4|35=0|", 1);
    let (result, notes) = client.request_collecting(
        "textDocument/hover",
        TestClient::hover_params("untitled:invalid", 0, 0),
    );
    assert_eq!(result, Err(harness::REQUEST_FAILED));
    let shown = shows(&notes);
    insta::assert_json_snapshot!("blank_name", shown);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "fix": {} } }),
    );
    client.wait_ready("untitled:invalid");
    assert!(!client.hover("untitled:invalid", 0, 0).unwrap().is_null());
    client.shutdown();
}

#[test]
fn removing_a_previously_loaded_dictionary_is_fatal_on_refresh() {
    let directory = std::env::temp_dir().join(format!(
        "venue-reader-dictionary-refresh-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("dictionary.xml");
    std::fs::write(&path, "<fix major='4' minor='4'><messages><message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields/></fix>").unwrap();
    let settings = json!({ "allowDictionaryPath": true, "dictionaries": [{ "name": "temporary", "path": path }] });
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer("workspace/configuration", json!([settings]));
    client.did_open("untitled:loaded", "8=FIX.4.4|35=0|", 1);
    std::fs::remove_file(&path).unwrap();
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": null }),
    );
    client.answer("workspace/configuration", json!([settings]));
    assert!(
        matches!(client.wait_for_exit(), venue_reader::server::ServerExit::Fatal(message) if message.contains("dictionary.xml"))
    );
    std::fs::remove_dir(directory).unwrap();
}

const SOUND: &str = "<fix major='4' minor='4'><messages>\
    <message name='NewOrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
    <field number='9001' name='SoundField' type='STRING'/></fields></fix>";
const DOUBLED: &str = "<fix major='4' minor='4'><messages>\
    <message name='NewOrderSingle' msgtype='D' msgcat='app'/>\
    <message name='OrderSingle' msgtype='D' msgcat='app'/></messages><fields>\
    <field number='9001' name='DoubledField' type='STRING'/></fields></fix>";
const BROKEN: &str = "<fix major='4' minor='4'><messages>\
    <message name='NewOrderSingle' msgtype='D'><component name='Instrumnt'/></message></messages><fields>\
    <field number='55' name='Symbol' type='STRING'/></fields></fix>";
const MISSPELT: &str = "<fix major='4' minor='4'><messages>\
    <message name='MarketDataRequest' msgtype='V' msgcat='app'><field name='PriceAggregation'/></message></messages><fields>\
    <field number='9001' name='MisspeltField' type='STRING'/></fields></fix>";

fn dictionary_notice(notes: &[(String, Value)], fixture: &Workspace, filename: &str) -> Value {
    let shown = shows(notes);
    assert_eq!(shown.len(), 1);
    let mut notice = shown[0].clone();
    let message = notice["message"].as_str().expect("dictionary message");
    let path = fixture.0.join(filename);
    let quoted_path = format!("{path:?}");
    assert!(message.contains(&quoted_path), "{message}");
    let stable_path = format!("<workspace>/{filename}");
    let canonical = path.canonicalize().unwrap();
    notice["message"] = json!(message
        .replace(&quoted_path, &format!("{stable_path:?}"))
        .replace(canonical.to_str().unwrap(), &stable_path));
    notice
}

/// Hover text over tag 9001 on `8=FIX.4.4|35=0|9001=x|`.
fn hover_9001(client: &mut TestClient, uri: &str) -> String {
    client.did_open(uri, "8=FIX.4.4|35=0|9001=x|", 1);
    client.hover(uri, 0, 16).unwrap().to_string()
}

#[test]
fn a_refused_dictionary_is_an_error_and_a_sound_one_beside_it_applies() {
    let fixture = Workspace::new("refused-dictionary");
    let broken = fixture.write("broken.xml", BROKEN);
    let sound = fixture.write("sound.xml", SOUND);
    let mut client = fixture.client(json!({ "allowDictionaryPath": true, "dictionaries": [
        { "path": broken, "name": "broken", "default": true },
        { "path": sound, "name": "sound", "default": true },
    ] }));
    assert!(hover_9001(&mut client, "untitled:orders").contains("SoundField"));
    let (_, notes) = client.request_collecting("shutdown", Value::Null);
    insta::assert_json_snapshot!(
        "refused_dictionary",
        dictionary_notice(&notes, &fixture, "broken.xml")
    );
}

#[test]
fn a_dictionary_with_a_doubled_message_type_applies_and_is_reported_as_an_error() {
    let fixture = Workspace::new("doubled-message");
    let mut client = fixture.client(fixture.desk_dictionary(DOUBLED));
    assert!(hover_9001(&mut client, "untitled:orders").contains("DoubledField"));
    let (_, notes) = client.request_collecting("shutdown", Value::Null);
    insta::assert_json_snapshot!(
        "duplicate_message_type",
        dictionary_notice(&notes, &fixture, "desk.xml")
    );
}

#[test]
fn a_message_defined_twice_under_one_name_is_reported_as_a_warning() {
    let fixture = Workspace::new("doubled-same-name");
    let mut client = fixture.client(
        fixture.desk_dictionary(&DOUBLED.replace("name='OrderSingle'", "name='NewOrderSingle'")),
    );
    assert!(hover_9001(&mut client, "untitled:orders").contains("DoubledField"));
    let (_, notes) = client.request_collecting("shutdown", Value::Null);
    insta::assert_json_snapshot!(
        "duplicate_message_name",
        dictionary_notice(&notes, &fixture, "desk.xml")
    );
}

#[test]
fn a_dictionary_with_an_undefined_field_applies_and_is_reported_as_an_error() {
    let fixture = Workspace::new("undefined-field");
    let mut client = fixture.client(fixture.desk_dictionary(MISSPELT));
    assert!(hover_9001(&mut client, "untitled:orders").contains("MisspeltField"));
    let (_, notes) = client.request_collecting("shutdown", Value::Null);
    insta::assert_json_snapshot!(
        "missing_field",
        dictionary_notice(&notes, &fixture, "desk.xml")
    );
}
