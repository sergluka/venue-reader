//! The JSON schemas are written by hand and refuse unknown rule ids, so a rule
//! missing from one of them is a setting an editor would flag as an error.

use fix_lsp::config::{user_file, Severity};
use fix_lsp::overrides::{merge, modeline, parse_client, project, SourceKind};
use fix_lsp::rules::Registry;
use serde_json::{json, Value};
use std::path::Path;

/// The `diagnostics.rules` object, wherever a schema nests it.
fn rules_of(schema: &Value) -> Option<&Value> {
    let properties = schema.get("properties")?;
    if let Some(rules) = properties.pointer("/diagnostics/properties/rules") {
        return Some(rules);
    }
    match schema {
        Value::Object(map) => map.values().find_map(rules_of),
        _ => None,
    }
}

#[test]
fn every_schema_lists_exactly_the_registered_rules() {
    let mut known: Vec<&str> = Registry::known_ids().to_vec();
    known.sort_unstable();
    for file in [
        "schema/fix-lsp.schema.json",
        "schema/fix-lsp-user.schema.json",
        "schema/fix-lsp-project.schema.json",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let schema: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("schema reads"))
                .expect("schema parses");
        let rules = rules_of(&schema).unwrap_or_else(|| panic!("{file}: no diagnostics.rules"));
        let mut listed: Vec<&str> = rules["properties"]
            .as_object()
            .expect("rules lists properties")
            .keys()
            .map(String::as_str)
            .collect();
        listed.sort_unstable();
        assert_eq!(listed, known, "{file}");
        assert_eq!(rules["additionalProperties"], false, "{file}");
    }
}

#[test]
fn begin_string_is_an_unknown_rule_in_every_configuration_source() {
    let toml = "[diagnostics.rules]\nbeginString = false\n[diagnostics.rules.integrity]\nenabled = true\nseverity = 'hint'\n";
    let modeline = b"# fix: diagnostics.rules.beginString=false diagnostics.rules.integrity.enabled=true diagnostics.rules.integrity.severity=hint";
    let layers = [
        (SourceKind::UserFile, user_file::parse(toml, Path::new("config.toml")).unwrap().layer),
        (SourceKind::Client, parse_client(json!({"diagnostics":{"rules":{"beginString":false,"integrity":{"enabled":true,"severity":"hint"}}}})).unwrap()),
        (SourceKind::Project, project::parse(toml, Path::new(".fix-lsp.toml")).unwrap().layer),
        (SourceKind::Modeline, modeline::parse(&[modeline], "file:///test.fix")),
    ];
    for (source, layer) in layers {
        let resolved = merge(&[layer], Registry::known_ids()).unwrap();
        assert_eq!(resolved.warnings.len(), 1, "{source:?}");
        let warning = &resolved.warnings[0];
        assert_eq!(warning.key, "diagnostics.rules.beginString");
        assert_eq!(warning.severity, Severity::Error);
        assert_eq!(warning.origin.layer, source);
        assert!(warning.message.contains("setting is ignored"));
        assert!(!resolved
            .config
            .diagnostics
            .rules
            .contains_key("beginString"));
        let integrity = &resolved.config.diagnostics.rules["integrity"];
        assert_eq!(integrity.enabled(), Some(true));
        assert_eq!(integrity.severity(), Some(Severity::Hint));
    }
}
