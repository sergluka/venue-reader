//! Synthetic public FIX patterns, observed through a generic LSP client.

mod harness;

use harness::{frame, hint_labels, TestClient, Workspace};
use serde_json::{json, Value};

const URI: &str = "file:///fix-versions.fix";

fn open(workspace: &Workspace, mut settings: Value, text: &str) -> TestClient {
    settings["inlayHints"] = json!({
        "tagNames": {"enabled": true},
        "enumValues": {"enabled": true},
        "groupPaths": {"enabled": true}
    });
    let mut client = workspace.client(settings);
    client.did_open(URI, text, 1);
    client
}

fn hover(client: &mut TestClient, text: &str, line: u32, field: &str) -> String {
    let contents = text.lines().nth(line as usize).expect("line");
    client
        .hover_markdown(URI, line, contents, field)
        .unwrap_or_default()
}

fn labels(client: &mut TestClient, text: &str, line: u32) -> Vec<String> {
    let end = text.lines().nth(line as usize).expect("line").len();
    let result = client
        .request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": URI},
                "range": {"start": {"line": line, "character": 0},
                          "end": {"line": line, "character": end}}
            }),
        )
        .expect("inlay hints");
    hint_labels(result.as_array().expect("hint array"))
}

fn diagnostics(client: &mut TestClient) -> Vec<Value> {
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["uri"], URI);
    assert_eq!(published["version"], 1);
    published["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .clone()
}

fn codes_on(diagnostics: &[Value], line: u32) -> Vec<&str> {
    let mut codes: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic["range"]["start"]["line"] == line)
        .map(|diagnostic| diagnostic["code"].as_str().expect("code"))
        .collect();
    codes.sort_unstable();
    codes
}

fn select(client: &mut TestClient, name: &str) -> Value {
    let result = client
        .request(
            "workspace/executeCommand",
            json!({
                "command": "fix.useDictionary", "arguments": [name]
            }),
        )
        .expect("registered selection");
    client.wait_ready(URI);
    result
}

#[test]
fn three_fix5_releases_resolve_independently_in_one_document() {
    let workspace = Workspace::new("version-releases");
    let releases = [("7", "5.0"), ("8", "5.0.SP1"), ("9", "5.0.SP2")];
    let text = releases
        .iter()
        .map(|(wire, _)| {
            frame(
                "FIXT.1.1",
                &format!("35=D|1128={wire}|49=SYNTHETIC_SENDER|54=1|"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut client = open(&workspace, json!({}), &text);
    assert!(diagnostics(&mut client).is_empty());
    for (line, (_, version)) in releases.iter().enumerate() {
        let line = line as u32;
        let side = hover(&mut client, &text, line, "54=");
        assert!(side.contains("**Buy**"), "{side}");
        assert!(
            side.contains(&format!("/fix-dictionary/{version}/tagNum_54.html")),
            "{side}"
        );
        let sender = hover(&mut client, &text, line, "49=");
        assert!(
            sender.contains("/fix-dictionary/fixt1.1/tagNum_49.html"),
            "{sender}"
        );
        let message = hover(&mut client, &text, line, "35=");
        assert!(message.contains("**NewOrderSingle**"), "{message}");
        assert!(
            message.contains("message") && message.contains("order"),
            "{message}"
        );
        let shown = labels(&mut client, &text, line);
        assert!(shown.contains(&"(Buy)".into()), "{shown:?}");
        assert!(shown.contains(&"(NewOrderSingle)".into()), "{shown:?}");
    }
    client.shutdown();
}

#[test]
fn conventional_and_fixt_messages_can_share_a_document() {
    let workspace = Workspace::new("version-mixed");
    let mut lines: Vec<_> = ["FIX.4.0", "FIX.4.1", "FIX.4.2", "FIX.4.3", "FIX.4.4"]
        .into_iter()
        .map(|version| frame(version, "35=D|54=1|"))
        .collect();
    lines.push(frame("FIXT.1.1", "35=D|1128=6|54=1|"));
    lines.push(frame("FIXT.1.1", "35=0|49=SYNTHETIC_SENDER|"));
    let text = lines.join("\n");
    let mut client = open(&workspace, json!({}), &text);
    assert!(diagnostics(&mut client).is_empty());
    for (line, version) in ["4.0", "4.1", "4.2", "4.3", "4.4", "4.4"]
        .iter()
        .enumerate()
    {
        let side = hover(&mut client, &text, line as u32, "54=");
        assert!(side.contains("**Buy**"), "{side}");
        assert!(
            side.contains(&format!("/fix-dictionary/{version}/")),
            "{side}"
        );
    }
    assert!(hover(&mut client, &text, 6, "35=").contains("**Heartbeat**"));
    client.shutdown();
}

#[test]
fn each_bundled_fix4_application_can_decode_over_fixt() {
    let workspace = Workspace::new("version-fixt-legacy");
    let releases = [
        ("2", "4.0"),
        ("3", "4.1"),
        ("4", "4.2"),
        ("5", "4.3"),
        ("6", "4.4"),
    ];
    let text = releases
        .iter()
        .map(|(code, _)| frame("FIXT.1.1", &format!("35=D|1128={code}|54=1|")))
        .collect::<Vec<_>>()
        .join("\n");
    let mut client = open(&workspace, json!({"defaultApplVerID": "FIX.5.0SP2"}), &text);
    assert!(diagnostics(&mut client).is_empty());
    for (line, (_, version)) in releases.iter().enumerate() {
        let side = hover(&mut client, &text, line as u32, "54=");
        assert!(side.contains("**Buy**"), "{side}");
        assert!(
            side.contains(&format!("/{version}/tagNum_54.html")),
            "{side}"
        );
        assert!(labels(&mut client, &text, line as u32).contains(&"(Buy)".into()));
    }
    client.shutdown();
}

#[test]
fn shared_text_outside_message_scope_keeps_application_definition() {
    let workspace = Workspace::new("version-shared-text");
    let text = frame("FIXT.1.1", "35=AE|1128=9|58=SYNTHETIC_REASON|1137=9|");
    let mut client = open(&workspace, json!({}), &text);
    assert!(diagnostics(&mut client).is_empty());
    let description = hover(&mut client, &text, 0, "58=");
    assert!(
        description.contains("**Text** (58) — String"),
        "{description}"
    );
    assert!(
        description.contains("/5.0.SP2/tagNum_58.html"),
        "{description}"
    );
    let shown = labels(&mut client, &text, 0);
    assert!(shown.contains(&"(Text)".into()), "{shown:?}");
    assert!(!shown.contains(&"(Text?)".into()), "{shown:?}");
    let symbols = client
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":URI}}),
        )
        .unwrap();
    assert!(
        symbols.to_string().contains("Text (58) = SYNTHETIC_REASON"),
        "{symbols}"
    );
    let transport = hover(&mut client, &text, 0, "1137=");
    assert!(
        transport.contains("/fixt1.1/tagNum_1137.html"),
        "{transport}"
    );
    client.shutdown();
}

#[test]
fn fixt_without_application_identity_keeps_transport_and_raw_application_values() {
    let workspace = Workspace::new("version-missing");
    let text = [
        frame("FIXT.1.1", "35=D|49=SYNTHETIC_SENDER|54=1|"),
        frame("FIXT.1.1", "35=6|49=SYNTHETIC_SENDER|"),
        frame("FIXT.1.1", "35=5|58=SYNTHETIC_LOGOUT|"),
        frame("FIXT.1.1", "49=SYNTHETIC_SENDER|"),
    ]
    .join("\n");
    let mut client = open(&workspace, json!({}), &text);
    let findings = diagnostics(&mut client);
    assert_eq!(codes_on(&findings, 0), ["unknownVersion"]);
    assert_eq!(codes_on(&findings, 1), ["unknownVersion"]);
    assert!(codes_on(&findings, 2).is_empty());
    assert_eq!(codes_on(&findings, 3), ["missingMsgType", "unknownVersion"]);
    for (line, message_context) in [
        (0, " for MsgType(35)=D"),
        (1, " for MsgType(35)=6"),
        (3, ""),
    ] {
        let diagnostic = findings
            .iter()
            .find(|diagnostic| {
                diagnostic["range"]["start"]["line"] == line
                    && diagnostic["code"] == "unknownVersion"
            })
            .unwrap();
        assert_eq!(
            diagnostic["message"],
            format!("Missing application version{message_context}. Set ApplVerID(1128), defaultApplVerID, or select an application dictionary.")
        );
    }
    assert!(hover(&mut client, &text, 0, "49=").contains("fixt1.1"));
    let side = hover(&mut client, &text, 0, "54=");
    assert!(side.contains("Value: `1`"), "{side}");
    assert!(!side.contains("**Buy**"), "{side}");
    assert!(!side.contains("[FIX Reference"), "{side}");
    assert!(!labels(&mut client, &text, 0).contains(&"(Buy)".into()));
    client.shutdown();
}

#[test]
fn explicit_default_and_selected_pair_have_the_documented_precedence() {
    let workspace = Workspace::new("version-defaults");
    let text = [
        frame("FIXT.1.1", "35=D|54=1|"),
        frame("FIXT.1.1", "35=D|1128=7|54=1|"),
    ]
    .join("\n");
    let mut client = open(&workspace, json!({"defaultApplVerID": "FIX.5.0SP1"}), &text);
    let findings = diagnostics(&mut client);
    assert!(findings.is_empty(), "{findings:?}");
    assert!(hover(&mut client, &text, 0, "54=").contains("/5.0.SP1/"));
    assert!(hover(&mut client, &text, 1, "54=").contains("/5.0/"));
    select(&mut client, "FIX.5.0SP2");
    client.wait_effective(URI, "dictionary", json!("FIX.5.0SP2"));
    assert!(hover(&mut client, &text, 0, "54=").contains("/5.0.SP2/"));
    assert!(hover(&mut client, &text, 1, "54=").contains("/5.0/"));
    client.shutdown();
}

#[test]
fn fix5_dictionary_names_are_rejected_as_wire_begin_strings_even_when_selected() {
    let workspace = Workspace::new("version-wire");
    let text = ["FIX.5.0", "FIX.5.0SP1", "FIX.5.0SP2"]
        .into_iter()
        .map(|version| frame(version, "35=D|54=1|"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut client = open(&workspace, json!({}), &text);
    let findings = diagnostics(&mut client);
    select(&mut client, "FIX.5.0SP2");
    for line in 0..3 {
        assert_eq!(codes_on(&findings, line), ["unknownVersion"]);
        assert!(!hover(&mut client, &text, line, "54=").contains("**Buy**"));
        assert!(!labels(&mut client, &text, line).contains(&"(Buy)".into()));
    }
    client.shutdown();
}

#[test]
fn missing_begin_string_stays_raw_under_modeline_and_session_selection() {
    let workspace = Workspace::new("version-fragments");
    let text = "# fix: dictionary=FIX.4.4\n35=D|54=1|";
    let mut client = open(&workspace, json!({}), text);
    select(&mut client, "FIX.5.0SP2");
    assert_eq!(
        codes_on(&diagnostics(&mut client), 1),
        ["beginString", "bodyLength", "checkSum"]
    );
    let side = hover(&mut client, text, 1, "54=");
    assert!(side.contains("Value: `1`"), "{side}");
    assert!(!side.contains("**Buy**"), "{side}");
    assert!(!side.contains("[FIX Reference"), "{side}");
    assert!(!labels(&mut client, text, 1).contains(&"(Buy)".into()));
    client.shutdown();
}

#[test]
fn integrity_is_checked_when_application_or_transport_cannot_resolve() {
    let workspace = Workspace::new("version-integrity");
    let text = [
        "8=FIXT.1.1|9=999|35=D|54=1|10=999|",
        "8=FIX.5.0SP2|9=999|35=D|54=1|10=999|",
    ]
    .join("\n");
    let mut client = open(&workspace, json!({}), &text);
    let findings = diagnostics(&mut client);
    for line in 0..2 {
        assert_eq!(
            codes_on(&findings, line),
            ["bodyLength", "checkSum", "unknownVersion"]
        );
    }
    client.shutdown();
}

#[test]
fn logon_defaults_never_influence_another_line_or_document() {
    let workspace = Workspace::new("version-logon");
    let text = [
        frame("FIXT.1.1", "35=A|98=0|108=30|1137=9|"),
        frame("FIXT.1.1", "35=D|54=1|"),
        frame("FIXT.1.1", "35=D|1128=8|54=1|"),
    ]
    .join("\n");
    let mut client = open(&workspace, json!({}), &text);
    let findings = diagnostics(&mut client);
    assert_eq!(codes_on(&findings, 0), Vec::<&str>::new());
    assert_eq!(codes_on(&findings, 1), ["unknownVersion"]);
    assert_eq!(codes_on(&findings, 2), Vec::<&str>::new());
    assert!(hover(&mut client, &text, 0, "35=").contains("**Logon**"));
    assert!(!hover(&mut client, &text, 1, "54=").contains("**Buy**"));
    assert!(hover(&mut client, &text, 2, "54=").contains("/5.0.SP1/"));
    let other = workspace.uri("other.fix");
    let application = frame("FIXT.1.1", "35=D|54=1|");
    client.did_open(&other, &application, 1);
    let published = client.wait_for("textDocument/publishDiagnostics");
    assert_eq!(published["uri"], other);
    assert_eq!(published["diagnostics"][0]["code"], "unknownVersion");
    let column = application.find("54=").unwrap() as u32;
    let answer = client.hover(&other, 0, column).unwrap();
    assert!(!answer["contents"]["value"]
        .as_str()
        .unwrap()
        .contains("**Buy**"));
    client.shutdown();
}

#[test]
fn prohibited_application_selectors_do_not_disable_administrative_decoding() {
    let workspace = Workspace::new("version-admin");
    let text = frame("FIXT.1.1", "35=A|1128=9|98=0|108=30|1137=9|");
    let mut client = open(&workspace, json!({}), &text);
    assert_eq!(codes_on(&diagnostics(&mut client), 0), ["unknownVersion"]);
    assert!(hover(&mut client, &text, 0, "35=").contains("**Logon**"));
    assert!(hover(&mut client, &text, 0, "98=").contains("**None"));
    assert!(labels(&mut client, &text, 0).contains(&"(Logon)".into()));
    client.shutdown();
}

#[test]
fn unregistered_release_extension_and_custom_identity_are_never_downgraded() {
    let workspace = Workspace::new("version-exact");
    let bodies = [
        "35=D|1128=0|54=1|",
        "35=D|1128=1|54=1|",
        "35=D|1128=10|54=1|",
        "35=D|1128=99|54=1|",
        "35=D|1128=9|1156=0|54=1|",
        "35=D|1128=9|1156=999|54=1|",
        "35=D|1128=9|1129=SYNTHETIC_PROFILE|54=1|",
    ];
    let text = bodies
        .iter()
        .copied()
        .chain(["35=D|54=1|"])
        .map(|body| frame("FIXT.1.1", body))
        .collect::<Vec<_>>()
        .join("\n");
    for settings in [json!({}), json!({"defaultApplVerID": "FIX.5.0SP2"})] {
        let has_default = settings.get("defaultApplVerID").is_some();
        let mut client = open(&workspace, settings, &text);
        let findings = diagnostics(&mut client);
        for line in 0..bodies.len() as u32 {
            assert_eq!(codes_on(&findings, line), ["unknownVersion"]);
            assert!(!hover(&mut client, &text, line, "54=").contains("**Buy**"));
            assert!(!labels(&mut client, &text, line).contains(&"(Buy)".into()));
        }
        let control = bodies.len() as u32;
        let side = hover(&mut client, &text, control, "54=");
        assert_eq!(side.contains("**Buy**"), has_default, "{side}");
        if has_default {
            assert!(codes_on(&findings, control).is_empty());
            assert!(side.contains("/5.0.SP2/"), "{side}");
        } else {
            assert_eq!(codes_on(&findings, control), ["unknownVersion"]);
        }
        client.shutdown();
    }
}

#[test]
fn identical_repeated_selectors_are_rejected_without_using_a_default() {
    let workspace = Workspace::new("version-duplicates");
    let text = [
        frame("FIXT.1.1", "35=D|1128=9|1128=9|54=1|"),
        frame("FIX.4.4", "35=D|35=D|54=1|"),
    ]
    .join("\n");
    let mut client = open(&workspace, json!({"defaultApplVerID": "FIX.5.0SP2"}), &text);
    let findings = diagnostics(&mut client);
    for line in 0..2 {
        assert_eq!(codes_on(&findings, line), ["unknownVersion"]);
        assert!(findings
            .iter()
            .any(|diagnostic| diagnostic["range"]["start"]["line"] == line
                && diagnostic["message"]
                    .as_str()
                    .unwrap()
                    .contains("Duplicate")));
        assert!(!hover(&mut client, &text, line, "54=").contains("**Buy**"));
    }
    client.shutdown();
}

// Deliberately different enums/types for shared tags expose a source-index mixup.
// Group shapes follow public NoHops(627) and NoPartyIDs(453) patterns.
const TRANSPORT: &str = r#"<fix type="FIXT" major="1" minor="1">
<header><field name="BeginString"/><field name="BodyLength"/><field name="MsgType"/>
<field name="ApplVerID"/><field name="SenderCompID"/>
<group name="NoHops"><field name="HopCompID"/><field name="HopRefID"/></group></header>
<trailer><field name="CheckSum"/></trailer>
<messages><message name="SyntheticHeartbeat" msgtype="0" msgcat="admin"/></messages>
<fields><field number="8" name="BeginString" type="STRING"/>
<field number="9" name="BodyLength" type="LENGTH"/>
<field number="10" name="CheckSum" type="STRING"/>
<field number="35" name="MsgType" type="STRING"><value enum="D" description="SYNTHETIC_TRANSPORT_D"/></field>
<field number="49" name="SenderCompID" type="STRING"><value enum="X" description="SYNTHETIC_TRANSPORT"/></field>
<field number="54" name="Side" type="INT"><value enum="1" description="SYNTHETIC_WRONG_OWNER"/></field>
<field number="627" name="NoHops" type="NUMINGROUP"/>
<field number="628" name="HopCompID" type="STRING"><value enum="X" description="SYNTHETIC_HOP"/></field>
<field number="630" name="HopRefID" type="SEQNUM"/>
<field number="1128" name="ApplVerID" type="STRING"/></fields></fix>"#;

const APPLICATION: &str = r#"<fix type="FIX" major="5" minor="0" servicepack="2">
<messages><message name="SyntheticOrder" msgtype="D" msgcat="app">
<field name="Side"/><group name="NoPartyIDs"><field name="PartyID"/><field name="Side"/>
<group name="NoPartySubIDs"><field name="PartySubID"/></group></group></message></messages>
<fields><field number="35" name="MsgType" type="STRING"/>
<field number="49" name="SenderCompID" type="INT"><value enum="X" description="SYNTHETIC_WRONG_OWNER"/></field>
<field number="54" name="Side" type="CHAR"><value enum="1" description="SYNTHETIC_APPLICATION"/></field>
<field number="448" name="PartyID" type="STRING"><value enum="X" description="SYNTHETIC_PARTY"/></field>
<field number="453" name="NoPartyIDs" type="NUMINGROUP"/>
<field number="523" name="PartySubID" type="STRING"/>
<field number="628" name="HopCompID" type="INT"><value enum="X" description="SYNTHETIC_WRONG_OWNER"/></field>
<field number="802" name="NoPartySubIDs" type="NUMINGROUP"/></fields></fix>"#;

fn pair(workspace: &Workspace) -> Value {
    json!({"allowDictionaryPath": true, "dictionaries": [{
        "name": "synthetic-pair", "transport": workspace.write("transport.xml", TRANSPORT),
        "application": workspace.write("application.xml", APPLICATION)
    }]})
}

#[test]
fn shared_scalar_outside_scopes_uses_the_application_type_and_enum() {
    let workspace = Workspace::new("version-shared-scalar");
    let application = APPLICATION.replace("<field name=\"Side\"/><group", "<group");
    let settings = json!({"allowDictionaryPath":true, "dictionaries":[{
        "name":"synthetic-pair", "transport":workspace.write("transport.xml", TRANSPORT),
        "application":workspace.write("application.xml", &application)
    }]});
    let text = format!(
        "# fix: dictionary=synthetic-pair\n{}",
        frame("FIXT.1.1", "35=D|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    assert!(diagnostics(&mut client).is_empty());
    let description = hover(&mut client, &text, 1, "54=");
    assert!(description.contains(" — CHAR\n"), "{description}");
    assert!(
        description.contains("SYNTHETIC_APPLICATION"),
        "{description}"
    );
    assert!(
        !description.contains("SYNTHETIC_WRONG_OWNER"),
        "{description}"
    );
    assert!(labels(&mut client, &text, 1).contains(&"(SYNTHETIC_APPLICATION)".into()));
    client.shutdown();
}

#[test]
fn fix5_path_shorthand_decodes_application_and_bundled_logon() {
    let workspace = Workspace::new("version-path-shorthand");
    let settings = json!({"allowDictionaryPath":true, "dictionaries":[{
        "name":"synthetic-path", "path":workspace.write("application.xml", APPLICATION)
    }]});
    let text = format!(
        "# fix: dictionary=synthetic-path\n{}\n{}",
        frame("FIXT.1.1", "35=A|98=0|108=30|1137=9|"),
        frame("FIXT.1.1", "35=D|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    assert!(diagnostics(&mut client).is_empty());
    let logon = hover(&mut client, &text, 1, "35=");
    assert!(logon.contains("**Logon**"), "{logon}");
    let encryption = hover(&mut client, &text, 1, "98=");
    assert!(
        encryption.contains("**None**") && encryption.contains("None / Other"),
        "{encryption}"
    );
    assert!(
        encryption.contains("/fixt1.1/tagNum_98.html"),
        "{encryption}"
    );
    assert!(hover(&mut client, &text, 2, "54=").contains("SYNTHETIC_APPLICATION"));
    assert!(labels(&mut client, &text, 2).contains(&"(SyntheticOrder)".into()));
    client.shutdown();
}

#[test]
fn combined_fixt_path_decodes_both_roles_using_declared_application_identity() {
    let workspace = Workspace::new("version-combined-path");
    let combined = TRANSPORT.replace(
        "<message name=\"SyntheticHeartbeat\" msgtype=\"0\" msgcat=\"admin\"/>",
        "<message name=\"SyntheticHeartbeat\" msgtype=\"0\" msgcat=\"admin\"/>\
         <message name=\"SyntheticOrder\" msgtype=\"D\" msgcat=\"app\"><field name=\"Side\"/></message>",
    ).replace("SYNTHETIC_WRONG_OWNER", "SYNTHETIC_COMBINED_SIDE");
    let settings = json!({"allowDictionaryPath":true, "dictionaries":[{
        "name":"synthetic-combined", "path":workspace.write("combined.xml", &combined),
        "applicationVersion":"FIX.5.0SP2"
    }]});
    let text = format!(
        "# fix: dictionary=synthetic-combined\n{}\n{}",
        frame("FIXT.1.1", "35=0|49=X|"),
        frame("FIXT.1.1", "35=D|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    assert!(diagnostics(&mut client).is_empty());
    assert!(hover(&mut client, &text, 1, "35=").contains("**SyntheticHeartbeat**"));
    let sender = hover(&mut client, &text, 1, "49=");
    assert!(
        sender.contains("SYNTHETIC_TRANSPORT") && sender.contains("/fixt1.1/"),
        "{sender}"
    );
    let side = hover(&mut client, &text, 2, "54=");
    assert!(
        side.contains(" — INT\n") && side.contains("/5.0.SP2/tagNum_54.html"),
        "{side}"
    );
    assert!(side.contains("SYNTHETIC_COMBINED_SIDE"), "{side}");
    assert!(hover(&mut client, &text, 2, "35=").contains("**SyntheticOrder**"));
    assert!(labels(&mut client, &text, 2).contains(&"(SYNTHETIC_COMBINED_SIDE)".into()));
    client.shutdown();
}

#[test]
fn complete_pair_owns_types_enums_messages_and_nested_groups() {
    let workspace = Workspace::new("version-owned");
    let text = format!(
        "# fix: dictionary=synthetic-pair\n{}",
        frame(
            "FIXT.1.1",
            "35=D|49=X|627=1|628=X|630=1|54=1|453=1|448=X|54=1|802=1|523=SYNTHETIC_SUB|"
        )
    );
    let mut client = open(&workspace, pair(&workspace), &text);
    assert!(diagnostics(&mut client).is_empty());
    for (field, expected, datatype) in [
        ("49=", "SYNTHETIC_TRANSPORT", "STRING"),
        ("54=", "SYNTHETIC_APPLICATION", "CHAR"),
        ("628=", "SYNTHETIC_HOP", "STRING"),
        ("448=", "SYNTHETIC_PARTY", "STRING"),
    ] {
        let description = hover(&mut client, &text, 1, field);
        assert!(description.contains(expected), "{description}");
        assert!(
            description.contains(&format!(" — {datatype}\n")),
            "{description}"
        );
        assert!(
            !description.contains("SYNTHETIC_WRONG_OWNER"),
            "{description}"
        );
    }
    let description = hover(&mut client, &text, 1, "35=");
    assert!(description.contains("**SyntheticOrder**"), "{description}");
    assert!(
        !description.contains("SYNTHETIC_TRANSPORT_D"),
        "{description}"
    );
    let description = hover(&mut client, &text, 1, "523=");
    assert!(
        description.contains("Group: `NoPartyIDs/1/NoPartySubIDs/1/PartySubID`"),
        "{description}"
    );
    let shown = labels(&mut client, &text, 1);
    assert_eq!(
        shown
            .iter()
            .filter(|label| *label == "(SYNTHETIC_APPLICATION)")
            .count(),
        2,
        "top-level and grouped Side(54): {shown:?}"
    );
    for expected in [
        "(SYNTHETIC_TRANSPORT)",
        "(SYNTHETIC_APPLICATION)",
        "(NoHops/1/HopCompID)",
        "(NoPartyIDs/1/NoPartySubIDs/1/PartySubID)",
        "(SyntheticOrder)",
    ] {
        assert!(shown.contains(&expected.into()), "{shown:?}");
    }
    client.shutdown();
}

#[test]
fn mismatching_complete_pair_discards_its_transport_and_identity_defaults_atomically() {
    let workspace = Workspace::new("version-atomic");
    let mut settings = pair(&workspace);
    settings["dictionaries"][0]["extensionPack"] = json!(42);
    settings["dictionaries"][0]["customVersion"] = json!("SYNTHETIC_PROFILE");
    let text = format!(
        "# fix: dictionary=synthetic-pair\n{}\n{}",
        frame("FIXT.1.1", "35=D|49=X|54=1|"),
        frame("FIXT.1.1", "35=D|1128=8|49=X|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    let findings = diagnostics(&mut client);
    assert!(codes_on(&findings, 1).is_empty(), "{findings:?}");
    assert_eq!(codes_on(&findings, 2), ["dictionaryVersion"]);
    assert!(hover(&mut client, &text, 1, "54=").contains("SYNTHETIC_APPLICATION"));
    let side = hover(&mut client, &text, 2, "54=");
    assert!(
        side.contains("**Buy**") && side.contains("/5.0.SP1/"),
        "{side}"
    );
    let sender = hover(&mut client, &text, 2, "49=");
    assert!(!sender.contains("SYNTHETIC_TRANSPORT"), "{sender}");
    assert!(sender.contains("/fixt1.1/"), "{sender}");
    let shown = labels(&mut client, &text, 2);
    assert!(shown.contains(&"(Buy)".into()), "{shown:?}");
    assert!(
        !shown.contains(&"(SYNTHETIC_TRANSPORT)".into()),
        "{shown:?}"
    );
    client.shutdown();
}

#[test]
fn transport_family_retains_its_source_while_application_changes_per_message() {
    let workspace = Workspace::new("version-family");
    let settings = json!({"allowDictionaryPath": true, "dictionaries": [{
        "name": "synthetic-family", "transport": workspace.write("transport.xml", TRANSPORT)
    }]});
    let text = format!(
        "# fix: dictionary=synthetic-family\n{}\n{}",
        frame("FIXT.1.1", "35=D|1128=7|49=X|54=1|"),
        frame("FIXT.1.1", "35=D|1128=9|49=X|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    assert!(diagnostics(&mut client).is_empty());
    for (line, version) in [(1, "5.0"), (2, "5.0.SP2")] {
        assert!(hover(&mut client, &text, line, "49=").contains("SYNTHETIC_TRANSPORT"));
        let side = hover(&mut client, &text, line, "54=");
        assert!(
            side.contains("**Buy**") && side.contains(&format!("/{version}/")),
            "{side}"
        );
        assert!(labels(&mut client, &text, line).contains(&"(NewOrderSingle)".into()));
    }
    client.shutdown();
}

#[test]
fn selection_command_reports_exact_application_identity_and_both_source_roles() {
    let workspace = Workspace::new("version-command");
    let mut settings = pair(&workspace);
    settings["dictionaries"][0]["extensionPack"] = json!(42);
    settings["dictionaries"][0]["customVersion"] = json!("SYNTHETIC_PROFILE");
    let transport = workspace.0.join("transport.xml").canonicalize().unwrap();
    let application = workspace.0.join("application.xml").canonicalize().unwrap();
    let text = frame("FIXT.1.1", "35=0|");
    let mut client = open(&workspace, settings, &text);
    assert_eq!(
        select(&mut client, "synthetic-pair"),
        json!({
            "name": "synthetic-pair", "version": "FIX.5.0SP2", "source": application,
            "transport": {"version": "FIXT.1.1", "source": transport},
            "application": {"version": "FIX.5.0SP2", "extensionPack": 42,
                "customVersion": "SYNTHETIC_PROFILE", "source": application}
        })
    );
    let report = client
        .describe(URI)
        .expect("committed dictionary definitions");
    assert_eq!(
        report["dictionaries"],
        json!([{
            "name": "synthetic-pair", "version": "FIX.5.0SP2",
            "path": application, "source": "client",
            "transport": {"version": "FIXT.1.1", "source": transport},
            "application": {"version": "FIX.5.0SP2", "extensionPack": 42,
                "customVersion": "SYNTHETIC_PROFILE", "source": application}
        }])
    );
    assert_eq!(
        select(&mut client, "FIXT.1.1"),
        json!({
            "name": "FIXT.1.1", "version": "FIXT.1.1", "source": "FIXT.1.1",
            "transport": {"version": "FIXT.1.1", "source": "FIXT.1.1"},
            "application": null
        })
    );
    assert_eq!(
        select(&mut client, "FIX.5.0SP2"),
        json!({
            "name": "FIX.5.0SP2", "version": "FIX.5.0SP2", "source": "FIX.5.0SP2",
            "transport": {"version": "FIXT.1.1", "source": "FIXT.1.1"},
            "application": {"version": "FIX.5.0SP2", "extensionPack": null,
                "customVersion": null, "source": "FIX.5.0SP2"}
        })
    );
    client.shutdown();
}

#[test]
fn modern_fix4_application_profile_requires_explicit_selection() {
    let workspace = Workspace::new("version-modern-fix4");
    let latest = APPLICATION.replace(
        r#"<fix type="FIX" major="5" minor="0" servicepack="2">"#,
        r#"<fix version="FIX.Latest">"#,
    );
    let settings = json!({"allowDictionaryPath": true, "dictionaries": [{
        "name": "synthetic-modern", "transport": "FIX.4.4",
        "application": workspace.write("latest.xml", &latest),
        "applicationVersion": "FIX.Latest", "default": true
    }]});
    let text = [
        frame("FIX.4.4", "35=D|49=SYNTHETIC_SENDER|54=1|"),
        frame("FIX.4.4", "35=D|1128=10|49=SYNTHETIC_SENDER|54=1|"),
    ]
    .join("\n");
    let mut client = open(&workspace, settings.clone(), &text);
    let findings = diagnostics(&mut client);
    assert!(codes_on(&findings, 0).is_empty(), "{findings:?}");
    let conventional = hover(&mut client, &text, 0, "54=");
    assert!(
        conventional.contains("**Buy**") && conventional.contains("/4.4/"),
        "{conventional}"
    );
    assert!(labels(&mut client, &text, 0).contains(&"(NewOrderSingle)".into()));
    let unselected = hover(&mut client, &text, 1, "54=");
    let unselected_hints = labels(&mut client, &text, 1);
    assert_eq!(codes_on(&findings, 1), ["unknownVersion"]);
    assert!(unselected.contains("Value: `1`"), "{unselected}");
    assert!(!unselected.contains("Value: `1` —"), "{unselected}");
    assert!(!unselected.contains("**Buy**"), "{unselected}");
    assert!(!unselected.contains("/fix-dictionary/"), "{unselected}");
    assert!(
        !unselected_hints.contains(&"(Buy)".into()),
        "{unselected_hints:?}"
    );
    let unselected_transport = hover(&mut client, &text, 1, "49=");
    assert!(
        unselected_transport.contains("/fix-dictionary/4.4/tagNum_49.html"),
        "{unselected_transport}"
    );
    assert!(
        !unselected.contains("SYNTHETIC_APPLICATION")
            && !unselected_hints.contains(&"(SYNTHETIC_APPLICATION)".into())
            && !unselected_hints.contains(&"(SyntheticOrder)".into()),
        "ApplVerID(1128) = 10 must not select the modern FIX4 profile: \
         hover={unselected}; hints={unselected_hints:?}; diagnostics={findings:?}"
    );
    let conventional_text = format!("# fix: dictionary=FIX.4.4\n{text}");
    let mut conventional_client = open(&workspace, settings, &conventional_text);
    let conventional_findings = diagnostics(&mut conventional_client);
    assert_eq!(
        codes_on(&conventional_findings, 2),
        ["dictionaryVersion", "unknownVersion"]
    );
    let conventional_selection = hover(&mut conventional_client, &conventional_text, 2, "54=");
    assert!(
        !conventional_selection.contains("SYNTHETIC_APPLICATION"),
        "{conventional_selection}"
    );
    assert!(!labels(&mut conventional_client, &conventional_text, 2)
        .contains(&"(SYNTHETIC_APPLICATION)".into()));
    conventional_client.shutdown();
    let selected = select(&mut client, "synthetic-modern");
    assert_eq!(selected["transport"]["version"], "FIX.4.4");
    assert_eq!(selected["application"]["version"], "FIX.Latest");
    client.wait_effective(URI, "dictionary", json!("synthetic-modern"));
    assert!(diagnostics(&mut client).is_empty());
    for line in 0..2 {
        let application = hover(&mut client, &text, line, "54=");
        assert!(
            application.contains("SYNTHETIC_APPLICATION"),
            "{application}"
        );
        assert!(!application.contains("**Buy**"), "{application}");
        let transport = hover(&mut client, &text, line, "49=");
        assert!(
            transport.contains("/fix-dictionary/4.4/tagNum_49.html"),
            "{transport}"
        );
        assert!(!transport.contains("SYNTHETIC_WRONG_OWNER"), "{transport}");
        let shown = labels(&mut client, &text, line);
        assert!(
            shown.contains(&"(SYNTHETIC_APPLICATION)".into()),
            "{shown:?}"
        );
        assert!(shown.contains(&"(SyntheticOrder)".into()), "{shown:?}");
    }
    client.shutdown();
}

#[test]
fn mismatching_modern_fix4_pair_does_not_activate_another_modern_default() {
    let workspace = Workspace::new("version-modern-mismatch");
    let latest = APPLICATION.replace(
        r#"<fix type="FIX" major="5" minor="0" servicepack="2">"#,
        r#"<fix version="FIX.Latest">"#,
    );
    let application = workspace.write("latest.xml", &latest);
    let settings = json!({"allowDictionaryPath": true, "dictionaries": [
        {"name": "synthetic-modern", "transport": "FIX.4.4",
         "application": application, "applicationVersion": "FIX.Latest"},
        {"name": "synthetic-modern-pack", "transport": "FIX.4.4",
         "application": application, "applicationVersion": "FIX.Latest",
         "extensionPack": 42, "default": true}
    ]});
    let text = format!(
        "# fix: dictionary=synthetic-modern\n{}\n{}",
        frame("FIX.4.4", "35=D|1128=10|1156=42|54=1|"),
        frame("FIX.4.4", "35=D|1128=6|54=1|")
    );
    let mut client = open(&workspace, settings, &text);
    let findings = diagnostics(&mut client);
    assert_eq!(
        codes_on(&findings, 1),
        ["dictionaryVersion", "unknownVersion"],
        "{findings:?}"
    );
    let unresolved = hover(&mut client, &text, 1, "54=");
    assert!(
        !unresolved.contains("SYNTHETIC_APPLICATION"),
        "{unresolved}"
    );
    assert!(!labels(&mut client, &text, 1).contains(&"(SYNTHETIC_APPLICATION)".into()));
    assert_eq!(codes_on(&findings, 2), ["dictionaryVersion", "unknownTag"]);
    let fallback = hover(&mut client, &text, 2, "54=");
    assert!(
        fallback.contains("**Buy**") && fallback.contains("/4.4/"),
        "{fallback}"
    );
    client.shutdown();
}

#[test]
fn project_and_modeline_selection_override_the_session_command() {
    let workspace = Workspace::new("version-selection");
    workspace.write(".fix-lsp.toml", "dictionary = \"FIX.5.0SP1\"\n");
    let uri = workspace.uri("selected.fix");
    let body = frame("FIXT.1.1", "35=D|54=1|");
    let mut client = workspace.client(json!({}));
    client.did_open(&uri, &body, 1);
    client
        .request(
            "workspace/executeCommand",
            json!({
                "command": "fix.useDictionary", "arguments": ["FIX.5.0"]
            }),
        )
        .unwrap();
    client.wait_ready(&uri);
    let column = body.find("54=").unwrap() as u32;
    let answer = client.hover(&uri, 0, column).unwrap();
    assert!(answer["contents"]["value"]
        .as_str()
        .unwrap()
        .contains("/5.0.SP1/"));
    let modeline = format!("# fix: dictionary=FIX.5.0SP2\n{body}");
    client.did_change_full(&uri, &modeline, 2);
    client.wait_effective(&uri, "dictionary", json!("FIX.5.0SP2"));
    let answer = client.hover(&uri, 1, column).unwrap();
    assert!(answer["contents"]["value"]
        .as_str()
        .unwrap()
        .contains("/5.0.SP2/"));
    client.shutdown();
}
