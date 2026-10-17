//! Sparse configuration layers; filesystem and main-loop state stay outside resolution.

pub mod catalog;
pub mod modeline;
pub mod project;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::config::{DictionarySpec, Severity, UserConfig};
use catalog::{Kind, Scope};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    Default,
    UserFile,
    Client,
    Session,
    Project,
    Modeline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Origin {
    pub layer: SourceKind,
    pub source: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

impl Origin {
    pub fn new(layer: SourceKind, source: impl Into<String>) -> Self {
        Self {
            layer,
            source: source.into(),
            line: 0,
            start: 0,
            end: 0,
        }
    }

    pub fn document_local(&self) -> bool {
        matches!(
            self.layer,
            SourceKind::Project | SourceKind::Modeline | SourceKind::Session
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Warning {
    pub key: String,
    pub message: String,
    pub origin: Origin,
    pub severity: Severity,
}

impl Warning {
    pub fn new(key: &str, message: impl Into<String>, origin: Origin) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
            origin,
            severity: Severity::Warning,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsLayer {
    pub candidates: BTreeMap<SettingPath, Candidate>,
    pub dictionaries: Vec<DictionarySpec>,
    pub origin: Origin,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SettingPath(Vec<String>);

impl SettingPath {
    /// Compares against a dotted key without building one. A rule id may
    /// itself contain dots, so the segments cannot be recovered by splitting
    /// the key -- the comparison has to run the other way.
    pub fn eq_dotted(&self, key: &str) -> bool {
        let mut rest = key;
        for (index, segment) in self.0.iter().enumerate() {
            if index > 0 {
                match rest.strip_prefix('.') {
                    Some(tail) => rest = tail,
                    None => return false,
                }
            }
            match rest.strip_prefix(segment.as_str()) {
                Some(tail) => rest = tail,
                None => return false,
            }
        }
        rest.is_empty()
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn dotted(&self) -> String {
        self.0.join(".")
    }
}

impl From<Vec<String>> for SettingPath {
    fn from(path: Vec<String>) -> Self {
        Self(path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub value: Value,
    pub origin: Origin,
}

impl SettingsLayer {
    pub fn new(origin: Origin) -> Self {
        Self {
            candidates: BTreeMap::new(),
            dictionaries: Vec::new(),
            origin,
            warnings: Vec::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: Value, origin: Origin) {
        self.set_path(key.split('.').map(str::to_owned).collect(), value, origin);
    }

    pub fn set_path(&mut self, path: Vec<String>, value: Value, origin: Origin) {
        self.candidates
            .insert(path.into(), Candidate { value, origin });
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.candidate(key).map(|candidate| &candidate.value)
    }

    pub fn candidate(&self, key: &str) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find_map(|(path, candidate)| path.eq_dotted(key).then_some(candidate))
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.candidate(key).is_some()
    }

    pub fn remove(&mut self, key: &str) -> Option<Candidate> {
        let path = self
            .candidates
            .keys()
            .find(|path| path.eq_dotted(key))?
            .clone();
        self.candidates.remove(&path)
    }

    pub fn warn(
        &mut self,
        key: &str,
        message: impl Into<String>,
        origin: Origin,
        severity: Severity,
    ) {
        self.warnings.push(Warning {
            key: key.into(),
            message: message.into(),
            origin,
            severity,
        });
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub config: UserConfig,
    pub values: BTreeMap<String, Value>,
    pub sources: BTreeMap<String, Origin>,
    pub warnings: Vec<Warning>,
}

pub fn parse_client(value: Value) -> Result<SettingsLayer, String> {
    parse_structured(value, Origin::new(SourceKind::Client, "client"), false)
}

pub fn parse_structured(
    value: Value,
    origin: Origin,
    strict: bool,
) -> Result<SettingsLayer, String> {
    if !value.is_object() {
        return Err("configuration must be an object".into());
    }
    let mut layer = SettingsLayer::new(origin);
    collect(&value, &mut Vec::new(), &mut layer, strict)?;
    Ok(layer)
}

fn collect(
    value: &Value,
    path: &mut Vec<String>,
    layer: &mut SettingsLayer,
    strict: bool,
) -> Result<(), String> {
    let key = path.join(".");
    if catalog::foreign(&key) {
        return Ok(());
    }
    if path.as_slice() == ["dictionaries"] {
        let entries: Vec<DictionarySpec> = serde_json::from_value(value.clone())
            .map_err(|error| format!("dictionaries: {error}"))?;
        for entry in &entries {
            entry
                .validate_shape()
                .map_err(|error| format!("dictionaries: {error}"))?;
        }
        layer.dictionaries = entries;
        return Ok(());
    }
    let canonical = path.iter().all(|part| !part.contains('.'));
    let setting = canonical.then(|| catalog::find(&key)).flatten();
    let dynamic = catalog::rule_kind(path);
    if let Some(kind) = setting.map(|setting| setting.kind).or(dynamic) {
        // A rule table is sparse; the boolean shorthand means only `enabled`.
        if path.len() == 3 && path[0] == "diagnostics" && path[1] == "rules" {
            if value.is_object() {
                for (name, child) in value.as_object().expect("object") {
                    path.push(name.clone());
                    collect(child, path, layer, strict)?;
                    path.pop();
                }
                return Ok(());
            }
            catalog::validate(&key, kind, value)?;
            let mut enabled = path.clone();
            enabled.push("enabled".into());
            layer.set_path(enabled, value.clone(), layer.origin.clone());
            return Ok(());
        }
        if key == "dictionary" && !layer.origin.document_local() {
            return Err("dictionary: selectors are document-local; use a default dictionary definition globally".into());
        }
        if let Err(error) = catalog::validate(&key, kind, value) {
            if strict || !matches!(kind, Kind::String) || !value.is_string() {
                return Err(error);
            }
            layer.warn(&key, error, layer.origin.clone(), Severity::Warning);
            return Ok(());
        }
        if layer.origin.document_local()
            && setting.is_some_and(|entry| entry.scope == Scope::Global)
        {
            layer.warn(
                &key,
                format!("{key} is global-only"),
                layer.origin.clone(),
                Severity::Warning,
            );
            return Ok(());
        }
        layer.set_path(path.clone(), value.clone(), layer.origin.clone());
        return Ok(());
    }
    let known_namespace = key.is_empty() || (canonical && catalog::namespace(&key));
    if known_namespace {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{key}: expected an object"))?;
        for (name, child) in object {
            path.push(name.clone());
            collect(child, path, layer, strict)?;
            path.pop();
        }
    } else if strict {
        return Err(format!("unknown setting {key}"));
    }
    Ok(())
}

pub fn public_config(config: &UserConfig) -> Value {
    let mut value = serde_json::to_value(config).expect("configuration serializes");
    value["inlayHints"]["tagNames"] = json!({"enabled": config.inlay_hints.tag_names(), "format": config.inlay_hints.tag_format()});
    value["inlayHints"]["enumValues"] = json!({"enabled": config.inlay_hints.enum_values(), "format": config.inlay_hints.value_format()});
    value["inlayHints"]["groupPaths"] = json!({"enabled": config.inlay_hints.group_paths()});
    filter_foreign(&mut value, "");
    value
}

pub fn merge(layers: &[SettingsLayer], known_rule_ids: &[&str]) -> Result<Resolved, String> {
    let mut document = public_config(&UserConfig::default());
    let mut sources = BTreeMap::new();
    let default = Origin::new(SourceKind::Default, "default");
    for setting in catalog::SETTINGS {
        sources.insert(setting.path.into(), default.clone());
    }
    let mut warnings = Vec::new();
    let mut dictionaries = Vec::new();
    for layer in layers {
        dictionaries.extend(layer.dictionaries.clone());
        let mut unknown_rules = BTreeSet::new();
        for (path, candidate) in &layer.candidates {
            if let Some(identifier) = unknown_rule(path, known_rule_ids) {
                if unknown_rules.insert(identifier) {
                    warnings.push(Warning {
                        key: format!("diagnostics.rules.{identifier}"),
                        message: format!(
                            "unknown diagnostic rule {identifier:?}; setting is ignored"
                        ),
                        origin: candidate.origin.clone(),
                        severity: Severity::Error,
                    });
                }
                continue;
            }
            set_nested(&mut document, path.as_slice(), candidate.value.clone());
            sources.insert(path.dotted(), candidate.origin.clone());
        }
        warnings.extend(layer.warnings.clone());
    }
    document["dictionaries"] =
        serde_json::to_value(dictionaries).expect("dictionary specs serialize");
    let selector = document
        .as_object_mut()
        .expect("config object")
        .remove("dictionary");
    let config = serde_json::from_value(document.clone())
        .map_err(|error| format!("configuration: {error}"))?;
    let mut values = BTreeMap::new();
    flatten(&document, "", &mut values);
    // Unknown literal dotted JSON keys must not impersonate canonical paths.
    for setting in catalog::SETTINGS {
        let path: Vec<String> = setting.path.split('.').map(str::to_owned).collect();
        if let Some(value) = nested(&document, &path) {
            values.insert(setting.path.into(), value.clone());
        }
    }
    values.insert("dictionary".into(), selector.unwrap_or(Value::Null));
    Ok(Resolved {
        config,
        values,
        sources,
        warnings,
    })
}

fn unknown_rule<'a>(path: &'a SettingPath, known_rule_ids: &[&str]) -> Option<&'a str> {
    let identifier = catalog::rule_id(path.as_slice())?;
    (!known_rule_ids.contains(&identifier)).then_some(identifier)
}

fn nested<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

fn flatten(value: &Value, prefix: &str, output: &mut BTreeMap<String, Value>) {
    if let Some(object) = value.as_object() {
        if object.is_empty() && !prefix.is_empty() {
            output.insert(prefix.into(), value.clone());
        }
        for (name, child) in object {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            flatten(child, &key, output);
        }
    } else {
        output.insert(prefix.into(), value.clone());
    }
}

pub fn set_nested(target: &mut Value, path: &[String], value: Value) {
    let Some((first, rest)) = path.split_first() else {
        *target = value;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let next = target
        .as_object_mut()
        .expect("object")
        .entry(first.clone())
        .or_insert(Value::Null);
    set_nested(next, rest, value);
}

pub fn filter_foreign(value: &mut Value, prefix: &str) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|name, child| {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if catalog::foreign(&key) {
                return false;
            }
            let was_empty = child.as_object().is_some_and(Map::is_empty);
            filter_foreign(child, &key);
            was_empty || !child.as_object().is_some_and(Map::is_empty)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_RULES: &[&str] = &["dictionaryVersion", "integrity"];

    #[test]
    fn application_defaults_merge_globally_and_per_document_including_null() {
        let global = parse_client(json!({"defaultApplVerID":"FIX.5.0"})).unwrap();
        let project = parse_structured(
            json!({"defaultApplVerID":"FIX.5.0SP2"}),
            Origin::new(SourceKind::Project, "project"),
            true,
        )
        .unwrap();
        let modeline = modeline::parse(&[b"# fix: defaultApplVerID=null"], "file:///test.fix");
        assert!(modeline.warnings.is_empty());
        let resolved = merge(&[global.clone(), project.clone()], KNOWN_RULES).unwrap();
        assert_eq!(
            resolved.config.default_appl_ver_id.as_deref(),
            Some("FIX.5.0SP2")
        );
        assert_eq!(
            resolved.sources["defaultApplVerID"].layer,
            SourceKind::Project
        );
        let cleared = merge(&[global, project, modeline], KNOWN_RULES).unwrap();
        assert_eq!(cleared.config.default_appl_ver_id, None);
        assert_eq!(cleared.values["defaultApplVerID"], Value::Null);
        assert!(!cleared.values.contains_key("defaultVersion"));
    }

    #[test]
    fn default_version_is_an_unknown_key_without_a_deprecated_alias() {
        assert!(catalog::find("defaultVersion").is_none());
        let client = parse_client(json!({"defaultVersion":"FIX.4.2"})).unwrap();
        assert!(client.candidates.is_empty());
        let resolved = merge(&[client], KNOWN_RULES).unwrap();
        assert!(!resolved.values.contains_key("defaultVersion"));
        assert!(parse_structured(
            json!({"defaultVersion":"FIX.4.2"}),
            Origin::new(SourceKind::UserFile, "user"),
            true
        )
        .is_err());
        let modeline = modeline::parse(&[b"# fix: defaultVersion=FIX.4.2"], "file:///test.fix");
        assert!(modeline.candidates.is_empty());
        assert_eq!(modeline.warnings.len(), 1);
        assert!(modeline.warnings[0].message.contains("unknown setting"));
    }

    #[test]
    fn defaults_have_every_catalog_leaf_without_foreign_settings() {
        let resolved = merge(&[], KNOWN_RULES).unwrap();
        for setting in catalog::SETTINGS {
            assert!(
                resolved.values.contains_key(setting.path),
                "{}",
                setting.path
            );
        }
        assert!(!resolved.config.allow_dictionary_path);
        assert!(resolved.config.overrides.modeline);
        assert!(resolved.config.overrides.project_config);
    }

    #[test]
    fn explicit_default_value_above_a_project_override_still_wins() {
        let project = parse_structured(
            json!({"hover":{"enabled":false}}),
            Origin::new(SourceKind::Project, "file:///project/.fix-lsp.toml"),
            true,
        )
        .unwrap();
        let modeline = modeline::parse(&[b"# fix: hover.enabled=true"], "file:///project/log.fix");
        let resolved = merge(&[project, modeline], KNOWN_RULES).unwrap();
        assert!(resolved.config.hover.enabled);
        assert_eq!(
            resolved.sources["hover.enabled"].layer,
            SourceKind::Modeline
        );
    }

    #[test]
    fn each_global_catalog_entry_rejects_document_candidates() {
        let defaults = merge(&[], KNOWN_RULES).unwrap();
        for setting in catalog::SETTINGS
            .iter()
            .filter(|entry| entry.scope == Scope::Global)
        {
            let mut object = json!({});
            set_nested(
                &mut object,
                &setting
                    .path
                    .split('.')
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                defaults.values[setting.path].clone(),
            );
            let layer = parse_structured(object, Origin::new(SourceKind::Project, "project"), true)
                .unwrap();
            assert!(!layer.contains_key(setting.path));
            assert_eq!(layer.warnings.len(), 1, "{}", setting.path);
        }
    }

    #[test]
    fn client_unknown_spelling_is_not_a_canonical_override() {
        let layer = parse_client(json!({"hover.enabled":false,"fix_hover_enabled":false,"annotate":{"tag":{"enabled":true}},"future":{"active":true}})).unwrap();
        assert!(layer.candidates.is_empty());
        let result = merge(&[layer], KNOWN_RULES).unwrap();
        assert!(result.config.hover.enabled);
        assert_eq!(result.values["hover.enabled"], json!(true));
    }

    #[test]
    fn invalid_render_candidate_falls_through_to_user_value() {
        let user = parse_structured(
            json!({"messageSummary":{"template":"{35}"}}),
            Origin::new(SourceKind::UserFile, "user"),
            true,
        )
        .unwrap();
        let client = parse_client(json!({"messageSummary":{"template":"{"}})).unwrap();
        assert_eq!(client.warnings.len(), 1);
        assert_eq!(
            merge(&[user, client], KNOWN_RULES)
                .unwrap()
                .config
                .message_summary
                .template,
            "{35}"
        );
        assert!(parse_client(json!({"messageSummary":{"template":12}})).is_err());
    }

    #[test]
    fn rule_settings_merge_per_leaf_and_preserve_opaque_keys() {
        let weaker = parse_client(json!({"diagnostics":{"rules":{"venue.rule":{"severity":"error","option.key":{"a":1},"enabled":false}}}})).unwrap();
        let stronger = parse_structured(
            json!({"diagnostics":{"rules":{"venue.rule":true}}}),
            Origin::new(SourceKind::Project, "project"),
            true,
        )
        .unwrap();
        let resolved = merge(&[weaker, stronger], &["venue.rule"]).unwrap();
        let rule = &resolved.config.diagnostics.rules["venue.rule"];
        assert_eq!(rule.enabled(), Some(true));
        assert_eq!(rule.severity(), Some(Severity::Error));
        assert_eq!(rule.options()["option.key"], json!({"a":1}));
    }

    #[test]
    fn foreign_values_never_reach_public_reports() {
        let layer = parse_client(json!({"formatter":{"width":80},"render":{"summary":{"position":"above"}},"future":true,"emptyFuture":{},"hover":{"enabled":false}})).unwrap();
        assert!(layer.warnings.is_empty());
        let resolved = merge(&[layer], KNOWN_RULES).unwrap();
        assert!(!resolved.values.keys().any(|key| catalog::foreign(key)));
        let public = public_config(&resolved.config);
        assert!(public.get("formatter").is_none());
        assert!(public.get("render").is_none());
        assert!(public.get("future").is_none());
        assert!(public.get("emptyFuture").is_none());
        assert!(!resolved.values.contains_key("emptyFuture"));
    }

    #[test]
    fn unknown_rule_is_an_error_and_is_omitted_from_effective_config() {
        let layer = parse_client(json!({
            "diagnostics": {
                "enabled": false,
                "rules": {
                    "integritY": {"enabled": false, "severity": "hint"},
                    "integrity": {"enabled": false}
                }
            }
        }))
        .unwrap();
        let resolved = merge(&[layer], KNOWN_RULES).unwrap();

        assert!(!resolved.config.diagnostics.enabled);
        assert!(!resolved.config.diagnostics.rules.contains_key("integritY"));
        assert_eq!(
            resolved.config.diagnostics.rules["integrity"].enabled(),
            Some(false)
        );
        assert!(!resolved
            .values
            .keys()
            .any(|key| key.starts_with("diagnostics.rules.integritY")));
        assert_eq!(resolved.warnings.len(), 1);
        assert_eq!(resolved.warnings[0].severity, Severity::Error);
        assert_eq!(resolved.warnings[0].key, "diagnostics.rules.integritY");
    }

    #[test]
    fn dictionary_arrays_accumulate_instead_of_replacing_weaker_definitions() {
        let user = parse_client(json!({"dictionaries":[{"name":"a","path":"a.xml"}]})).unwrap();
        let client = parse_client(json!({"dictionaries":[{"name":"b","path":"b.xml"}]})).unwrap();
        assert_eq!(
            merge(&[user, client], KNOWN_RULES)
                .unwrap()
                .config
                .dictionaries
                .len(),
            2
        );
    }
}
