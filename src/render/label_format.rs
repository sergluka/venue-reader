//! How a label is written: literal text and a nameless hole, `{}`.
//!
//! **The hole has no name, because each setting draws exactly one thing.**
//! `inlayHints.tagNames.format` draws what a tag is called;
//! `.enumValues.format` draws what a value means. Naming that would be naming
//! "the thing this setting is about", and a named vocabulary let a value format
//! ask for a field's name — which parsed, drew nonsense, and nothing caught.
//!
//! So this knows nothing about FIX: the caller resolves what goes in the hole.
//!
//! **An empty value suppresses the whole label**, its literals included, which
//! is what the hard-coded `({name})` did for a tag the dictionary does not
//! define. A format with no hole has nothing that can come up empty, so it
//! draws on every field — `enumValues.format = "!"` annotates values that match
//! no enum. That widens what the toggle means, deliberately.
//!
//! The syntax is `template`'s, so `{{` and `}}` write literal braces and
//! `{{}}` is a literal `{}`.
//!
//! A format may hold the hole more than once, and each one repeats the same
//! value. Not refused, because it cannot surprise: `{name}` is refused for
//! asking after something this grammar does not have, while `{}{}` asks twice
//! after something it does.

use std::sync::LazyLock;

use super::template::{scan, Element, TemplateError};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    /// `{}` — whatever the caller resolved.
    Hole,
}

/// Cloned and compared because `render::Options` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelFormat {
    pieces: Vec<Piece>,
}

/// The schema default for both settings, and it is the same string: they draw
/// different things, in the same shape. Reproduces the labels as they read
/// before they could be configured.
pub const DEFAULT_LABEL_FORMAT: &str = "({})";

static DEFAULT: LazyLock<LabelFormat> =
    LazyLock::new(|| LabelFormat::parse(DEFAULT_LABEL_FORMAT).expect("the schema default parses"));

impl LabelFormat {
    pub fn default_label() -> &'static LabelFormat {
        &DEFAULT
    }

    pub fn parse(text: &str) -> Result<LabelFormat, TemplateError> {
        let pieces = scan(text, parse_slot)?
            .into_iter()
            .map(|element| match element {
                Element::Literal(text) => Piece::Literal(text),
                Element::Slot(()) => Piece::Hole,
            })
            .collect();
        Ok(LabelFormat { pieces })
    }

    /// The label, or `None` when there is none to draw.
    pub fn render(&self, value: Option<&str>) -> Option<String> {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => out.push_str(text),
                Piece::Hole => match value {
                    Some(value) if !value.is_empty() => out.push_str(value),
                    _ => return None,
                },
            }
        }
        // A format of nothing but empty literals has nothing to say either.
        (!out.is_empty()).then_some(out)
    }
}

fn parse_slot(body: &str, at: usize) -> Result<(), TemplateError> {
    // Named rather than ignored, so a format written against the old
    // vocabulary says what it got wrong instead of drawing nothing.
    if body.is_empty() {
        Ok(())
    } else {
        Err(TemplateError::UnknownRef {
            name: body.to_owned(),
            at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(format: &str, value: Option<&str>) -> Option<String> {
        LabelFormat::parse(format)
            .expect("the format parses")
            .render(value)
    }

    #[test]
    fn the_hole_takes_whatever_the_caller_resolved() {
        assert_eq!(draw("({})", Some("MsgType")).as_deref(), Some("(MsgType)"));
        assert_eq!(draw(" is {}", Some("Bid")).as_deref(), Some(" is Bid"));
        assert_eq!(
            draw("<{}>", Some("NoMDEntries/1/MDEntryType")).as_deref(),
            Some("<NoMDEntries/1/MDEntryType>"),
            "a group path is one value, not parts"
        );
    }

    /// The rule the whole grammar rests on, since there are no fallbacks.
    #[test]
    fn an_empty_value_suppresses_the_label_and_its_literals() {
        assert_eq!(draw("({})", None), None);
        assert_eq!(draw("({})", Some("")), None);
    }

    /// Nothing can come up empty, so nothing suppresses: the toggle widens from
    /// "label the enums" to "draw this after every value".
    #[test]
    fn a_format_with_no_hole_draws_whatever_the_value_is() {
        assert_eq!(draw("!", None).as_deref(), Some("!"));
        assert_eq!(draw("", Some("x")), None, "and an empty one never draws");
    }

    /// Not refused: every hole is the same value, so repeating it is only
    /// repetition. `{name}` is refused because it names something that does
    /// not exist; a second `{}` does not.
    #[test]
    fn more_than_one_hole_repeats_the_value() {
        assert_eq!(draw("{}{}", Some("Ab")).as_deref(), Some("AbAb"));
        assert_eq!(draw("a{}b{}c", Some("X")).as_deref(), Some("aXbXc"));
        assert_eq!(draw("{}{}", None), None, "and one empty is all empty");
    }

    #[test]
    fn braces_are_written_by_doubling_them() {
        assert_eq!(draw("{{{}}}", Some("x")).as_deref(), Some("{x}"));
        assert_eq!(draw("{{}}", Some("x")).as_deref(), Some("{}"));
        assert_eq!(draw("a}b", Some("x")).as_deref(), Some("a}b"));
    }

    /// The names the first draft had, and the summary template's suffixes, are
    /// all one error: this grammar has one hole and no vocabulary.
    #[test]
    fn anything_inside_the_braces_is_an_unknown_reference() {
        for body in ["name", "tag", "path", "label", "35", "name:raw"] {
            assert_eq!(
                LabelFormat::parse(&format!("({{{body}}})")),
                Err(TemplateError::UnknownRef {
                    name: body.to_owned(),
                    at: 1
                }),
                "{body}"
            );
        }
    }

    #[test]
    fn an_unclosed_brace_says_where() {
        assert_eq!(
            LabelFormat::parse("ab{"),
            Err(TemplateError::Unclosed { at: 2 })
        );
    }

    #[test]
    fn the_schema_default_is_the_label_as_it_always_read() {
        assert_eq!(
            draw(DEFAULT_LABEL_FORMAT, Some("MsgSeqNum")).as_deref(),
            Some("(MsgSeqNum)")
        );
        assert_eq!(draw(DEFAULT_LABEL_FORMAT, None), None);
        assert_eq!(
            &LabelFormat::parse(DEFAULT_LABEL_FORMAT).unwrap(),
            LabelFormat::default_label()
        );
    }
}
