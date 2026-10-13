//! Validation rules over a decoded line.
//!
//! A rule is a pure function of a `MessageAnalysis`, the line bytes and the
//! name the caller gives the line's dictionary, which is what keeps
//! `diagnostics.rules.*` and dictionary names out of `CtxFingerprint`:
//! changing either invalidates no cached analysis.
//!
//! Spans are bytes. Conversion to LSP positions happens at the publish site and
//! nowhere else, so a rule never constructs a protocol type.

pub mod integrity;
pub mod structure;
pub mod unknown;

use std::cell::OnceCell;
use std::sync::LazyLock;

use crate::codec::{ByteOffset, Integrity, LineAnalysis, MessageAnalysis};
use crate::config::{Diagnostics, Severity};
use crate::ctx::Labels;
use crate::tag;

/// Every rule this server knows, in the order they are checked.
pub const BUILTINS: &[&dyn Rule] = &[
    &structure::BeginString,
    &structure::UnknownVersion,
    &structure::DictionaryVersion,
    &integrity::Integrity,
    &unknown::UnknownMsgType,
    &unknown::UnknownTag,
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
    /// At least one source is known under an explicit, valid interpretation.
    Bound,
}

/// What a rule sees.
pub struct LineCtx<'a> {
    pub bytes: &'a [u8],
    pub message: &'a MessageAnalysis,
    pub selected_version: Option<&'a str>,
    /// The names of the dictionaries that decoded the line — configured, else
    /// the file name; `None` for bundled tables.
    pub labels: Labels<'a>,
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
            labels: Labels::default(),
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

    fn bound(&self) -> bool {
        self.message.is_bound()
    }
}

pub trait Rule: Sync {
    /// The rule's key under `diagnostics.rules`, and the `rule` in the `data` of
    /// every diagnostic it reports. A rule may report more than one `code`.
    fn id(&self) -> &'static str;
    fn precondition(&self) -> Precondition;
    fn default_severity(&self) -> Severity;
    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag>;

    /// How many findings `check` would return, without rendering them. Only for
    /// a rule whose findings carry no `document_key`: those are deduplicated by
    /// the whole-document walk, which needs them in full.
    fn count(&self, _ctx: &LineCtx) -> Option<usize> {
        None
    }
}

/// What one check asks of a line. The default: every enabled rule, every
/// finding rendered, no dictionary name.
#[derive(Debug, Default)]
pub struct Scope<'a> {
    pub labels: Labels<'a>,
    /// Findings this severe or less are counted rather than rendered, by
    /// rules that can: the walk's cap would refuse them anyway.
    pub count_from: Option<Severity>,
    /// Only this rule, and nothing when it is disabled.
    pub only: Option<&'a str>,
}

/// One line's findings under a [`Scope`].
#[derive(Debug, Default)]
pub struct Checked {
    pub diags: Vec<Diag>,
    pub counted: usize,
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

    /// Exactly `rules`, for tests that need a rule of their own.
    #[cfg(test)]
    pub(crate) fn of_rules(
        rules: Vec<(&'static dyn Rule, Severity)>,
        selected_version: Option<&str>,
    ) -> Registry {
        Registry {
            rules,
            selected_version: selected_version.map(str::to_owned),
        }
    }

    /// Empty for a line that is not a message: there is nothing to check.
    pub fn check(&self, bytes: &[u8], analysis: &LineAnalysis, scope: &Scope) -> Checked {
        let mut checked = Checked::default();
        let Some(message) = analysis.message() else {
            return checked;
        };
        let mut ctx = LineCtx::new(bytes, message);
        ctx.selected_version = self.selected_version.as_deref();
        ctx.labels = scope.labels;
        let is_fix = message.first(tag::BEGIN_STRING).is_some();
        let bound = OnceCell::new();
        for (rule, severity) in &self.rules {
            if scope.only.is_some_and(|id| id != rule.id()) {
                continue;
            }
            let holds = match rule.precondition() {
                Precondition::Fields => !message.fields.is_empty(),
                Precondition::FixMessage => is_fix,
                Precondition::Bound => *bound.get_or_init(|| ctx.bound()),
            };
            if !holds {
                continue;
            }
            let refused = scope
                .count_from
                .is_some_and(|from| severity.rank() >= from.rank());
            match refused.then(|| rule.count(&ctx)).flatten() {
                Some(count) => checked.counted += count,
                None => checked.diags.extend(rule.check(&ctx, *severity)),
            }
        }
        checked
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
        Registry::from_config(config)
            .check(line.as_bytes(), &analysis, &Scope::default())
            .diags
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

    /// One finding per line it runs on, saying the label it was given.
    struct Probe;

    impl Rule for Probe {
        fn id(&self) -> &'static str {
            "probe"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Bound
        }
        fn default_severity(&self) -> Severity {
            Severity::Info
        }
        fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
            vec![Diag {
                code: self.id(),
                rule: self.id(),
                start: ByteOffset::new(0),
                end: ByteOffset::new(1),
                message: ctx.labels.application.unwrap_or("no label").to_owned(),
                severity,
                document_key: None,
            }]
        }
        fn count(&self, _ctx: &LineCtx) -> Option<usize> {
            Some(1)
        }
    }

    fn probe(ctx: &Ctx) -> Registry {
        Registry::of_rules(vec![(&Probe, Severity::Info)], ctx.selected_version())
    }

    fn only(id: &str) -> Scope<'_> {
        Scope {
            only: Some(id),
            ..Scope::default()
        }
    }

    fn bound(ctx: &Ctx, line: &[u8]) -> bool {
        let analysis = analyze(line, ctx);
        !probe(ctx)
            .check(line, &analysis, &Scope::default())
            .diags
            .is_empty()
    }

    #[test]
    fn a_line_is_bound_by_its_own_begin_string() {
        let ctx = Ctx::for_test("FIX.4.4");
        assert!(bound(&ctx, b"8=FIX.4.4|35=0|"));
        assert!(
            bound(&ctx, b"8=FIXT.1.1|35=0|"),
            "administrative FIXT messages need only transport"
        );
        assert!(
            !bound(&ctx, b"8=FIX.9.9|35=0|"),
            "unsupported wire identity"
        );
        assert!(!bound(&ctx, b"8=|35=0|"));
        assert!(!bound(&ctx, b"8=\xff|35=0|"));
        assert!(!bound(&ctx, b"55=X|"), "a fragment with nothing selected");
    }

    #[test]
    fn a_selection_never_binds_fragments_or_unsupported_wire_versions() {
        for selected in ["FIX.4.4", "FIX.5.0SP2", "FIX.9.1"] {
            let context = Ctx::for_test_selected(selected);
            for line in [
                b"55=X|".as_slice(),
                b"8=FIX.9.9|35=0|",
                b"8=FIX.5.0SP2|35=0|",
                b"8=|35=0|",
                b"8=\xff|35=0|",
            ] {
                assert!(!bound(&context, line), "{selected}: {line:?}");
            }
        }
    }

    #[test]
    fn transport_binds_an_unresolved_application_without_guessing_its_body() {
        let context = Ctx::for_test("FIX.4.4");
        assert!(bound(&context, b"8=FIXT.1.1|35=D|55=X|"));
        let line = b"8=FIXT.1.1|35=D|9000=x|";
        let analysis = analyze(line, &context);
        let checked = Registry::from_config(&Diagnostics::default()).check(
            line,
            &analysis,
            &Scope::default(),
        );
        assert!(checked
            .diags
            .iter()
            .any(|diag| diag.code == "unknownVersion"));
        assert!(!checked
            .diags
            .iter()
            .any(|diag| diag.code == "unknownTag" || diag.code == "unknownMsgType"));
    }

    #[test]
    fn the_label_reaches_the_rule() {
        let ctx = Ctx::for_test("FIX.4.4");
        let line = b"8=FIX.4.4|35=0|";
        let analysis = analyze(line, &ctx);
        let scope = Scope {
            labels: Labels {
                transport: None,
                application: Some("desk"),
            },
            ..Scope::default()
        };
        let found = probe(&ctx).check(line, &analysis, &scope).diags;
        assert_eq!(found[0].message, "desk");
    }

    #[test]
    fn one_rule_can_be_run_alone() {
        let ctx = Ctx::for_test("FIX.4.4");
        let bad = b"8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        let analysis = analyze(bad, &ctx);
        let registry = Registry::from_config(&Diagnostics::default());
        let found = registry.check(bad, &analysis, &only("integrity")).diags;
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|diag| diag.rule == "integrity"));
        assert!(registry
            .check(bad, &analysis, &only("beginString"))
            .diags
            .is_empty());

        let mut config = Diagnostics::default();
        config
            .rules
            .insert("integrity".into(), RuleSetting::Enabled(false));
        let registry = Registry::from_config(&config);
        assert!(
            registry
                .check(bad, &analysis, &only("integrity"))
                .diags
                .is_empty(),
            "disabled"
        );
    }

    #[test]
    fn a_finding_past_the_refusal_is_counted_not_rendered() {
        let ctx = Ctx::for_test("FIX.4.4");
        let line = b"8=FIX.4.4|35=0|";
        let analysis = analyze(line, &ctx);
        let registry = probe(&ctx);
        let within = |count_from| {
            registry.check(
                line,
                &analysis,
                &Scope {
                    count_from,
                    ..Scope::default()
                },
            )
        };
        let rendered = within(None);
        assert_eq!((rendered.diags.len(), rendered.counted), (1, 0));
        let counted = within(Some(Severity::Info));
        assert_eq!((counted.diags.len(), counted.counted), (0, 1));
        let less_severe = within(Some(Severity::Hint));
        assert_eq!(
            (less_severe.diags.len(), less_severe.counted),
            (1, 0),
            "info is not refused yet"
        );

        // A rule that cannot count is rendered whatever the refusal.
        let bad = b"8=FIX.4.4|9=7|35=0|34=1|49=A|56=B|10=001|";
        let analysis = analyze(bad, &ctx);
        let checked = Registry::from_config(&Diagnostics::default()).check(
            bad,
            &analysis,
            &Scope {
                count_from: Some(Severity::Error),
                ..Scope::default()
            },
        );
        assert_eq!((checked.diags.len(), checked.counted), (2, 0));
    }

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
                "integrity",
                "unknownMsgType",
                "unknownTag",
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
