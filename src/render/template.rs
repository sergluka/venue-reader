//! Summary templates are parsed at configuration time; branches may be bare or quoted.

use std::sync::LazyLock;

use crate::tag;

/// How a slot renders the field it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    /// `.value`, the default: the bytes as they appear on the line.
    Raw,
    /// `.valueName`: the enum label, or for MsgType(35) the message name.
    Name,
    Tag,
    TagName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Literal(String),
    /// `{54.valueName?:"fallback"}`. The fallback is a template of its own.
    Placeholder {
        tag: u32,
        fmt: Fmt,
        fallback: Option<Box<Template>>,
    },
    /// `{54?"then":"else"}`; `values` is `None` for a presence test.
    Condition {
        tag: u32,
        values: Option<Vec<String>>,
        then: Template,
        otherwise: Template,
    },
    /// The per-MsgType detail, placed by the user rather than appended.
    Details,
}

impl Piece {
    fn mentions_details(&self) -> bool {
        match self {
            Piece::Details => true,
            Piece::Placeholder { fallback, .. } => fallback
                .as_ref()
                .is_some_and(|branch| branch.mentions_details()),
            Piece::Condition {
                then, otherwise, ..
            } => then.mentions_details() || otherwise.mentions_details(),
            Piece::Literal(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub(crate) pieces: Vec<Piece>,
    mentions_details: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("unclosed `{{` at byte {at}")]
    Unclosed { at: usize },
    #[error("empty reference at byte {at}")]
    EmptyRef { at: usize },
    #[error("unknown reference {name:?} at byte {at}")]
    UnknownRef { name: String, at: usize },
    #[error("unknown format {fmt:?} at byte {at}")]
    UnknownFmt { fmt: String, at: usize },
    #[error("invalid template expression at byte {at}")]
    Syntax { at: usize },
}

pub const DEFAULT_TEMPLATE: &str =
    "{52?:???}: {34?:???}: {sender?:???}=>{target?:???} | {msgName?:???}{43=Y? [POSS DUP]}";
static DEFAULT: LazyLock<Template> =
    LazyLock::new(|| Template::parse(DEFAULT_TEMPLATE).expect("schema default"));
static DEFAULT_SYMBOL: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(crate::config::DEFAULT_SYMBOL_TEMPLATE).expect("symbol default")
});

impl Template {
    pub fn default_template() -> &'static Template {
        &DEFAULT
    }
    pub fn default_symbol_template() -> &'static Template {
        &DEFAULT_SYMBOL
    }
    pub fn mentions_details(&self) -> bool {
        self.mentions_details
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
    pub(crate) fn has_nonempty_literal(&self) -> bool {
        self.pieces
            .iter()
            .any(|piece| matches!(piece, Piece::Literal(text) if !text.is_empty()))
    }
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        Self::parse_depth(text, 0)
    }
    fn parse_depth(text: &str, depth: usize) -> Result<Self, TemplateError> {
        if depth > 64 {
            return Err(TemplateError::Syntax { at: 0 });
        }
        let pieces: Vec<Piece> =
            scan_with_nesting(text, |body, at| parse_slot(body, at, depth), true)?
                .into_iter()
                .map(|element| match element {
                    Element::Literal(text) => Piece::Literal(text),
                    Element::Slot(piece) => piece,
                })
                .collect();
        let mentions_details = pieces.iter().any(Piece::mentions_details);
        Ok(Self {
            pieces,
            mentions_details,
        })
    }
}

fn quoted(input: &mut &str, at: usize) -> Result<String, TemplateError> {
    *input = input.trim_start();
    let mut stream = serde_json::Deserializer::from_str(input).into_iter::<String>();
    let value = stream
        .next()
        .and_then(Result::ok)
        .ok_or(TemplateError::Syntax { at })?;
    *input = &input[stream.byte_offset()..];
    Ok(value)
}

fn condition_value(input: &mut &str, at: usize) -> Result<String, TemplateError> {
    *input = input.trim_start();
    if input.starts_with('"') {
        return quoted(input, at);
    }
    let end = input.find([',', '?']).unwrap_or(input.len());
    let value = input[..end].trim();
    if value.is_empty() || value.contains(['{', '}', '"']) {
        return Err(TemplateError::Syntax { at });
    }
    *input = &input[end..];
    Ok(value.to_owned())
}

fn branch_text(input: &mut &str, at: usize) -> Result<String, TemplateError> {
    if input.trim_start().starts_with('"') {
        return quoted(input, at);
    }
    let mut position = 0;
    while position < input.len() {
        match input.as_bytes()[position] {
            b'{' => {
                position = slot_end(input.as_bytes(), position + 1, true)
                    .ok_or(TemplateError::Unclosed { at })?
                    + 1;
            }
            b':' => break,
            b'"' => return Err(TemplateError::Syntax { at }),
            _ => position += 1,
        }
    }
    if input[..position].trim().is_empty() {
        return Err(TemplateError::Syntax { at });
    }
    let value = input[..position].to_owned();
    *input = &input[position..];
    Ok(value)
}

fn parse_slot(body: &str, at: usize, depth: usize) -> Result<Piece, TemplateError> {
    let boundary = body
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .unwrap_or(body.len());
    let head = &body[..boundary];
    let (name, selector) = head
        .split_once('.')
        .map_or((head, None), |(name, selector)| (name, Some(selector)));
    if name.is_empty() {
        return Err(TemplateError::EmptyRef { at });
    }
    let mut rest = body[boundary..].trim_start();
    if name == "details" {
        // The detail is a template of its own: nothing to select, fall back to
        // or test, so anything after the name is an error rather than a no-op.
        return if selector.is_none() && rest.is_empty() {
            Ok(Piece::Details)
        } else {
            Err(TemplateError::Syntax { at })
        };
    }
    let (tag, default_fmt) = match name {
        "msgName" => (tag::MSG_TYPE, Fmt::Name),
        "sender" => (tag::SENDER_COMP_ID, Fmt::Raw),
        "target" => (tag::TARGET_COMP_ID, Fmt::Raw),
        _ => (
            name.parse().map_err(|_| TemplateError::UnknownRef {
                name: name.to_owned(),
                at,
            })?,
            Fmt::Raw,
        ),
    };
    let fmt = match selector {
        None => default_fmt,
        Some("value") => Fmt::Raw,
        Some("valueName") => Fmt::Name,
        Some("tag") => Fmt::Tag,
        Some("tagName") => Fmt::TagName,
        Some(fmt) => {
            return Err(TemplateError::UnknownFmt {
                fmt: fmt.to_owned(),
                at,
            })
        }
    };
    let branch = |value: String| Template::parse_depth(&value, depth + 1);
    if rest.is_empty() {
        return Ok(Piece::Placeholder {
            tag,
            fmt,
            fallback: None,
        });
    }
    if let Some(after) = rest.strip_prefix("?:") {
        rest = after;
        let fallback = branch(branch_text(&mut rest, at)?)?;
        if !rest.trim().is_empty() {
            return Err(TemplateError::Syntax { at });
        }
        return Ok(Piece::Placeholder {
            tag,
            fmt,
            fallback: Some(Box::new(fallback)),
        });
    }
    // A condition tests the field itself, never a representation of it.
    if selector.is_some() {
        return Err(TemplateError::Syntax { at });
    }
    let values = if let Some(after) = rest.strip_prefix('=') {
        rest = after;
        Some(vec![condition_value(&mut rest, at)?])
    } else if let Some(after) = rest
        .strip_prefix("in")
        .filter(|after| after.starts_with(char::is_whitespace))
    {
        rest = after;
        let mut values = vec![condition_value(&mut rest, at)?];
        while let Some(after) = rest.trim_start().strip_prefix(',') {
            rest = after;
            values.push(condition_value(&mut rest, at)?);
        }
        Some(values)
    } else {
        None
    };
    rest = rest
        .trim_start()
        .strip_prefix('?')
        .ok_or(TemplateError::Syntax { at })?;
    let then = branch(branch_text(&mut rest, at)?)?;
    let otherwise = if let Some(after) = rest.trim_start().strip_prefix(':') {
        rest = after;
        branch(branch_text(&mut rest, at)?)?
    } else {
        branch(String::new())?
    };
    if !rest.trim().is_empty() {
        return Err(TemplateError::Syntax { at });
    }
    Ok(Piece::Condition {
        tag,
        values,
        then,
        otherwise,
    })
}

/// One element of either grammar, before its vocabulary is applied.
pub(crate) enum Element<S> {
    Literal(String),
    Slot(S),
}

/// The syntax both grammars share. `slot` is handed each body and the byte
/// offset of its `{`, and decides what that body means.
pub(crate) fn scan<S>(
    text: &str,
    slot: impl Fn(&str, usize) -> Result<S, TemplateError>,
) -> Result<Vec<Element<S>>, TemplateError> {
    scan_with_nesting(text, slot, false)
}

fn scan_with_nesting<S>(
    text: &str,
    slot: impl Fn(&str, usize) -> Result<S, TemplateError>,
    nested: bool,
) -> Result<Vec<Element<S>>, TemplateError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut at = 0;

    while at < bytes.len() {
        match bytes[at] {
            b'{' if bytes.get(at + 1) == Some(&b'{') => {
                literal.push('{');
                at += 2;
            }
            b'}' if bytes.get(at + 1) == Some(&b'}') => {
                literal.push('}');
                at += 2;
            }
            b'{' => {
                let open = at;
                let close =
                    slot_end(bytes, at + 1, nested).ok_or(TemplateError::Unclosed { at: open })?;
                if !literal.is_empty() {
                    out.push(Element::Literal(std::mem::take(&mut literal)));
                }
                out.push(Element::Slot(slot(&text[open + 1..close], open)?));
                at = close + 1;
            }
            _ => {
                let start = at;
                while at < bytes.len() && bytes[at] != b'{' && bytes[at] != b'}' {
                    at += 1;
                }
                literal.push_str(&text[start..at]);
                if at < bytes.len() && bytes[at] == b'}' && bytes.get(at + 1) != Some(&b'}') {
                    // A bare `}` is literal; only `{` opens a slot.
                    literal.push('}');
                    at += 1;
                }
            }
        }
    }
    if !literal.is_empty() {
        out.push(Element::Literal(literal));
    }
    Ok(out)
}

// Label formats keep their separate, non-nested slot grammar.
fn slot_end(bytes: &[u8], from: usize, nested: bool) -> Option<usize> {
    let (mut quoted, mut escaped) = (false, false);
    let mut depth = 0;
    for (at, &byte) in bytes.iter().enumerate().skip(from) {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b'{' if nested && !quoted => depth += 1,
            b'}' if !quoted && depth == 0 => return Some(at),
            b'}' if !quoted => depth -= 1,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expressions_and_nested_quoted_templates() {
        for source in [
            DEFAULT_TEMPLATE,
            "{{35}}",
            "{54.tag} {54.tagName} {54.value} {54.valueName}",
            r#"{54?:""}"#,
            r#"{43="Y"?" [POSS DUP]"}"#,
            r#"{150 in "F","G"?"{38?:\"???\"}@{44?:\"MKT\"}":"other"}"#,
        ] {
            Template::parse(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        }
    }
    #[test]
    fn optional_quotes_produce_the_same_expressions() {
        for (bare, quoted) in [
            (
                r#"{54=1?{54.valueName?:UNKNOWN}:other}"#,
                r#"{54="1"?"{54.valueName?:\"UNKNOWN\"}":"other"}"#,
            ),
            (
                r#"{150 in F,G?trade:other}"#,
                r#"{150 in "F","G"?"trade":"other"}"#,
            ),
            (r#"{43=Y? [POSS DUP]}"#, r#"{43="Y"?" [POSS DUP]"}"#),
            (r#"{54?{54?yes:no}}"#, r#"{54?"{54?\"yes\":\"no\"}"}"#),
            (r#"{54?:???}"#, r#"{54?:"???"}"#),
            (
                r#"{54=1?"Side: {54.valueName}":other}"#,
                r#"{54="1"?"Side: {54.valueName}":"other"}"#,
            ),
            (
                r#"{54 in 1,"2"?"{{text}}":other}"#,
                r#"{54 in "1","2"?"{{text}}":"other"}"#,
            ),
            (r#"{54? Привет : Пока }"#, r#"{54?" Привет ":" Пока "}"#),
        ] {
            assert_eq!(
                Template::parse(bare).unwrap(),
                Template::parse(quoted).unwrap(),
                "{bare}"
            );
        }
    }

    #[test]
    fn malformed_unquoted_branches_are_rejected() {
        for source in [
            "{54?}",
            "{54?:}",
            "{54?yes:}",
            "{54=?yes}",
            "{54 in 1,?yes}",
            "{54?yes:no:extra}",
            "{54?{54}",
            "{54?unquoted\"quote}",
        ] {
            assert!(Template::parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn rejects_obsolete_and_malformed_expressions() {
        for source in [
            "{54|UNKNOWN}",
            "{54:name}",
            "{possDup}",
            r#"{54="Y"?:"no"}"#,
            r#"{54 in ?"yes"}"#,
            r#"{54?"yes":"{unknown}"}"#,
            r#"{54 in "Y",?"yes"}"#,
            r#"{details?:"none"}"#,
            "{details.tagName}",
            r#"{details?"yes"}"#,
        ] {
            assert!(Template::parse(source).is_err(), "{source}");
        }
    }
    #[test]
    fn nested_details_prevent_automatic_duplication() {
        assert!(Template::parse(r#"{54?"{details}"}"#)
            .unwrap()
            .mentions_details());
        assert!(!Template::parse(r#"{54?"literal"}"#)
            .unwrap()
            .has_nonempty_literal());
    }
}
