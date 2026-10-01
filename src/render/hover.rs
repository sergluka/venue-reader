//! What the dictionary — or, failing it, standard FIX — knows about one field, as markdown.

use std::fmt::Write as _;

use crate::codec::{Field, MessageAnalysis};
use crate::dictionary::{bundled, standard, FieldIdx, Lifecycle, Retirement, Standard, Version};
use crate::tag;

use super::Options;

/// The field at `at`, or `None` when neither the dictionary nor standard FIX
/// can name its tag. `label` names the custom dictionary that decoded the
/// line, if it has one.
///
/// No `Dict` argument: a `FieldIdx`/`EnumIdx` means nothing except against the
/// dictionary that produced it, which the analysis pins as `analysis.dict`.
/// Taking one separately would let a caller pair an index with the wrong tables.
pub fn hover(
    line: &[u8],
    analysis: &MessageAnalysis,
    at: usize,
    options: &Options,
    label: Option<&str>,
) -> Option<String> {
    let field = analysis.fields.get(at)?;
    match field.def() {
        Some(idx) => Some(defined(line, analysis, field, idx, options, label)),
        None => undefined(line, analysis, field, options, label),
    }
}

/// A field the bound dictionary defines: everything it knows, then how the
/// tag stands against standard FIX.
fn defined(
    line: &[u8],
    analysis: &MessageAnalysis,
    field: &Field,
    idx: FieldIdx,
    options: &Options,
    label: Option<&str>,
) -> String {
    let dict = &analysis.dict;
    let def = dict.field(idx);
    let version = Version::parse(&analysis.version);
    let standing = Standard::of_tag(version, field.tag);
    let custom = !dict.is_static();
    // Bundled tables carry no label, so `label` alone says "custom" here.
    let note = standing_note(version, &standing, Some(def.name()), label, custom);

    let mut about = format!("**{}** ({})", def.name(), field.tag);
    if !def.datatype().is_empty() {
        about.push_str(" — ");
        about.push_str(def.datatype());
    }
    let mut about = vec![about];
    // Trimmed: the XML descriptions carry a trailing newline, which would show
    // as a blank line before the section rule.
    let description = def.description().trim_end();
    if !description.is_empty() {
        about.push(description.to_owned());
    }
    let mut facts = def.lifecycle();
    // A custom tag has no standard life, and under a "Not in" note the
    // dictionary's own dates could contradict the standard's: neither shows
    // one. A bundled date its own files contradict is not repeated.
    if note.is_some() && standing != Standard::InVersion {
        facts = Lifecycle::default();
    } else if !custom && !standard::date_holds(field.tag) {
        facts.added = None;
    }
    about.extend(note);
    about.extend(lifecycle(&facts));

    let mut value = Vec::new();
    let raw = field.value(line);
    if !raw.is_empty() {
        let mut line_ = value_line(raw);
        let label = field.enum_value().map(|at| dict.enum_at(idx, at));
        if let Some(label) = &label {
            line_.push_str(" — **");
            line_.push_str(label.name());
            line_.push_str("**");
        }
        value.push(line_);
        if let Some(label) = &label {
            // Only when it says something the name did not.
            if !label.description().is_empty() && label.description() != label.name() {
                value.push(label.description().to_owned());
            }
            value.extend(lifecycle(&Lifecycle {
                added: (!label.added().is_empty()).then_some(label.added()),
                deprecated: None,
                removed_in: None,
            }));
        }
        if field.tag == tag::MSG_TYPE {
            if let Some(message) = field
                .text(line)
                .and_then(|value| dict.message_idx(value))
                .map(|at| dict.message(at))
            {
                if !message.description().is_empty() {
                    value.push(message.description().to_owned());
                }
                if !message.category().is_empty() {
                    value.push(format!("*Category: {}*", message.category()));
                }
            }
        }
        if let Some(path) = analysis.group_path(field, line) {
            value.push(format!("Group: `{path}`"));
        }
    }

    let link = link(options, &analysis.version, field.tag, &standing);
    sections(about, value, link)
}

/// A field the bound dictionary lacks: what the two witnesses support and
/// nothing more — standard FIX's name and standing, the raw value, the group
/// it sits in. No datatype, description or life: the dictionary has none, and
/// FIX Latest's would speak for a version that lacks the tag. `None` where no
/// name can be claimed (`Disputed`, `Nowhere`).
fn undefined(
    line: &[u8],
    analysis: &MessageAnalysis,
    field: &Field,
    options: &Options,
    label: Option<&str>,
) -> Option<String> {
    let version = Version::parse(&analysis.version);
    let standing = Standard::of_tag(version, field.tag);
    let name = standard::name_of(&standing, field.tag)?;
    let custom = !analysis.dict.is_static();

    let mut about = vec![format!("**{name}** ({})", field.tag)];
    // What tells this apart from a tag the desk's dictionary defines, whose
    // note reads the same. A bundled binding's note already names the version.
    if custom {
        about.push(match label {
            Some(label) => format!("*Not defined by \"{label}\"*"),
            None => "*Not defined by this dictionary*".to_owned(),
        });
    }
    // No bound name: nothing for the standard one to differ from.
    about.extend(standing_note(version, &standing, None, label, custom));

    let mut value = Vec::new();
    let raw = field.value(line);
    if !raw.is_empty() {
        value.push(value_line(raw));
    }
    // A Repository dictionary can list a member it never declares; the codec
    // still places it in its entry, and the hint already draws the path.
    if let Some(path) = analysis.group_path(field, line) {
        value.push(format!("Group: `{path}`"));
    }

    let link = link(options, &analysis.version, field.tag, &standing);
    Some(sections(about, value, link))
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
fn link(options: &Options, version: &str, tag: u32, standing: &Standard) -> Option<String> {
    (!options.tag_url.is_versioned() || *standing == Standard::InVersion)
        .then(|| options.tag_url.of(version, tag))
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
    let def = match standing {
        Standard::Disputed => return None,
        Standard::InVersion => {
            let tables = bundled(version?).version;
            return custom.then(|| format!("*Standard {tables}*"));
        }
        Standard::Nowhere => {
            return Some(match label {
                Some(label) => format!("*Custom field, introduced in \"{label}\"*"),
                None => "*Custom field*".to_owned(),
            });
        }
        Standard::OtherVersion(def) | Standard::Unbundled(def) => def,
    };
    // A version without bundled tables cannot be checked, so "Not in" it is not said.
    let mut note = format!("*Not in {}", bundled(version?).version);
    if let Some(release) = def.introduced() {
        let _ = write!(note, ", introduced in {release}");
    } else {
        match def.retirement {
            Some(Retirement::After(release)) => {
                let _ = write!(note, ", retired after {release}");
            }
            Some(Retirement::Gone) => note.push_str(", retired"),
            None => {}
        }
    }
    if bound_name.is_some_and(|bound| bound != def.name) {
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

    /// PartySubIDType(803) is what the join rule is for: FIX44.xml lists none
    /// of its values, so without Orchestra's own-set codes the hover would show
    /// a bare `1`. Codes 27 and up came with extension packs and are not 4.4's.
    #[test]
    fn a_value_orchestra_restored_is_labelled_like_any_other() {
        // The line's own BeginString decides the version, so it varies too.
        let party = |version: &str, value: &str| {
            format!(
                "8={version}|9=57|35=D|453=1|448=A|447=D|452=1|802=1|523=S1|803={value}|10=171|"
            )
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
            rendered("8=FIX.5.0|9=57|35=0|1328=x|10=171|", "FIX.5.0", 1328).as_deref(),
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
            rendered("8=FIX.4.4|9=57|35=0|2891=x|10=171|", "FIX.4.4", 2891).as_deref(),
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
            rendered("8=FIX.4.2|9=57|35=0|220=USD|10=171|", "FIX.4.2", 220),
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
            rendered("8=FIX.5.0|9=57|35=0|1328=|10=171|", "FIX.5.0", 1328).as_deref(),
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

    /// A FIXT line reads the FIX.5.0SP2 tables while keeping its own version
    /// string, so its lifecycle facts are 5.0SP2's. Correct, and pinned so that
    /// nobody "fixes" it into reporting FIXT's own non-existent editions.
    #[test]
    fn a_fixt_line_reports_the_tables_it_actually_read() {
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
            ("8=FIX.5.0SP2|9=5|35=0|1328=x|10=0|", "FIX.5.0SP2"),
            ("8=FIXT.1.1|9=5|35=0|1328=x|10=0|", "FIXT.1.1"),
        ] {
            let hover = rendered(line, version, 1328).expect("1328 is in the tables");
            assert!(hover.contains("*Introduced in FIX.5.0SP1*"), "{hover}");
        }

        // NoStrategyParameters(957): FIX.4.4 EP2, first in the FIX.5.0 file.
        let parameters =
            rendered("8=FIX.5.0|9=5|35=0|957=1|10=0|", "FIX.5.0", 957).expect("957 is in 5.0");
        assert!(
            parameters.contains("*Introduced in FIX.5.0*"),
            "{parameters}"
        );

        // DefaultApplExtID(1407): EP56, and the files put it in SP2 rather than
        // the SP1 the pack implies — a date that does not hold is not printed.
        let ext_id = rendered("8=FIX.5.0SP2|9=5|35=0|1407=x|10=0|", "FIX.5.0SP2", 1407)
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

    /// The link follows the line's own version string, which for a FIXT line is
    /// `FIXT.1.1` — and the site documents its tables under 5.0.SP2.
    #[test]
    fn a_fixt_line_links_into_the_tables_it_reads() {
        let line = "8=FIXT.1.1|9=57|35=0|49=CLIENT1|56=BROKER1|10=171|";
        let hover = rendered(line, "FIX.4.4", 49).expect("49 is known");
        assert!(hover.ends_with("/5.0.SP2/tagNum_49.html)"), "{hover}");
    }

    // --- how a tag stands against standard FIX --------------------------

    use crate::dictionary::ParsedDict;
    use crate::render::TagUrl;

    use crate::dictionary::parse::fixtures::{field, quickfix, repository};

    /// Hover over `tag` on a one-field line of `version`, `dict` answering for it.
    fn under(version: &str, dict: ParsedDict, tag: u32, options: &Options) -> String {
        let line = format!("8={version}|35=0|{tag}=x|");
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
        let qfj_only = rendered("8=FIX.5.0|9=5|35=0|9000=1|10=0|", "FIX.5.0", 9000).unwrap();
        assert_eq!(
            qfj_only,
            "**NoNestedUserData** (9000) — INT\n\n*Custom field*\n\n---\n\nValue: `1`"
        );

        let sp2 = rendered("8=FIX.5.0SP2|9=5|35=0|1328=x|10=0|", "FIX.5.0SP2", 1328).unwrap();
        assert!(sp2.ends_with("/5.0.SP2/tagNum_1328.html)"), "{sp2}");
        assert!(
            !sp2.contains("Standard FIX"),
            "no note under bundled tables: {sp2}"
        );

        // ApplReportType(1426): in the SP1 file, dated SP2.
        let disputed = rendered("8=FIX.5.0SP1|9=5|35=0|1426=0|10=0|", "FIX.5.0SP1", 1426).unwrap();
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
    fn a_custom_tag_has_no_lifecycle_even_from_its_own_dictionary() {
        let dict = repository(
            "FIX.4.4",
            "<Field added='FIX.4.4' deprecated='FIX.4.4'><Tag>20001</Tag><Name>DeskMode</Name><Type>String</Type></Field>",
        );
        assert_eq!(
            under("FIX.4.4", dict, 20001, &Options::default()),
            "**DeskMode** (20001) — String\n\n*Custom field, introduced in \"selected\"*\n\n---\n\nValue: `x`"
        );
    }

    /// Under a "Not in" note the dictionary's own dates could contradict the
    /// standard's: RejectText(1328) cannot be deprecated in FIX.4.4.
    #[test]
    fn under_a_not_in_note_the_dictionarys_own_lifecycle_goes() {
        let dict = repository(
            "FIX.4.4",
            "<Field added='FIX.4.4' deprecated='FIX.4.4'><Tag>1328</Tag><Name>RejectText</Name><Type>String</Type></Field>",
        );
        let shown = under("FIX.4.4", dict, 1328, &Options::default());
        assert!(
            !shown.contains("Introduced in") && !shown.contains("Deprecated in"),
            "{shown}"
        );
    }

    /// BenchmarkCurveCurrency(220): dated FIX.4.2, first in the FIX.4.3 file.
    #[test]
    fn a_disputed_tag_keeps_the_dictionarys_own_date_and_no_note() {
        let dict = repository(
            "FIX.4.2",
            "<Field added='FIX.4.2'><Tag>220</Tag><Name>BenchmarkCurveCurrency</Name><Type>String</Type></Field>\
             <Field added='FIX.4.2'><Tag>559</Tag><Name>SecurityListRequestType</Name><Type>int</Type></Field>",
        );
        let disputed = under("FIX.4.2", dict.clone(), 220, &Options::default());
        assert!(
            !disputed.contains("Not in") && !disputed.contains("Standard FIX"),
            "{disputed}"
        );
        assert!(
            disputed.contains("*Introduced in FIX.4.2*"),
            "the dictionary's own: {disputed}"
        );
        assert!(!disputed.contains("[FIX Reference"), "{disputed}");

        let later = under("FIX.4.2", dict, 559, &Options::default());
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
    fn session_tags_the_sp1_file_lacks_are_not_claimed_absent() {
        for (tag, name) in [(108, "HeartBtInt"), (1407, "DefaultApplExtID")] {
            let shown = custom("FIX.5.0SP1", tag, name);
            assert!(
                !shown.contains("Not in") && !shown.contains("Standard FIX"),
                "{shown}"
            );
            assert!(!shown.contains("[FIX Reference"), "{shown}");
        }
        let listed = custom("FIX.5.0SP1", 1400, "EncryptedPasswordMethod");
        assert!(listed.contains("*Standard FIX.5.0SP1*"), "{listed}");
        assert!(listed.ends_with("/5.0.SP1/tagNum_1400.html)"), "{listed}");
        let before = custom("FIX.5.0", 1407, "DefaultApplExtID");
        assert!(before.contains("*Not in FIX.5.0*"), "{before}");
        assert!(!before.contains("[FIX Reference"), "{before}");
    }

    #[test]
    fn a_fixt_binding_is_compared_with_the_tables_it_reads() {
        let fixt = |tag, name| custom("FIXT.1.1", tag, name);
        assert!(fixt(1328, "RejectText").contains("*Standard FIX.5.0SP2*"));
        assert!(fixt(20001, "DeskMode").contains("*Custom field, introduced in \"selected\"*"));
        assert!(fixt(47, "Rule80A").contains("*Not in FIX.5.0SP2, retired after FIX.4.3*"));
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
            "**Symbol** (55)\n\n*Not defined by \"selected\"*\n\n---\n\nValue: `x`"
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

    /// A repository-format dictionary may list a group member it never
    /// declares as a field (QuickFIX drops such a name; Repository keeps the
    /// number). The codec still places the field in its entry, so hover says
    /// so, as it does for a defined member — the hint already draws the path.
    #[test]
    fn an_undefined_group_member_keeps_its_group_path() {
        use crate::dictionary::parse::{fixtures::source, parse_repository_bytes, RepoSources};
        let dict = parse_repository_bytes(&RepoSources {
            fields: source(
                "Fields.xml",
                "<Fields version='FIX.4.4'>\
                 <Field added='FIX.4.4'><Tag>268</Tag><Name>NoMDEntries</Name><Type>NumInGroup</Type></Field>\
                 <Field added='FIX.4.4'><Tag>269</Tag><Name>MDEntryType</Name><Type>char</Type></Field>\
                 </Fields>"
                    .to_owned(),
            ),
            enums: source("Enums.xml", "<Enums version='FIX.4.4'/>".to_owned()),
            messages: Some(source(
                "Messages.xml",
                "<Messages><Message><ComponentID>1</ComponentID><MsgType>W</MsgType><Name>MarketData</Name><CategoryID>T</CategoryID></Message></Messages>".to_owned(),
            )),
            msg_contents: Some(source(
                "MsgContents.xml",
                "<MsgContents>\
                 <MsgContent><ComponentID>1</ComponentID><TagText>268</TagText><Indent>0</Indent><Position>1</Position></MsgContent>\
                 <MsgContent><ComponentID>1</ComponentID><TagText>269</TagText><Indent>1</Indent><Position>2</Position></MsgContent>\
                 <MsgContent><ComponentID>1</ComponentID><TagText>1328</TagText><Indent>1</Indent><Position>3</Position></MsgContent>\
                 </MsgContents>"
                    .to_owned(),
            )),
            components: Some(source("Components.xml", "<Components/>".to_owned())),
        })
        .expect("a dictionary with one group");
        let line = "8=FIX.4.4|35=W|268=1|269=0|1328=x|";
        let (bytes, message, at) = at_tag_in(&Ctx::for_test_with_dict("FIX.4.4", dict), line, 1328);
        let shown = hover(&bytes, &message, at, &Options::default(), Some("selected"))
            .expect("standard FIX names it");
        assert!(
            shown.ends_with("Value: `x`\n\nGroup: `NoMDEntries/1/1328`"),
            "{shown}"
        );
    }

    /// No bundled tables to check "Not in" against, and no site page to link.
    #[test]
    fn a_version_without_bundled_tables_claims_nothing_but_custom() {
        for version in ["FIX.3.0", "FIX.9.1"] {
            for (template, links) in [
                (crate::config::DocumentLink::default().url, false),
                ("https://fix.internal/{fixVersion}/{tag}".to_owned(), true),
                ("https://fix.internal/t{tag}".to_owned(), true),
            ] {
                let options = with_url(&template);
                let standard = under(
                    version,
                    quickfix(version, &field(55, "Symbol")),
                    55,
                    &options,
                );
                assert!(
                    !standard.contains("Not in") && !standard.contains("Standard FIX"),
                    "{standard}"
                );
                assert_eq!(
                    standard.contains("[FIX Reference"),
                    links,
                    "{version} {template}"
                );
                let custom = under(
                    version,
                    quickfix(version, &field(20001, "DeskMode")),
                    20001,
                    &options,
                );
                assert!(
                    custom.contains("*Custom field, introduced in \"selected\"*"),
                    "{custom}"
                );
            }
        }
    }
}
