mod harness;

use std::fs;
use std::time::{Duration, Instant};

use harness::{file_uri, TestClient, Workspace, INVALID_PARAMS, REQUEST_FAILED};
use serde_json::{json, Value};

const MESSAGE: &str = "8=FIX.4.4|35=D|54=1|";

fn wait_effective(client: &mut TestClient, uri: &str, key: &str, expected: Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let report = client.describe(uri);
        if let Ok(report) = &report {
            if report["effective"][key] == expected {
                return report.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{key} did not become {expected}: {report:?}"
        );
        std::thread::yield_now();
    }
}

fn refresh(client: &TestClient, settings: Value) {
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"fix":settings}}),
    );
}

fn dictionary_xml(field_name: &str) -> String {
    format!(
        r#"<fix type="FIX" major="4" minor="4">
<header><field name="BeginString" required="Y"/><field name="MsgType" required="Y"/></header>
<messages><message name="NewOrderSingle" msgtype="D" msgcat="app"><field name="{field_name}" required="N"/></message></messages>
<trailer/><components/>
<fields><field number="8" name="BeginString" type="STRING"/><field number="35" name="MsgType" type="STRING"><value enum="D" description="NEW_ORDER_SINGLE"/></field><field number="9001" name="{field_name}" type="STRING"/></fields>
</fix>"#
    )
}

#[test]
fn conflicting_child_dictionary_name_is_an_error_before_the_child_path_is_read() {
    let fixture = Workspace::new("overrides");
    fixture.write("parent.xml", &dictionary_xml("ParentField"));
    fixture.write(
        ".fix-lsp.toml",
        "dictionary = 'venue'\n[[dictionaries]]\nname = 'venue'\npath = 'parent.xml'\n",
    );
    fixture.write(
        "logs/.fix-lsp.toml",
        "[[dictionaries]]\nname = 'venue'\npath = 'does-not-exist.xml'\ndefault = true\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let document = fixture.uri("logs/orders.fix");
    let report = client.did_open(&document, "8=FIX.4.4|35=D|9001=x|", 1);
    assert_eq!(report["effective"]["dictionary"], json!("venue"));
    let warnings = report["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|warning| warning["severity"] == "error"
        && warning["message"].as_str().unwrap().contains("venue")));
    assert!(client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("ParentField"));
    client.shutdown();
}

#[test]
fn independent_project_subtrees_may_define_the_same_dictionary_name() {
    let fixture = Workspace::new("overrides");
    for (directory, name) in [("left", "LeftField"), ("right", "RightField")] {
        fixture.write(&format!("{directory}/dict.xml"), &dictionary_xml(name));
        fixture.write(
            &format!("{directory}/.fix-lsp.toml"),
            "dictionary = 'venue'\n[[dictionaries]]\nname = 'venue'\npath = 'dict.xml'\n",
        );
    }
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let left = fixture.uri("left/orders.fix");
    let right = fixture.uri("right/orders.fix");
    client.did_open(&left, "8=FIX.4.4|35=D|9001=x|", 1);
    client.did_open(&right, "8=FIX.4.4|35=D|9001=x|", 1);
    assert!(client
        .hover(&left, 0, 16)
        .unwrap()
        .to_string()
        .contains("LeftField"));
    assert!(client
        .hover(&right, 0, 16)
        .unwrap()
        .to_string()
        .contains("RightField"));
    for document in [&left, &right] {
        let report = client.describe(document).unwrap();
        assert_eq!(report["effective"]["dictionary"], json!("venue"));
        assert!(report["warnings"].as_array().unwrap().is_empty());
    }
    client.shutdown();
}

#[test]
fn a_project_direct_path_is_resolved_beside_the_supplying_file() {
    let fixture = Workspace::new("overrides");
    fixture.write("logs/dict.xml", &dictionary_xml("ProjectPathField"));
    fixture.write("logs/.fix-lsp.toml", "dictionary = './dict.xml'\n");
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let document = fixture.uri("logs/orders.fix");
    client.did_open(&document, "8=FIX.4.4|35=D|9001=x|", 1);
    assert_eq!(
        client.describe(&document).unwrap()["effective"]["dictionary"],
        json!("./dict.xml")
    );
    assert!(client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("ProjectPathField"));
    client.shutdown();
}

#[test]
fn project_dictionary_definitions_are_refused_without_filesystem_permission() {
    let fixture = Workspace::new("overrides");
    fixture.write(
        ".fix-lsp.toml",
        "[[dictionaries]]\nname = 'blocked-venue'\npath = 'does-not-exist.xml'\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    let report = client.did_open(&document, MESSAGE, 1);
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| {
            let message = warning["message"].as_str().unwrap();
            message.contains("allowDictionaryPath") && message.contains("blocked-venue")
        }));
    client.shutdown();
}

#[test]
fn selecting_a_global_name_does_not_reload_its_xml() {
    let fixture = Workspace::new("overrides");
    let path = fixture.write("dict.xml", &dictionary_xml("BeforeReload"));
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false},"dictionaries":[{"name":"venue","path":"dict.xml","default":true}]}),
    );
    let first = fixture.uri("one.fix");
    let second = fixture.uri("two.fix");
    client.did_open(&first, "8=FIX.4.4|35=D|9001=x|", 1);
    client.did_open(&second, "8=FIX.4.4|35=D|9001=x|", 1);
    let before = client.describe(&first).unwrap();
    assert!(client
        .hover(&first, 0, 16)
        .unwrap()
        .to_string()
        .contains("BeforeReload"));
    fs::write(&path, dictionary_xml("AfterReload")).unwrap();
    assert_eq!(client.describe(&first).unwrap(), before);
    assert!(client
        .hover(&second, 0, 16)
        .unwrap()
        .to_string()
        .contains("BeforeReload"));
    let selected = client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["venue"]}),
        )
        .unwrap();
    assert_eq!(selected["name"], json!("venue"));
    assert_eq!(selected["version"], json!("FIX.4.4"));
    assert_eq!(
        selected["source"],
        json!(path.canonicalize().unwrap().to_string_lossy())
    );
    for document in [&first, &second] {
        wait_effective(&mut client, document, "dictionary", json!("venue"));
        assert!(client
            .hover(document, 0, 16)
            .unwrap()
            .to_string()
            .contains("BeforeReload"));
    }
    client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["FIX.4.2"]}),
        )
        .unwrap();
    assert!(
        client
            .hover(&second, 0, 16)
            .unwrap()
            .to_string()
            .contains("BeforeReload"),
        "the command selects from the committed registry without reloading XML"
    );
    client.shutdown();
}

#[test]
fn twenty_documents_share_one_project_dictionary_within_the_batch_deadline() {
    let fixture = Workspace::new("shared-project-dictionary");
    fixture.write("dict.xml", &dictionary_xml("SharedField"));
    fixture.write(
        ".fix-lsp.toml",
        "[[dictionaries]]\nname = 'shared'\npath = 'dict.xml'\ndefault = true\n",
    );
    let mut client = fixture.client(json!({
        "allowDictionaryPath":true,"diagnostics":{"enabled":false}
    }));
    let documents: Vec<_> = (0..20)
        .map(|index| fixture.uri(&format!("logs/{index}.fix")))
        .collect();
    for document in &documents {
        client.did_open(document, "8=FIX.4.4|35=D|9001=x|", 1);
    }
    for document in &documents {
        client.wait_ready(document);
        assert!(client
            .hover(document, 0, 16)
            .unwrap()
            .to_string()
            .contains("SharedField"));
    }
    client.shutdown();
}

#[test]
fn a_direct_command_path_is_rejected() {
    let fixture = Workspace::new("overrides");
    fixture.write("dict.xml", &dictionary_xml("DirectCommand"));
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let document = fixture.uri("orders.fix");
    client.did_open(&document, "8=FIX.4.4|35=D|9001=x|", 1);
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["./dict.xml"]}),
        ),
        Err(REQUEST_FAILED)
    );
    assert!(!client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("DirectCommand"));
    client.shutdown();
}

#[test]
fn a_removed_session_name_does_not_reappear_automatically() {
    let fixture = Workspace::new("overrides");
    fixture.write("dict.xml", &dictionary_xml("RememberedChoice"));
    let settings = json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false},"dictionaries":[{"name":"venue","path":"dict.xml"}]});
    let mut client = TestClient::start();
    fixture.initialize(&mut client, settings.clone());
    let document = fixture.uri("orders.fix");
    client.did_open(&document, "8=FIX.4.4|35=D|9001=x|", 1);
    client
        .request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["venue"]}),
        )
        .unwrap();
    wait_effective(&mut client, &document, "dictionary", json!("venue"));
    assert!(client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("RememberedChoice"));
    refresh(
        &client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let removed = wait_effective(&mut client, &document, "dictionary", Value::Null);
    assert!(removed["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| {
            warning["severity"] == "warning"
                && warning["message"].as_str().unwrap().contains("venue")
        }));
    assert!(!client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("RememberedChoice"));
    refresh(&client, settings);
    wait_effective(&mut client, &document, "dictionary", Value::Null);
    assert!(!client
        .hover(&document, 0, 16)
        .unwrap()
        .to_string()
        .contains("RememberedChoice"));
    client.shutdown();
}

#[test]
fn the_uri_less_command_cannot_select_a_project_only_name() {
    let fixture = Workspace::new("overrides");
    fixture.write("dict.xml", &dictionary_xml("ProjectOnly"));
    fixture.write(
        ".fix-lsp.toml",
        "[[dictionaries]]\nname = 'project-only'\npath = 'dict.xml'\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"allowDictionaryPath":true,"diagnostics":{"enabled":false}}),
    );
    let document = fixture.uri("orders.fix");
    client.did_open(&document, MESSAGE, 1);
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.useDictionary","arguments":["project-only"]})
        ),
        Err(REQUEST_FAILED)
    );
    assert!(client.describe(&document).is_ok());
    client.shutdown();
}

#[test]
fn dictionary_version_mismatch_uses_normal_rule_gates_and_one_finding_per_version() {
    let fixture = Workspace::new("overrides");
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!("# fix: dictionary=FIX.4.2\n{MESSAGE}\n{MESSAGE}"),
        1,
    );
    let report = client.wait_for("textDocument/publishDiagnostics");
    let mismatches: Vec<_> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["code"] == "dictionaryVersion")
        .collect();
    assert_eq!(mismatches.len(), 1, "{report}");
    assert_eq!(mismatches[0]["severity"], json!(2));
    client.did_change_full(&document, &format!("# fix: dictionary=FIX.4.2 diagnostics.rules.dictionaryVersion=false\n{MESSAGE}\n{MESSAGE}"), 2);
    wait_effective(
        &mut client,
        &document,
        "diagnostics.rules.dictionaryVersion.enabled",
        json!(false),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let report = client.wait_for("textDocument/publishDiagnostics");
        if report["version"] == 2
            && !report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["code"] == "dictionaryVersion")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mismatch diagnostic did not clear"
        );
    }
    client.shutdown();
}

#[test]
fn all_layers_merge_sparsely_and_describe_reports_effective_values() {
    let fixture = Workspace::new("overrides");
    let user_file = fixture.write(
        "personal/fix-lsp.toml",
        "hover.enabled = false\ninlayHints.tagNames.enabled = true\ndiagnostics.enabled = false\n",
    );
    fixture.write(
        ".fix-lsp.toml",
        "hover.enabled = false\ninlayHints.enumValues.enabled = true\nformatter.width = 80\n",
    );
    fixture.write(
        "logs/.fix-lsp.toml",
        "inlayHints.tagNames.enabled = false\n",
    );
    let first = fixture.uri("logs/one.fix");
    let second = fixture.uri("logs/two.fix");
    let mut client = TestClient::start_with_user_file(Some(user_file));
    let initialized = fixture.initialize(&mut client, json!({"hover":{"enabled":true}}));
    assert_eq!(
        initialized["capabilities"]["executeCommandProvider"]["commands"],
        json!([
            "fix.decode",
            "fix.useDictionary",
            "fix.describeOverrides",
            "fix.describeTag"
        ])
    );
    client.did_open(&first, &format!("# fix: hover.enabled=true\n{MESSAGE}"), 1);
    client.did_open(&second, MESSAGE, 1);
    let first_report = client.describe(&first).unwrap();
    let second_report = client.describe(&second).unwrap();
    assert_eq!(first_report["effective"]["hover.enabled"], json!(true));
    assert_eq!(second_report["effective"]["hover.enabled"], json!(false));
    assert_eq!(
        first_report["effective"]["inlayHints.tagNames.enabled"],
        json!(false)
    );
    assert_eq!(
        first_report["effective"]["inlayHints.enumValues.enabled"],
        json!(true)
    );
    assert_eq!(
        first_report["effective"]["diagnostics.enabled"],
        json!(false)
    );
    assert_eq!(
        first_report["effective"]["allowDictionaryPath"],
        json!(false)
    );
    assert_eq!(
        first_report["sources"]["hover.enabled"]["layer"],
        json!("modeline")
    );
    assert!(!first_report.to_string().contains("formatter"));
    assert!(!client.hover(&first, 1, 12).unwrap().is_null());
    assert!(client.hover(&second, 0, 12).unwrap().is_null());
    client.shutdown();
}

#[test]
fn removing_a_client_leaf_reveals_personal_defaults_without_erasing_other_values() {
    let fixture = Workspace::new("overrides");
    let user_file = fixture.write(
        "personal/fix-lsp.toml",
        "hover.enabled = false\ninlayHints.tagNames.enabled = true\ndiagnostics.enabled = false\n",
    );
    let mut client = TestClient::start_with_user_file(Some(user_file));
    fixture.initialize(&mut client, json!({"hover":{"enabled":true}}));
    let document = fixture.uri("orders.fix");
    client.did_open(&document, MESSAGE, 1);
    assert_eq!(
        client.describe(&document).unwrap()["effective"]["hover.enabled"],
        json!(true)
    );
    refresh(&client, json!({}));
    let report = wait_effective(&mut client, &document, "hover.enabled", json!(false));
    assert_eq!(
        report["effective"]["inlayHints.tagNames.enabled"],
        json!(true)
    );
    assert_eq!(
        report["sources"]["hover.enabled"]["layer"],
        json!("userFile")
    );
    client.shutdown();
}

#[test]
fn describe_does_not_reload_edited_user_or_project_toml() {
    let fixture = Workspace::new("overrides");
    let user_file = fixture.write(
        "personal/fix-lsp.toml",
        "inlayHints.tagNames.enabled = true\ndiagnostics.enabled = false\n",
    );
    let project_file = fixture.write(".fix-lsp.toml", "hover.enabled = false\n");
    let mut client = TestClient::start_with_user_file(Some(user_file.clone()));
    fixture.initialize(&mut client, json!({}));
    let document = fixture.uri("orders.fix");
    let before = client.did_open(&document, MESSAGE, 1);
    fs::write(
        &user_file,
        "inlayHints.tagNames.enabled = false\ndiagnostics.enabled = false\n",
    )
    .unwrap();
    fs::write(&project_file, "hover.enabled = true\n").unwrap();
    assert_eq!(client.describe(&document).unwrap(), before);
    refresh(&client, json!({}));
    let after = wait_effective(&mut client, &document, "hover.enabled", json!(true));
    assert_eq!(
        after["effective"]["inlayHints.tagNames.enabled"],
        json!(false)
    );
    client.shutdown();
}

#[test]
fn modeline_only_refresh_reuses_the_committed_project_snapshot() {
    let fixture = Workspace::new("modeline-project-snapshot");
    let project_file = fixture.write(".fix-lsp.toml", "hover.enabled = false\n");
    let mut client = fixture.client(json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(&document, MESSAGE, 1);
    assert_eq!(
        client.describe(&document).unwrap()["effective"]["hover.enabled"],
        false
    );
    fs::write(&project_file, "hover.enabled = true\n").unwrap();
    client.did_change_full(
        &document,
        &format!("# fix: semanticTokens.enabled=false\n{MESSAGE}"),
        2,
    );
    let report = wait_effective(
        &mut client,
        &document,
        "semanticTokens.enabled",
        json!(false),
    );
    assert_eq!(report["effective"]["hover.enabled"], false);
    client.shutdown();
}

#[test]
fn project_filters_do_not_stop_ancestor_or_child_merging() {
    let fixture = Workspace::new("overrides");
    fixture.write(".fix-lsp.toml", "inlayHints.tagNames.enabled = true\n");
    fixture.write(
        "logs/.fix-lsp.toml",
        "files = ['other/*.fix']\nhover.enabled = false\n",
    );
    fixture.write(
        "logs/venue/.fix-lsp.toml",
        "inlayHints.enumValues.enabled = true\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("logs/venue/orders.fix");
    let report = client.did_open(&document, MESSAGE, 1);
    assert_eq!(report["effective"]["hover.enabled"], json!(true));
    assert_eq!(
        report["effective"]["inlayHints.tagNames.enabled"],
        json!(true)
    );
    assert_eq!(
        report["effective"]["inlayHints.enumValues.enabled"],
        json!(true)
    );
    client.shutdown();
}

#[test]
fn duplicate_modeline_settings_fall_through_and_repair_clears_errors_with_rules_off() {
    let fixture = Workspace::new("overrides");
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!("# fix: hover.enabled=false\n# fix: hover.enabled=false\n{MESSAGE}"),
        1,
    );
    let report = client.describe(&document).unwrap();
    assert_eq!(report["effective"]["hover.enabled"], json!(true));
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["severity"] == "error"
            && warning["message"].as_str().unwrap().contains("duplicate")));
    let diagnostic = client.wait_for("textDocument/publishDiagnostics");
    assert!(diagnostic["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["code"] == "modeline" && entry["severity"] == 1));
    client.did_change_full(
        &document,
        &format!("# fix: hover.enabled=false\n{MESSAGE}"),
        2,
    );
    let report = wait_effective(&mut client, &document, "hover.enabled", json!(false));
    assert!(report["warnings"].as_array().unwrap().is_empty());
    let cleared = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(cleared["version"], json!(2));
    assert!(cleared["diagnostics"].as_array().unwrap().is_empty());
    client.shutdown();
}

#[test]
fn duplicate_toml_keys_reject_one_file_and_keep_valid_parent_values() {
    let fixture = Workspace::new("overrides");
    fixture.write(".fix-lsp.toml", "hover.enabled = false\n");
    fixture.write(
        "logs/.fix-lsp.toml",
        "hover.enabled = true\nhover.enabled = true\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("logs/orders.fix");
    let report = client.did_open(&document, MESSAGE, 1);
    assert_eq!(report["effective"]["hover.enabled"], json!(false));
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["severity"] == "error"));
    client.shutdown();
}

#[test]
fn document_sources_cannot_enable_filesystem_dictionaries_or_change_global_url() {
    let fixture = Workspace::new("overrides");
    fixture.write(
        ".fix-lsp.toml",
        "allowDictionaryPath = true\ndocumentLink.url = 'https://project.invalid/{tag}'\n",
    );
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!("# fix: allowDictionaryPath=true dictionary=./missing.xml\n{MESSAGE}"),
        1,
    );
    let report = client.describe(&document).unwrap();
    assert_eq!(report["effective"]["allowDictionaryPath"], json!(false));
    assert!(
        report["effective"]["dictionary"].is_null(),
        "a refused selector is not effective"
    );
    assert!(report["effective"]["documentLink.url"]
        .as_str()
        .unwrap()
        .contains("onixs.biz"));
    assert!(!report["warnings"].as_array().unwrap().is_empty());
    client.shutdown();
}

#[test]
fn disabling_file_sources_removes_their_values_and_configuration_warnings() {
    let fixture = Workspace::new("overrides");
    fixture.write(".fix-lsp.toml", "hover.enabled = false\n");
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    let document = fixture.uri("orders.fix");
    client.did_open(
        &document,
        &format!("# fix: semanticTokens.enabled=false unknown.option=true\n{MESSAGE}"),
        1,
    );
    let before = client.describe(&document).unwrap();
    assert_eq!(before["effective"]["hover.enabled"], json!(false));
    assert_eq!(before["effective"]["semanticTokens.enabled"], json!(false));
    refresh(
        &client,
        json!({"diagnostics":{"enabled":false},"overrides":{"modeline":false,"projectConfig":false}}),
    );
    let after = wait_effective(&mut client, &document, "hover.enabled", json!(true));
    assert_eq!(after["effective"]["semanticTokens.enabled"], json!(true));
    assert!(after["warnings"].as_array().unwrap().is_empty());
    client.shutdown();
}

#[test]
fn non_file_documents_ignore_modelines_and_scoped_client_objects() {
    let fixture = Workspace::new("overrides");
    let mut client = TestClient::start();
    fixture.initialize(
        &mut client,
        json!({"diagnostics":{"enabled":false},"documentOverrides":{"hover":{"enabled":false}}}),
    );
    let document = "untitled:orders.fix";
    client.did_open(
        document,
        &format!("# fix: hover.enabled=false\n{MESSAGE}"),
        1,
    );
    assert_eq!(
        client.describe(document).unwrap()["effective"]["hover.enabled"],
        json!(true)
    );
    client.shutdown();
}

#[test]
fn describe_validates_arguments_and_open_document_lifetime() {
    let fixture = Workspace::new("overrides");
    let mut client = TestClient::start();
    fixture.initialize(&mut client, json!({"diagnostics":{"enabled":false}}));
    assert_eq!(
        client.request(
            "workspace/executeCommand",
            json!({"command":"fix.describeOverrides","arguments":[]})
        ),
        Err(INVALID_PARAMS)
    );
    let document = fixture.uri("orders.fix");
    assert_eq!(client.describe(&document), Err(REQUEST_FAILED));
    client.did_open(&document, MESSAGE, 1);
    assert!(client.describe(&document).is_ok());
    client.did_close(&document);
    assert_eq!(client.describe(&document), Err(REQUEST_FAILED));
    client.shutdown();
}

#[test]
fn an_open_during_a_rejected_pull_waits_for_a_later_valid_update() {
    let fixture = Workspace::new("overrides");
    let mut client = TestClient::start();
    client.initialize_with(
        json!({"rootUri":file_uri(&fixture.0),"capabilities":TestClient::caps_with_pull()}),
    );
    let (request, method, params) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    assert_eq!(params, json!({"items":[{"section":"fix"}]}));
    let document = fixture.uri("orders.fix");
    client.did_open_unready(&document, MESSAGE, 1);
    assert_eq!(client.describe(&document), Err(REQUEST_FAILED));
    client.reply(request, json!([{"hover":{"enabled":12}}]));
    client.wait_for_log("configuration rejected");
    assert_eq!(client.describe(&document), Err(REQUEST_FAILED));
    client.did_change_full(
        &document,
        &format!("# fix: hover.enabled=false\n{MESSAGE}"),
        2,
    );
    client.notify("workspace/didChangeConfiguration", json!({"settings":null}));
    let (request, method, params) = client.recv_server_request();
    assert_eq!(method, "workspace/configuration");
    assert_eq!(params, json!({"items":[{"section":"fix"}]}));
    client.reply(request, json!([{"diagnostics":{"enabled":false}}]));
    let report = client.wait_ready(&document);
    assert_eq!(report["effective"]["hover.enabled"], json!(false));
    client.shutdown();
}
