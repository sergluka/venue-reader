//! The JSON schemas are written by hand and refuse unknown rule ids, so a rule
//! missing from one of them is a setting an editor would flag as an error.

use serde_json::{json, Value};
use std::path::Path;
use venue_reader::config::{user_file, Severity};
use venue_reader::overrides::{merge, modeline, parse_client, project, SourceKind};
use venue_reader::rules::Registry;

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
        "schema/venue-reader.schema.json",
        "schema/venue-reader-user.schema.json",
        "schema/venue-reader-project.schema.json",
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
fn user_schemas_list_the_registered_limits_with_their_effective_defaults() {
    use std::collections::BTreeSet;
    use venue_reader::overrides::catalog::{Scope, SETTINGS};

    let registered: BTreeSet<_> = SETTINGS
        .iter()
        .filter_map(|setting| setting.path.strip_prefix("limits."))
        .collect();
    let defaults = serde_json::to_value(venue_reader::config::Config::default()).unwrap();
    for filename in ["venue-reader.schema.json", "venue-reader-user.schema.json"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schema")
            .join(filename);
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let limits = &schema["properties"]["limits"];
        assert_eq!(limits["x-venue-reader-scope"], "user-global");
        let properties = limits["properties"].as_object().unwrap();
        assert_eq!(
            properties
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            registered
        );
        for (name, property) in properties {
            assert_eq!(property["type"], "integer", "{filename}: {name}");
            assert_eq!(
                property["default"], defaults["limits"][name],
                "{filename}: {name}"
            );
        }
        assert_eq!(properties["decodeResultBytes"]["minimum"], 0);
        assert_eq!(properties["decodeResultBytes"]["default"], 8 * 1024 * 1024);
    }
    assert!(SETTINGS
        .iter()
        .filter(|setting| setting.path.starts_with("limits."))
        .all(|setting| matches!(setting.scope, Scope::Global)));
    let project: Value =
        serde_json::from_str(include_str!("../schema/venue-reader-project.schema.json")).unwrap();
    assert!(project["properties"].get("limits").is_none());
}

#[test]
fn begin_string_is_an_unknown_rule_in_every_configuration_source() {
    let toml = "[diagnostics.rules]\nbeginString = false\n[diagnostics.rules.integrity]\nenabled = true\nseverity = 'hint'\n";
    let modeline = b"# fix: diagnostics.rules.beginString=false diagnostics.rules.integrity.enabled=true diagnostics.rules.integrity.severity=hint";
    let layers = [
        (SourceKind::UserFile, user_file::parse(toml, Path::new("config.toml")).unwrap().layer),
        (SourceKind::Client, parse_client(json!({"diagnostics":{"rules":{"beginString":false,"integrity":{"enabled":true,"severity":"hint"}}}})).unwrap()),
        (SourceKind::Project, project::parse(toml, Path::new(".venue-reader.toml")).unwrap().layer),
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

/// The property a dotted setting path names, following a shared `$defs` entry
/// where the schema reuses one.
fn property<'a>(schema: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(schema, |node, segment| {
        let node = node
            .get("allOf")
            .and_then(|all| all[0]["$ref"].as_str())
            .map_or(node, |reference| {
                schema
                    .pointer(reference.trim_start_matches('#'))
                    .expect("a local $ref")
            });
        node.get("properties")?.get(segment)
    })
}

#[test]
fn every_schema_spells_the_catalog_and_the_template_defaults() {
    use venue_reader::config::DEFAULT_SYMBOL_TEMPLATE;
    use venue_reader::overrides::catalog::{Scope, SETTINGS};
    use venue_reader::render::DEFAULT_TEMPLATE;

    for file in [
        "schema/venue-reader.schema.json",
        "schema/venue-reader-user.schema.json",
        "schema/venue-reader-project.schema.json",
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for setting in SETTINGS {
            let Some(property) = property(&schema, setting.path) else {
                // Each schema lists the sections its source may set; the
                // summary section is in every one of them.
                assert!(
                    !setting.path.starts_with("messageSummary."),
                    "{file}: {} is missing",
                    setting.path
                );
                continue;
            };
            // Only a document setting a modeline may not assign carries the mark.
            let marked = property.get("x-venue-reader-modeline") == Some(&json!(false));
            assert_eq!(
                marked,
                setting.scope == Scope::Document && !setting.modeline,
                "{file}: {}",
                setting.path
            );
        }
        assert_eq!(
            property(&schema, "messageSummary.template").unwrap()["default"],
            DEFAULT_TEMPLATE,
            "{file}"
        );
        assert_eq!(
            property(&schema, "documentSymbol.template").unwrap()["default"],
            DEFAULT_SYMBOL_TEMPLATE,
            "{file}"
        );
    }
}
