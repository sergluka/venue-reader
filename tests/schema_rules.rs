//! The JSON schemas are written by hand and refuse unknown rule ids, so a rule
//! missing from one of them is a setting an editor would flag as an error.

use fix_lsp::rules::Registry;
use serde_json::Value;

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
