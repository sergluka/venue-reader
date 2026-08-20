//! Canonical settings shared by structured sources and modelines.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Boolean,
    String,
    Unsigned,
    Depth,
    Severity,
    Opaque,
}

#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub path: &'static str,
    pub scope: Scope,
    pub kind: Kind,
}

macro_rules! settings {
    ($(($path:literal, $scope:ident, $kind:ident)),* $(,)?) => {
        pub const SETTINGS: &[Setting] = &[$(Setting {
            path: $path, scope: Scope::$scope, kind: Kind::$kind,
        }),*];
    }
}

settings! {
    ("defaultVersion", Global, String),
    ("allowDictionaryPath", Global, Boolean),
    ("documentLink.url", Global, String),
    ("overrides.modeline", Global, Boolean),
    ("overrides.projectConfig", Global, Boolean),
    ("limits.dictionaryBytes", Global, Unsigned),
    ("limits.diagnosticsPerDocument", Global, Unsigned),
    ("limits.rangeLines", Global, Unsigned),
    ("limits.codeLensLines", Global, Unsigned),
    ("limits.documentSymbolItems", Global, Unsigned),
    ("limits.semanticTokensFullLines", Global, Unsigned),
    ("limits.cachedLines", Global, Unsigned),
    ("inlayHints.tagNames.enabled", Document, Boolean),
    ("inlayHints.tagNames.format", Document, String),
    ("inlayHints.enumValues.enabled", Document, Boolean),
    ("inlayHints.enumValues.format", Document, String),
    ("inlayHints.groupPaths.enabled", Document, Boolean),
    ("messageSummary.enabled", Document, Boolean),
    ("messageSummary.template", Document, String),
    ("messageSummary.details", Document, Boolean),
    ("messageSummary.channels.codeLens", Document, Boolean),
    ("messageSummary.channels.inlayHints", Document, Boolean),
    ("diagnostics.enabled", Document, Boolean),
    ("hover.enabled", Document, Boolean),
    ("semanticTokens.enabled", Document, Boolean),
    ("documentSymbol.enabled", Document, Boolean),
    ("documentSymbol.depth", Document, Depth),
    ("documentSymbol.template", Document, String),
    ("dictionary", Document, String),
}

pub fn find(path: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| setting.path == path)
}

/// The rule id a `diagnostics.rules.<id>...` path names. The one place that
/// knows the shape of the catalog's only dynamic-key namespace.
pub fn rule_id(path: &[String]) -> Option<&str> {
    let [namespace, rules, identifier, ..] = path else {
        return None;
    };
    (namespace == "diagnostics" && rules == "rules").then_some(identifier.as_str())
}

pub fn rule_kind(path: &[String]) -> Option<Kind> {
    if rule_id(path).is_none_or(str::is_empty) {
        return None;
    }
    match path.get(3).map(String::as_str) {
        None | Some("enabled") if path.len() <= 4 => Some(Kind::Boolean),
        Some("severity") if path.len() == 4 => Some(Kind::Severity),
        Some(_) => Some(Kind::Opaque),
        None => None,
    }
}

pub fn foreign(path: &str) -> bool {
    path == "formatter"
        || path.starts_with("formatter.")
        || matches!(
            path,
            "render.summary.position"
                | "render.group.highlight.enabled"
                | "render.group.highlight.target"
                | "annotate.title.position"
                | "annotate.group.highlight.enabled"
        )
}

pub fn namespace(path: &str) -> bool {
    SETTINGS
        .iter()
        .any(|setting| setting.path.starts_with(&format!("{path}.")))
        || matches!(
            path,
            "diagnostics.rules"
                | "render"
                | "render.summary"
                | "render.group"
                | "render.group.highlight"
                | "annotate"
                | "annotate.title"
                | "annotate.group"
                | "annotate.group.highlight"
        )
}

pub fn validate(path: &str, kind: Kind, value: &Value) -> Result<(), String> {
    let valid = match kind {
        Kind::Boolean => value.is_boolean(),
        Kind::String => value.is_string(),
        Kind::Unsigned => value.as_u64().is_some(),
        Kind::Depth => matches!(value.as_str(), Some("messages" | "fields" | "full")),
        Kind::Severity => matches!(value.as_str(), Some("error" | "warning" | "info" | "hint")),
        Kind::Opaque => true,
    };
    if !valid {
        return Err(format!("{path}: expected {kind:?}"));
    }
    if path == "limits.documentSymbolItems" && value.as_u64() == Some(0) {
        return Err(format!("{path}: must be at least 1"));
    }
    if matches!(path, "dictionary" | "defaultVersion")
        && value.as_str().is_some_and(|text| text.trim().is_empty())
    {
        return Err(format!("{path}: must not be blank"));
    }
    if let Some(text) = value.as_str() {
        match path {
            "messageSummary.template" | "documentSymbol.template" => {
                crate::render::Template::parse(text).map_err(|error| format!("{path}: {error}"))?;
            }
            "inlayHints.tagNames.format" | "inlayHints.enumValues.format" => {
                crate::render::LabelFormat::parse(text)
                    .map_err(|error| format!("{path}: {error}"))?;
            }
            "documentLink.url" => {
                crate::render::TagUrl::validate(text)
                    .map_err(|error| format!("{path}: {error}"))?;
            }
            _ => {}
        }
    }
    Ok(())
}
