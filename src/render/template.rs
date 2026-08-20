//! The message-summary template grammar: `{ ref [":" fmt] ["|" fallback] }`.
//!
//! The *scan* below — literals, `{{`/`}}`, a bare `}`, and where a slot's body
//! begins and ends — is shared with `field_format`, which has its own smaller
//! vocabulary. Only what a body means differs between the two, so only that is
//! written twice.
//!
//! Parsed once at configuration time into `render::Options`, so a broken
//! template is reported when it is committed rather than on every render.
//!
//! "Summary", not "codeLens": it is data, and two channels carry it.

use std::sync::LazyLock;

/// What a slot points at: a tag, or one of the aliases the grammar accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Tag(u32),
    /// `msgName`, equivalent to `35:name`.
    MsgName,
    /// `sender`, equivalent to `49`.
    Sender,
    /// `target`, equivalent to `56`.
    Target,
    /// ` [POSS DUP]` when PossDupFlag(43) is `Y`, else empty. Never uses its fallback.
    ///
    /// An alias because the grammar has no conditional yet; see After-v1.
    PossDup,
    /// The per-MsgType detail string, or empty.
    Details,
}

/// How a slot renders the value it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    /// The bytes as they appear on the line. The default.
    Raw,
    /// The enum label, or for MsgType(35) the message name — hence not `EnumName`.
    /// Falls back to the raw value when nothing matches.
    Name,
}

/// One element of a parsed template: rendering walks these in order and
/// concatenates what each produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Text that renders as itself, including `{{` and `}}` unescaped to a
    /// single brace.
    Literal(String),
    /// A `{…}` group: look something up, render it one of two ways, and fall
    /// back to fixed text when it is not there.
    Placeholder {
        reference: Ref,
        fmt: Fmt,
        /// Used when the tag is absent or its value is empty.
        fallback: String,
    },
}

/// A parsed template: a flat sequence of literals and slots.
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
}

/// The schema default (`schema/fix-lsp.schema.json`).
pub const DEFAULT_TEMPLATE: &str =
    "{52|???}: {34|???}: {49|???}=>{56|???} | {35:name|???}{possDup}";

static DEFAULT: LazyLock<Template> =
    LazyLock::new(|| Template::parse(DEFAULT_TEMPLATE).expect("the schema default parses"));

static DEFAULT_SYMBOL: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(crate::config::DEFAULT_SYMBOL_TEMPLATE).expect("the schema default parses")
});

impl Template {
    pub fn default_template() -> &'static Template {
        &DEFAULT
    }

    /// The outline's default, which is not the summary's: a row read in a tree
    /// wants the identifying tags first, without the timestamp that leads a
    /// line-end annotation.
    pub fn default_symbol_template() -> &'static Template {
        &DEFAULT_SYMBOL
    }

    /// Fixed when the template is parsed; the render path asks once per line.
    pub fn mentions_details(&self) -> bool {
        self.mentions_details
    }

    pub fn parse(text: &str) -> Result<Template, TemplateError> {
        let pieces: Vec<Piece> = scan(text, parse_slot)?
            .into_iter()
            .map(|element| match element {
                Element::Literal(text) => Piece::Literal(text),
                Element::Slot(piece) => piece,
            })
            .collect();
        let mentions_details = pieces.iter().any(|piece| {
            matches!(
                piece,
                Piece::Placeholder {
                    reference: Ref::Details,
                    ..
                }
            )
        });
        Ok(Template {
            pieces,
            mentions_details,
        })
    }
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
                let close = at
                    + 1
                    + bytes[at + 1..]
                        .iter()
                        .position(|&b| b == b'}')
                        .ok_or(TemplateError::Unclosed { at: open })?;
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

fn parse_slot(body: &str, at: usize) -> Result<Piece, TemplateError> {
    // The fallback runs to the closing brace, so it may contain `:` — split it
    // off first.
    let (head, fallback) = match body.split_once('|') {
        Some((head, fallback)) => (head, fallback.to_owned()),
        None => (body, String::new()),
    };
    let (name, fmt) = match head.split_once(':') {
        Some((name, fmt)) => (name, Some(fmt)),
        None => (head, None),
    };
    if name.is_empty() {
        return Err(TemplateError::EmptyRef { at });
    }

    let reference = match name {
        "msgName" => Ref::MsgName,
        "sender" => Ref::Sender,
        "target" => Ref::Target,
        "possDup" => Ref::PossDup,
        "details" => Ref::Details,
        digits => digits
            .parse::<u32>()
            .map(Ref::Tag)
            .map_err(|_| TemplateError::UnknownRef {
                name: name.to_owned(),
                at,
            })?,
    };
    let fmt = match fmt {
        None | Some("raw") => Fmt::Raw,
        Some("name") => Fmt::Name,
        Some(other) => {
            return Err(TemplateError::UnknownFmt {
                fmt: other.to_owned(),
                at,
            })
        }
    };
    Ok(Piece::Placeholder {
        reference,
        fmt,
        fallback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{analyze, Ctx};
    use crate::render::summary::summary;
    use crate::render::Options;

    /// Render a template against one line, main part only.
    fn render_with(template: &Template, line: &str) -> String {
        let ctx = Ctx::for_test("FIX.4.4");
        let analysis = analyze(line.as_bytes(), &ctx);
        let message = analysis.message().expect("a message");
        let options = Options {
            template: template.clone(),
            details: false,
            ..Options::default()
        };
        summary(line.as_bytes(), message, &options).main
    }

    #[test]
    fn the_schema_default_parses() {
        let template = Template::parse(DEFAULT_TEMPLATE).unwrap();
        assert!(!template.mentions_details());
        assert_eq!(&template, Template::default_template());
    }

    #[test]
    fn braces_escape() {
        let template = Template::parse("{{35}}").unwrap();
        assert_eq!(render_with(&template, "8=FIX.4.4|35=0"), "{35}");
    }

    #[test]
    fn aliases_and_formats_parse() {
        Template::parse("{msgName} {sender} {target} {possDup} {details} {35:raw} {35:name}")
            .unwrap();
    }

    #[test]
    fn grammar_errors_are_reported_with_offsets() {
        assert!(matches!(
            Template::parse("{35"),
            Err(TemplateError::Unclosed { at: 0 })
        ));
        assert!(matches!(
            Template::parse("{}"),
            Err(TemplateError::EmptyRef { .. })
        ));
        assert!(matches!(
            Template::parse("{nope}"),
            Err(TemplateError::UnknownRef { .. })
        ));
        assert!(matches!(
            Template::parse("{35:weird}"),
            Err(TemplateError::UnknownFmt { .. })
        ));
    }

    #[test]
    fn a_fallback_runs_to_the_closing_brace() {
        let template = Template::parse("{99|no: value}").unwrap();
        assert_eq!(render_with(&template, "8=FIX.4.4|35=0"), "no: value");
    }
}
