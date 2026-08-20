//! The labels drawn beside the bytes.
//!
//! The whole label belongs to the label rather than to whoever draws it —
//! parentheses, spacing and all — because a client renders a hint verbatim and
//! because the shape is a user's to choose (`inlayHints.tagNames.format`).

use crate::codec::{Field, MessageAnalysis};
use crate::config::InlayHints;

use super::Options;

/// How a field's tag is spelled. **The label without the switch that decides
/// whether to draw it**, so a channel that is not a hint — `fix.decode` —
/// spells a field the same way rather than growing a second opinion about what
/// a tag is called.
pub fn tag_label(
    analysis: &MessageAnalysis,
    field: &Field,
    line: &[u8],
    options: &Options,
) -> Option<String> {
    // What a tag is called: its group path inside a group when `group_paths`
    // asks, else the dictionary's name. The format takes it and knows nothing
    // else about the field.
    let path = options
        .group_paths
        .then(|| analysis.group_path(field, line))
        .flatten();
    let name = match &path {
        Some(path) => Some(path.as_str()),
        None => analysis.field_name(field),
    };
    options.tag_format.render(name)
}

/// How a field's value is spelled. Switch-free for the reason [`tag_label`] is.
pub fn value_label(analysis: &MessageAnalysis, field: &Field, options: &Options) -> Option<String> {
    options.value_format.render(analysis.enum_label(field))
}

/// What to draw after a field's tag, or `None` when nothing should be.
///
/// `tag_names` is the gate. What is drawn is the format's, and what goes in
/// its hole is resolved above: `group_paths` chooses between the path and the
/// name, and finding neither suppresses the label the way an undefined tag
/// always did.
pub fn tag(
    analysis: &MessageAnalysis,
    at: usize,
    hints: &InlayHints,
    line: &[u8],
    options: &Options,
) -> Option<String> {
    if !hints.tag_names() {
        return None;
    }
    tag_label(analysis, analysis.fields.get(at)?, line, options)
}

/// What to draw after a field's value.
pub fn value(
    analysis: &MessageAnalysis,
    at: usize,
    hints: &InlayHints,
    options: &Options,
) -> Option<String> {
    if !hints.enum_values() {
        return None;
    }
    value_label(analysis, analysis.fields.get(at)?, options)
}

/// The summary's parts, in the order they are drawn.
///
/// `main` can be empty — the template is a user's to write — and LSP forbids an
/// empty label part, so the four shapes are enumerated rather than assembled:
/// both, main alone, details alone, and neither, which draws nothing at all.
/// The separator is the caller's because `render::summary` deliberately returns
/// the two apart; a channel that joins them must supply it.
pub const SEPARATOR: &str = " | ";

pub fn summary_parts(
    line: &[u8],
    analysis: &MessageAnalysis,
    options: &Options,
) -> Option<Vec<String>> {
    let summary = super::summary(line, analysis, options);
    let main = (!summary.main.is_empty()).then_some(summary.main);
    let details = summary.details.filter(|text| !text.is_empty());
    match (main, details) {
        (Some(main), Some(details)) => Some(vec![main, SEPARATOR.to_owned(), details]),
        (Some(main), None) => Some(vec![main]),
        (None, Some(details)) => Some(vec![details]),
        (None, None) => None,
    }
}

/// The same summary as one string, for a channel that has nowhere to put parts.
pub fn summary_joined(
    line: &[u8],
    analysis: &MessageAnalysis,
    options: &Options,
) -> Option<String> {
    Some(summary_parts(line, analysis, options)?.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::config::Config;
    use crate::ctx::{analyze, Ctx};

    fn all_on() -> InlayHints {
        serde_json::from_value(serde_json::json!({
            "tagNames": { "enabled": true },
            "enumValues": { "enabled": true },
        }))
        .expect("hints")
    }

    fn one(line: &str) -> (Vec<u8>, MessageAnalysis) {
        let bytes = line.as_bytes().to_vec();
        let LineAnalysis::Message(message) = analyze(&bytes, &Ctx::for_test("FIX.4.4")) else {
            panic!("a message");
        };
        (bytes, message)
    }

    fn at(analysis: &MessageAnalysis, tag: u32) -> usize {
        analysis
            .fields
            .iter()
            .position(|field| field.tag == tag)
            .expect("the tag is on the line")
    }

    const HEARTBEAT: &str =
        "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
    /// An order has a second half to its summary; a session message does not.
    const ORDER: &str = "8=FIX.4.4|9=139|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=213|";
    const GROUPS: &str =
        "8=FIX.4.4|9=124|35=W|34=3|49=EXCHANGE|56=CLIENT|52=20251026-09:05:00.000|\
55=BTCUSD|268=2|269=0|270=65000.50|271=1.5|269=1|270=65001.00|271=2.0|10=145|";

    #[test]
    fn a_tag_is_labelled_with_its_name() {
        let (bytes, analysis) = one(HEARTBEAT);
        let index = at(&analysis, 49);
        assert_eq!(
            tag(&analysis, index, &all_on(), &bytes, &Options::default()).as_deref(),
            Some("(SenderCompID)")
        );
    }

    #[test]
    fn a_value_is_labelled_with_its_enum() {
        let (_, analysis) = one(HEARTBEAT);
        let index = at(&analysis, 35);
        assert_eq!(
            value(&analysis, index, &all_on(), &Options::default()).as_deref(),
            Some("(Heartbeat)")
        );
        // 49 has no enum values at all.
        let plain = at(&analysis, 49);
        assert_eq!(
            value(&analysis, plain, &all_on(), &Options::default()),
            None
        );
    }

    /// Inside a repeating group the path replaces the name — one label, not two.
    #[test]
    fn a_field_in_a_group_is_labelled_with_its_path() {
        let (bytes, analysis) = one(GROUPS);
        let index = at(&analysis, 269);
        assert_eq!(
            tag(&analysis, index, &all_on(), &bytes, &Options::default()).as_deref(),
            Some("(NoMDEntries/1/MDEntryType)")
        );
    }

    /// `tagNames` gates and `groupPaths` modifies. Under the schema's own
    /// defaults — tags off, paths on — a field inside a group must draw
    /// nothing; reading the two as independent switches would draw the path.
    ///
    /// The two now arrive from different places — the gate from `InlayHints`,
    /// the modifier from `Options`, which is what lets `fix.decode` honour it —
    /// so the config they are built from is the thing that keeps them agreeing.
    #[test]
    fn group_paths_is_a_modifier_and_not_a_second_gate() {
        let (bytes, analysis) = one(GROUPS);
        let index = at(&analysis, 269);
        let defaults = Config::default();
        assert!(
            !defaults.inlay_hints.tag_names() && defaults.inlay_hints.group_paths(),
            "the shape"
        );
        let rendered = Options::from_config(&defaults).0;
        assert_eq!(
            tag(&analysis, index, &defaults.inlay_hints, &bytes, &rendered),
            None
        );

        let paths_off: Config = serde_json::from_value(serde_json::json!({
            "inlayHints": {
                "tagNames": { "enabled": true },
                "groupPaths": { "enabled": false },
            }
        }))
        .expect("config");
        let rendered = Options::from_config(&paths_off).0;
        assert_eq!(
            tag(&analysis, index, &paths_off.inlay_hints, &bytes, &rendered).as_deref(),
            Some("(MDEntryType)"),
            "annotated, but by name"
        );
    }

    /// A format reaches both channels, and they are two settings rather than
    /// one: wiring both to the same parsed field would pass every test that
    /// only ever changes one of them.
    #[test]
    fn each_channel_reads_its_own_format() {
        let (bytes, analysis) = one(HEARTBEAT);
        let config: Config = serde_json::from_value(serde_json::json!({
            "inlayHints": {
                "tagNames": { "enabled": true, "format": "<{}>" },
                "enumValues": { "enabled": true, "format": " means {}" },
            }
        }))
        .expect("config");
        let rendered = Options::from_config(&config).0;
        let index = at(&analysis, 35);
        assert_eq!(
            tag(&analysis, index, &config.inlay_hints, &bytes, &rendered).as_deref(),
            Some("<MsgType>")
        );
        assert_eq!(
            value(&analysis, index, &config.inlay_hints, &rendered).as_deref(),
            Some(" means Heartbeat")
        );
    }

    #[test]
    fn a_tag_the_dictionary_does_not_know_is_not_labelled() {
        let (bytes, analysis) = one("8=FIX.4.4|9=57|35=0|9999=x|10=171|");
        let index = at(&analysis, 9999);
        assert_eq!(
            tag(&analysis, index, &all_on(), &bytes, &Options::default()),
            None
        );
    }

    /// LSP forbids an empty label part, and the template is the user's to
    /// write, so every shape the summary can take is enumerated — and each of
    /// the four is *constructed* here, rather than allowed for by a disjunction
    /// that any one of them satisfies.
    #[test]
    fn the_summary_takes_one_of_four_shapes() {
        let empty_template = || Options {
            template: crate::render::Template::parse("").expect("an empty template"),
            ..Options::default()
        };

        // Both: an order has details to say. Exactly three parts, and the
        // middle one is the separator this channel does not draw itself.
        let (bytes, analysis) = one(ORDER);
        let parts = summary_parts(&bytes, &analysis, &Options::default()).expect("a summary");
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[1], SEPARATOR);
        assert_eq!(
            summary_joined(&bytes, &analysis, &Options::default()).as_deref(),
            Some(parts.concat().as_str())
        );

        // Details alone: nothing in the template, but an order still has them.
        let parts = summary_parts(&bytes, &analysis, &empty_template()).expect("a summary");
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(!parts[0].is_empty() && parts[0] != SEPARATOR, "{parts:?}");

        // Main alone: a session message has no second half to its summary.
        let (bytes, analysis) = one(HEARTBEAT);
        let parts = summary_parts(&bytes, &analysis, &Options::default()).expect("a summary");
        assert_eq!(parts.len(), 1, "{parts:?}");

        // Neither: an empty template and nothing to detail draws nothing at
        // all, rather than an empty part LSP forbids.
        let nothing = Options {
            details: false,
            ..empty_template()
        };
        assert_eq!(summary_parts(&bytes, &analysis, &nothing), None);
    }
}
