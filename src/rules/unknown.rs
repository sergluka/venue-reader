//! What the bound dictionary lacks.
//!
//! The fact is about the dictionary, not the standard: "FIX.4.4 does not
//! define tag 9000". How standard FIX sees the tag is a clause after it, in
//! the words hover's note uses, so the two channels cannot phrase one tag two
//! ways. Both rules run only on a line whose dictionary was chosen
//! ([`Precondition::Bound`]): a fallback dictionary lacking something says
//! nothing about the line.

use super::{Diag, LineCtx, Precondition, Rule};
use crate::codec::Field;
use crate::config::Severity;
use crate::dictionary::{standard, Standard, StandardDef, Version};
use crate::tag;

/// A tag the line's dictionary does not define, at every occurrence.
pub struct UnknownTag;

impl Rule for UnknownTag {
    fn id(&self) -> &'static str {
        "unknownTag"
    }

    fn precondition(&self) -> Precondition {
        Precondition::Bound
    }

    fn default_severity(&self) -> Severity {
        Severity::Info
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        undefined(ctx)
            .map(|field| Diag {
                code: self.id(),
                rule: self.id(),
                start: field.tag_start,
                end: field.tag_end(),
                message: undefined_message(ctx, field),
                severity,
                document_key: None,
            })
            .collect()
    }

    fn count(&self, ctx: &LineCtx) -> Option<usize> {
        Some(undefined(ctx).count())
    }
}

/// A MsgType(35) the line's dictionary does not list. Only the first
/// MsgType(35) counts, as it is the one the codec reads the message by.
pub struct UnknownMsgType;

impl Rule for UnknownMsgType {
    fn id(&self) -> &'static str {
        "unknownMsgType"
    }

    fn precondition(&self) -> Precondition {
        Precondition::Bound
    }

    fn default_severity(&self) -> Severity {
        Severity::Info
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        unlisted(ctx)
            .map(|field| Diag {
                code: self.id(),
                rule: self.id(),
                start: field.value_start(),
                end: field.value_end(),
                message: format!(
                    "{} does not define MsgType(35) = {}",
                    subject(ctx),
                    String::from_utf8_lossy(field.value(ctx.bytes))
                ),
                severity,
                document_key: None,
            })
            .into_iter()
            .collect()
    }

    fn count(&self, ctx: &LineCtx) -> Option<usize> {
        Some(usize::from(unlisted(ctx).is_some()))
    }
}

/// The first unlisted MsgType(35) under a nonempty table with a session layer.
fn unlisted<'a>(ctx: &'a LineCtx) -> Option<&'a Field> {
    let message = ctx.message;
    let field = message.first(tag::MSG_TYPE)?;
    let listed = message.msg_type.is_some();
    // A loaded file always has messages; only a dictionary built in code can
    // have an empty table.
    (!field.value(ctx.bytes).is_empty()
        && !listed
        && message.dict.has_session_layer()
        && message.dict.schema().messages != 0)
        .then_some(field)
}

fn undefined<'a>(ctx: &'a LineCtx) -> impl Iterator<Item = &'a Field> {
    ctx.message
        .fields
        .iter()
        .filter(|field| field.def().is_none())
}

fn undefined_message(ctx: &LineCtx, field: &Field) -> String {
    let version = Version::parse(&ctx.message.version);
    let standing = Standard::of_tag(version, field.tag);
    let named = match standard::name_of(&standing, field.tag) {
        Some(name) => format!("{name}({})", field.tag),
        // The digits on the line: a run that overflowed is `u32::MAX` in `tag`.
        None => format!(
            "tag {}",
            String::from_utf8_lossy(field.tag_digits(ctx.bytes))
        ),
    };
    // Under bundled tables the subject already says which version lacks it, so
    // only when the tag arrived or left is added.
    let clause = if ctx.message.dict.is_static() {
        standing.def().and_then(StandardDef::clause)
    } else {
        standing.note(version)
    };
    let mut message = format!("{} does not define {named}", subject(ctx));
    if let Some(clause) = clause {
        message.push_str("; ");
        message.push_str(&clause);
    }
    message
}

/// The dictionary as the reader knows it: the bundled version, or the custom
/// dictionary's name — configured, else its file name.
fn subject(ctx: &LineCtx) -> String {
    let dict = &ctx.message.dict;
    match (dict.is_static(), ctx.label) {
        (true, _) => dict.version().unwrap_or_default().to_owned(),
        (false, Some(label)) => format!("\"{label}\""),
        (false, None) => "this dictionary".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Diagnostics;
    use crate::ctx::{analyze, Ctx};
    use crate::rules::{Registry, Scope};

    /// What `rule` reports on `line`, as (span, message).
    fn findings(rule: &str, ctx: &Ctx, line: &str, label: Option<&str>) -> Vec<(String, String)> {
        let bytes = line.as_bytes();
        let analysis = analyze(bytes, ctx);
        let scope = Scope {
            label,
            only: Some(rule),
            ..Scope::default()
        };
        Registry::with_selected_version(&Diagnostics::default(), ctx.selected_version())
            .check(bytes, &analysis, &scope)
            .diags
            .into_iter()
            .map(|diag| {
                let span = &line[diag.start.index()..diag.end.index()];
                (span.to_owned(), diag.message)
            })
            .collect()
    }

    fn found_in(ctx: &Ctx, line: &str, label: Option<&str>) -> Vec<(String, String)> {
        findings("unknownTag", ctx, line, label)
    }

    fn found(line: &str) -> Vec<(String, String)> {
        found_in(&Ctx::for_test("FIX.4.4"), line, None)
    }

    /// What the rule says about `tag`'s first occurrence on `line`.
    fn said(ctx: &Ctx, line: &str, label: Option<&str>, tag: &str) -> String {
        found_in(ctx, line, label)
            .into_iter()
            .find(|(span, _)| span == tag)
            .unwrap_or_else(|| panic!("tag {tag} reported on {line}"))
            .1
    }

    #[test]
    fn under_bundled_tables_the_version_lacks_it() {
        let bundled = Ctx::for_test("FIX.4.4");
        let cases = [
            (
                "8=FIX.5.0|35=0|1328=x|",
                "1328",
                "FIX.5.0 does not define RejectText(1328); introduced in FIX.5.0SP1",
            ),
            (
                "8=FIX.4.4|35=0|47=A|",
                "47",
                "FIX.4.4 does not define Rule80A(47); retired after FIX.4.3",
            ),
            (
                "8=FIX.4.4|35=0|2891=x|",
                "2891",
                "FIX.4.4 does not define UPICode(2891); introduced in FIX Latest",
            ),
            (
                "8=FIX.4.2|35=0|51=x|",
                "51",
                "FIX.4.2 does not define SendingDate(51)",
            ),
            (
                "8=FIX.4.4|35=0|9000=x|",
                "9000",
                "FIX.4.4 does not define tag 9000",
            ),
            (
                "8=FIX.5.0SP1|35=0|108=30|",
                "108",
                "FIX.5.0SP1 does not define tag 108",
            ),
            (
                "8=FIXT.1.1|35=0|47=A|",
                "47",
                "FIX.5.0SP2 does not define Rule80A(47); retired after FIX.4.3",
            ),
        ];
        for (line, tag, expected) in cases {
            assert_eq!(said(&bundled, line, None, tag), expected, "{line}");
        }
    }

    #[test]
    fn under_a_custom_dictionary_its_name_lacks_it() {
        let selected = Ctx::for_test_selected("FIX.4.4");
        let line = "8=FIX.4.4|35=0|55=X|1328=x|9000=x|";
        let cases = [
            ("55", "\"desk\" does not define Symbol(55); standard FIX.4.4"),
            ("1328", "\"desk\" does not define RejectText(1328); not in FIX.4.4, introduced in FIX.5.0SP1"),
            ("9000", "\"desk\" does not define tag 9000"),
        ];
        for (tag, expected) in cases {
            assert_eq!(said(&selected, line, Some("desk"), tag), expected);
        }
        assert_eq!(
            said(&selected, line, None, "1328"),
            "this dictionary does not define RejectText(1328); not in FIX.4.4, introduced in FIX.5.0SP1"
        );
        assert_eq!(
            said(
                &Ctx::for_test_selected("FIX.5.0SP1"),
                "8=FIX.5.0SP1|35=0|108=30|",
                Some("desk"),
                "108"
            ),
            "\"desk\" does not define tag 108",
            "disputed: no name, no clause"
        );
        let elsewhere = Ctx::for_test_selected("FIX.9.1");
        assert_eq!(
            said(&elsewhere, "8=FIX.9.1|35=0|1328=x|", Some("desk"), "1328"),
            "\"desk\" does not define RejectText(1328)",
            "a version without bundled tables cannot be checked against"
        );
    }

    fn type_found(ctx: &Ctx, line: &str, label: Option<&str>) -> Vec<(String, String)> {
        findings("unknownMsgType", ctx, line, label)
    }

    /// A custom FIX.4.4 dictionary listing `messages`.
    fn listing(messages: &str) -> std::sync::Arc<Ctx> {
        use crate::dictionary::parse::{fixtures::source, parse_quickfix_bytes};
        let xml = format!(
            "<fix version='FIX.4.4'><messages>{messages}</messages>\
             <fields><field number='35' name='MsgType' type='STRING'/></fields></fix>"
        );
        let dict = parse_quickfix_bytes(&source("custom.xml", xml)).unwrap();
        Ctx::for_test_with_dict("FIX.4.4", dict)
    }

    #[test]
    fn a_message_type_the_dictionary_does_not_list() {
        let bundled = Ctx::for_test("FIX.4.4");
        assert_eq!(
            type_found(&bundled, "8=FIX.4.4|35=U1|", None),
            [(
                "U1".to_owned(),
                "FIX.4.4 does not define MsgType(35) = U1".to_owned()
            )]
        );
        assert_eq!(
            type_found(&bundled, "8=FIXT.1.1|35=U1|", None),
            [(
                "U1".to_owned(),
                "FIX.5.0SP2 does not define MsgType(35) = U1".to_owned()
            )]
        );
        let session = listing("<message name='Heartbeat' msgtype='0' msgcat='admin'/>");
        assert_eq!(
            type_found(&session, "8=FIX.4.4|35=D|", Some("desk")),
            [(
                "D".to_owned(),
                "\"desk\" does not define MsgType(35) = D".to_owned()
            )]
        );
    }

    #[test]
    fn a_listed_or_absent_message_type_is_not_reported() {
        let bundled = Ctx::for_test("FIX.4.4");
        for line in [
            "8=FIX.4.4|35=D|",
            "8=FIX.4.4|55=X|",
            "8=FIX.4.4|35=|",
            "8=FIX.4.4|35=D|35=U1|",
            "8=FIX.9.9|35=U1|",
        ] {
            assert!(type_found(&bundled, line, None).is_empty(), "{line}");
        }
    }

    /// Bundled FIX.5.0 and FIX.5.0SP1 keep empty session blocks until FIX 5
    /// handling, so their tables prove nothing about a type.
    #[test]
    fn bundled_dictionaries_without_a_session_layer_do_not_report_message_types() {
        let bundled = Ctx::for_test("FIX.4.4");
        for line in [
            "8=FIX.5.0|35=0|",
            "8=FIX.5.0|35=U1|",
            "8=FIX.5.0SP1|35=A|",
            "8=FIX.5.0SP1|35=U1|",
        ] {
            assert!(type_found(&bundled, line, None).is_empty(), "{line}");
        }
    }

    #[test]
    fn a_dictionary_without_messages_does_not_report_message_types() {
        let empty = Ctx::for_test_with_dict(
            "FIX.4.4",
            crate::dictionary::ParsedDict {
                version: Some("FIX.4.4".to_owned()),
                ..Default::default()
            },
        );
        assert!(empty.resolve(Some(b"FIX.4.4")).dict.has_session_layer());
        assert!(type_found(&empty, "8=FIX.4.4|35=U1|", None).is_empty());
        let line = b"8=FIX.4.4|35=U1|";
        let analysis = analyze(line, &empty);
        let ctx = LineCtx::new(line, analysis.message().unwrap());
        assert_eq!(UnknownMsgType.count(&ctx), Some(0));
    }

    #[test]
    fn an_application_only_custom_dictionary_reports_unknown_message_types() {
        let application = listing("<message name='NewOrderSingle' msgtype='D' msgcat='app'/>");
        assert_eq!(
            type_found(&application, "8=FIX.4.4|35=U1|", Some("desk")),
            [(
                "U1".to_owned(),
                "\"desk\" does not define MsgType(35) = U1".to_owned()
            )]
        );
        assert!(type_found(&application, "8=FIX.4.4|35=D|", None).is_empty());
        assert_eq!(
            type_found(&Ctx::for_test_selected("FIX.4.4"), "8=FIX.4.4|35=U1|", None),
            [(
                "U1".to_owned(),
                "this dictionary does not define MsgType(35) = U1".to_owned()
            )]
        );
        let line = b"8=FIX.4.4|35=U1|";
        let analysis = analyze(line, &application);
        let ctx = LineCtx::new(line, analysis.message().unwrap());
        assert_eq!(UnknownMsgType.count(&ctx), Some(1));
    }

    #[test]
    fn neither_rule_silences_the_other() {
        let bundled = Ctx::for_test("FIX.4.4");
        let line = "8=FIX.4.4|35=U1|9000=x|";
        assert_eq!(type_found(&bundled, line, None).len(), 1);
        assert_eq!(found(line).len(), 1);
    }

    /// Every binding × standing × line shape: what is said about a tag does not
    /// depend on the line's MsgType(35), an unknown type adds at most one
    /// finding of its own, and a fragment is checked only under a selection
    /// that decoded it.
    #[test]
    fn the_matrix_of_bindings_standings_and_line_shapes() {
        // InVersion (only reachable under a custom binding), OtherVersion,
        // Unbundled, Nowhere; then Disputed, which needs FIX.5.0SP1.
        let cases = [
            ("FIX.4.4", "55"),
            ("FIX.4.4", "1328"),
            ("FIX.4.4", "2891"),
            ("FIX.4.4", "9000"),
            ("FIX.5.0SP1", "108"),
        ];
        let bundled = Ctx::for_test("FIX.4.4");
        for (version, tag) in cases {
            let custom = Ctx::for_test_selected(version);
            // A path selection is a custom binding named after its file.
            let bindings: [(&str, &Ctx, Option<&str>); 4] = [
                ("bundled", &bundled, None),
                ("custom labelled", &custom, Some("desk")),
                ("custom unlabelled", &custom, None),
                ("path-selected", &custom, Some("desk.xml")),
            ];
            for (binding, ctx, label) in bindings {
                let tagged = |line: String| {
                    found_in(ctx, &line, label)
                        .into_iter()
                        .filter(|(span, _)| span == tag)
                        .collect::<Vec<_>>()
                };
                let known = tagged(format!("8={version}|35=0|{tag}=x|"));
                let unknown_line = format!("8={version}|35=U1|{tag}=x|");
                let unknown = tagged(unknown_line.clone());
                let none = tagged(format!("8={version}|{tag}=x|"));
                let fragment = tagged(format!("{tag}=x|"));
                let case = format!("{binding}, {version} tag {tag}");
                assert_eq!(known, unknown, "{case}");
                assert_eq!(known, none, "{case}");
                assert!(type_found(ctx, &unknown_line, label).len() <= 1, "{case}");
                let decoded_by_selection = ctx.selected_version().is_some_and(|selected| {
                    crate::dictionary::versions_compatible(selected, "FIX.4.4")
                });
                if decoded_by_selection {
                    assert_eq!(fragment, none, "{case}: the selection decoded it");
                } else {
                    assert!(fragment.is_empty(), "{case}: nothing chose the fallback");
                }
                let defined = binding == "bundled" && tag == "55";
                assert_eq!(known.len(), usize::from(!defined), "{case}");
            }
        }
    }

    #[test]
    fn never_called_a_custom_field() {
        let selected = Ctx::for_test_selected("FIX.4.4");
        for (_, message) in found_in(&selected, "8=FIX.4.4|35=0|9000=x|5001=y|", Some("desk")) {
            assert!(!message.contains("Custom field"), "{message}");
        }
    }

    #[test]
    fn an_overflowing_tag_is_named_by_its_digits() {
        assert_eq!(
            found("8=FIX.4.4|35=0|99999999999=x|"),
            [(
                "99999999999".to_owned(),
                "FIX.4.4 does not define tag 99999999999".to_owned()
            )]
        );
    }

    #[test]
    fn every_occurrence_is_reported_on_its_tag() {
        let line = "8=FIX.4.4|35=0|9000=a|55=X|9000=b|";
        let spans: Vec<String> = found(line).into_iter().map(|(span, _)| span).collect();
        assert_eq!(spans, ["9000", "9000"]);
        assert!(found("8=FIX.4.4|9=5|35=0|10=000|").is_empty());
    }

    #[test]
    fn the_count_is_what_check_would_return() {
        let ctx = Ctx::for_test("FIX.4.4");
        for line in [
            "8=FIX.4.4|35=0|9000=a|9000=b|1328=x|",
            "8=FIX.4.4|35=U1|",
            "8=FIX.4.4|35=|",
            "8=FIX.4.4|35=0|",
            "55=X|",
        ] {
            let analysis = analyze(line.as_bytes(), &ctx);
            let message = analysis.message().expect("a message");
            let mut line_ctx = LineCtx::new(line.as_bytes(), message);
            line_ctx.selected_version = ctx.selected_version();
            assert_eq!(
                UnknownTag.count(&line_ctx),
                Some(UnknownTag.check(&line_ctx, Severity::Info).len()),
                "{line}"
            );
            assert_eq!(
                UnknownMsgType.count(&line_ctx),
                Some(UnknownMsgType.check(&line_ctx, Severity::Info).len()),
                "{line}"
            );
        }
    }
}
