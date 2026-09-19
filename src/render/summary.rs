//! The one-line message summary.
//!
//! Two properties worth stating, because both were decided against a working
//! alternative. `Summary` carries `main` and `details` clean and lets the
//! client join them, because a separator baked into the text is one a client
//! cannot choose. And a missing enum label renders the template's fallback
//! rather than any spelling of "nothing".

use crate::codec::MessageAnalysis;
use crate::tag::{self, exec_type};

use super::template::{Fmt, Piece, Ref, Template};
use super::Options;

const UNKNOWN: &str = "???";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub main: String,
    pub details: Option<String>,
}

/// `line` is the raw bytes the analysis was produced from: `MessageAnalysis`
/// stores spans, not text, so the caller that holds the line supplies it rather
/// than every field carrying a copy. Whether a line is a message at all is the
/// caller's `match`.
pub fn summary(line: &[u8], analysis: &MessageAnalysis, options: &Options) -> Summary {
    let template = &options.template;
    let main = render(line, analysis, template);
    // `details: true` appends the detail as its own part; a template that names
    // `{details}` has already placed it.
    let details = (options.details && !template.mentions_details())
        .then(|| self::details(line, analysis))
        .flatten();
    Summary { main, details }
}

/// One template rendered against one message.
///
/// Shared with the outline, which renders a different template against the same
/// slots: a message's identity is a message's identity wherever it is drawn.
pub fn render(line: &[u8], analysis: &MessageAnalysis, template: &Template) -> String {
    // Rendered on every visible line on every scroll, so the slots write into
    // one buffer rather than each returning a String to be copied and dropped.
    let mut out = String::with_capacity(96);
    for piece in &template.pieces {
        match piece {
            Piece::Literal(text) => out.push_str(text),
            Piece::Placeholder {
                reference,
                fmt,
                fallback,
            } => render_placeholder(&mut out, line, analysis, reference, *fmt, fallback),
        }
    }
    out
}

fn render_placeholder(
    out: &mut String,
    line: &[u8],
    analysis: &MessageAnalysis,
    reference: &Ref,
    fmt: Fmt,
    fallback: &str,
) {
    // `{msgName}` is `{35:name}`, so the alias settles the format here rather
    // than in a second match on the same value.
    let (tag, fmt) = match reference {
        Ref::Tag(tag) => (*tag, fmt),
        Ref::MsgName => (35, Fmt::Name),
        Ref::Sender => (49, fmt),
        Ref::Target => (56, fmt),
        Ref::PossDup => {
            if analysis.is_poss_dup(line) {
                out.push_str(" [POSS DUP]");
            }
            return;
        }
        Ref::Details => {
            if let Some(detail) = details(line, analysis) {
                out.push_str(&detail);
            }
            return;
        }
    };

    let Some(field) = analysis.first(tag) else {
        out.push_str(fallback);
        return;
    };
    if fmt == Fmt::Name {
        if let Some(label) = analysis.enum_label(field) {
            out.push_str(label);
            return;
        }
    }
    // The field is already in hand; going back through `raw` would scan the
    // field vector for the same tag a second time.
    match field.text(line) {
        Some("") | None => out.push_str(fallback),
        Some(value) => out.push_str(value),
    }
}

/// Raw value of the first occurrence of a tag, as text.
fn raw<'a>(line: &'a [u8], analysis: &MessageAnalysis, tag: u32) -> Option<&'a str> {
    analysis.first(tag)?.text(line)
}

/// Per-MsgType detail line.
pub fn details(line: &[u8], analysis: &MessageAnalysis) -> Option<String> {
    let msg_name = analysis.label(tag::MSG_TYPE)?;

    let text_of = |tag: u32| raw(line, analysis, tag);
    let label_of = |tag: u32| analysis.label(tag);
    // `&str`, not `String`: every one of these goes straight into a `format!`,
    // which takes a borrow.
    let or = |tag: u32, missing: &'static str| text_of(tag).unwrap_or(missing);

    let detail = match msg_name {
        "ExecutionReport" => {
            let exec_type = label_of(tag::EXEC_TYPE).unwrap_or(UNKNOWN);
            if matches!(
                text_of(tag::EXEC_TYPE),
                Some(exec_type::TRADE | exec_type::TRADE_CORRECT)
            ) {
                format!(
                    "{exec_type} {}@{} ClOrdId={} ExecId={}",
                    or(tag::ORDER_QTY, UNKNOWN),
                    or(tag::PRICE, "MKT"),
                    text_of(tag::CL_ORD_ID).unwrap_or_default(),
                    or(tag::EXEC_ID, "MISSING"),
                )
            } else {
                exec_type.to_owned()
            }
        }
        "NewOrderSingle" => format!(
            "{} {} {} {} {} @ {}",
            label_of(tag::SIDE).unwrap_or(UNKNOWN),
            label_of(tag::TIME_IN_FORCE).unwrap_or(UNKNOWN),
            label_of(tag::ORD_TYPE).unwrap_or(UNKNOWN),
            or(tag::ORDER_QTY, UNKNOWN),
            or(tag::SYMBOL, UNKNOWN),
            or(tag::PRICE, "MKT"),
        ),
        "Logout" => text_of(tag::TEXT).unwrap_or_default().to_owned(),
        "ResendRequest" => format!(
            "{} - {}",
            or(tag::BEGIN_SEQ_NO, "MISSING"),
            or(tag::END_SEQ_NO, "MISSING")
        ),
        "SequenceReset" => {
            let new_seq = or(tag::NEW_SEQ_NO, "MISSING");
            match text_of(tag::GAP_FILL_FLAG) {
                Some(tag::YES) => format!("Gap Fill to {new_seq}"),
                _ => format!("Reset to {new_seq}"),
            }
        }
        _ => String::new(),
    };
    (!detail.is_empty()).then_some(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::ctx::{analyze, Ctx};
    use crate::render::Template;

    fn options(template: &str, details: bool) -> Options {
        Options {
            template: Template::parse(template).expect("the test template parses"),
            details,
            ..Options::default()
        }
    }

    fn default_options() -> Options {
        Options::default()
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

    fn main_of(line: &str) -> String {
        let (bytes, message) = message_of(line);
        summary(&bytes, &message, &default_options()).main
    }

    fn details_of(line: &str) -> Option<String> {
        let (bytes, message) = message_of(line);
        details(&bytes, &message)
    }

    #[test]
    fn the_default_template_reproduces_the_lua_main_chunk() {
        let (bytes, analysis) =
            one("8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|");
        let (bytes, message) = (bytes, expect(analysis));
        let rendered = summary(&bytes, &message, &default_options());
        assert_eq!(
            rendered.main,
            "20251026-09:02:00.000: 1: CLIENT1=>BROKER1 | Heartbeat"
        );
        assert_eq!(rendered.details, None, "Heartbeat has no details");
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
            details_of("8=FIX.4.4|35=D|54=1|59=0|40=2|38=10|55=BTCUSD|44=65000").as_deref(),
            Some("Buy Day Limit 10 BTCUSD @ 65000")
        );
    }

    #[test]
    fn missing_enum_labels_render_as_question_marks_not_nil() {
        // The fallback, not a spelling of "nothing"; see the module doc.
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
        let (bytes, analysis) = one("8=FIX.4.4|35=5|58=bye");
        let rendered = summary(
            &bytes,
            &expect(analysis),
            &options("{35:name} [{details}]", true),
        );
        assert_eq!(rendered.main, "Logout [bye]");
        assert_eq!(rendered.details, None);
    }

    /// The `details` tests around this one call `details()` directly, so
    /// nothing would notice if `summary` dropped the flag on the floor.
    #[test]
    fn details_off_suppresses_the_second_part() {
        let (bytes, message) = message_of("8=FIX.4.4|35=5|58=bye");
        assert!(
            summary(&bytes, &message, &default_options())
                .details
                .is_some(),
            "the fixture has details to suppress"
        );
        let quiet = Options {
            details: false,
            ..default_options()
        };
        assert_eq!(summary(&bytes, &message, &quiet).details, None);
    }

    #[test]
    fn non_message_lines_have_no_summary() {
        for line in ["", "# c"] {
            let (_, analysis) = one(line);
            assert!(analysis.message().is_none(), "{line:?}");
        }
    }
}
