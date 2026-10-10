//! Whether the line is a FIX message at all, and whether its version is known.
//!
//! `unknownVersion` exists because decoding against the fallback version is a
//! guess, and a guess the reader cannot see is one they cannot check.

use super::{Diag, LineCtx, Precondition, Rule};
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
            message: "Not a FIX message: missing BeginString (tag 8)".into(),
            severity,
            document_key: None,
        }]
    }
}

/// A BeginString the dictionaries do not answer for. The line was still decoded
/// — against `defaultVersion` — so the reader should know which tables produced
/// the names they are looking at.
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
        let Some(field) = ctx.message.first(tag::BEGIN_STRING) else {
            return Vec::new();
        };
        // Compared against what the line *resolved* to rather than against the
        // known-version list: a custom dictionary bound to its own version
        // resolves to itself and must not be called unknown.
        let Some(written) = ctx.begin_string() else {
            return Vec::new();
        };
        if written == ctx.message.version.as_ref() {
            return Vec::new();
        }
        vec![Diag {
            code: self.id(),
            rule: self.id(),
            start: field.tag_start,
            end: field.value_end(),
            message: format!(
                "Unknown BeginString {written}; decoded as {}",
                ctx.message.version
            ),
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
        if crate::dictionary::versions_compatible(selected, resolved) {
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
        let analysis = crate::ctx::analyze(line.as_bytes(), &context);
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
        for line in ["8=FIX.4.4|35=0|", "8=FIX.9.9|35=0|", "35=0|"] {
            let warnings = dictionary_warnings(line, "FIX.4.2", &config);
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].severity, crate::config::Severity::Warning);
            assert!(warnings[0].message.contains("FIX.4.4"));
        }
    }

    #[test]
    fn selected_dictionary_alias_is_asymmetric() {
        let config = crate::config::Diagnostics::default();
        assert!(dictionary_warnings("8=FIXT.1.1|35=0|", "FIX.5.0SP2", &config).is_empty());
        assert_eq!(
            dictionary_warnings("8=FIX.5.0SP2|35=0|", "FIXT.1.1", &config).len(),
            1
        );
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
            "Not a FIX message: missing BeginString (tag 8)"
        );
    }

    #[test]
    fn an_unknown_version_is_reported_and_names_what_it_decoded_as() {
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
            "Unknown BeginString FIX.9.9; decoded as FIX.4.4"
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
        for version in ["FIX.4.2", "FIX.4.4", "FIX.5.0SP2", "FIXT.1.1"] {
            let line = format!("8={version}|9=1|35=0|10=001|");
            assert!(
                !codes(&line).contains(&"unknownVersion"),
                "{version} reported"
            );
        }
    }
}
