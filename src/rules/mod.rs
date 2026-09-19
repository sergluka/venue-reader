//! Validation rules over a decoded line.
//!
//! A rule is a pure function of a `MessageAnalysis` plus the line bytes, which
//! is what keeps `diagnostics.rules.*` out of `CtxFingerprint`: toggling one
//! invalidates no cached analysis.
//!
//! Spans are bytes. Conversion to LSP positions happens at the publish site and
//! nowhere else, so a rule never constructs a protocol type.

pub mod integrity;
pub mod structure;

use std::cell::OnceCell;
use std::sync::LazyLock;

use crate::codec::{ByteOffset, Integrity, LineAnalysis, MessageAnalysis};
use crate::config::{Diagnostics, Severity};
use crate::tag;

/// Every rule this server knows, in the order they are checked.
pub const BUILTINS: &[&dyn Rule] = &[
    &structure::BeginString,
    &structure::UnknownVersion,
    &structure::DictionaryVersion,
    &integrity::Integrity,
];

/// A problem on one line, in bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    /// What is wrong: the diagnostic's `code`.
    pub code: &'static str,
    /// The rule that reported it — its `diagnostics.rules` key, and the `rule`
    /// in the diagnostic's `data`. Not the same string as `code`: BodyLength and
    /// CheckSum are two problems with one cause, one switch and one repair.
    pub rule: &'static str,
    pub start: ByteOffset,
    pub end: ByteOffset,
    pub message: String,
    pub severity: Severity,
    /// Whole-document consumers keep the first diagnostic for each
    /// `(rule, key)`. Line-scoped consumers need no special cases.
    pub document_key: Option<std::sync::Arc<str>>,
}

/// One replacement in a line, in bytes. `start == end` is an insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: ByteOffset,
    pub end: ByteOffset,
    pub text: String,
}

/// The corrected line, as the edits that get it there. One per line rather than
/// one per problem: correcting BodyLength changes the bytes the CheckSum is
/// taken over, so applying half would leave the line wrong in a new way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub title: String,
    pub edits: Vec<Edit>,
}

/// What a line must already be for a rule to have anything to say about it.
///
/// Stated as a precondition rather than as an ordering rule, so it lives on
/// the rule that has it: BodyLength arithmetic over a fragment is arithmetic
/// about nothing, and that is a fact about the integrity rules rather than
/// about which rule ran first. It also holds when `beginString` is switched
/// off, where a silencing scheme would stop silencing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// Any line the tokenizer read fields from.
    Fields,
    /// …and it carries a BeginString.
    FixMessage,
}

/// What a rule sees.
pub struct LineCtx<'a> {
    pub bytes: &'a [u8],
    pub message: &'a MessageAnalysis,
    pub selected_version: Option<&'a str>,
    /// Both integrity rules need this and neither owns it: shared scratch,
    /// computed at most once per line.
    integrity: OnceCell<Integrity>,
}

impl<'a> LineCtx<'a> {
    pub fn new(bytes: &'a [u8], message: &'a MessageAnalysis) -> LineCtx<'a> {
        LineCtx {
            bytes,
            message,
            selected_version: None,
            integrity: OnceCell::new(),
        }
    }

    pub fn integrity(&self) -> &Integrity {
        self.integrity
            .get_or_init(|| self.message.integrity(self.bytes))
    }

    /// BeginString(8)'s value as text, when the line carries one and it is UTF-8.
    pub fn begin_string(&self) -> Option<&'a str> {
        self.message.first(tag::BEGIN_STRING)?.text(self.bytes)
    }
}

pub trait Rule: Sync {
    /// The rule's key under `diagnostics.rules`, and the `rule` in the `data` of
    /// every diagnostic it reports. A rule may report more than one `code`.
    fn id(&self) -> &'static str;
    fn precondition(&self) -> Precondition;
    fn default_severity(&self) -> Severity;
    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag>;
}

/// The enabled rules and the severity each was configured with. Rebuilt on
/// every committed configuration.
pub struct Registry {
    rules: Vec<(&'static dyn Rule, Severity)>,
    selected_version: Option<String>,
}

impl Registry {
    /// The ids `BUILTINS` claims. `overrides/` needs them to tell a typo from a
    /// rule and cannot depend on `rules/`, so they are passed in as a slice.
    pub fn known_ids() -> &'static [&'static str] {
        static KNOWN: LazyLock<Vec<&'static str>> =
            LazyLock::new(|| BUILTINS.iter().map(|rule| rule.id()).collect());
        &KNOWN
    }

    pub fn from_config(config: &Diagnostics) -> Registry {
        Self::with_selected_version(config, None)
    }

    pub fn with_selected_version(config: &Diagnostics, selected_version: Option<&str>) -> Registry {
        let rules = if config.enabled {
            BUILTINS
                .iter()
                .filter_map(|rule| {
                    let setting = config.rules.get(rule.id());
                    let enabled = setting.and_then(|s| s.enabled()).unwrap_or(true);
                    let severity = setting
                        .and_then(|s| s.severity())
                        .unwrap_or_else(|| rule.default_severity());
                    enabled.then_some((*rule, severity))
                })
                .collect()
        } else {
            Vec::new()
        };
        Registry {
            rules,
            selected_version: selected_version.map(str::to_owned),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Empty for a line that is not a message: there is nothing to check.
    pub fn check_line(&self, bytes: &[u8], analysis: &LineAnalysis) -> Vec<Diag> {
        let Some(message) = analysis.message() else {
            return Vec::new();
        };
        let mut ctx = LineCtx::new(bytes, message);
        ctx.selected_version = self.selected_version.as_deref();
        let is_fix = message.first(tag::BEGIN_STRING).is_some();
        let mut out = Vec::new();
        for (rule, severity) in &self.rules {
            let holds = match rule.precondition() {
                Precondition::Fields => !message.fields.is_empty(),
                Precondition::FixMessage => is_fix,
            };
            if holds {
                out.extend(rule.check(&ctx, *severity));
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::RuleSetting;
    use crate::ctx::{analyze, Ctx};

    /// Analyse one line against the bundled FIX.4.4 tables and run every rule.
    pub(crate) fn diags(line: &str) -> Vec<Diag> {
        diags_with(line, &Diagnostics::default())
    }

    pub(crate) fn diags_with(line: &str, config: &Diagnostics) -> Vec<Diag> {
        let ctx = Ctx::for_test("FIX.4.4");
        let analysis = analyze(line.as_bytes(), &ctx);
        Registry::from_config(config).check_line(line.as_bytes(), &analysis)
    }

    /// What each diagnostic says is wrong, not which rule said it.
    pub(crate) fn codes(line: &str) -> Vec<&'static str> {
        diags(line).into_iter().map(|d| d.code).collect()
    }

    pub(crate) fn repair_of(line: &str) -> Option<Repair> {
        let ctx = Ctx::for_test("FIX.4.4");
        let analysis = analyze(line.as_bytes(), &ctx);
        let message = analysis.message()?;
        integrity::repair(&LineCtx::new(line.as_bytes(), message))
    }

    /// The line as the repair leaves it. Applied back to front so that an edit
    /// does not move the span of the one before it.
    pub(crate) fn repaired(line: &str) -> String {
        let Some(repair) = repair_of(line) else {
            return line.to_owned();
        };
        let mut bytes = line.as_bytes().to_vec();
        for edit in repair.edits.iter().rev() {
            bytes.splice(edit.start.index()..edit.end.index(), edit.text.bytes());
        }
        String::from_utf8(bytes).expect("the fixtures are text")
    }

    const FRAGMENT: &str = "268=2|269=0|270=1.5|269=1|270=1.6";

    #[test]
    fn every_builtin_id_is_distinct() {
        let mut ids: Vec<&str> = BUILTINS.iter().map(|rule| rule.id()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "an id is also a config key: {ids:?}");
    }

    #[test]
    fn a_line_that_is_not_a_message_is_not_checked() {
        assert!(codes("").is_empty());
        assert!(codes("# a comment").is_empty());
    }

    #[test]
    fn a_fragment_gets_one_begin_string_diagnostic_and_nothing_else() {
        assert_eq!(codes(FRAGMENT), vec!["beginString"]);
    }

    /// What a silencing scheme keyed on *reporting* would get wrong: disabling
    /// `beginString` would stop it silencing anything, and a dumped repeating
    /// group would start collecting "Missing CheckSum". The precondition is on
    /// the rule, so switching another rule off cannot reach it.
    #[test]
    fn a_fragment_stays_silent_even_with_begin_string_disabled() {
        let mut config = Diagnostics::default();
        config
            .rules
            .insert("beginString".into(), RuleSetting::Enabled(false));
        assert!(diags_with(FRAGMENT, &config).is_empty());
    }

    #[test]
    fn disabling_diagnostics_disables_every_rule() {
        let config = Diagnostics {
            enabled: false,
            ..Diagnostics::default()
        };
        assert!(Registry::from_config(&config).is_empty());
        assert!(diags_with(FRAGMENT, &config).is_empty());
    }

    #[test]
    fn a_configured_severity_reaches_the_diagnostic() {
        let mut config = Diagnostics::default();
        config.rules.insert(
            "integrity".into(),
            RuleSetting::Detailed {
                enabled: None,
                severity: Some(Severity::Hint),
                rest: Default::default(),
            },
        );
        let bad = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        let check_sum = diags_with(bad, &config)
            .into_iter()
            .find(|d| d.code == "checkSum")
            .expect("a checkSum diagnostic");
        assert_eq!(check_sum.severity, Severity::Hint);
    }

    /// BodyLength and CheckSum are one property of a line — the trailer
    /// arithmetic — and one repair. Two switches over one property produce a
    /// state nothing can answer: half of it silenced, and an edit that must
    /// still correct both.
    #[test]
    fn integrity_is_one_rule_reporting_two_codes() {
        let ids: Vec<&str> = BUILTINS.iter().map(|rule| rule.id()).collect();
        assert_eq!(
            ids,
            vec![
                "beginString",
                "unknownVersion",
                "dictionaryVersion",
                "integrity"
            ]
        );

        let bad = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        let found = diags(bad);
        let mut codes: Vec<&str> = found.iter().map(|d| d.code).collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            vec!["bodyLength", "checkSum"],
            "the codes do not move"
        );
        assert!(
            found.iter().all(|d| d.rule == "integrity"),
            "one rule owns both"
        );
    }

    #[test]
    fn disabling_integrity_silences_both_codes_and_nothing_else() {
        let mut config = Diagnostics::default();
        config
            .rules
            .insert("integrity".into(), RuleSetting::Enabled(false));
        let bad = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        assert!(diags_with(bad, &config).is_empty());
        assert_eq!(
            diags_with(FRAGMENT, &config)
                .into_iter()
                .map(|d| d.code)
                .collect::<Vec<_>>(),
            vec!["beginString"],
            "an unrelated rule is untouched"
        );
    }

    /// The keys 4a-1 shipped are gone. Silently doing nothing is what the
    /// unknown-id warning exists to prevent.
    #[test]
    fn the_old_per_quantity_keys_are_now_unknown() {
        let mut config = Diagnostics::default();
        for id in ["bodyLength", "checkSum"] {
            config.rules.insert(id.into(), RuleSetting::Enabled(false));
        }
        for id in ["bodyLength", "checkSum"] {
            assert!(
                !Registry::known_ids().contains(&id),
                "{id} is claimed again"
            );
        }
        let bad = "8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        assert_eq!(diags_with(bad, &config).len(), 2, "and they change nothing");
    }
}
