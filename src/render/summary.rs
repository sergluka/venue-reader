//! Render summaries from source-owned analysis without retaining rendered text.
//!
//! One renderer, two outputs: a plain string for CodeLens and the outline, and
//! typed parts for inlay hints. Only the parts output pays for styles.

use std::borrow::Cow;
use std::sync::LazyLock;

use super::template::{Fmt, Piece, Template};
use super::tokens::{self, Kind, LineMods, Style};
use super::Options;
use crate::codec::{Field, MessageAnalysis};
use crate::dictionary::{Standard, Version};

/// Between the main template and the appended detail, when both are nonempty.
pub const SEPARATOR: &str = " | ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPart {
    pub value: String,
    /// `None` is base styling: literals, fallbacks, and every part when
    /// metadata was not requested.
    pub style: Option<Style>,
}

/// Where rendered text goes. The style is a closure so that the plain string
/// never computes one.
trait Output {
    fn write(&mut self, text: &str, style: impl FnOnce() -> Option<Style>);
    fn len(&self) -> usize;
    fn truncate(&mut self, len: usize);
}

impl Output for String {
    fn write(&mut self, text: &str, _: impl FnOnce() -> Option<Style>) {
        self.push_str(text);
    }
    fn len(&self) -> usize {
        String::len(self)
    }
    fn truncate(&mut self, len: usize) {
        String::truncate(self, len);
    }
}

impl Output for Vec<SummaryPart> {
    fn write(&mut self, text: &str, style: impl FnOnce() -> Option<Style>) {
        // LSP forbids an empty label part.
        if !text.is_empty() {
            self.push(SummaryPart {
                value: text.to_owned(),
                style: style(),
            });
        }
    }
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }
}

/// The summary as one string, for the channel that cannot keep parts apart.
/// `None` when there is nothing to say.
pub fn summary_joined(
    line: &[u8],
    analysis: &MessageAnalysis,
    options: &Options,
) -> Option<String> {
    let mut out = String::with_capacity(96);
    Scene::new(line, analysis, false).summary(&mut out, options);
    (!out.is_empty()).then_some(out)
}

/// The summary as label parts, carrying styles when `styled`.
pub fn structured(
    line: &[u8],
    analysis: &MessageAnalysis,
    options: &Options,
    styled: bool,
) -> Vec<SummaryPart> {
    let mut out = Vec::with_capacity(16);
    Scene::new(line, analysis, styled).summary(&mut out, options);
    out
}

/// One template against one message. Shared with the outline, which renders a
/// different template against the same slots.
pub fn render(line: &[u8], analysis: &MessageAnalysis, template: &Template) -> String {
    let mut out = String::with_capacity(96);
    Scene::new(line, analysis, false).render(&mut out, template, None);
    out
}

/// One message, and what every slot rendered against it shares.
struct Scene<'a> {
    line: &'a [u8],
    analysis: &'a MessageAnalysis,
    /// `Some` when parts carry styles; the plain output never asks.
    mods: Option<LineMods>,
}

impl<'a> Scene<'a> {
    fn new(line: &'a [u8], analysis: &'a MessageAnalysis, styled: bool) -> Self {
        Self {
            line,
            analysis,
            mods: styled.then(|| tokens::line_modifiers(line, analysis)),
        }
    }

    /// The main template, then the built-in detail behind a separator when
    /// both say something.
    fn summary(&self, out: &mut impl Output, options: &Options) {
        self.render(out, &options.template, None);
        if !options.details || options.template.mentions_details() {
            return;
        }
        let Some(template) = detail_template(self.analysis) else {
            return;
        };
        let main = out.len();
        if main > 0 {
            out.write(SEPARATOR, || None);
        }
        let before = out.len();
        self.render(out, template, None);
        if out.len() == before {
            out.truncate(main);
        }
    }

    /// `literal` is the style of the literals in this template: the marker
    /// style of a selected flag branch, else none.
    fn render(&self, out: &mut impl Output, template: &Template, literal: Option<Style>) {
        for piece in &template.pieces {
            match piece {
                Piece::Literal(text) => out.write(text, || literal),
                Piece::Details => {
                    if let Some(template) = detail_template(self.analysis) {
                        self.render(out, template, None);
                    }
                }
                Piece::Placeholder { tag, fmt, fallback } => {
                    self.placeholder(out, *tag, *fmt, fallback.as_deref(), literal)
                }
                Piece::Condition {
                    tag,
                    values,
                    then,
                    otherwise,
                } => {
                    let field = self.analysis.first(*tag);
                    let selected = match values {
                        None => field.is_some(),
                        Some(values) => field.is_some_and(|field| {
                            values
                                .iter()
                                .any(|value| value.as_bytes() == field.value(self.line))
                        }),
                    };
                    // A selected singleton equality names the value exactly, so
                    // the classifier can say whether it is a marker. Presence
                    // and lists with alternatives do not.
                    let marker =
                        selected && values.as_ref().is_some_and(|values| values.len() == 1);
                    let style = marker
                        .then(|| field.and_then(|field| self.value_style(field)))
                        .flatten()
                        .filter(|style| style.kind == Kind::Flag)
                        .or(literal);
                    self.render(out, if selected { then } else { otherwise }, style);
                }
            }
        }
    }

    fn placeholder(
        &self,
        out: &mut impl Output,
        tag: u32,
        fmt: Fmt,
        fallback: Option<&Template>,
        literal: Option<Style>,
    ) {
        let field = self.analysis.first(tag);
        match fmt {
            Fmt::Tag => out.write(&tag.to_string(), || self.property_style(field)),
            Fmt::TagName => match self.tag_name(field, tag) {
                Some(name) => out.write(&name, || self.property_style(field)),
                None => match fallback {
                    Some(fallback) => self.render(out, fallback, literal),
                    None => out.write(&tag.to_string(), || self.property_style(field)),
                },
            },
            Fmt::Raw | Fmt::Name => {
                let value = field
                    .and_then(|field| Some((field, self.value(field, fmt, fallback.is_some())?)));
                match (value, fallback) {
                    (Some((field, value)), _) => out.write(value, || self.value_style(field)),
                    (None, Some(fallback)) => self.render(out, fallback, literal),
                    (None, None) => {}
                }
            }
        }
    }

    /// What a `.value` or `.valueName` slot shows; `None` hands over to the
    /// fallback.
    fn value(&self, field: &Field, fmt: Fmt, has_fallback: bool) -> Option<&'a str> {
        let raw = field.text(self.line).filter(|raw| !raw.is_empty())?;
        if fmt == Fmt::Name {
            if let Some(label) = super::value_name(self.analysis, field) {
                return Some(label);
            }
            // An unlisted value of a closed enum is the one wrong value a
            // fallback replaces. Without a dictionary verdict the raw spelling
            // stands, fallback or not.
            if has_fallback && self.analysis.enum_closed(field) {
                return None;
            }
        }
        Some(raw)
    }

    /// The field's own name, else its standard name marked as foreign.
    fn tag_name(&self, field: Option<&Field>, tag: u32) -> Option<Cow<'a, str>> {
        let own = match field {
            Some(field) => self.analysis.field_name(field),
            None => self.analysis.absent_field_name(tag),
        };
        if let Some(own) = own {
            return Some(Cow::Borrowed(own));
        }
        // Only the standard identity the line is known to have: a field in hand
        // says which, an absent tag has only the version to go by.
        let standing = match field {
            Some(field) => self.analysis.standard(field),
            None => Standard::of_tag(Version::parse(&self.analysis.version), tag),
        };
        super::hint::foreign_name(&standing, tag).map(Cow::Owned)
    }

    fn value_style(&self, field: &Field) -> Option<Style> {
        self.mods
            .as_ref()
            .map(|mods| tokens::value_style(self.line, self.analysis, field, mods))
    }

    /// `.tag` and `.tagName` are properties even when the field is absent.
    fn property_style(&self, field: Option<&Field>) -> Option<Style> {
        self.mods.as_ref().map(|mods| Style {
            kind: Kind::Property,
            mods: field.map_or(0, |field| {
                tokens::field_modifiers(self.analysis, field, mods)
            }),
        })
    }
}

/// The raw value when present, `missing` when absent, and nothing when present
/// but empty: the distinction the old renderers drew with raw-field access.
fn keep_empty(tag: u32, missing: &str) -> String {
    format!(r#"{{{tag}=""?"":{{{tag}?:{missing}}}}}"#)
}

/// The built-in detail for the resolved message definition, selected by name.
fn detail_template(analysis: &MessageAnalysis) -> Option<&'static Template> {
    static BUILTIN: LazyLock<[(&str, Template); 5]> = LazyLock::new(|| {
        let trade = format!(
            " {}@{} ClOrdId={{11}} ExecId={}",
            keep_empty(38, "???"),
            keep_empty(44, "MKT"),
            keep_empty(17, "MISSING"),
        );
        [
            (
                "ExecutionReport",
                format!("{{150.valueName?:???}}{{150 in F,G?{trade}}}"),
            ),
            (
                "NewOrderSingle",
                format!(
                    "{{54.valueName?:???}} {{59.valueName?:???}} {{40.valueName?:???}} {} {} @ {}",
                    keep_empty(38, "???"),
                    keep_empty(55, "???"),
                    keep_empty(44, "MKT"),
                ),
            ),
            ("Logout", "{58}".to_owned()),
            (
                "ResendRequest",
                format!(
                    "{} - {}",
                    keep_empty(7, "MISSING"),
                    keep_empty(16, "MISSING")
                ),
            ),
            (
                "SequenceReset",
                format!(
                    "{{123=Y?Gap Fill to :Reset to }}{}",
                    keep_empty(36, "MISSING")
                ),
            ),
        ]
        .map(|(name, source)| {
            let template = Template::parse(&source).expect("built-in detail template");
            (name, template)
        })
    });
    let name = analysis.message_definition()?.name();
    BUILTIN
        .iter()
        .find(|(message, _)| *message == name)
        .map(|(_, template)| template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::ctx::{analyze, Ctx};
    use crate::render::{Template, DEFAULT_TEMPLATE};

    const HEARTBEAT: &str =
        "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
    const ORDER: &str = "8=FIX.4.4|35=D|54=1|59=0|40=2|38=10|55=BTCUSD|44=65000|";

    fn options(template: &str, details: bool) -> Options {
        Options {
            template: Template::parse(template).expect("the test template parses"),
            details,
            ..Options::default()
        }
    }

    /// The line's bytes and its analysis; the caller unwraps the message.
    fn one(line: &str) -> (Vec<u8>, LineAnalysis) {
        let bytes = line.as_bytes().to_vec();
        let analysis = analyze(&bytes, &Ctx::for_test("FIX.4.4"));
        (bytes, analysis)
    }

    fn expect(analysis: LineAnalysis) -> MessageAnalysis {
        match analysis {
            LineAnalysis::Message(message) => message,
            other => panic!("expected a message, got {:?}", other.kind()),
        }
    }

    fn message_of(line: &str) -> (Vec<u8>, MessageAnalysis) {
        let (bytes, analysis) = one(line);
        (bytes, expect(analysis))
    }

    /// The default template alone, without the appended detail.
    fn main_of(line: &str) -> String {
        let (bytes, message) = message_of(line);
        render(&bytes, &message, Template::default_template())
    }

    fn details_of(line: &str) -> Option<String> {
        let (bytes, message) = message_of(line);
        let text = render(&bytes, &message, detail_template(&message)?);
        (!text.is_empty()).then_some(text)
    }

    fn with_template(source: &str, line: &str) -> String {
        let (bytes, message) = message_of(line);
        render(&bytes, &message, &Template::parse(source).unwrap())
    }

    fn text_of(parts: &[SummaryPart]) -> String {
        parts.iter().map(|part| part.value.as_str()).collect()
    }

    #[test]
    fn the_default_template_reproduces_the_lua_main_chunk() {
        let (bytes, message) = message_of(HEARTBEAT);
        assert_eq!(
            summary_joined(&bytes, &message, &Options::default()).as_deref(),
            Some("20251026-09:02:00.000: 1: CLIENT1=>BROKER1 | Heartbeat"),
            "Heartbeat has no details"
        );
    }

    #[test]
    fn absent_tags_use_their_fallbacks() {
        assert_eq!(main_of("8=FIX.4.4"), "???: ???: ???=>??? | ???");
    }

    #[test]
    fn poss_dup_is_appended() {
        assert!(main_of("8=FIX.4.4|35=0|43=Y").ends_with(" [POSS DUP]"));
        assert!(!main_of("8=FIX.4.4|35=0|43=N").ends_with(" [POSS DUP]"));
    }

    #[test]
    fn new_order_single_details() {
        assert_eq!(
            details_of(ORDER).as_deref(),
            Some("Buy Day Limit 10 BTCUSD @ 65000")
        );
    }

    #[test]
    fn missing_enum_labels_render_as_question_marks_not_nil() {
        assert_eq!(
            details_of("8=FIX.4.4|35=D|54=Z|59=Z|40=Z|55=BTCUSD").as_deref(),
            Some("??? ??? ??? ??? BTCUSD @ MKT")
        );
    }

    #[test]
    fn execution_report_details_both_branches() {
        // FIX.4.4 ExecType F is `Trade`; `Fill` is the 4.2 spelling of 150=2,
        // and the branch is chosen by value so both dictionaries reach it.
        assert_eq!(
            details_of("8=FIX.4.4|35=8|150=F|38=100|44=100.25|11=c1|17=e1").as_deref(),
            Some("Trade 100@100.25 ClOrdId=c1 ExecId=e1")
        );
        assert_eq!(details_of("8=FIX.4.4|35=8|150=0").as_deref(), Some("New"));
        assert_eq!(details_of("8=FIX.4.4|35=8").as_deref(), Some("???"));
    }

    #[test]
    fn logout_resend_and_sequence_reset_details() {
        assert_eq!(details_of("8=FIX.4.4|35=5|58=bye").as_deref(), Some("bye"));
        assert_eq!(
            details_of("8=FIX.4.4|35=2|7=1|16=9").as_deref(),
            Some("1 - 9")
        );
        assert_eq!(
            details_of("8=FIX.4.4|35=2|7=1").as_deref(),
            Some("1 - MISSING")
        );
        assert_eq!(
            details_of("8=FIX.4.4|35=4|123=Y|36=7").as_deref(),
            Some("Gap Fill to 7")
        );
        assert_eq!(
            details_of("8=FIX.4.4|35=4|36=7").as_deref(),
            Some("Reset to 7")
        );
    }

    #[test]
    fn a_repeated_tag_resolves_to_its_first_occurrence() {
        assert!(main_of("8=FIX.4.4|35=0|34=1|34=2").contains(": 1: "));
    }

    #[test]
    fn a_details_placeholder_suppresses_the_appended_part() {
        let (bytes, message) = message_of("8=FIX.4.4|35=5|58=bye");
        assert_eq!(
            summary_joined(
                &bytes,
                &message,
                &options("{35.valueName} [{details}]", true)
            )
            .as_deref(),
            Some("Logout [bye]")
        );
    }

    /// LSP forbids an empty label part, and the template is the user's to
    /// write, so every shape the summary can take is constructed here rather
    /// than allowed for by a disjunction that any one of them satisfies.
    #[test]
    fn the_summary_takes_one_of_four_shapes() {
        let empty = || options("", true);
        let separators =
            |parts: &[SummaryPart]| parts.iter().filter(|part| part.value == SEPARATOR).count();

        // Both: an order has details to say, behind one more separator than
        // the default template spells itself.
        let (bytes, message) = message_of(ORDER);
        let parts = structured(&bytes, &message, &Options::default(), false);
        let main_only = structured(&bytes, &message, &options(DEFAULT_TEMPLATE, false), false);
        assert_eq!(separators(&parts), separators(&main_only) + 1, "{parts:?}");
        assert!(parts.iter().all(|part| !part.value.is_empty()));
        assert_eq!(
            summary_joined(&bytes, &message, &Options::default()),
            Some(text_of(&parts))
        );

        // Details alone: nothing in the template, but an order still has them,
        // and no separator leads them.
        let parts = structured(&bytes, &message, &empty(), false);
        assert_eq!(separators(&parts), 0, "{parts:?}");
        assert_eq!(Some(text_of(&parts)), details_of(ORDER));

        // Main alone: a session message has no second half to its summary.
        let (bytes, message) = message_of(HEARTBEAT);
        let parts = structured(&bytes, &message, &Options::default(), false);
        assert_eq!(text_of(&parts), main_of(HEARTBEAT));

        // Neither: nothing at all, rather than an empty part LSP forbids.
        let nothing = options("", false);
        assert!(structured(&bytes, &message, &nothing, false).is_empty());
        assert_eq!(summary_joined(&bytes, &message, &nothing), None);
    }

    #[test]
    fn non_messages_have_no_summary() {
        for line in ["", "# a comment", "not fix at all"] {
            let (_, analysis) = one(line);
            assert!(analysis.message().is_none(), "{line:?}");
        }
    }

    #[test]
    fn selectors_and_fallbacks_distinguish_resolution_from_presence() {
        let line = "8=FIX.4.4|35=D|54=1|38=0|43=N|58=|";
        assert_eq!(
            with_template("{54.tag}/{54.tagName}/{54.value}/{54.valueName}/{54}", line),
            "54/Side/1/Buy/1"
        );
        assert_eq!(
            with_template(
                r#"{38?:"bad"}/{43?:"bad"}/{58?:"empty"}/{99999?:"absent"}"#,
                line
            ),
            "0/N/empty/absent"
        );
        assert_eq!(
            with_template(
                r#"{58?"present":"absent"}/{58=""?"empty":"nonempty"}/{99999=""?"empty":"absent"}"#,
                line
            ),
            "present/empty/absent"
        );
        assert_eq!(
            with_template("{54.tag}/{54.tagName}", "8=FIX.4.4|35=D|"),
            "54/Side"
        );
        assert_eq!(
            with_template(
                r#"{54.valueName?:"UNKNOWN"}/{54.valueName}"#,
                "8=FIX.4.4|35=D|54=Z|"
            ),
            "UNKNOWN/Z"
        );
        assert_eq!(
            with_template(r#"{35.valueName?:"UNKNOWN"}"#, "8=FIX.4.4|35=ZZZZ|"),
            "UNKNOWN"
        );
        assert_eq!(
            with_template(r#"{99999.tag}/{99999.tagName?:"custom"}"#, line),
            "99999/custom"
        );
        assert_eq!(with_template(r#"{54?:""}"#, "8=FIX.4.4|35=D|"), "");
    }

    #[test]
    fn quoted_branches_preserve_delimiters_unicode_and_nested_templates() {
        assert_eq!(
            with_template(
                r#"{150 in "F","G"?"{38?:\"???\"}@{44?:\"MKT\"}":"other"}"#,
                "8=FIX.4.4|35=8|150=F|38=10|"
            ),
            "10@MKT"
        );
        assert_eq!(
            with_template(r#"{54="1"?"{{x}}:|?,\"✓\"":"no"}"#, "8=FIX.4.4|35=D|54=1|"),
            "{x}:|?,\"✓\""
        );
        assert_eq!(
            with_template(r#"{54?:"{38?:\"none\"}"}"#, "8=FIX.4.4|35=D|38=10|"),
            "10"
        );
        assert_eq!(
            with_template(r#"{54 in "1","2"?"yes"}"#, "8=FIX.4.4|35=D|54=3|"),
            ""
        );
    }

    #[test]
    fn undecodable_values_are_present_but_do_not_equal_unicode() {
        let line = b"8=FIX.4.4|35=D|54=\xff|";
        let analysis = analyze(line, &Ctx::for_test("FIX.4.4"));
        assert_eq!(
            render(
                line,
                analysis.message().unwrap(),
                &Template::parse(
                    r#"{54?"present":"absent"}/{54="�"?"equal":"different"}/{54?:"fallback"}"#
                )
                .unwrap()
            ),
            "present/different/fallback"
        );
    }

    #[test]
    fn unavailable_and_extensible_enum_sets_keep_nonempty_raw_values() {
        for field in [
            r#"<field number="54" name="Side" type="CHAR"/>"#,
            r#"<field number="54" name="Side" type="CHAR" allowOtherValues="true"><value enum="1" description="Buy"/></field>"#,
        ] {
            let dictionary = crate::dictionary::parse::fixtures::quickfix("FIX.4.4", field);
            let context = Ctx::for_test_with_dict("FIX.4.4", dictionary);
            let line = b"8=FIX.4.4|35=D|54=Z|";
            let analysis = analyze(line, &context);
            assert_eq!(
                render(
                    line,
                    analysis.message().unwrap(),
                    &Template::parse(r#"{54.valueName?:"UNKNOWN"}"#).unwrap()
                ),
                "Z"
            );
        }
        let line = b"8=FIXT.1.1|35=D|";
        let analysis = analyze(line, &Ctx::for_test("FIX.4.4"));
        let message = analysis.message().unwrap();
        assert_eq!(
            render(
                line,
                message,
                &Template::parse(r#"{35.valueName?:"UNKNOWN"}/{54.tagName}/{1128.tagName}"#)
                    .unwrap()
            ),
            "D/Side?/ApplVerID",
            "no application: the standard name is marked, the transport's is owned"
        );
    }

    #[test]
    fn structured_text_matches_plain_and_only_active_marker_literals_are_flags() {
        let (bytes, message) = message_of("8=FIX.4.4|35=8|43=Y|150=F|38=10|44=5|49=ALPHA|56=BETA|");
        let options = options(
            r#"{sender}=>{target}{43="Y"?" [POSS DUP]{38}"}{43 in "Y"?" also"}{43?" present"}{43 in "Y","N"?" boolean"}{43="N"?"":" not N"}"#,
            true,
        );
        let parts = structured(&bytes, &message, &options, true);
        assert_eq!(
            Some(text_of(&parts)),
            summary_joined(&bytes, &message, &options)
        );
        assert!(parts.iter().all(|part| !part.value.is_empty()));
        let code_of = |value: &str| {
            parts
                .iter()
                .find(|part| part.value == value)
                .unwrap_or_else(|| panic!("{value:?} in {parts:?}"))
                .style
                .map(|style| style.kind.code())
        };
        assert_eq!(code_of(" [POSS DUP]"), Some('B'));
        assert_eq!(code_of(" also"), Some('B'));
        assert_eq!(code_of("10"), Some('Q'), "a placeholder keeps its own type");
        assert_eq!(code_of(" present"), None);
        assert_eq!(code_of(" boolean"), None);
        assert_eq!(code_of(" not N"), None);
        assert_eq!(code_of("ALPHA"), Some('S'));
        let mods_of = |value: &str| {
            parts
                .iter()
                .find(|part| part.value == value)
                .unwrap()
                .style
                .unwrap()
                .mods
        };
        assert_ne!(mods_of("ALPHA") & tokens::Mod::Peer1.bit(), 0);
        assert_ne!(mods_of("BETA") & tokens::Mod::Peer2.bit(), 0);
        assert!(structured(&bytes, &message, &options, false)
            .iter()
            .all(|part| part.style.is_none()));
    }

    #[test]
    fn builtin_raw_values_preserve_empty_values_and_explicit_details_once() {
        assert_eq!(
            details_of("8=FIX.4.4|35=8|150=F|38=|44=|17=|").as_deref(),
            Some("Trade @ ClOrdId= ExecId=")
        );
        let (line, message) = message_of("8=FIX.4.4|35=5|58=bye|");
        assert_eq!(
            text_of(&structured(
                &line,
                &message,
                &options(r#"{58?"{details}"}"#, true),
                true
            )),
            "bye"
        );
    }
    #[test]
    fn optional_quotes_keep_quoted_and_bare_templates_equivalent() {
        for (bare, quoted, expected) in [
            (
                "{54=1?{54.valueName?:UNKNOWN}:other}",
                r#"{54="1"?"{54.valueName?:\"UNKNOWN\"}":"other"}"#,
                "Buy",
            ),
            (
                "{54 in 1,2?Side {54.valueName}:other}",
                r#"{54 in "1","2"?"Side {54.valueName}":"other"}"#,
                "Side Buy",
            ),
            (
                "{99999?:{58?:missing}}",
                r#"{99999?:"{58?:\"missing\"}"}"#,
                "missing",
            ),
            (
                r#"{54=1?"Side: {54.valueName}":other}"#,
                r#"{54="1"?"Side: {54.valueName}":"other"}"#,
                "Side: Buy",
            ),
            (
                "{54=1? before {54.valueName} after :other}",
                r#"{54="1"?" before {54.valueName} after ":"other"}"#,
                " before Buy after ",
            ),
            (
                "{54=2?wrong: no match }",
                r#"{54="2"?"wrong":" no match "}"#,
                " no match ",
            ),
        ] {
            let line = "8=FIX.4.4|35=D|54=1|";
            assert_eq!(with_template(bare, line), expected, "{bare}");
            assert_eq!(with_template(quoted, line), expected, "{quoted}");
        }
        assert_eq!(with_template("{54=2?absent}", ORDER), "");
        assert_eq!(
            with_template(r#"{58=""?empty:nonempty}"#, "8=FIX.4.4|35=D|58=|"),
            "empty"
        );
        assert_eq!(
            with_template("{43=Y? [POSS DUP]}", "8=FIX.4.4|35=D|43=Y|"),
            " [POSS DUP]"
        );
    }
}
