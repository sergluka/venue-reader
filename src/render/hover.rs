//! What the dictionary — or, failing it, standard FIX — knows about one field, as markdown.

use std::fmt::Write as _;

use crate::codec::MessageAnalysis;
use crate::dictionary::{
    bundled, reference_message_source, reference_source, standard, Dict, EnumIdx, FieldIdx,
    Lifecycle, MessageView, Standard, Version,
};
use crate::tag;

use super::Options;

/// Source-local indices are resolved only against the analysis that owns them.
pub fn hover(
    line: &[u8],
    analysis: &MessageAnalysis,
    at: usize,
    options: &Options,
    label: Option<&str>,
) -> Option<String> {
    let field = analysis.fields.get(at)?;
    let dict = analysis.field_dict(field);
    let version = analysis.field_version(field);
    let against = version.and_then(Version::parse);
    let source = match (dict, field.def()) {
        (Some(dict), Some(index)) => Source::Defined(dict, index),
        _ => Source::Standard,
    };
    let About {
        paragraphs,
        standing,
    } = about(
        source,
        field.tag,
        against,
        analysis.standard(field),
        label,
        dict.is_some_and(|dict| !dict.is_static()),
    )?;

    let message = (field.tag == tag::MSG_TYPE)
        .then(|| analysis.message_definition())
        .flatten();
    let label = field
        .enum_value()
        .filter(|_| field.tag != tag::MSG_TYPE || message.is_some());
    let mut value = value_paragraph(source, field.value(line), label, message);
    if let Some(path) = analysis.group_path(field, line) {
        value.push(format!("Group: `{path}`"));
    }

    let link = link(options, version, field.tag, &standing);
    Some(sections(paragraphs, value, link))
}

/// What `fix.describeTag` says about `tag`, or `None` when neither a bundled
/// edition nor FIX Latest defines it.
///
/// No version to measure against: the question is what the tag means, not
/// whether some edition has it. So the reference source answers: FIXT 1.1 for
/// a transport field, else the newest edition that defines the tag — the
/// current meaning, and the fullest code set. The tag's own lifespan says when
/// it arrived and, where it left, when it left. Hover's "not in this version"
/// note has no subject here and is not printed.
pub fn describe(tag: u32, value: Option<&str>, options: &Options) -> Option<String> {
    let answering = reference_source(tag);
    // Bound here rather than in the `match`: a `Dict` built inside an arm is
    // dropped at its end, and `Source` borrows it.
    let dict = answering.map(|version| Dict::Static(bundled(version)));
    let idx = dict.as_ref().and_then(|dict| dict.field_idx(tag));
    let source = match (&dict, idx) {
        (Some(dict), Some(idx)) => Source::Defined(dict, idx),
        // Only FIX Latest knows the tag: a name, and nothing a dictionary
        // would vouch for.
        _ => Source::Standard,
    };

    let standing = dict.as_ref().map_or_else(
        || Standard::of_tag(None, tag),
        |dict| Standard::of_source(dict, tag),
    );
    let About { mut paragraphs, .. } = about(source, tag, None, standing, None, false)?;
    // When it arrived is the one fact FIX Latest alone can vouch for. Not in
    // `about`, which hover also calls without a version — for a line whose
    // version has no bundled tables — and which must not grow a line there.
    if let Standard::Unbundled(def) = Standard::of_tag(None, tag) {
        paragraphs.extend(lifecycle(&Lifecycle {
            added: def.introduced(),
            ..Lifecycle::default()
        }));
    }

    let label = match source {
        Source::Defined(dict, idx) => value.and_then(|value| dict.enum_idx(idx, value)),
        Source::Standard => None,
    };
    let message_dict = value
        .filter(|_| tag == tag::MSG_TYPE)
        .and_then(reference_message_source)
        .map(|version| Dict::Static(bundled(version)));
    let message = message_dict
        .as_ref()
        .and_then(|dict| Some(dict.message(dict.message_idx(value?)?)));
    let value = value_paragraph(
        source,
        value.map(str::as_bytes).unwrap_or_default(),
        label.filter(|_| tag != tag::MSG_TYPE || message.is_some()),
        message,
    );
    let link = link(options, answering.map(Version::as_str), tag, &standing);
    Some(sections(paragraphs, value, link))
}

/// Where a field's facts come from: the dictionary that defines its tag, or
/// standard FIX alone, which gives a name and nothing a dictionary would
/// vouch for.
#[derive(Clone, Copy)]
enum Source<'a> {
    Defined(&'a Dict, FieldIdx),
    Standard,
}

/// The dictionary half of an answer, and the standing its caller needs for the
/// reference link.
struct About {
    paragraphs: Vec<String>,
    standing: Standard,
}

/// What is known about one tag, and how it stands against standard FIX at
/// `against`. `None` where no name can be claimed (`Disputed`, `Nowhere`).
///
/// `custom` is a fact about the *binding*, not about `source`: it is what makes
/// `*Standard FIX.4.4*` and `*Not defined by "desk"*` sayable at all. A
/// description passes `false`, so it can print neither.
fn about(
    source: Source,
    tag: u32,
    against: Option<Version>,
    standing: Standard,
    label: Option<&str>,
    custom: bool,
) -> Option<About> {
    let mut paragraphs = Vec::new();
    match source {
        Source::Defined(dict, idx) => {
            let def = dict.field(idx);
            let note = standing_note(against, &standing, Some(def.name()), label, custom);

            let mut header = format!("**{}** ({tag})", def.name());
            if !def.datatype().is_empty() {
                header.push_str(" — ");
                header.push_str(def.datatype());
            }
            paragraphs.push(header);
            // Trimmed: the XML descriptions carry a trailing newline, which
            // would show as a blank line before the section rule.
            let description = def.description().trim_end();
            if !description.is_empty() {
                paragraphs.push(description.to_owned());
            }
            // Empty for a custom dictionary, which dates nothing.
            let mut facts = def.lifecycle();
            // Under a note the tables' own dates could contradict the
            // standard's: a QuickFIX/J-only tag would read as removed. A
            // bundled date its own files contradict is not repeated.
            if note.is_some() && standing != Standard::InVersion {
                facts = Lifecycle::default();
            } else if !standard::date_holds(tag) {
                facts.added = None;
            }
            paragraphs.extend(note);
            paragraphs.extend(lifecycle(&facts));
        }
        Source::Standard => {
            paragraphs.push(format!(
                "**{}** ({tag})",
                standard::name_of(&standing, tag)?
            ));
            // What tells this apart from a tag the desk's dictionary defines,
            // whose note reads the same. A bundled binding's note already names
            // the version.
            if custom {
                paragraphs.push(match label {
                    Some(label) => format!("*Not defined by \"{label}\"*"),
                    None => "*Not defined by this dictionary*".to_owned(),
                });
            }
            // No bound name: nothing for the standard one to differ from.
            paragraphs.extend(standing_note(against, &standing, None, label, custom));
        }
    }
    Some(About {
        paragraphs,
        standing,
    })
}

/// The value half of an answer: the raw value, its enum label where `label`
/// names one, and for MsgType the message it names. Empty for an empty value.
///
/// Standard FIX names a tag and says nothing about the values of one, so a
/// `Standard` source renders the value alone.
fn value_paragraph(
    source: Source,
    raw: &[u8],
    label: Option<EnumIdx>,
    message: Option<MessageView<'_>>,
) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    let label = match source {
        Source::Defined(dict, index) => label.map(|label| dict.enum_at(index, label)),
        Source::Standard => None,
    };
    let mut value = Vec::new();
    let mut line = value_line(raw);
    let name = message
        .as_ref()
        .map(MessageView::name)
        .or_else(|| label.as_ref().map(|label| label.name()));
    if let Some(name) = name {
        line.push_str(" — **");
        line.push_str(name);
        line.push_str("**");
    }
    value.push(line);
    if let Some(label) = &label {
        if message.is_none()
            && !label.description().is_empty()
            && label.description() != label.name()
        {
            value.push(label.description().to_owned());
        }
        value.extend(lifecycle(&Lifecycle {
            added: (!label.added().is_empty()).then_some(label.added()),
            deprecated: None,
            removed_in: None,
        }));
    }
    if let Some(message) = message {
        if !message.description().is_empty() {
            value.push(message.description().to_owned());
        }
        if !message.category().is_empty() {
            value.push(format!("*Category: {}*", message.category()));
        }
    }
    value
}

/// Backticks keep markdown metacharacters in a raw value inert. A value
/// containing a backtick itself breaks the span — an accepted edge.
fn value_line(raw: &[u8]) -> String {
    format!("Value: `{}`", String::from_utf8_lossy(raw))
}

/// The reference link, when the template may point at the tag. The label
/// names whatever site the URL points at, so a desk that redirects
/// `documentLink.url` at its own browser does not get a link still claiming
/// to be Onixs. A link into a version's pages asserts the tag is there.
fn link(options: &Options, version: Option<&str>, tag: u32, standing: &Standard) -> Option<String> {
    if options.tag_url.requires_version() && version.is_none() {
        return None;
    }
    (!options.tag_url.is_versioned() || *standing == Standard::InVersion)
        .then(|| options.tag_url.of(version.unwrap_or_default(), tag))
        .flatten()
        .map(|url| match options.tag_url.host() {
            Some(host) => format!("[FIX Reference: {host}]({url})"),
            None => format!("[FIX Reference]({url})"),
        })
}

fn sections(about: Vec<String>, value: Vec<String>, link: Option<String>) -> String {
    [about.join("\n\n"), value.join("\n\n")]
        .into_iter()
        .chain(link)
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

/// How standard FIX sees the tag, as one italic line; nothing where the
/// sources disagree, or under a bundled dictionary where it is standard.
///
/// Only FIX versions are named: an extension pack is how a tag got into a
/// release, not a version of its own.
fn standing_note(
    version: Option<Version>,
    standing: &Standard,
    bound_name: Option<&str>,
    label: Option<&str>,
    custom: bool,
) -> Option<String> {
    let note = match standing {
        Standard::InVersion if !custom => return None,
        Standard::Nowhere => {
            return Some(match label.filter(|_| custom) {
                Some(label) => format!("*Custom field, introduced in \"{label}\"*"),
                None => "*Custom field*".to_owned(),
            });
        }
        _ => standing.note(version)?,
    };
    // The note starts with "standard" or "not": ASCII, one byte.
    let mut note = format!("*{}{}", note[..1].to_ascii_uppercase(), &note[1..]);
    if let Some(def) = standing
        .def()
        .filter(|def| bound_name.is_some_and(|bound| bound != def.name))
    {
        let _ = write!(note, " as \"{}\"", def.name);
    }
    note.push('*');
    Some(note)
}

/// When the thing entered the protocol, when it was deprecated, and when it
/// was removed — as one italic line, or nothing when none is known.
///
/// Three verbs in one tense, so the paragraph reads as a life. It was `Not in
/// {v}+` first, which broke that parallel and made the reader decode a suffix
/// that could mean "including 4.4" or "after 4.4"; the field only ever holds a
/// version nothing comes back from, so "removed" is what it already meant.
fn lifecycle(facts: &Lifecycle) -> Option<String> {
    let said = [
        facts.added.map(|v| format!("Introduced in {v}")),
        facts.deprecated.map(|v| format!("Deprecated in {v}")),
        facts.removed_in.map(|v| format!("Removed in {v}")),
    ];
    let said: Vec<String> = said.into_iter().flatten().collect();
    (!said.is_empty()).then(|| format!("*{}*", said.join(" · ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LineAnalysis;
    use crate::ctx::{analyze, Ctx};
    use crate::dictionary::newest_defining;

    /// The line, its analysis under `ctx`, and the index of the field carrying `tag`.
    fn at_tag_in(ctx: &Ctx, line: &str, tag: u32) -> (Vec<u8>, MessageAnalysis, usize) {
        let bytes = line.as_bytes().to_vec();
        let LineAnalysis::Message(message) = analyze(&bytes, ctx) else {
            panic!("the fixture is a message");
        };
        let at = message
            .fields
            .iter()
            .position(|field| field.tag == tag)
            .unwrap_or_else(|| panic!("tag {tag} is on the line"));
        (bytes, message, at)
    }

    fn at_tag(line: &str, version: &str, tag: u32) -> (Vec<u8>, MessageAnalysis, usize) {
        at_tag_in(&Ctx::for_test(version), line, tag)
    }

    fn rendered(line: &str, version: &str, tag: u32) -> Option<String> {
        let (bytes, analysis, at) = at_tag(line, version, tag);
        hover(&bytes, &analysis, at, &Options::default(), None)
    }

    const HEARTBEAT: &str =
        "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";

    fn described(tag: u32) -> Option<String> {
        describe(tag, None, &Options::default())
    }

    /// The point of one renderer, as an equality: a description *is* the hover
    /// for that tag on a line of its reference source. No version is asked
    /// about, so the two can differ in nothing — not the header, not the
    /// lifespan, not the link.
    #[test]
    fn a_description_is_the_hover_under_the_reference_source() {
        for (tag, value) in [
            (49u32, "CLIENT1"),
            (54, "1"),
            (108, "30"),
            (368, "1"),
            (803, "1"),
            (1328, "x"),
            (1407, "x"),
            (47, "A"),   // newest edition FIX.4.3, retired after it
            (9000, "1"), // QuickFIX-only: *Custom field*, and no link either side
        ] {
            let version = reference_source(tag).unwrap_or(Version::Fixt11);
            let version = version.as_str();
            let prefix = match version {
                "FIX.5.0" => "8=FIXT.1.1|9=5|35=D|1128=7|".to_owned(),
                "FIX.5.0SP1" => "8=FIXT.1.1|9=5|35=D|1128=8|".to_owned(),
                "FIX.5.0SP2" => "8=FIXT.1.1|9=5|35=D|1128=9|".to_owned(),
                "FIXT.1.1" => "8=FIXT.1.1|9=5|35=A|".to_owned(),
                _ => format!("8={version}|9=5|35=0|"),
            };
            let line = format!("{prefix}{tag}={value}|10=0|");
            let (bytes, analysis, at) = at_tag(&line, version, tag);
            let hovered = hover(&bytes, &analysis, at, &Options::default(), None).expect("hover");
            assert_eq!(
                describe(tag, Some(value), &Options::default()).as_deref(),
                Some(hovered.as_str()),
                "tag {tag} under {version}"
            );

            // And without a value: an empty one leaves hover no value section,
            // which is the shape a description with no value argument has.
            let bare = format!("{prefix}{tag}=|10=0|");
            let (bytes, analysis, at) = at_tag(&bare, version, tag);
            let hovered = hover(&bytes, &analysis, at, &Options::default(), None).expect("hover");
            assert_eq!(
                describe(tag, None, &Options::default()).as_deref(),
                Some(hovered.as_str()),
                "tag {tag} under {version}, no value"
            );
        }
    }

    #[test]
    fn msg_type_reference_and_hover_resolve_the_application_message() {
        let line = "8=FIXT.1.1|9=5|35=D|1128=9|10=0|";
        let (bytes, analysis, at) = at_tag(line, "FIX.5.0SP2", tag::MSG_TYPE);
        let hovered = hover(&bytes, &analysis, at, &Options::default(), None).expect("hover");
        let described =
            describe(tag::MSG_TYPE, Some("D"), &Options::default()).expect("description");
        for rendered in [&hovered, &described] {
            assert!(
                rendered.contains("Value: `D` — **NewOrderSingle**"),
                "{rendered}"
            );
            assert!(
                rendered.contains("The NewOrderSingle(35=D) message"),
                "{rendered}"
            );
        }
        assert!(hovered.ends_with("/fixt1.1/tagNum_35.html)"), "{hovered}");
        assert_eq!(described, hovered);
    }

    /// A description is asked about a tag, not about a version, so it names
    /// none it was not asked about: no opener, and never hover's "not in".
    #[test]
    fn a_description_names_no_version_it_was_not_asked_about() {
        for tag in [49u32, 54, 108, 1328, 47, 2891, 9000] {
            let text = described(tag).expect("described");
            assert!(!text.contains("*Under "), "tag {tag}: {text}");
            assert!(!text.contains("Not in "), "tag {tag}: {text}");
        }
    }

    /// A tag the newest edition lacks is described from the edition that has
    /// it, with its lifespan — the fact a person asking about a tag wants, and
    /// the one hover's note used to crowd out.
    #[test]
    fn a_tag_is_described_by_the_newest_edition_that_defines_it() {
        let text = described(1328).expect("described");
        assert!(
            text.starts_with("**RejectText** (1328) — String\n\n"),
            "{text}"
        );
        assert!(text.contains("*Introduced in FIX.5.0SP1*"), "{text}");
        assert!(
            text.ends_with("/fix-dictionary/5.0.SP2/tagNum_1328.html)"),
            "{text}"
        );
    }

    /// A tag the protocol dropped: when it went is as much of its life as when
    /// it came, and the link points at the edition that still had it.
    #[test]
    fn a_retired_tag_is_described_with_the_edition_it_left() {
        let text = described(47).expect("described");
        assert!(text.starts_with("**Rule80A** (47) — CHAR\n\n"), "{text}");
        assert!(text.contains("*Removed in FIX.4.4*"), "{text}");
        assert!(
            text.ends_with("/fix-dictionary/4.3/tagNum_47.html)"),
            "{text}"
        );
    }

    /// A tag only FIX Latest knows: no bundled file vouches for a datatype or a
    /// description, but when it entered is still known, and is said.
    #[test]
    fn a_tag_only_fix_latest_knows_is_described_by_name_and_arrival() {
        assert_eq!(
            described(2891).as_deref(),
            Some("**UPICode** (2891)\n\n*Introduced in FIX Latest*")
        );
        assert_eq!(
            describe(2891, Some("x"), &Options::default()).as_deref(),
            Some("**UPICode** (2891)\n\n*Introduced in FIX Latest*\n\n---\n\nValue: `x`")
        );
    }

    /// Where the two sources disagree about some edition, nothing is said about
    /// that edition — but the tag's own life is not in doubt.
    #[test]
    fn a_tag_the_sources_dispute_somewhere_still_has_its_lifespan() {
        let text = described(108).expect("described");
        assert!(text.starts_with("**HeartBtInt** (108) — int\n\n"), "{text}");
        assert!(text.contains("*Introduced in FIX.3.0*"), "{text}");
    }

    /// The six QuickFIX-only tags live in the FIX.5.0 file alone and are
    /// standard nowhere: hover's `*Custom field*` line, and no life to claim.
    #[test]
    fn a_quickfix_only_tag_is_described_as_a_custom_field() {
        assert_eq!(
            described(9000).as_deref(),
            Some("**NoNestedUserData** (9000) — INT\n\n*Custom field*")
        );
    }

    /// Flat reference templates need no bundled version.
    #[test]
    fn a_description_links_what_a_bundled_edition_answers_for() {
        let flat = with_url("https://fix.internal/t{tag}");
        assert!(
            describe(9000, None, &flat)
                .expect("described")
                .ends_with("[FIX Reference: fix.internal](https://fix.internal/t9000)"),
            "a flat template needs no version"
        );
        assert!(
            describe(2891, None, &flat)
                .expect("described")
                .contains("[FIX Reference"),
            "a version-independent reference does not need a bundled edition"
        );

        let off = with_url("");
        for tag in [9000u32, 2891, 1328] {
            assert!(
                !describe(tag, None, &off)
                    .expect("described")
                    .contains("[FIX Reference"),
                "links are off: tag {tag}"
            );
        }
    }

    #[test]
    fn a_tag_in_no_table_has_no_description() {
        assert_eq!(described(9999), None);
        assert_eq!(described(20001), None);
    }

    /// The whole answer for one plain field, asserted in full rather than in
    /// fragments: the order of the paragraphs is as much the contract as their
    /// text, and only an equality check holds it.
    #[test]
    fn a_plain_field_renders_its_name_type_description_and_value() {
        assert_eq!(
            rendered(HEARTBEAT, "FIX.4.4", 49).as_deref(),
            Some(
                "**SenderCompID** (49) — String\n\n\
                 Assigned value used to identify firm sending message.\n\n\
                 *Introduced in FIX.2.7*\n\n\
                 ---\n\n\
                 Value: `CLIENT1`\n\n\
                 ---\n\n\
                 [FIX Reference: onixs.biz](https://www.onixs.biz/fix-dictionary/4.4/tagNum_49.html)"
            )
        );
    }

    /// MsgType(35) is the one field whose value pulls a message definition in
    /// beside the enum label.
    #[test]
    fn msg_type_carries_the_message_description_and_category() {
        assert_eq!(
            rendered(HEARTBEAT, "FIX.4.4", 35).as_deref(),
            Some(
                "**MsgType** (35) — String\n\n\
                 Defines message type ALWAYS THIRD FIELD IN MESSAGE. (Always unencrypted)\n\n\
                 *Introduced in FIX.2.7*\n\n\
                 ---\n\n\
                 Value: `0` — **Heartbeat**\n\n\
                 *Introduced in FIX.2.7*\n\n\
                 The Heartbeat monitors the status of the communication link and identifies \
                 when the last of a string of messages was not received.\n\n\
                 *Category: Session*\n\n\
                 ---\n\n\
                 [FIX Reference: onixs.biz](https://www.onixs.biz/fix-dictionary/4.4/tagNum_35.html)"
            )
        );
    }

    /// Two things to note: MDEntryType(269)'s `Bid` has no description
    /// paragraph, because the enum's description is its name; and the enum
    /// carries a lifecycle paragraph of its own, between the value and the
    /// group path.
    #[test]
    fn a_field_inside_a_group_names_its_path() {
        let line = "8=FIX.4.4|9=124|35=W|34=3|49=EXCHANGE|56=CLIENT|52=20251026-09:05:00.000|\
55=BTCUSD|268=2|269=0|270=65000.50|271=1.5|269=1|270=65001.00|271=2.0|10=145|";
        let hover = rendered(line, "FIX.4.4", 269).expect("269 is known");
        assert!(
            hover.contains(
                "Value: `0` — **Bid**\n\n*Introduced in FIX.4.2*\n\nGroup: `NoMDEntries/1/MDEntryType`"
            ),
            "{hover}"
        );
    }

    /// The one corner the merge changes: the group line used to sit inside the
    /// non-empty-value branch here, while the undefined path always drew it
    /// unconditionally. Unified on the latter — a group member sits in its
    /// entry whether or not it carries a value.
    #[test]
    fn a_group_member_with_an_empty_value_still_names_its_group() {
        let line = "8=FIX.4.4|9=124|35=W|34=3|49=EXCHANGE|56=CLIENT|52=20251026-09:05:00.000|\
55=BTCUSD|268=1|269=|270=65000.50|271=1.5|10=145|";
        let hover = rendered(line, "FIX.4.4", 269).expect("269 is known");
        assert!(!hover.contains("Value:"), "{hover}");
        assert!(
            hover.contains("\n\n---\n\nGroup: `NoMDEntries/1/MDEntryType`\n\n---\n\n"),
            "the group line is the whole value section: {hover}"
        );
    }

    /// PartySubIDType(803) is what the join rule is for: FIX44.xml lists none
    /// of its values, so without Orchestra's own-set codes the hover would show
    /// a bare `1`. Codes 27 and up came with extension packs and are not 4.4's.
    #[test]
    fn a_value_orchestra_restored_is_labelled_like_any_other() {
        // The line's own BeginString decides the version, so it varies too.
        let party = |version: &str, value: &str| {
            let prefix = if version == "FIX.5.0SP2" {
                "8=FIXT.1.1|9=57|35=D|1128=9|".to_owned()
            } else {
                format!("8={version}|9=57|35=D|")
            };
            format!("{prefix}453=1|448=A|447=D|452=1|802=1|523=S1|803={value}|10=171|")
        };
        let in_44 = rendered(&party("FIX.4.4", "1"), "FIX.4.4", 803).expect("803 is known");
        assert!(in_44.contains("Value: `1` — **Firm**"), "{in_44}");

        let late = rendered(&party("FIX.4.4", "28"), "FIX.4.4", 803).expect("the field is known");
        assert!(late.contains("Value: `28`\n"), "{late}");
        assert!(!late.contains("MarketMaker"), "4.4 never had it: {late}");

        let in_sp2 = rendered(&party("FIX.5.0SP2", "28"), "FIX.5.0SP2", 803).expect("803 is known");
        assert!(in_sp2.contains("Value: `28` — **MarketMaker**"), "{in_sp2}");
    }

    /// A field that only borrows its code set takes the labels and no values:
    /// QuoteEntryRejectReason(368) keeps the nine FIX44.xml gives it, spelled
    /// the way Orchestra spells them rather than `UNKNOWN_SYMBOL`.
    #[test]
    fn a_shared_code_set_relabels_what_the_version_already_had() {
        let line = "8=FIX.4.4|9=57|35=b|368=1|10=171|";
        let hover = rendered(line, "FIX.4.4", 368).expect("368 is known");
        assert!(hover.contains("Value: `1` — **UnknownSymbol**"), "{hover}");
    }

    #[test]
    fn a_tag_the_dictionary_does_not_know_is_silent() {
        let line = "8=FIX.4.4|9=57|35=0|9999=x|10=171|";
        assert_eq!(rendered(line, "FIX.4.4", 9999), None);
    }

    /// An empty value drops the whole Value section. No sample log has a known
    /// field with one, so this test is the only thing holding it.
    #[test]
    fn a_known_field_with_an_empty_value_has_no_value_section() {
        let line = "8=FIX.4.4|9=57|35=0|49=|56=BROKER1|10=171|";
        let hover = rendered(line, "FIX.4.4", 49).expect("49 is known");
        assert!(!hover.contains("Value:"), "{hover}");
        assert!(hover.starts_with("**SenderCompID** (49)"), "{hover}");
    }

    /// A FIX value may be raw bytes. Rendering them lossily keeps the section;
    /// dropping it would make the hover disagree with what the line shows.
    #[test]
    fn a_value_that_is_not_utf8_is_still_shown() {
        let mut bytes = b"8=FIX.4.4|9=57|35=0|49=".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(b"|56=BROKER1|10=171|");
        let LineAnalysis::Message(message) = analyze(&bytes, &Ctx::for_test("FIX.4.4")) else {
            panic!("a message");
        };
        let at = message.fields.iter().position(|f| f.tag == 49).expect("49");
        let hover = hover(&bytes, &message, at, &Options::default(), None).expect("49 is known");
        assert!(hover.contains("Value: `"), "{hover}");
    }

    /// The three facts in protocol order, joined by ` · `. No bundled tag
    /// holds all three: a removed tag is not in FIX Latest, which is where
    /// `added` and `deprecated` come from.
    #[test]
    fn all_three_lifecycle_facts_read_as_one_line() {
        let facts = Lifecycle {
            added: Some("FIX.2.7"),
            deprecated: Some("FIX.4.3"),
            removed_in: Some("FIX.4.4"),
        };
        assert_eq!(
            lifecycle(&facts).as_deref(),
            Some("*Introduced in FIX.2.7 · Deprecated in FIX.4.3 · Removed in FIX.4.4*")
        );
    }

    /// Version-exact, which is the whole point: the same tag under two versions
    /// gives two different answers, and under a third, which lacks the field,
    /// says only what standard FIX knows.
    ///
    /// RepurchaseTerm(226) is dated FIX.4.2 by FIX Latest but first appears in
    /// the FIX.4.3 file, so no introduction is claimed under either version.
    #[test]
    fn the_lifecycle_is_the_lines_own_versions() {
        let under_43 = rendered("8=FIX.4.3|9=57|35=D|226=5|10=171|", "FIX.4.3", 226)
            .expect("RepurchaseTerm is in 4.3");
        assert!(!under_43.contains("Introduced in"), "{under_43}");
        assert!(!under_43.contains("Deprecated in"), "{under_43}");
        let under_44 = rendered("8=FIX.4.4|9=57|35=D|226=5|10=171|", "FIX.4.4", 226)
            .expect("RepurchaseTerm is in 4.4");
        assert!(
            under_44.contains("\n\n*Deprecated in FIX.4.4*\n"),
            "{under_44}"
        );

        let under_42 =
            rendered("8=FIX.4.2|9=57|35=D|47=A|10=171|", "FIX.4.2", 47).expect("Rule80A is in 4.2");
        assert!(under_42.contains("*Removed in FIX.4.4*"), "{under_42}");
        // Not "a different lifecycle line": 4.4 has no such field, so the
        // dictionary has no life to tell; what is said is standard FIX's.
        assert_eq!(
            rendered("8=FIX.4.4|9=57|35=D|47=A|10=171|", "FIX.4.4", 47).as_deref(),
            Some(
                "**Rule80A** (47)\n\n*Not in FIX.4.4, retired after FIX.4.3*\n\n---\n\nValue: `A`"
            )
        );
    }

    // --- a tag the bound dictionary does not define ----------------------

    /// The whole answer, as for a defined field: name and standing from the
    /// two witnesses, the raw value, and nothing the bound dictionary could
    /// not have said — no datatype, no description, no life, no link.
    #[test]
    fn an_undefined_tag_is_named_and_placed_by_standard_fix() {
        assert_eq!(
            rendered(
                "8=FIXT.1.1|9=57|35=D|1128=7|1328=x|10=171|",
                "FIX.5.0",
                1328
            )
            .as_deref(),
            Some(
                "**RejectText** (1328)\n\n\
                 *Not in FIX.5.0, introduced in FIX.5.0SP1*\n\n\
                 ---\n\n\
                 Value: `x`"
            )
        );
    }

    #[test]
    fn an_undefined_tag_only_fix_latest_knows_is_dated_fix_latest() {
        assert_eq!(
            rendered("8=FIX.4.4|9=57|35=D|2891=x|10=171|", "FIX.4.4", 2891).as_deref(),
            Some("**UPICode** (2891)\n\n*Not in FIX.4.4, introduced in FIX Latest*\n\n---\n\nValue: `x`")
        );
    }

    /// With no bound name there is nothing for the standard name to differ
    /// from, so the `as "…"` clause of the note never fires.
    #[test]
    fn an_undefined_tag_has_no_bound_name_to_differ_from() {
        let shown = rendered("8=FIX.4.4|9=57|35=0|1008=x|10=171|", "FIX.4.4", 1008)
            .expect("FIX Latest names it");
        assert!(shown.starts_with("**SideTrdSubType** (1008)"), "{shown}");
        assert!(!shown.contains(" as \""), "{shown}");
    }

    /// Where the sources dispute the tag, nothing is named; where neither
    /// knows it, there is nothing to name. Both stay `None`, as before.
    #[test]
    fn a_disputed_or_unknown_undefined_tag_has_no_hover() {
        assert_eq!(
            rendered("8=FIX.4.2|9=57|35=D|220=USD|10=171|", "FIX.4.2", 220),
            None,
            "disputed"
        );
        assert_eq!(
            rendered("8=FIX.4.4|9=57|35=0|9000=x|10=171|", "FIX.4.4", 9000),
            None,
            "nowhere"
        );
    }

    #[test]
    fn an_undefined_tag_with_no_value_has_no_value_section() {
        assert_eq!(
            rendered("8=FIXT.1.1|9=57|35=D|1128=7|1328=|10=171|", "FIX.5.0", 1328).as_deref(),
            Some("**RejectText** (1328)\n\n*Not in FIX.5.0, introduced in FIX.5.0SP1*")
        );
    }

    /// A tag every bundled version defines reports no boundary — otherwise the
    /// line would end with a removal that never happens.
    #[test]
    fn a_tag_that_never_leaves_says_only_when_it_arrived() {
        let hover = rendered(HEARTBEAT, "FIX.4.4", 35).expect("35 is known");
        assert!(hover.contains("*Introduced in FIX.2.7*"), "{hover}");
        assert!(!hover.contains("Removed in"), "{hover}");
    }

    /// Transport fields keep their independently published lifecycle facts.
    #[test]
    fn a_fixt_line_reports_transport_field_lifecycle() {
        let line = "8=FIXT.1.1|9=57|35=0|49=CLIENT1|56=BROKER1|10=171|";
        let hover = rendered(line, "FIX.5.0SP2", 49).expect("49 is known");
        assert!(hover.contains("*Introduced in FIX.2.7*"), "{hover}");
    }

    /// An extension pack says how a tag reached a release, not which, so the
    /// line names the release that shipped the pack — the first file that has
    /// the tag — and never the pack or its base. The standing note under a
    /// custom dictionary says the same release for the same tag.
    #[test]
    fn an_ep_dated_tag_is_dated_by_the_release_that_shipped_it() {
        // RejectText(1328): FIX.5.0 EP55, first in the FIX.5.0SP1 file.
        for (line, version) in [
            ("8=FIXT.1.1|9=5|35=D|1128=9|1328=x|10=0|", "FIX.5.0SP2"),
            ("8=FIXT.1.1|9=5|35=D|1128=9|1328=x|10=0|", "FIXT.1.1"),
        ] {
            let hover = rendered(line, version, 1328).expect("1328 is in the tables");
            assert!(hover.contains("*Introduced in FIX.5.0SP1*"), "{hover}");
        }

        // NoStrategyParameters(957): FIX.4.4 EP2, first in the FIX.5.0 file.
        let parameters = rendered("8=FIXT.1.1|9=5|35=D|1128=7|957=1|10=0|", "FIX.5.0", 957)
            .expect("957 is in 5.0");
        assert!(
            parameters.contains("*Introduced in FIX.5.0*"),
            "{parameters}"
        );

        // DefaultApplExtID(1407): EP56, and the files put it in SP2 rather than
        // the SP1 the pack implies — a date that does not hold is not printed.
        let ext_id = rendered(
            "8=FIXT.1.1|9=5|35=D|1128=9|1407=x|10=0|",
            "FIX.5.0SP2",
            1407,
        )
        .expect("1407 is in the SP2 tables");
        assert!(!ext_id.contains("Introduced in"), "{ext_id}");
    }

    /// A configured URL takes its own label with it; the default's is
    /// `onixs.biz`.
    #[test]
    fn a_configured_url_is_labelled_with_its_own_host() {
        let (bytes, analysis, at) = at_tag(HEARTBEAT, "FIX.4.4", 49);
        let options = Options {
            tag_url: crate::render::TagUrl::new("https://fix.internal/t{tag}"),
            ..Options::default()
        };
        let labelled = hover(&bytes, &analysis, at, &options, None).expect("49 is known");
        assert!(
            labelled.ends_with("[FIX Reference: fix.internal](https://fix.internal/t49)"),
            "{labelled}"
        );

        let unnamed = Options {
            tag_url: crate::render::TagUrl::new("/local/{tag}.html"),
            ..Options::default()
        };
        let no_host = hover(&bytes, &analysis, at, &unnamed, None).expect("49 is known");
        assert!(
            !no_host.contains("[FIX Reference"),
            "a non-HTTP reference URL produces no link: {no_host}"
        );
    }

    /// Transport links use FIXT pages even with an application default.
    #[test]
    fn a_fixt_line_links_to_its_transport_reference() {
        let line = "8=FIXT.1.1|9=57|35=0|49=CLIENT1|56=BROKER1|10=171|";
        let hover = rendered(line, "FIX.4.4", 49).expect("49 is known");
        assert!(hover.ends_with("/fixt1.1/tagNum_49.html)"), "{hover}");
    }

    // --- how a tag stands against standard FIX --------------------------

    use crate::dictionary::ParsedDict;
    use crate::render::TagUrl;

    use crate::dictionary::parse::fixtures::{field, quickfix};

    /// Hover over `tag` on a one-field line of `version`, `dict` answering for it.
    fn under(version: &str, dict: ParsedDict, tag: u32, options: &Options) -> String {
        let line = match version {
            "FIX.5.0" => format!("8=FIXT.1.1|35=D|1128=7|{tag}=x|"),
            "FIX.5.0SP1" => format!("8=FIXT.1.1|35=D|1128=8|{tag}=x|"),
            "FIX.5.0SP2" => format!("8=FIXT.1.1|35=D|1128=9|{tag}=x|"),
            _ => format!("8={version}|35=0|{tag}=x|"),
        };
        let (bytes, message, at) = at_tag_in(&Ctx::for_test_with_dict(version, dict), &line, tag);
        hover(&bytes, &message, at, options, Some("selected")).expect("something to hover")
    }

    /// `under` for a custom dictionary of `version` that does *not* declare `tag`.
    fn missing_from(version: &str, tag: u32, options: &Options) -> String {
        under(version, quickfix(version, &field(58, "Text")), tag, options)
    }

    /// `under` for a custom dictionary of `version` declaring only `tag`.
    fn custom(version: &str, tag: u32, name: &str) -> String {
        under(
            version,
            quickfix(version, &field(tag, name)),
            tag,
            &Options::default(),
        )
    }

    fn custom_fix44(tag: u32, name: &str) -> String {
        custom("FIX.4.4", tag, name)
    }

    fn with_url(template: &str) -> Options {
        Options {
            tag_url: TagUrl::new(template),
            ..Options::default()
        }
    }

    const ONIXS_44: &str = "[FIX Reference: onixs.biz](https://www.onixs.biz/fix-dictionary/4.4";

    #[test]
    fn a_standard_tag_says_so_under_a_custom_dictionary() {
        assert_eq!(
            custom_fix44(55, "Symbol"),
            format!(
                "**Symbol** (55) — STRING\n\n*Standard FIX.4.4*\n\n---\n\nValue: `x`\n\n---\n\n\
                 {ONIXS_44}/tagNum_55.html)"
            )
        );
    }

    /// RejectText(1328) is `FIX.5.0 EP55`: the pack shipped in FIX.5.0SP1.
    #[test]
    fn a_later_tag_names_the_release_that_introduced_it() {
        assert_eq!(
            custom_fix44(1328, "RejectText"),
            "**RejectText** (1328) — STRING\n\n*Not in FIX.4.4, introduced in FIX.5.0SP1*\n\n\
             ---\n\nValue: `x`"
        );
        assert!(
            custom_fix44(2891, "UPICode").contains("*Not in FIX.4.4, introduced in FIX Latest*")
        );
        let fix50 = custom("FIX.5.0", 1328, "RejectText");
        assert!(
            fix50.contains("*Not in FIX.5.0, introduced in FIX.5.0SP1*"),
            "{fix50}"
        );
    }

    #[test]
    fn the_standard_name_is_given_only_when_it_differs() {
        assert!(custom_fix44(1008, "SideTrdSubTyp")
            .contains("*Not in FIX.4.4, introduced in FIX.5.0 as \"SideTrdSubType\"*"));
        assert!(!custom_fix44(1328, "RejectText").contains(" as \""));
    }

    #[test]
    fn a_retired_tag_says_after_which_release() {
        assert_eq!(
            custom_fix44(47, "Rule80A"),
            "**Rule80A** (47) — STRING\n\n*Not in FIX.4.4, retired after FIX.4.3*\n\n---\n\nValue: `x`"
        );
        // ExecTransType(20) leaves after FIX.4.3 and `FIX50.xml` declares it
        // again: at FIX.4.4 that is a gap, and after FIX.5.0 a retirement whose
        // release the files cannot name.
        assert!(custom_fix44(20, "ExecTransType").contains("*Not in FIX.4.4*"));
        let past = custom("FIX.5.0SP1", 20, "ExecTransType");
        assert!(past.contains("*Not in FIX.5.0SP1, retired*"), "{past}");
        // SendingDate(51) is only in `FIX43.xml`: under FIX.4.2 it is not
        // retired, it is not there yet.
        assert_eq!(
            custom("FIX.4.2", 51, "SendingDate"),
            "**SendingDate** (51) — STRING\n\n*Not in FIX.4.2*\n\n---\n\nValue: `x`"
        );
    }

    #[test]
    fn a_custom_tag_names_its_dictionary_and_has_no_versioned_link() {
        let dict = quickfix(
            "FIX.4.4",
            "<field number='20001' name='DeskMode' type='INT'><value enum='x' description='ALPHA'/></field>",
        );
        assert_eq!(
            under("FIX.4.4", dict, 20001, &Options::default()),
            "**DeskMode** (20001) — INT\n\n*Custom field, introduced in \"selected\"*\n\n---\n\n\
             Value: `x` — **ALPHA**"
        );
        let own = custom_fix44(9000, "NoNestedUserData");
        assert!(
            own.contains("*Custom field, introduced in \"selected\"*"),
            "{own}"
        );
        assert!(!own.contains("[FIX Reference"), "{own}");
    }

    /// A flat template is the desk's own browser, which knows its own tags.
    #[test]
    fn only_a_versioned_template_is_withheld() {
        let flat = under(
            "FIX.4.4",
            quickfix("FIX.4.4", &field(20001, "DeskMode")),
            20001,
            &with_url("https://fix.internal/t{tag}"),
        );
        assert!(
            flat.contains("*Custom field, introduced in \"selected\"*"),
            "{flat}"
        );
        assert!(
            flat.ends_with("[FIX Reference: fix.internal](https://fix.internal/t20001)"),
            "{flat}"
        );
        let versioned = under(
            "FIX.4.4",
            quickfix("FIX.4.4", &field(20001, "DeskMode")),
            20001,
            &with_url("https://fix.internal/{version}/t{tag}"),
        );
        assert!(!versioned.contains("[FIX Reference"), "{versioned}");
    }

    #[test]
    fn bundled_dictionaries_change_only_where_the_sources_do() {
        // No source dates the QuickFIX/J-only tags, so no life is claimed either.
        let qfj_only =
            rendered("8=FIXT.1.1|9=5|35=D|1128=7|9000=1|10=0|", "FIX.5.0", 9000).unwrap();
        assert_eq!(
            qfj_only,
            "**NoNestedUserData** (9000) — INT\n\n*Custom field*\n\n---\n\nValue: `1`"
        );

        let sp2 = rendered(
            "8=FIXT.1.1|9=5|35=D|1128=9|1328=x|10=0|",
            "FIX.5.0SP2",
            1328,
        )
        .unwrap();
        assert!(sp2.ends_with("/5.0.SP2/tagNum_1328.html)"), "{sp2}");
        assert!(
            !sp2.contains("Standard FIX"),
            "no note under bundled tables: {sp2}"
        );

        // ApplReportType(1426): in the SP1 file, dated SP2.
        let disputed = rendered(
            "8=FIXT.1.1|9=5|35=D|1128=8|1426=0|10=0|",
            "FIX.5.0SP1",
            1426,
        )
        .unwrap();
        let about = disputed.split("\n\n---\n\n").next().unwrap();
        assert!(!about.contains("Not in"), "{disputed}");
        assert!(
            !about.contains("Introduced in"),
            "values keep theirs: {disputed}"
        );
        assert!(!disputed.contains("[FIX Reference"), "{disputed}");

        let symbol = rendered("8=FIX.4.4|9=5|35=0|55=X|10=0|", "FIX.4.4", 55).unwrap();
        assert!(symbol.contains("*Introduced in FIX.2.7*"), "{symbol}");
    }

    #[test]
    fn a_custom_tag_has_no_lifecycle() {
        assert_eq!(
            custom_fix44(20001, "DeskMode"),
            "**DeskMode** (20001) — STRING\n\n*Custom field, introduced in \"selected\"*\n\n---\n\nValue: `x`"
        );
    }

    /// BenchmarkCurveCurrency(220): dated FIX.4.2, first in the FIX.4.3 file.
    #[test]
    fn a_disputed_tag_has_no_note() {
        let disputed = custom("FIX.4.2", 220, "BenchmarkCurveCurrency");
        assert!(
            !disputed.contains("Not in") && !disputed.contains("Standard FIX"),
            "{disputed}"
        );
        assert!(!disputed.contains("[FIX Reference"), "{disputed}");

        let later = custom("FIX.4.2", 559, "SecurityListRequestType");
        assert!(
            later.contains("*Not in FIX.4.2, introduced in FIX.4.3*"),
            "{later}"
        );
        assert!(!later.contains("Introduced in"), "{later}");

        let before = custom("FIX.4.1", 220, "BenchmarkCurveCurrency");
        assert!(
            before.contains("*Not in FIX.4.1*"),
            "no date the files contradict: {before}"
        );
    }

    #[test]
    fn transport_fields_are_not_judged_against_application_history() {
        for code in ["7", "8", "9"] {
            for tag in [108, 1400, 1407] {
                let line = format!("8=FIXT.1.1|35=A|{tag}=30|");
                let shown = rendered(&line, "FIX.4.4", tag).expect("transport field");
                assert!(!shown.contains("Not in"), "{shown}");
                assert!(
                    shown.ends_with(&format!("/fixt1.1/tagNum_{tag}.html)")),
                    "{shown}"
                );
                let application = format!("8=FIXT.1.1|35=D|1128={code}|{tag}=30|");
                let shown = rendered(&application, "FIX.4.4", tag).expect("transport field");
                assert!(!shown.contains("Not in"), "{shown}");
            }
        }
    }

    #[test]
    fn standalone_transport_reference_does_not_invent_application_membership() {
        let text = described(1407).expect("transport reference");
        assert!(text.starts_with("**DefaultApplExtID** (1407)"), "{text}");
        assert!(text.ends_with("/fixt1.1/tagNum_1407.html)"), "{text}");
        assert!(newest_defining(1407).is_none());
    }

    /// The one fact that tells this apart from a tag the desk's dictionary
    /// defines: `*Standard FIX.4.4*` alone would read the same for both.
    #[test]
    fn an_undefined_tag_under_a_custom_dictionary_says_which_dictionary_lacks_it() {
        assert_eq!(
            missing_from("FIX.4.4", 55, &Options::default()),
            "**Symbol** (55)\n\n\
             *Not defined by \"selected\"*\n\n\
             *Standard FIX.4.4*\n\n\
             ---\n\n\
             Value: `x`\n\n\
             ---\n\n\
             [FIX Reference: onixs.biz](https://www.onixs.biz/fix-dictionary/4.4/tagNum_55.html)"
        );
        let elsewhere = missing_from("FIX.4.4", 1328, &Options::default());
        assert!(
            elsewhere.starts_with("**RejectText** (1328)"),
            "{elsewhere}"
        );
        assert!(
            elsewhere.contains(
                "*Not defined by \"selected\"*\n\n*Not in FIX.4.4, introduced in FIX.5.0SP1*"
            ),
            "{elsewhere}"
        );
        assert!(!elsewhere.contains("[FIX Reference"), "{elsewhere}");
        let flat = missing_from("FIX.4.4", 1328, &with_url("https://fix.internal/t{tag}"));
        assert!(flat.ends_with("(https://fix.internal/t1328)"), "{flat}");
    }

    /// No tables means no standing note — not no hover: the name and the
    /// dictionary's silence are still known.
    #[test]
    fn an_undefined_tag_under_a_version_without_tables_keeps_its_name() {
        assert_eq!(
            missing_from("FIX.9.1", 55, &Options::default()),
            "**Symbol** (55)\n\n---\n\nValue: `x`"
        );
        let flat = missing_from("FIX.9.1", 55, &with_url("https://fix.internal/t{tag}"));
        assert!(flat.ends_with("(https://fix.internal/t55)"), "{flat}");
    }

    /// A custom dictionary the session gave no name to is still named as the
    /// one that lacks the tag.
    #[test]
    fn an_unnamed_custom_dictionary_is_still_said_to_lack_the_tag() {
        let line = "8=FIX.4.4|35=0|55=x|";
        let ctx = Ctx::for_test_with_dict("FIX.4.4", quickfix("FIX.4.4", &field(58, "Text")));
        let (bytes, message, at) = at_tag_in(&ctx, line, 55);
        let shown = hover(
            &bytes,
            &message,
            at,
            &with_url("https://fix.internal/t{tag}"),
            None,
        );
        assert_eq!(
            shown.as_deref(),
            Some(
                "**Symbol** (55)\n\n*Not defined by this dictionary*\n\n*Standard FIX.4.4*\n\n---\n\n\
                 Value: `x`\n\n---\n\n[FIX Reference: fix.internal](https://fix.internal/t55)"
            )
        );
    }

    #[test]
    fn unsupported_wire_versions_cannot_restore_dictionary_definitions() {
        for version in ["FIX.9.1", "FIX.5.0", "FIX.5.0SP1", "FIX.5.0SP2"] {
            let line = format!("8={version}|35=D|55=SYNTHETIC|20001=1|");
            let context =
                Ctx::for_test_with_dict(version, quickfix(version, &field(20001, "SyntheticMode")));
            let (bytes, message, at) = at_tag_in(&context, &line, 55);
            for (template, links) in [
                ("https://reference.example/{version}/{tag}", false),
                ("https://reference.example/{fixVersion}/{tag}", false),
                ("https://reference.example/{tag}", true),
            ] {
                let shown = hover(&bytes, &message, at, &with_url(template), Some("selected"))
                    .expect("standard name");
                assert!(
                    !shown.contains("Not in") && !shown.contains("Not defined by"),
                    "{shown}"
                );
                assert_eq!(shown.contains("[FIX Reference"), links, "{shown}");
            }
            let at = message
                .fields
                .iter()
                .position(|field| field.tag == 20001)
                .unwrap();
            assert!(hover(&bytes, &message, at, &Options::default(), Some("selected")).is_none());
        }
    }
}
