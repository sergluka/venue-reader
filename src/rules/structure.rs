//! Whether the line is a FIX message at all, and whether its version is known.
//!
//! Resolution failures are explicit facts, independent of rule settings.

use super::{Diag, LineCtx, Precondition, Rule};
use crate::codec::ResolutionIssue;
use crate::config::Severity;
use crate::tag;

/// The tokenizer parses any run of `tag=value` pairs, so a log can hold
/// fragments — a dumped repeating group, a truncated line — that the other
/// rules can say nothing useful about. This is what says so, and the same fact
/// is what their `FixMessage` precondition tests.
pub struct BeginString;

impl Rule for BeginString {
    fn id(&self) -> &'static str {
        "beginString"
    }

    fn precondition(&self) -> Precondition {
        Precondition::Fields
    }

    fn default_severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        let fields = &ctx.message.fields;
        // The precondition guarantees the line has fields; a line that also has
        // a BeginString is a message, and there is nothing to report.
        if fields.is_empty() || ctx.message.first(tag::BEGIN_STRING).is_some() {
            return Vec::new();
        }
        vec![Diag {
            code: self.id(),
            rule: self.id(),
            start: fields[0].tag_start,
            end: fields[fields.len() - 1].value_end(),
            message: "Not a FIX message: missing BeginString(8)".into(),
            severity,
            document_key: None,
        }]
    }
}

/// An explicit failure to resolve the message's transport or application.
pub struct UnknownVersion;

impl Rule for UnknownVersion {
    fn id(&self) -> &'static str {
        "unknownVersion"
    }

    fn precondition(&self) -> Precondition {
        Precondition::FixMessage
    }

    fn default_severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        let Some(issue) = ctx.message.resolution.issue else {
            return Vec::new();
        };
        let (selector, message) = match issue {
            // The existing beginString rule owns this finding even when disabled.
            ResolutionIssue::MissingBeginString => return Vec::new(),
            ResolutionIssue::UnsupportedBeginString => {
                let written = ctx
                    .message
                    .first(tag::BEGIN_STRING)
                    .map(|field| String::from_utf8_lossy(field.value(ctx.bytes)))
                    .unwrap_or_default();
                (
                    tag::BEGIN_STRING,
                    format!("Unsupported BeginString(8) = {written}; no dictionary interpretation"),
                )
            }
            ResolutionIssue::InvalidSelector(selector) => {
                (selector.tag(), format!("Invalid {}", selector.name()))
            }
            ResolutionIssue::DuplicateSelector(selector) => {
                (selector.tag(), format!("Duplicate {}", selector.name()))
            }
            ResolutionIssue::MissingApplication => {
                let message_context = ctx
                    .message
                    .first(tag::MSG_TYPE)
                    .map(|field| {
                        format!(
                            " for MsgType(35)={}",
                            String::from_utf8_lossy(field.value(ctx.bytes))
                        )
                    })
                    .unwrap_or_default();
                (
                    tag::BEGIN_STRING,
                    format!("Missing application version{message_context}. Set ApplVerID(1128), defaultApplVerID, or select an application dictionary."),
                )
            }
            ResolutionIssue::UnregisteredApplication => (
                tag::APPL_VER_ID,
                "Application identity has no registered dictionary".into(),
            ),
            ResolutionIssue::SelectorInGroup(selector) => (
                selector.tag(),
                format!("{} occurs inside a repeating group", selector.name()),
            ),
            ResolutionIssue::AmbiguousSelector(selector) => (
                selector.tag(),
                format!("{} has unresolved structural ownership", selector.name()),
            ),
            ResolutionIssue::ProhibitedSelector(selector) => (
                selector.tag(),
                format!(
                    "{} is prohibited on an administrative message",
                    selector.name()
                ),
            ),
        };
        let field = if matches!(issue, ResolutionIssue::DuplicateSelector(_)) {
            ctx.message
                .fields
                .iter()
                .filter(|field| field.tag == selector)
                .nth(1)
        } else {
            ctx.message.first(selector)
        };
        let Some(field) = field.or_else(|| ctx.message.first(tag::BEGIN_STRING)) else {
            return Vec::new();
        };
        vec![Diag {
            code: self.id(),
            rule: self.id(),
            start: field.tag_start,
            end: field.value_end(),
            message,
            severity,
            document_key: None,
        }]
    }
}

pub struct DictionaryVersion;

impl Rule for DictionaryVersion {
    fn id(&self) -> &'static str {
        "dictionaryVersion"
    }

    fn precondition(&self) -> Precondition {
        Precondition::Fields
    }

    fn default_severity(&self) -> Severity {
        Severity::Warning
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        let Some(selected) = ctx.selected_version else {
            return Vec::new();
        };
        let resolved = ctx.message.version.as_ref();
        if !ctx.message.resolution.selected_mismatch {
            return Vec::new();
        }
        let Some(field) = ctx
            .message
            .first(tag::BEGIN_STRING)
            .or_else(|| ctx.message.fields.first())
        else {
            return Vec::new();
        };
        vec![Diag {
            code: self.id(),
            rule: self.id(),
            start: field.tag_start,
            end: field.value_end(),
            message: format!("Selected dictionary version {selected} does not match resolved message version {resolved}"),
            severity,
            document_key: Some(ctx.message.version.clone()),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{codes, diags};
    use crate::codec::byte;

    fn dictionary_warnings(
        line: &str,
        selected: &str,
        config: &crate::config::Diagnostics,
    ) -> Vec<super::Diag> {
        let context = crate::ctx::Ctx::for_test("FIX.4.4");
        let mut analysis = crate::ctx::analyze(line.as_bytes(), &context);
        let crate::codec::LineAnalysis::Message(message) = &mut analysis else {
            panic!("synthetic message");
        };
        message.resolution.selected_mismatch = true;
        crate::rules::Registry::with_selected_version(config, Some(selected))
            .check(
                line.as_bytes(),
                &analysis,
                &crate::rules::Scope {
                    only: Some("dictionaryVersion"),
                    ..Default::default()
                },
            )
            .diags
    }

    #[test]
    fn selected_dictionary_mismatch_uses_the_resolved_version() {
        let config = crate::config::Diagnostics::default();
        for line in ["8=FIX.4.4|35=0|", "8=FIX.4.4|35=D|"] {
            let warnings = dictionary_warnings(line, "FIX.4.2", &config);
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].severity, crate::config::Severity::Warning);
            assert!(warnings[0].message.contains("FIX.4.4"));
        }
    }

    #[test]
    fn selected_transport_family_does_not_compare_application_versions() {
        let context = crate::ctx::Ctx::for_test("FIX.4.4");
        let line = b"8=FIXT.1.1|35=D|1128=9|";
        let analysis = crate::ctx::analyze(line, &context);
        let registry = crate::rules::Registry::with_selected_version(
            &crate::config::Diagnostics::default(),
            Some("FIXT.1.1"),
        );
        assert!(registry
            .check(
                line,
                &analysis,
                &crate::rules::Scope {
                    only: Some("dictionaryVersion"),
                    ..Default::default()
                }
            )
            .diags
            .is_empty());
    }

    #[test]
    fn dictionary_warning_obeys_rule_and_global_gates_and_severity() {
        let mut config = crate::config::Diagnostics::default();
        config.rules.insert(
            "dictionaryVersion".into(),
            crate::config::RuleSetting::Enabled(false),
        );
        assert!(dictionary_warnings("8=FIX.4.4|35=0|", "FIX.4.2", &config).is_empty());
        config.rules.insert(
            "dictionaryVersion".into(),
            crate::config::RuleSetting::Detailed {
                enabled: Some(true),
                severity: Some(crate::config::Severity::Hint),
                rest: Default::default(),
            },
        );
        assert_eq!(
            dictionary_warnings("8=FIX.4.4|35=0|", "FIX.4.2", &config)[0].severity,
            crate::config::Severity::Hint
        );
        config.enabled = false;
        assert!(dictionary_warnings("8=FIX.4.4|35=0|", "FIX.4.2", &config).is_empty());
    }

    #[test]
    fn a_valid_message_reports_nothing_structural() {
        let line = "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|\
52=20251026-09:02:00.000|10=171|";
        assert!(codes(line).is_empty(), "{:?}", diags(line));
    }

    #[test]
    fn a_fragment_is_reported_over_all_of_its_fields() {
        let line = "35=D|49=CLIENT1|56=BROKER1|11=ORDER1|55=BTCUSD|54=1|38=10";
        let found = diags(line);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "beginString");
        assert_eq!(found[0].start, byte(0));
        assert_eq!(found[0].end, byte(line.len() as u32));
        assert_eq!(
            found[0].message,
            "Not a FIX message: missing BeginString(8)"
        );
    }

    #[test]
    fn an_unsupported_version_reports_no_dictionary_interpretation() {
        let line = "8=FIX.9.9|9=52|35=0|49=CLIENT1|56=BROKER1|\
52=20260101-00:00:00.000|10=001|";
        let found = diags(line);
        let unknown = found
            .iter()
            .find(|d| d.code == "unknownVersion")
            .expect("reported");
        assert_eq!((unknown.start, unknown.end), (byte(0), byte(9)));
        assert_eq!(
            unknown.message,
            "Unsupported BeginString(8) = FIX.9.9; no dictionary interpretation"
        );
    }

    /// The case a silencing scheme would have got wrong. Tiers — "a tier that
    /// reports silences the tiers below it" — would have let `unknownVersion`
    /// suppress the trailer arithmetic on this line. But BodyLength and
    /// CheckSum are computed from the bytes, not from the dictionary: not
    /// knowing which version a line claims says nothing about whether it counts
    /// its own bytes correctly. Hence a precondition on the rule that has one,
    /// not an ordering between rules.
    #[test]
    fn an_unknown_version_does_not_silence_the_integrity_rules() {
        let line = "8=FIX.9.9|9=50|35=0|49=CLIENT1|56=BROKER1|\
52=20260101-00:00:00.000|10=001|";
        let mut found = codes(line);
        found.sort_unstable();
        assert_eq!(found, vec!["bodyLength", "checkSum", "unknownVersion"]);
    }

    #[test]
    fn a_known_version_is_not_reported() {
        for version in [
            "FIX.4.0", "FIX.4.1", "FIX.4.2", "FIX.4.3", "FIX.4.4", "FIXT.1.1",
        ] {
            let line = format!("8={version}|9=1|35=0|10=001|");
            assert!(
                !codes(&line).contains(&"unknownVersion"),
                "{version} reported"
            );
        }
    }
    #[test]
    fn every_resolution_issue_is_reported_without_string_inference() {
        use super::{LineCtx, ResolutionIssue, Rule, UnknownVersion};
        use crate::config::Severity;

        let context = crate::ctx::Ctx::for_test("FIX.4.4");
        let line = b"8=FIX.4.4|35=D|1128=9|1156=0|1129=sample|";
        for issue in [
            ResolutionIssue::UnsupportedBeginString,
            ResolutionIssue::InvalidSelector(crate::codec::Selector::APPL_VER_ID),
            ResolutionIssue::DuplicateSelector(crate::codec::Selector::APPL_VER_ID),
            ResolutionIssue::MissingApplication,
            ResolutionIssue::UnregisteredApplication,
            ResolutionIssue::SelectorInGroup(crate::codec::Selector::APPL_VER_ID),
            ResolutionIssue::AmbiguousSelector(crate::codec::Selector::APPL_VER_ID),
            ResolutionIssue::ProhibitedSelector(crate::codec::Selector::APPL_VER_ID),
        ] {
            let mut analysis = crate::ctx::analyze(line, &context);
            let crate::codec::LineAnalysis::Message(message) = &mut analysis else {
                panic!("synthetic message");
            };
            message.resolution.issue = Some(issue);
            let found = UnknownVersion.check(&LineCtx::new(line, message), Severity::Warning);
            assert_eq!(found.len(), 1, "{issue:?}");
            assert_eq!(found[0].code, "unknownVersion");
            assert_eq!(found[0].rule, "unknownVersion");
        }
    }

    #[test]
    fn missing_begin_string_is_not_reported_twice() {
        use super::{LineCtx, ResolutionIssue, Rule, UnknownVersion};

        let context = crate::ctx::Ctx::for_test("FIX.4.4");
        let line = b"35=D|55=X|";
        let analysis = crate::ctx::analyze(line, &context);

        let message = analysis.message().unwrap();

        assert_eq!(
            message.resolution.issue,
            Some(ResolutionIssue::MissingBeginString)
        );
        assert!(UnknownVersion
            .check(
                &LineCtx::new(line, message),
                crate::config::Severity::Warning
            )
            .is_empty());
    }

    #[test]
    fn repeated_identical_selectors_are_reported_at_the_second_occurrence() {
        let line = "8=FIXT.1.1|35=D|1128=9|1128=9|";
        let found = diags(line);

        let diagnostic = found
            .iter()
            .find(|diagnostic| diagnostic.code == "unknownVersion")
            .unwrap();

        assert_eq!(diagnostic.message, "Duplicate ApplVerID(1128)");
        assert_eq!(
            &line[diagnostic.start.index()..diagnostic.end.index()],
            "1128=9"
        );
        assert_eq!(diagnostic.start.index(), line.rfind("1128=9").unwrap());
    }

    #[test]
    fn a_group_selector_is_reported_after_group_annotations_are_cleared() {
        use crate::dictionary::parse::{fixtures::source, parse_quickfix_bytes};

        let dictionary = parse_quickfix_bytes(&source(
            "synthetic.xml",
            "<fix version='FIX.4.4'>\
             <header><field name='BeginString'/><field name='MsgType'/></header>\
             <messages><message name='NewOrderSingle' msgtype='D' msgcat='app'>\
             <group name='NoPartyIDs'><field name='PartyID'/><field name='ApplVerID'/></group>\
             </message></messages><fields>\
             <field number='8' name='BeginString' type='STRING'/>\
             <field number='35' name='MsgType' type='STRING'/>\
             <field number='453' name='NoPartyIDs' type='NUMINGROUP'/>\
             <field number='448' name='PartyID' type='STRING'/>\
             <field number='1128' name='ApplVerID' type='STRING'/>\
             </fields></fix>"
                .into(),
        ))
        .unwrap();
        let context = crate::ctx::Ctx::for_test_with_dict("FIX.4.4", dictionary);
        let line = b"8=FIX.4.4|35=D|453=1|448=sample|1128=6|";
        let analysis = crate::ctx::analyze(line, &context);
        let message = analysis.message().unwrap();
        assert_eq!(
            message.resolution.issue,
            Some(super::ResolutionIssue::SelectorInGroup(
                crate::codec::Selector::APPL_VER_ID
            ))
        );
        assert!(message.groups.is_empty());
        assert!(message
            .fields
            .iter()
            .all(|field| field.group_idx().is_none()));
        let checked = crate::rules::Registry::from_config(&crate::config::Diagnostics::default())
            .check(
                line,
                &analysis,
                &crate::rules::Scope {
                    only: Some("unknownVersion"),
                    ..Default::default()
                },
            );
        assert_eq!(checked.diags.len(), 1);
        let diagnostic = &checked.diags[0];
        assert_eq!(
            &line[diagnostic.start.index()..diagnostic.end.index()],
            b"1128=6"
        );
    }
}
