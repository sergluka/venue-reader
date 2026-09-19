//! What the dictionary knows about one field, as markdown.

use crate::codec::MessageAnalysis;
use crate::dictionary::Lifecycle;
use crate::tag;

use super::Options;

/// The field at `at`, or `None` when the dictionary does not know its tag.
///
/// No `Dict` argument: a `FieldIdx`/`EnumIdx` means nothing except against the
/// dictionary that produced it, which the analysis pins as `analysis.dict`.
/// Taking one separately would let a caller pair an index with the wrong tables.
pub fn hover(
    line: &[u8],
    analysis: &MessageAnalysis,
    at: usize,
    options: &Options,
) -> Option<String> {
    let field = analysis.fields.get(at)?;
    let dict = &analysis.dict;
    let idx = field.def()?;
    let def = dict.field(idx);

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
    about.extend(lifecycle(&def.lifecycle()));

    let mut value = Vec::new();
    let raw = field.value(line);
    if !raw.is_empty() {
        // Backticks keep markdown metacharacters in a raw value inert. A value
        // containing a backtick itself breaks the span — an accepted edge.
        let mut line_ = format!("Value: `{}`", String::from_utf8_lossy(raw));
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

    // The label names whatever site the URL points at, so a desk that redirects
    // `documentLink.url` at its own browser does not get a link still claiming
    // to be Onixs.
    let link =
        options
            .tag_url
            .of(&analysis.version, field.tag)
            .map(|url| match options.tag_url.host() {
                Some(host) => format!("[FIX Reference: {host}]({url})"),
                None => format!("[FIX Reference]({url})"),
            });

    let sections = [about.join("\n\n"), value.join("\n\n")]
        .into_iter()
        .chain(link)
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>();
    Some(sections.join("\n\n---\n\n"))
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

    /// The line, its analysis, and the index of the field carrying `tag`.
    fn at_tag(line: &str, version: &str, tag: u32) -> (Vec<u8>, MessageAnalysis, usize) {
        let bytes = line.as_bytes().to_vec();
        let LineAnalysis::Message(message) = analyze(&bytes, &Ctx::for_test(version)) else {
            panic!("the fixture is a message");
        };
        let at = message
            .fields
            .iter()
            .position(|field| field.tag == tag)
            .unwrap_or_else(|| panic!("tag {tag} is on the line"));
        (bytes, message, at)
    }

    fn rendered(line: &str, version: &str, tag: u32) -> Option<String> {
        let (bytes, analysis, at) = at_tag(line, version, tag);
        hover(&bytes, &analysis, at, &Options::default())
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
        let hover = hover(&bytes, &message, at, &Options::default()).expect("49 is known");
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
    /// gives two different answers, and under a third gives none at all because
    /// the field does not exist there.
    #[test]
    fn the_lifecycle_is_the_lines_own_versions() {
        let under_43 = rendered("8=FIX.4.3|9=57|35=D|226=5|10=171|", "FIX.4.3", 226)
            .expect("RepurchaseTerm is in 4.3");
        assert!(under_43.contains("*Introduced in FIX.4.2*"), "{under_43}");
        let under_44 = rendered("8=FIX.4.4|9=57|35=D|226=5|10=171|", "FIX.4.4", 226)
            .expect("RepurchaseTerm is in 4.4");
        assert!(
            under_44.contains("*Introduced in FIX.4.2 · Deprecated in FIX.4.4*"),
            "{under_44}"
        );

        let under_42 =
            rendered("8=FIX.4.2|9=57|35=D|47=A|10=171|", "FIX.4.2", 47).expect("Rule80A is in 4.2");
        assert!(under_42.contains("*Removed in FIX.4.4*"), "{under_42}");
        // Not "a different lifecycle line": 4.4 has no such field, so there is
        // nothing to hover at all.
        assert_eq!(
            rendered("8=FIX.4.4|9=57|35=D|47=A|10=171|", "FIX.4.4", 47),
            None
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

    /// A configured URL takes its own label with it; the default's is
    /// `onixs.biz`.
    #[test]
    fn a_configured_url_is_labelled_with_its_own_host() {
        let (bytes, analysis, at) = at_tag(HEARTBEAT, "FIX.4.4", 49);
        let options = Options {
            tag_url: crate::render::TagUrl::new("https://fix.internal/t{tag}"),
            ..Options::default()
        };
        let labelled = hover(&bytes, &analysis, at, &options).expect("49 is known");
        assert!(
            labelled.ends_with("[FIX Reference: fix.internal](https://fix.internal/t49)"),
            "{labelled}"
        );

        let unnamed = Options {
            tag_url: crate::render::TagUrl::new("/local/{tag}.html"),
            ..Options::default()
        };
        let no_host = hover(&bytes, &analysis, at, &unnamed).expect("49 is known");
        assert!(
            !no_host.contains("[FIX Reference"),
            "a non-HTTP reference URL produces no link: {no_host}"
        );
    }

    /// The link follows the line's own version string, which for a FIXT line is
    /// `FIXT.1.1` — and the site publishes it as 5.0.
    #[test]
    fn a_fixt_line_links_to_the_five_oh_page() {
        let line = "8=FIXT.1.1|9=57|35=0|49=CLIENT1|56=BROKER1|10=171|";
        let hover = rendered(line, "FIX.4.4", 49).expect("49 is known");
        assert!(hover.ends_with("/5.0/tagNum_49.html)"), "{hover}");
    }
}
