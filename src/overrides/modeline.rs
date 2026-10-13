//! Byte-preserving edge-window scanning without line analysis.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{catalog, Origin, SettingsLayer, SourceKind};
use crate::config::Severity;

pub const EDGE_LINES: usize = 5;

pub fn is_edge(line: usize, count: usize) -> bool {
    line < count && (line < EDGE_LINES || line >= count.saturating_sub(EDGE_LINES))
}

pub fn edge_indices(count: usize) -> impl Iterator<Item = usize> {
    let head = count.min(EDGE_LINES);
    (0..head).chain(count.saturating_sub(EDGE_LINES).max(head)..count)
}

pub fn content_lines(lines: &[&[u8]]) -> usize {
    lines.len() - usize::from(lines.last().is_some_and(|line| line.is_empty()))
}

pub fn parse(lines: &[&[u8]], source: &str) -> SettingsLayer {
    parse_numbered(
        lines.iter().copied().enumerate(),
        content_lines(lines),
        source,
    )
}

pub fn parse_numbered<'a>(
    lines: impl IntoIterator<Item = (usize, &'a [u8])>,
    count: usize,
    source: &str,
) -> SettingsLayer {
    let origin = Origin::new(SourceKind::Modeline, source);
    let mut layer = SettingsLayer::new(origin);
    let mut seen = BTreeMap::<String, Origin>::new();
    let edges: BTreeMap<_, _> = lines
        .into_iter()
        .filter(|(line, _)| is_edge(*line, count))
        .collect();
    for (line_number, line) in edges {
        let Some(payload) = payload_start(line) else {
            continue;
        };
        for (start, end) in chunks(line, payload) {
            let origin = Origin {
                line: line_number,
                start,
                end,
                ..layer.origin.clone()
            };
            let Ok(chunk) = std::str::from_utf8(&line[start..end]) else {
                layer.warn(
                    "",
                    "modeline assignment is not UTF-8",
                    origin,
                    Severity::Warning,
                );
                continue;
            };
            let Some((written_key, raw)) = chunk.split_once('=') else {
                layer.warn("", "expected key=value", origin, Severity::Warning);
                continue;
            };
            if catalog::foreign(written_key) {
                continue;
            }
            let mut path: Vec<String> = written_key.split('.').map(str::to_owned).collect();
            let setting = catalog::find(written_key);
            let kind = setting
                .map(|entry| entry.kind)
                .or_else(|| catalog::rule_kind(&path));
            let Some(kind) = kind else {
                layer.warn(
                    written_key,
                    format!("unknown setting {written_key}"),
                    origin,
                    Severity::Warning,
                );
                continue;
            };
            if path.len() == 3 && path[0] == "diagnostics" && path[1] == "rules" {
                path.push("enabled".into());
            }
            let key = path.join(".");
            if let Some(first) = seen.get(&key) {
                layer.remove(&key);
                layer.warn(
                    &key,
                    format!(
                        "duplicate setting {key}; first defined at {}:{}:{}",
                        first.source,
                        first.line + 1,
                        first.start + 1
                    ),
                    origin,
                    Severity::Error,
                );
                continue;
            }
            seen.insert(key.clone(), origin.clone());
            if setting.is_some_and(|entry| entry.scope == catalog::Scope::Global) {
                layer.warn(
                    &key,
                    format!("{key} is global-only"),
                    origin,
                    Severity::Warning,
                );
                continue;
            }
            if raw == "unset" {
                continue;
            }
            match scalar(raw, kind)
                .and_then(|value| catalog::validate(&key, kind, &value).map(|()| value))
            {
                Ok(value) => layer.set_path(path, value, origin),
                Err(error) => layer.warn(&key, error, origin, Severity::Warning),
            }
        }
    }
    layer
}

fn payload_start(line: &[u8]) -> Option<usize> {
    let first = line.iter().position(|byte| !byte.is_ascii_whitespace())?;
    if line[first] != b'#' {
        return None;
    }
    let body = first + 1;
    line[body..]
        .windows(4)
        .enumerate()
        .find_map(|(offset, window)| {
            let start = body + offset;
            (window == b"fix:" && (offset == 0 || matches!(line[start - 1], b' ' | b'\t')))
                .then_some(start + 4)
        })
}

fn chunks(line: &[u8], payload: usize) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut start = payload;
    let mut quote = None;
    let mut escaped = false;
    for (offset, byte) in line.iter().copied().enumerate().skip(payload) {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if byte == b':' || byte.is_ascii_whitespace() {
            if start < offset {
                result.push((start, offset));
            }
            start = offset + 1;
        }
    }
    if start < line.len() {
        result.push((start, line.len()));
    }
    result
}

fn scalar(raw: &str, kind: catalog::Kind) -> Result<Value, String> {
    let raw = raw.replace("\\:", ":");
    if raw.starts_with(['\'', '"']) {
        let parsed: toml::Value =
            toml::from_str(&format!("value = {raw}")).map_err(|error| error.to_string())?;
        return serde_json::to_value(&parsed["value"]).map_err(|error| error.to_string());
    }
    match kind {
        catalog::Kind::ApplicationVersion if raw == "null" => Ok(Value::Null),
        catalog::Kind::String
        | catalog::Kind::ApplicationVersion
        | catalog::Kind::Depth
        | catalog::Kind::Severity => Ok(Value::String(raw)),
        catalog::Kind::Boolean => raw
            .parse::<bool>()
            .map(Value::Bool)
            .map_err(|_| "expected true or false".into()),
        catalog::Kind::Unsigned => raw
            .parse::<u64>()
            .map(Value::from)
            .map_err(|_| "expected an unsigned integer".into()),
        catalog::Kind::Opaque => Ok(serde_json::from_str(&raw).unwrap_or(Value::String(raw))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scan(text: &str) -> SettingsLayer {
        parse(
            &text
                .as_bytes()
                .split(|byte| *byte == b'\n')
                .collect::<Vec<_>>(),
            "file:///test.fix",
        )
    }

    #[test]
    fn only_comment_markers_with_whitespace_are_configuration() {
        let layer = scan("8=FIX.4.4|58=# fix: hover.enabled=false\n# prefix:fix:hover.enabled=false\n#fix: hover.enabled=false:");
        assert_eq!(layer.get("hover.enabled"), Some(&json!(false)));
        assert_eq!(layer.candidate("hover.enabled").unwrap().origin.line, 2);
        assert!(layer.warnings.is_empty());
    }

    #[test]
    fn duplicate_keys_in_separate_windows_are_all_rejected() {
        let layer = scan("# fix: hover.enabled=false\n1\n2\n3\n4\n5\n6\n7\n8\n9\n# fix: hover.enabled=false semanticTokens.enabled=false\n");
        assert!(!layer.contains_key("hover.enabled"));
        assert_eq!(layer.get("semanticTokens.enabled"), Some(&json!(false)));
        assert_eq!(layer.warnings[0].severity, Severity::Error);
    }

    #[test]
    fn quoted_values_keep_colons_spaces_and_escaped_colons() {
        let layer =
            scan(r#"# fix: messageSummary.template="{35\:name}: a, b":dictionary=venue\:a:"#);
        assert_eq!(
            layer.get("messageSummary.template"),
            Some(&json!("{35:name}: a, b"))
        );
        assert_eq!(layer.get("dictionary"), Some(&json!("venue:a")));
    }

    #[test]
    fn rule_boolean_shorthand_and_unset_share_duplicate_detection() {
        let layer = scan(
            "# fix: diagnostics.rules.integrity=false diagnostics.rules.integrity.enabled=unset",
        );
        assert!(layer.candidates.is_empty());
        assert_eq!(layer.warnings[0].severity, Severity::Error);
        assert!(scan("# fix: hover.enabled=unset").candidates.is_empty());
        assert_eq!(
            scan("# fix: dictionary=\"unset\"").get("dictionary"),
            Some(&json!("unset"))
        );
    }

    #[test]
    fn trailing_newline_does_not_consume_bottom_window() {
        let layer = scan("1\n2\n3\n4\n5\n6\n# fix:hover.enabled=false\n8\n9\n10\n11\n");
        assert_eq!(layer.get("hover.enabled"), Some(&json!(false)));
    }

    #[test]
    fn numbered_edges_preserve_positions_and_deduplicate_overlap() {
        let line = b"# fix: hover.enabled=false".as_slice();
        let layer = parse_numbered([(999, line), (999, line)], 1000, "file:///large.fix");
        assert_eq!(layer.candidate("hover.enabled").unwrap().origin.line, 999);
        assert!(layer.warnings.is_empty());
    }

    #[test]
    fn foreign_keys_are_silent_and_global_keys_are_rejected() {
        let layer = scan("# fix: formatter.width=80 render.summary.position=above allowDictionaryPath=true defaultVersion=FIX.4.2");
        assert!(layer.candidates.is_empty());
        assert_eq!(layer.warnings.len(), 2);
    }

    #[test]
    fn spans_are_bytes_even_after_non_ascii_and_invalid_bytes() {
        let layer = parse(&[b"# \xff fix: hover.enabled=bad"], "file:///bytes.fix");
        assert_eq!(layer.warnings[0].origin.start, 9);
        assert_eq!(layer.warnings[0].origin.end, 26);
    }
}
