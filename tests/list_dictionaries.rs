//! Generic dictionary discovery and selection over the LSP boundary.

mod harness;

use harness::{file_uri, TestClient, Workspace, INVALID_PARAMS, REQUEST_FAILED};
use serde_json::{json, Value};

const BUNDLED: [&str; 9] = [
    "FIX.4.0",
    "FIX.4.1",
    "FIX.4.2",
    "FIX.4.3",
    "FIX.4.4",
    "FIX.5.0",
    "FIX.5.0SP1",
    "FIX.5.0SP2",
    "FIXT.1.1",
];

fn registration(name: &str, version: &str) -> Value {
    json!({"name":name,"transport":version,"application":version})
}

fn list(client: &mut TestClient) -> Vec<Value> {
    let result = client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.listDictionaries","arguments":[]}),
        )
        .expect("dictionary list");
    assert_eq!(result.as_object().expect("result object").len(), 1);
    result["dictionaries"].as_array().expect("choices").clone()
}

fn select(client: &mut TestClient, name: &str) -> Result<Value, i32> {
    client.request(
        "workspace/executeCommand",
        json!({"command":"fix.useDictionary","arguments":[name]}),
    )
}

fn assert_round_trip(client: &mut TestClient, entry: &Value) {
    assert_eq!(entry.as_object().expect("choice object").len(), 8);
    let selected = select(client, entry["name"].as_str().expect("selector")).unwrap();
    assert_eq!(selected["name"], entry["name"]);
    let mut binding = entry.as_object().unwrap().clone();
    for field in ["label", "kind", "definedIn"] {
        binding.remove(field);
    }
    assert_eq!(selected, Value::Object(binding));
}

fn global_names(entries: &[Value]) -> Vec<&str> {
    entries
        .iter()
        .filter(|entry| entry["kind"] == "global")
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect()
}

#[test]
fn canonical_choices_round_trip_without_any_open_document() {
    let mut client = TestClient::start();
    client.initialize(&["utf-8"]);
    client.wait_base_ready();
    let choices = list(&mut client);
    assert_eq!(choices.len(), BUNDLED.len());
    for (entry, expected) in choices.iter().zip(BUNDLED) {
        assert_eq!(entry["name"], expected);
        assert_eq!(entry["label"], format!("{expected} (bundled)"));
        assert_eq!(entry["kind"], "bundled");
        assert!(entry["definedIn"].is_null());
        assert_eq!(entry["version"], expected);
        assert_eq!(entry["source"], expected);
        let transport = if expected.starts_with("FIX.5.") {
            "FIXT.1.1"
        } else {
            expected
        };
        assert_eq!(
            entry["transport"],
            json!({"version":transport,"source":transport})
        );
        if expected == "FIXT.1.1" {
            assert!(entry["application"].is_null());
        } else {
            assert_eq!(
                entry["application"],
                json!({
                    "version":expected,"source":expected,
                    "extensionPack":null,"customVersion":null
                })
            );
        }
        assert_round_trip(&mut client, entry);
    }
    let omitted = client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.listDictionaries"}),
        )
        .unwrap();
    assert_eq!(omitted["dictionaries"], json!(choices));
    client.shutdown();
}

#[test]
fn global_sources_aliases_and_duplicate_winners_are_reported_accurately() {
    let fixture = Workspace::new("list-global-sources");
    let user_file = fixture.write(
        "user.toml",
        "[[dictionaries]]\nname='same'\ntransport='FIX.4.2'\napplication='FIX.4.2'\n",
    );
    let external = fixture.write(
        "synthetic.xml",
        include_str!("../xml/custom/synthetic/oe-fix44.xml"),
    );
    let unusual_name = "  Alias \"β\"  ";
    let mut client = TestClient::start_with_user_file(Some(user_file.clone()));
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"dictionaries":[
            registration("zulu", "FIX.4.4"),
            registration("same", "FIX.4.4"),
            registration(unusual_name, "FIX.4.4"),
            {"name":"external","path":external},
            registration("Alpha", "FIX.4.4")
        ]}),
    );
    client.wait_base_ready();
    let entries = list(&mut client);
    assert_eq!(
        global_names(&entries),
        vec![unusual_name, "Alpha", "external", "same", "zulu"]
    );
    let canonical_source = external.canonicalize().unwrap();
    for entry in entries.iter().filter(|entry| entry["kind"] == "global") {
        let name = entry["name"].as_str().unwrap();
        assert_eq!(entry["label"], format!("{name} (global)"));
        if name == "same" {
            assert_eq!(entry["definedIn"], file_uri(&user_file));
            assert_eq!(entry["version"], "FIX.4.2");
            assert_eq!(entry["source"], "FIX.4.2");
        } else {
            assert_eq!(entry["definedIn"], "client");
            assert_eq!(entry["version"], "FIX.4.4");
        }
        if name == "external" {
            assert_eq!(entry["source"], json!(canonical_source));
            assert_eq!(entry["transport"]["source"], json!(canonical_source));
            assert_eq!(entry["application"]["source"], json!(canonical_source));
        }
        assert_round_trip(&mut client, entry);
    }
    assert_eq!(list(&mut client), entries);
    client.shutdown();
}

#[test]
fn listing_excludes_rejected_and_path_gated_names_but_keeps_default_conflicts() {
    let fixture = Workspace::new("list-rejected-names");
    let mut first_default = registration("conflict-a", "FIX.4.4");
    first_default["default"] = json!(true);
    let mut second_default = registration("conflict-b", "FIX.4.4");
    second_default["default"] = json!(true);
    let mut client = fixture.client(json!({"dictionaries":[
        first_default,
        second_default,
        registration("FIX.4.4", "FIX.4.2"),
        registration("FIX.2.7", "FIX.4.2"),
        {
            "name":"invalid-binding","transport":"FIX.4.4","application":"FIX.4.4",
            "applicationVersion":"FIX.4.2"
        },
        {"name":"blocked-path","path":fixture.0.join("not-opened.xml")}
    ]}));
    client.wait_base_ready();
    let entries = list(&mut client);
    assert_eq!(global_names(&entries), vec!["conflict-a", "conflict-b"]);
    for entry in entries.iter().filter(|entry| entry["kind"] == "global") {
        assert_round_trip(&mut client, entry);
    }
    assert_eq!(select(&mut client, "invalid-binding"), Err(REQUEST_FAILED));
    assert_eq!(select(&mut client, "blocked-path"), Err(REQUEST_FAILED));
    client.shutdown();
}

#[test]
fn project_names_are_excluded_and_document_overrides_survive_selection() {
    let fixture = Workspace::new("list-project-precedence");
    fixture.write(
        ".venue-reader.toml",
        "dictionary='project-only'\n[[dictionaries]]\nname='project-only'\ntransport='FIX.4.2'\napplication='FIX.4.2'\n",
    );
    let mut client = fixture.client(json!({"dictionaries":[registration("global", "FIX.4.4")]}));
    let project_uri = fixture.uri("project.fix");
    let modeline_uri = fixture.uri("modeline.fix");
    client.did_open(&project_uri, "35=0|", 1);
    client.did_open(&modeline_uri, "# fix: dictionary=FIX.4.3\n35=0|", 1);
    let before_project = client.describe(&project_uri).unwrap();
    let before_modeline = client.describe(&modeline_uri).unwrap();
    let project_hover = client.hover(&project_uri, 0, 1).unwrap();
    let modeline_hover = client.hover(&modeline_uri, 1, 1).unwrap();
    let entries = list(&mut client);
    assert_eq!(global_names(&entries), vec!["global"]);
    assert_eq!(client.describe(&project_uri).unwrap(), before_project);
    assert_eq!(client.describe(&modeline_uri).unwrap(), before_modeline);
    assert_eq!(select(&mut client, "project-only"), Err(REQUEST_FAILED));
    select(&mut client, "global").unwrap();
    client.did_open(&fixture.uri("after-selection.fix"), "35=0|", 1);
    let project = client.describe(&project_uri).unwrap();
    let modeline = client.describe(&modeline_uri).unwrap();
    assert_eq!(project["effective"]["dictionary"], "project-only");
    assert_eq!(project["sources"]["dictionary"]["layer"], "project");
    assert_eq!(modeline["effective"]["dictionary"], "FIX.4.3");
    assert_eq!(modeline["sources"]["dictionary"]["layer"], "modeline");
    assert_eq!(client.hover(&project_uri, 0, 1).unwrap(), project_hover);
    assert_eq!(client.hover(&modeline_uri, 1, 1).unwrap(), modeline_hover);
    client.shutdown();
}

#[test]
fn argument_validation_precedes_initial_readiness_and_bundled_selection_still_works() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    for arguments in [
        json!([null]),
        json!(["file:///a.fix"]),
        json!([{}]),
        json!({}),
    ] {
        assert_eq!(
            client.request(
                "workspace/executeCommand",
                json!({"command":"fix.listDictionaries","arguments":arguments}),
            ),
            Err(INVALID_PARAMS)
        );
    }
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.listDictionaries","arguments":[]}),
        ),
        Err(REQUEST_FAILED)
    );
    assert_eq!(select(&mut client, "FIX.4.2").unwrap()["name"], "FIX.4.2");
    client.answer_base_pull();
    client.wait_base_ready();
    assert_eq!(list(&mut client).len(), BUNDLED.len());
    client.shutdown();
}

#[test]
fn a_rejected_configuration_refuses_discovery_until_a_valid_update() {
    let mut client = TestClient::start();
    client.initialize_caps(TestClient::caps_with_pull());
    client.answer(
        "workspace/configuration",
        json!([{"dictionaries":"invalid"}]),
    );
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.listDictionaries"}),
        ),
        Err(REQUEST_FAILED)
    );
    client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
    client.answer_base_pull();
    client.wait_base_ready();
    assert_eq!(list(&mut client).len(), BUNDLED.len());
    client.shutdown();
}

#[test]
fn listing_does_not_reserve_removed_or_rebound_names() {
    let fixture = Workspace::new("list-registry-updates");
    let mut client = fixture.client(json!({"dictionaries":[registration("mutable", "FIX.4.4")]}));
    client.wait_base_ready();
    assert_eq!(global_names(&list(&mut client)), vec!["mutable"]);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{"dictionaries":[]}}}),
    );
    client.did_open("untitled:removed", "35=0|", 1);
    assert!(global_names(&list(&mut client)).is_empty());
    assert_eq!(select(&mut client, "mutable"), Err(REQUEST_FAILED));
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{"dictionaries":[registration("mutable", "FIX.4.2")]}}}),
    );
    client.did_open("untitled:restored", "35=0|", 1);
    let old_entry = list(&mut client).pop().unwrap();
    assert_eq!(old_entry["name"], "mutable");
    assert_eq!(old_entry["version"], "FIX.4.2");
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":{"dictionaries":[registration("mutable", "FIX.4.3")]}}}),
    );
    client.did_open("untitled:rebound", "35=0|", 1);
    assert_eq!(
        select(&mut client, "mutable").unwrap()["version"],
        "FIX.4.3"
    );
    assert_eq!(list(&mut client).last().unwrap()["version"], "FIX.4.3");
    client.shutdown();
}

#[test]
fn an_oversized_inventory_is_refused_without_a_partial_result() {
    let fixture = Workspace::new("list-result-budget");
    let oversized_name = "a".repeat(600_000);
    let mut client =
        fixture.client(json!({"dictionaries":[registration(&oversized_name, "FIX.4.4")]}));
    client.wait_base_ready();
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.listDictionaries"}),
        ),
        Err(REQUEST_FAILED)
    );
    assert_eq!(
        select(&mut client, &oversized_name).unwrap()["name"],
        oversized_name
    );
    client.shutdown();
}
