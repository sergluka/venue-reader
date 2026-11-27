//! Shared safe-repair composition for range and document actions.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Diag, Edit, Fixability, LineCtx, Registry, Repair, Scope};
use crate::codec::{ByteOffset, LineAnalysis};
use crate::ctx::{analyze, Ctx, DictionaryNames};

pub const FIX_ALL_TITLE: &str = "Fix all";

#[derive(Clone, Copy)]
pub enum RepairPolicy {
    DiagnosticOnly,
    Safe(Provider),
}

/// Contributing repairs must reduce their findings without regressing earlier repairs.
/// Enabled finalizers must remain eligible before and after body repairs.
#[derive(Clone, Copy)]
pub struct Provider {
    /// Must return Some when an eligible automatic finding is claimed.
    pub repair: fn(&LineCtx) -> Option<Repair>,
    /// Safety check beyond Rule::precondition.
    pub eligible: fn(&LineCtx) -> bool,
    /// Selects automatic findings of this rule; eligibility is checked separately.
    pub claims: fn(&LineCtx, &Diag) -> bool,
    /// Rule IDs whose eligibility is required when enabled, recursively.
    pub dependencies: &'static [&'static str],
    /// Finalizers run after body repairs and must remain eligible across them.
    pub finalizer: bool,
}

#[derive(Debug)]
pub struct LineRepair {
    pub title: String,
    pub edits: Vec<Edit>,
    /// Only findings from the original snapshot may be attached to an action.
    pub diags: Vec<Diag>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RepairError {
    Cancelled,
    Unsafe(String),
}

impl fmt::Display for RepairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Repair cancelled"),
            Self::Unsafe(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RepairError {}

type Result<T> = std::result::Result<T, RepairError>;

fn checkpoint(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(RepairError::Cancelled)
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> RepairError {
    RepairError::Unsafe(message.into())
}

/// Cached analysis must belong to the supplied bytes and context.
pub fn repair_line(
    bytes: &[u8],
    cached: Option<&LineAnalysis>,
    ctx: &Ctx,
    registry: &Registry,
    names: &DictionaryNames,
    cancelled: &AtomicBool,
) -> Result<Option<LineRepair>> {
    checkpoint(cancelled)?;
    if registry.is_empty() {
        return Ok(None);
    }
    let mut fresh_analysis;
    let mut analysis = match cached {
        Some(analysis) => analysis,
        None => {
            fresh_analysis = analyze(bytes, ctx);
            &fresh_analysis
        }
    };
    let original_diags = findings(bytes, analysis, registry, names)?;
    checkpoint(cancelled)?;
    let Some(message) = analysis.message() else {
        return Ok(None);
    };
    if !original_diags
        .iter()
        .any(|diag| diag.kind.fixability == Fixability::Automatic)
    {
        return Ok(None);
    }
    let mut text: Option<(&str, String)> = None;
    let mut pieces = Pieces(vec![Piece::Original(0..bytes.len())]);
    let line = registry.line_ctx(bytes, message, names.of_analysis(message));
    let original_claims = original_diags
        .iter()
        .filter(|diag| {
            registry.rules.iter().any(|(rule, _)| {
                automatic_for(diag, rule.id())
                    && match rule.repair_policy() {
                        RepairPolicy::Safe(provider) => (provider.claims)(&line, diag),
                        RepairPolicy::DiagnosticOnly => false,
                    }
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut diags = original_diags;
    let mut repaired_rules = HashSet::new();
    let mut titles = Vec::new();
    let mut ceilings: HashMap<&'static str, usize> = HashMap::new();
    loop {
        checkpoint(cancelled)?;
        let mut contributions = Vec::new();
        let mut changed = false;
        for finalizer in [false, true] {
            let Some(message) = analysis.message() else {
                continue;
            };
            let current_bytes = text
                .as_ref()
                .map_or(bytes, |(_, current)| current.as_bytes());
            let line = registry.line_ctx(current_bytes, message, names.of_analysis(message));
            let candidates = candidates(&line, &diags, registry, finalizer, cancelled)?;
            if candidates.is_empty() {
                continue;
            }
            if text.is_none() {
                let original = std::str::from_utf8(bytes)
                    .map_err(|_| invalid("Cannot repair a non-UTF-8 line"))?;
                text = Some((original, original.to_owned()));
            }
            let (original, current) = text.as_mut().expect("applicable repair has UTF-8 text");
            let mut edits = Vec::new();
            for (rule, repair) in candidates {
                contributions.push((rule, automatic_count(&diags, rule)));
                repaired_rules.insert(rule);
                if !titles.contains(&repair.title) {
                    titles.push(repair.title);
                }
                edits.extend(repair.edits);
            }
            normalize_edits(current, &mut edits)?;
            if edits.is_empty() {
                return Err(invalid("A repair provider proposed no change"));
            }
            pieces.apply(&edits, cancelled)?;
            let next = pieces.text(original);
            if next == *current {
                return Err(invalid("A repair provider proposed no change"));
            }
            *current = next;
            fresh_analysis = analyze(current.as_bytes(), ctx);
            if let LineAnalysis::Skipped(reason) = &fresh_analysis {
                return Err(invalid(reason.to_string()));
            }
            analysis = &fresh_analysis;
            if !finalizer {
                let message = analysis
                    .message()
                    .ok_or_else(|| invalid("A body repair removed the message"))?;
                let line =
                    registry.line_ctx(current.as_bytes(), message, names.of_analysis(message));
                if !finalizers_eligible(&line, registry)? {
                    return Err(invalid("A body repair made a finalizer ineligible"));
                }
            }
            diags = findings(current.as_bytes(), analysis, registry, names)?;
            changed = true;
        }
        if !changed {
            break;
        }
        // New providers may become eligible; repaired providers must not regress.
        for (rule, ceiling) in &mut ceilings {
            let remaining = automatic_count(&diags, rule);
            if remaining > *ceiling {
                return Err(invalid(format!(
                    "Repair composition reintroduced findings: {rule}"
                )));
            }
            *ceiling = remaining;
        }
        for (rule, before) in contributions {
            let remaining = automatic_count(&diags, rule);
            if remaining >= before {
                return Err(invalid(format!(
                    "Repair provider did not reduce its findings: {rule}"
                )));
            }
            ceilings.insert(rule, remaining);
        }
    }
    checkpoint(cancelled)?;
    let Some((original, _)) = text else {
        return Ok(None);
    };
    let edits = pieces.edits(original)?;
    if edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(LineRepair {
        title: if titles.len() == 1 {
            titles.remove(0)
        } else {
            FIX_ALL_TITLE.into()
        },
        edits,
        diags: original_claims
            .into_iter()
            .filter(|diag| repaired_rules.contains(diag.rule))
            .collect(),
    }))
}

fn automatic_for(diag: &Diag, rule: &str) -> bool {
    diag.rule == rule && diag.kind.fixability == Fixability::Automatic
}

fn automatic_count(diags: &[Diag], rule: &str) -> usize {
    diags
        .iter()
        .filter(|diag| automatic_for(diag, rule))
        .count()
}

fn findings(
    bytes: &[u8],
    analysis: &LineAnalysis,
    registry: &Registry,
    names: &DictionaryNames,
) -> Result<Vec<Diag>> {
    let labels = analysis
        .message()
        .map(|message| names.of_analysis(message))
        .unwrap_or_default();
    let diags = registry
        .check(
            bytes,
            analysis,
            &Scope {
                labels,
                ..Default::default()
            },
        )
        .diags;
    for diag in &diags {
        if diag.kind.fixability == Fixability::Automatic
            && !registry.rules.iter().any(|(rule, _)| {
                rule.id() == diag.rule && matches!(rule.repair_policy(), RepairPolicy::Safe(_))
            })
        {
            return Err(invalid(format!(
                "Automatic cause has no provider: {}",
                diag.rule
            )));
        }
    }
    Ok(diags)
}

fn candidates(
    line: &LineCtx,
    diags: &[Diag],
    registry: &Registry,
    finalizer: bool,
    cancelled: &AtomicBool,
) -> Result<Vec<(&'static str, Repair)>> {
    let mut candidates = Vec::new();
    for (rule, _) in &registry.rules {
        checkpoint(cancelled)?;
        let RepairPolicy::Safe(provider) = rule.repair_policy() else {
            continue;
        };
        if provider.finalizer != finalizer
            || !diags
                .iter()
                .any(|diag| automatic_for(diag, rule.id()) && (provider.claims)(line, diag))
            || !eligible(rule.id(), line, registry, &mut Vec::new())?
        {
            continue;
        }
        // Enabled finalizers must remain applicable to body edits.
        if !finalizer && !finalizers_eligible(line, registry)? {
            continue;
        }
        let repair = (provider.repair)(line).ok_or_else(|| {
            invalid(format!(
                "Eligible automatic cause has no repair: {}",
                rule.id()
            ))
        })?;
        candidates.push((rule.id(), repair));
    }
    Ok(candidates)
}

fn finalizers_eligible(line: &LineCtx, registry: &Registry) -> Result<bool> {
    for (rule, _) in &registry.rules {
        if matches!(rule.repair_policy(), RepairPolicy::Safe(provider) if provider.finalizer)
            && !eligible(rule.id(), line, registry, &mut Vec::new())?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn eligible(
    id: &str,
    line: &LineCtx,
    registry: &Registry,
    visiting: &mut Vec<&'static str>,
) -> Result<bool> {
    let Some((rule, _)) = registry.rules.iter().find(|(rule, _)| rule.id() == id) else {
        // Disabled dependencies do not disable another rule's repairs.
        return Ok(true);
    };
    if visiting.contains(&rule.id()) {
        return Err(invalid(format!("Cyclic repair dependency: {id}")));
    }
    let RepairPolicy::Safe(provider) = rule.repair_policy() else {
        return Ok(false);
    };
    if !rule.precondition().holds(line) || !(provider.eligible)(line) {
        return Ok(false);
    }
    visiting.push(rule.id());
    for dependency in provider.dependencies {
        if !eligible(dependency, line, registry, visiting)? {
            visiting.pop();
            return Ok(false);
        }
    }
    visiting.pop();
    Ok(true)
}

fn normalize_edits(current: &str, edits: &mut Vec<Edit>) -> Result<()> {
    edits.sort_by_key(|edit| (edit.start, edit.end));
    edits.dedup();
    let mut previous: Option<&Edit> = None;
    for edit in edits.iter() {
        let start = edit.start.index();
        let end = edit.end.index();
        if start > end
            || end > current.len()
            || !current.is_char_boundary(start)
            || !current.is_char_boundary(end)
            || edit.text.contains(['\n', '\r'])
        {
            return Err(invalid("A repair contains an invalid line edit"));
        }
        if let Some(previous) = previous {
            if edit.start < previous.end
                || (edit.start == previous.end
                    && (edit.start == edit.end || previous.start == previous.end))
            {
                return Err(invalid("Repair providers proposed overlapping edits"));
            }
        }
        previous = Some(edit);
    }
    edits.retain(|edit| current[edit.start.index()..edit.end.index()] != edit.text);
    Ok(())
}

#[derive(Clone)]
enum Piece {
    Original(Range<usize>),
    Inserted(String),
}

impl Piece {
    fn len(&self) -> usize {
        match self {
            Self::Original(range) => range.len(),
            Self::Inserted(text) => text.len(),
        }
    }

    fn slice(&self, range: Range<usize>) -> Result<Self> {
        Ok(match self {
            Self::Original(original) => {
                Self::Original(original.start + range.start..original.start + range.end)
            }
            Self::Inserted(text) => Self::Inserted(
                text.get(range)
                    .ok_or_else(|| invalid("A repair splits inserted Unicode text"))?
                    .to_owned(),
            ),
        })
    }
}

struct Pieces(Vec<Piece>);

impl Pieces {
    fn apply(&mut self, edits: &[Edit], cancelled: &AtomicBool) -> Result<()> {
        let mut output = Vec::new();
        let mut source = self.0.iter();
        let mut piece = source.next();
        let mut offset = 0;
        let mut position = 0;
        for edit in edits {
            checkpoint(cancelled)?;
            for (boundary, keep) in [(edit.start.index(), true), (edit.end.index(), false)] {
                while position < boundary {
                    checkpoint(cancelled)?;
                    let current =
                        piece.ok_or_else(|| invalid("Edit extends past composed line"))?;
                    let length = (boundary - position).min(current.len() - offset);
                    if keep && length > 0 {
                        output.push(current.slice(offset..offset + length)?);
                    }
                    position += length;
                    offset += length;
                    if offset == current.len() {
                        piece = source.next();
                        offset = 0;
                    }
                }
            }
            if !edit.text.is_empty() {
                output.push(Piece::Inserted(edit.text.clone()));
            }
        }
        if let Some(current) = piece {
            if offset < current.len() {
                output.push(current.slice(offset..current.len())?);
            }
        }
        output.extend(source.cloned());
        self.0 = output;
        Ok(())
    }

    fn text(&self, original: &str) -> String {
        let mut result = String::with_capacity(self.0.iter().map(Piece::len).sum());
        for piece in &self.0 {
            match piece {
                Piece::Original(range) => result.push_str(&original[range.clone()]),
                Piece::Inserted(text) => result.push_str(text),
            }
        }
        result
    }

    fn edits(&self, original: &str) -> Result<Vec<Edit>> {
        let mut edits = Vec::new();
        let mut cursor = 0;
        let mut text = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Original(range) => {
                    if range.start != cursor || !text.is_empty() {
                        push_edit(
                            &mut edits,
                            original,
                            cursor..range.start,
                            std::mem::take(&mut text),
                        )?;
                    }
                    cursor = range.end;
                }
                Piece::Inserted(inserted) => text.push_str(inserted),
            }
        }
        push_edit(&mut edits, original, cursor..original.len(), text)?;
        Ok(edits)
    }
}

fn push_edit(
    edits: &mut Vec<Edit>,
    original: &str,
    range: Range<usize>,
    text: String,
) -> Result<()> {
    if original[range.clone()] != text {
        edits.push(Edit {
            start: ByteOffset::from_len(range.start)
                .ok_or_else(|| invalid("Repair offset exceeds byte-coordinate range"))?,
            end: ByteOffset::from_len(range.end)
                .ok_or_else(|| invalid("Repair offset exceeds byte-coordinate range"))?,
            text,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::byte;
    use crate::config::{Diagnostics, RuleSetting, Severity};
    use crate::rules::{integrity::Integrity, DiagnosticKind, Precondition, Rule};

    struct Symbol;
    struct Conflict;
    struct MissingDependency;
    struct CyclicDependency;
    struct NoProgress;
    struct Unlocked;
    struct ErasesMessage;
    struct RemovesEnvelope;
    struct Unregistered;
    struct Regressor {
        mark_text: bool,
    }

    fn symbol_field<'a>(line: &'a LineCtx<'a>) -> Option<&'a crate::codec::Field> {
        line.message.first(55)
    }

    fn symbolic(line: &LineCtx, severity: Severity, rule: &'static str, always: bool) -> Vec<Diag> {
        let Some(field) = symbol_field(line) else {
            return Vec::new();
        };
        if !always && field.value(line.bytes) != b"BAD" {
            return Vec::new();
        }
        vec![Diag {
            kind: DiagnosticKind {
                code: "testRepair".into(),
                fixability: Fixability::Automatic,
            },
            rule,
            start: field.value_start(),
            end: field.value_end(),
            message: "Synthetic symbol repair".into(),
            severity,
            document_key: None,
        }]
    }

    fn replace_symbol(line: &LineCtx, value: &str) -> Option<Repair> {
        let field = symbol_field(line)?;
        Some(Repair {
            title: "Fix synthetic symbol".into(),
            edits: vec![Edit {
                start: field.value_start(),
                end: field.value_end(),
                text: value.into(),
            }],
        })
    }

    macro_rules! provider {
        ($name:ident, $id:literal, $dependencies:expr, $always:expr, $repair:expr) => {
            impl Rule for $name {
                fn id(&self) -> &'static str {
                    $id
                }
                fn precondition(&self) -> Precondition {
                    Precondition::Fields
                }
                fn default_severity(&self) -> Severity {
                    Severity::Warning
                }
                fn repair_policy(&self) -> RepairPolicy {
                    RepairPolicy::Safe(Provider {
                        repair: $repair,
                        eligible: |line| symbol_field(line).is_some(),
                        claims: |_, _| true,
                        dependencies: $dependencies,
                        finalizer: false,
                    })
                }
                fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
                    symbolic(line, severity, self.id(), $always)
                }
            }
        };
    }

    provider!(Symbol, "symbol", &[], false, |line| replace_symbol(
        line,
        "GOOD-LONG"
    ));
    provider!(Conflict, "conflict", &[], false, |line| replace_symbol(
        line, "OTHER"
    ));
    provider!(
        MissingDependency,
        "missingDependency",
        &["notEnabled"],
        false,
        |line| replace_symbol(line, "GOOD")
    );
    provider!(
        CyclicDependency,
        "cyclicDependency",
        &["cyclicDependency"],
        false,
        |line| replace_symbol(line, "GOOD")
    );
    provider!(NoProgress, "noProgress", &[], true, |line| {
        let field = symbol_field(line)?;
        replace_symbol(
            line,
            if field.value(line.bytes) == b"BAD" {
                "GOOD"
            } else {
                "BAD"
            },
        )
    });

    provider!(ErasesMessage, "erasesMessage", &[], false, |line| Some(
        Repair {
            title: "Fault injection".into(),
            edits: vec![Edit {
                start: byte(0),
                end: byte(line.bytes.len().try_into().unwrap()),
                text: String::new()
            }],
        }
    ));
    provider!(RemovesEnvelope, "removesEnvelope", &[], false, |line| Some(
        Repair {
            title: "Fault injection".into(),
            edits: vec![Edit {
                start: byte(0),
                end: byte(line.bytes.len().try_into().unwrap()),
                text: "55=BAD|".into()
            }],
        }
    ));

    impl Rule for Unregistered {
        fn id(&self) -> &'static str {
            "unregistered"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::DiagnosticOnly
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            symbolic(line, severity, self.id(), false)
        }
    }

    impl Rule for Regressor {
        fn id(&self) -> &'static str {
            "regressor"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::Safe(Provider {
                repair: if self.mark_text {
                    |line| {
                        let mut repair = replace_symbol(line, "BAD")?;
                        let text = line.message.first(58)?;
                        repair.edits.push(Edit {
                            start: text.value_start(),
                            end: text.value_end(),
                            text: "done".into(),
                        });
                        Some(repair)
                    }
                } else {
                    |line| replace_symbol(line, "BAD")
                },
                eligible: |_| true,
                claims: |_, _| true,
                dependencies: &[],
                finalizer: false,
            })
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            if symbol_field(line).is_some_and(|field| field.value(line.bytes) == b"GOOD-LONG") {
                symbolic(line, severity, self.id(), true)
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn invalid_provider_contracts_fail_with_the_specific_reason() {
        for (rule, reason) in [
            (
                &Unregistered as &dyn Rule,
                "Automatic cause has no provider: unregistered",
            ),
            (&ErasesMessage, "A body repair removed the message"),
            (
                &RemovesEnvelope,
                "A body repair made a finalizer ineligible",
            ),
        ] {
            let registry = registry(vec![rule, &Integrity]);
            assert_eq!(
                repaired("8=FIX.4.4|35=D|55=BAD|", &registry).unwrap_err(),
                invalid(reason)
            );
        }
    }

    #[test]
    fn later_providers_cannot_restore_old_states_or_reintroduce_findings() {
        let original = "8=FIX.4.4|35=D|55=BAD|58=pending|";
        let envelope = registry(vec![&Integrity]);
        let original = apply(original, &repaired(original, &envelope).unwrap().unwrap());
        for regressor in [
            &Regressor { mark_text: false },
            &Regressor { mark_text: true },
        ] {
            let registry = registry(vec![&Symbol, regressor, &Integrity]);
            assert_eq!(
                repaired(&original, &registry).unwrap_err(),
                invalid("Repair composition reintroduced findings: symbol")
            );
        }
    }

    impl Rule for Unlocked {
        fn id(&self) -> &'static str {
            "unlocked"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::Safe(Provider {
                eligible: |line| line.message.first(58).is_some(),
                claims: |_, _| true,
                dependencies: &["symbol"],
                finalizer: false,
                repair: |line| {
                    let field = line.message.first(58)?;
                    Some(Repair {
                        title: "Fix unlocked text".into(),
                        edits: vec![Edit {
                            start: field.value_start(),
                            end: field.value_end(),
                            text: "unlocked".into(),
                        }],
                    })
                },
            })
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            if !symbol_field(line).is_some_and(|field| field.value(line.bytes) == b"GOOD-LONG") {
                return Vec::new();
            }
            let Some(field) = line.message.first(58) else {
                return Vec::new();
            };
            if field.value(line.bytes) != b"pending" {
                return Vec::new();
            }
            vec![Diag {
                kind: DiagnosticKind {
                    code: "testRepair".into(),
                    fixability: Fixability::Automatic,
                },
                rule: self.id(),
                start: field.value_start(),
                end: field.value_end(),
                severity,
                message: "Synthetic unlocked repair".into(),
                document_key: None,
            }]
        }
    }

    #[test]
    fn one_provider_can_unlock_a_new_fixable_finding_without_reducing_total_count() {
        let line = "8=FIX.4.4|35=D|55=BAD|58=pending|";
        let integrity = registry(vec![&super::super::integrity::Integrity]);
        let valid = apply(line, &repaired(line, &integrity).unwrap().unwrap());
        let registry = registry(vec![
            &Unlocked,
            &Symbol,
            &super::super::integrity::Integrity,
        ]);
        let repair = repaired(&valid, &registry).unwrap().unwrap();
        let result = apply(&valid, &repair);
        assert!(result.contains("55=GOOD-LONG|58=unlocked|"));
        assert_eq!(repair.diags.len(), 1);
        assert_eq!(repair.diags[0].rule, "symbol");
        assert!(super::super::tests::codes(&result).is_empty());
        assert!(repaired(&result, &registry).unwrap().is_none());
    }

    struct Partial;

    impl Rule for Partial {
        fn id(&self) -> &'static str {
            "partial"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::Safe(Provider {
                eligible: |line| symbol_field(line).is_some(),
                claims: |line, diag| {
                    symbol_field(line).is_some_and(|field| field.value_start() == diag.start)
                },
                dependencies: &[],
                finalizer: false,
                repair: |line| replace_symbol(line, "GOOD"),
            })
        }
        fn check(&self, line: &LineCtx, severity: Severity) -> Vec<Diag> {
            line.message
                .fields
                .iter()
                .filter(|field| field.tag == 55 && field.value(line.bytes) == b"BAD")
                .map(|field| Diag {
                    kind: DiagnosticKind {
                        code: "testRepair".into(),
                        fixability: Fixability::Automatic,
                    },
                    rule: self.id(),
                    start: field.value_start(),
                    end: field.value_end(),
                    severity,
                    message: "Synthetic partially eligible repair".into(),
                    document_key: None,
                })
                .collect()
        }
    }

    #[test]
    fn partially_eligible_provider_claims_only_the_occurrence_it_repairs() {
        let line = "8=FIX.4.4|35=D|55=BAD|55=BAD|";
        let registry = registry(vec![&Partial, &super::super::integrity::Integrity]);
        let repair = repaired(line, &registry).unwrap().unwrap();
        let claims: Vec<_> = repair
            .diags
            .iter()
            .filter(|diag| diag.rule == "partial")
            .collect();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].start.index(), line.find("BAD").unwrap());
        let result = apply(line, &repair);
        assert!(result.contains("55=GOOD|55=BAD|"));
        assert!(repaired(&result, &registry).unwrap().is_none());
    }

    fn registry(rules: Vec<&'static dyn Rule>) -> Registry {
        Registry::of_rules(
            rules
                .into_iter()
                .map(|rule| (rule, rule.default_severity()))
                .collect(),
            None,
        )
    }

    fn repaired(line: &str, registry: &Registry) -> Result<Option<LineRepair>> {
        let ctx = Ctx::for_test("FIX.4.4");
        let names = DictionaryNames::new(ctx.registry(), ctx.choice());
        repair_line(
            line.as_bytes(),
            None,
            &ctx,
            registry,
            &names,
            &AtomicBool::new(false),
        )
    }

    fn apply(line: &str, repair: &LineRepair) -> String {
        let mut current = line.to_owned();
        for edit in repair.edits.iter().rev() {
            current.replace_range(edit.start.index()..edit.end.index(), &edit.text);
        }
        current
    }

    #[test]
    fn nonintegrity_provider_joins_automatically_and_preserves_original_coordinates() {
        let registry = registry(vec![&Symbol, &super::super::integrity::Integrity]);
        let line = "8=FIX.4.4|9=999|35=D|58=🦊|55=BAD|10=000|";
        let repair = repaired(line, &registry).unwrap().unwrap();
        assert_eq!(repair.edits.len(), 3);
        assert_eq!(repair.diags.len(), 3);
        let symbol = repair
            .diags
            .iter()
            .find(|diag| diag.rule == "symbol")
            .unwrap();
        assert_eq!(&line[symbol.start.index()..symbol.end.index()], "BAD");
        let result = apply(line, &repair);
        assert!(result.contains("35=D|58=🦊|55=GOOD-LONG|"));
        assert!(super::super::tests::codes(&result).is_empty());
        assert!(repaired(&result, &registry).unwrap().is_none());
        for pair in repair.edits.windows(2) {
            assert!(pair[0].end < pair[1].start);
        }
    }

    #[test]
    fn newly_required_envelope_repairs_are_not_claimed_as_original_diagnostics() {
        let integrity = registry(vec![&super::super::integrity::Integrity]);
        let line = "8=FIX.4.4|35=D|55=BAD|";
        let valid = apply(line, &repaired(line, &integrity).unwrap().unwrap());
        let registry = registry(vec![&super::super::integrity::Integrity, &Symbol]);
        let repair = repaired(&valid, &registry).unwrap().unwrap();
        assert_eq!(repair.diags.len(), 1);
        assert_eq!(repair.diags[0].rule, "symbol");
        assert!(super::super::tests::codes(&apply(&valid, &repair)).is_empty());
    }

    #[test]
    fn disabled_dependencies_do_not_exclude_semantic_repairs() {
        let only_symbol = registry(vec![&Symbol]);
        let line = "8=FIX.4.4|35=D|55=BAD|";
        let repair = repaired(line, &only_symbol).unwrap().unwrap();
        assert_eq!(apply(line, &repair), "8=FIX.4.4|35=D|55=GOOD-LONG|");
        let missing = registry(vec![
            &MissingDependency,
            &super::super::integrity::Integrity,
        ]);
        let repair = repaired(line, &missing).unwrap().unwrap();
        assert!(apply(line, &repair).contains("55=GOOD|"));
        assert!(repair
            .diags
            .iter()
            .any(|diag| diag.rule == "missingDependency"));
        assert!(repair.diags.iter().any(|diag| diag.rule == "integrity"));
    }

    #[test]
    fn enabled_ineligible_integrity_excludes_semantic_repairs() {
        let both = registry(vec![&Symbol, &super::super::integrity::Integrity]);
        for line in ["35=D|55=BAD|", "10=167|8=FIX.4.4|35=D|55=BAD|"] {
            assert!(repaired(line, &both).unwrap().is_none());
        }
    }

    #[test]
    fn provider_order_does_not_change_composition() {
        let line = "8=FIX.4.4|35=D|55=BAD|";
        let first = repaired(
            line,
            &registry(vec![&Symbol, &super::super::integrity::Integrity]),
        )
        .unwrap()
        .unwrap();
        let second = repaired(
            line,
            &registry(vec![&super::super::integrity::Integrity, &Symbol]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.edits, second.edits);
        assert_eq!(first.title, second.title);
    }

    #[test]
    fn conflicts_and_dependency_cycles_fail_without_partial_edits() {
        for (rules, reason) in [
            (
                vec![
                    &Symbol as &dyn Rule,
                    &Conflict,
                    &super::super::integrity::Integrity,
                ],
                "Repair providers proposed overlapping edits",
            ),
            (
                vec![
                    &CyclicDependency as &dyn Rule,
                    &super::super::integrity::Integrity,
                ],
                "Cyclic repair dependency: cyclicDependency",
            ),
        ] {
            assert_eq!(
                repaired("8=FIX.4.4|35=D|55=BAD|", &registry(rules)).unwrap_err(),
                invalid(reason)
            );
        }
    }

    #[test]
    fn nonprogressing_provider_fails_without_an_arbitrary_round_limit() {
        let registry = registry(vec![&NoProgress, &super::super::integrity::Integrity]);
        assert_eq!(
            repaired("8=FIX.4.4|35=D|55=BAD|", &registry).unwrap_err(),
            invalid("Repair provider did not reduce its findings: noProgress")
        );
    }

    #[test]
    fn integrity_preserves_existing_edits_titles_and_claims() {
        let registry = Registry::from_config(&Diagnostics::default());
        for line in [
            "8=FIX.4.4|9=999|35=D|10=000|",
            "8=FIX.4.4|35=D|",
            "8=FIX.4.4|9=0005|35=0|10=000|",
            "8=FIX.9.9|35=0|",
        ] {
            let expected = super::super::tests::repair_of(line).unwrap();
            let actual = repaired(line, &registry).unwrap().unwrap();
            assert_eq!(actual.title, expected.title, "{line}");
            assert_eq!(actual.edits, expected.edits, "{line}");
            assert_eq!(
                actual.diags,
                super::super::tests::diags(line)
                    .into_iter()
                    .filter(|diag| diag.rule == "integrity")
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn disabled_rules_and_global_disable_produce_no_repairs() {
        let mut config = Diagnostics::default();
        config
            .rules
            .insert("integrity".into(), RuleSetting::Enabled(false));
        assert!(repaired("8=FIX.4.4|35=D|", &Registry::from_config(&config))
            .unwrap()
            .is_none());
        config.rules.clear();
        config.enabled = false;
        assert!(repaired("8=FIX.4.4|35=D|", &Registry::from_config(&config))
            .unwrap()
            .is_none());
    }

    #[test]
    fn lines_without_automatic_findings_need_no_utf8_repair_buffer() {
        let ctx = Ctx::for_test("FIX.4.4");
        let names = DictionaryNames::new(ctx.registry(), ctx.choice());
        let registry = registry(vec![&Symbol, &super::super::unknown::UnknownTag]);
        for (bytes, diagnostic_only) in [
            (b"".as_slice(), false),
            (b"ordinary \xff log".as_slice(), false),
            (b"8=FIX.4.4|35=D|55=GOOD|58=\xff|".as_slice(), false),
            (b"8=FIX.4.4|35=D|55=GOOD|999999=\xff|".as_slice(), true),
        ] {
            let analysis = analyze(bytes, &ctx);
            let diags = findings(bytes, &analysis, &registry, &names).unwrap();
            assert_eq!(!diags.is_empty(), diagnostic_only);
            assert!(diags
                .iter()
                .all(|diag| diag.kind.fixability != Fixability::Automatic));
            assert!(repair_line(
                bytes,
                None,
                &ctx,
                &registry,
                &names,
                &AtomicBool::new(false)
            )
            .unwrap()
            .is_none());
            assert!(matches!(
                repair_line(bytes, None, &ctx, &registry, &names, &AtomicBool::new(true)),
                Err(RepairError::Cancelled)
            ));
        }
    }

    #[test]
    fn automatic_repairs_reject_non_utf8_lines() {
        let ctx = Ctx::for_test("FIX.4.4");
        let names = DictionaryNames::new(ctx.registry(), ctx.choice());
        let registry = registry(vec![&Symbol, &Integrity]);
        for payload in [b"\xe9".as_slice(), b"\xff\xf0\x9f\x99\x82\x80"] {
            let mut original = b"8=FIX.4.4|9=999|35=D|55=BAD|58=".to_vec();
            original.extend_from_slice(payload);
            original.extend_from_slice(b"|10=000|");
            assert_eq!(
                repair_line(
                    &original,
                    None,
                    &ctx,
                    &registry,
                    &names,
                    &AtomicBool::new(false),
                )
                .unwrap_err(),
                invalid("Cannot repair a non-UTF-8 line")
            );
        }
    }

    #[test]
    fn automatic_repairs_preserve_unicode_and_count_the_resulting_octets() {
        let ctx = Ctx::for_test("FIX.4.4");
        let names = DictionaryNames::new(ctx.registry(), ctx.choice());
        let registry = registry(vec![&Symbol, &Integrity]);
        for payload in ["é".as_bytes(), "🙂".as_bytes()] {
            let mut original = b"8=FIX.4.4|9=999|35=D|55=BAD|58=".to_vec();
            original.extend_from_slice(payload);
            original.extend_from_slice(b"|10=000|");
            let repair = repair_line(
                &original,
                None,
                &ctx,
                &registry,
                &names,
                &AtomicBool::new(false),
            )
            .unwrap()
            .unwrap();
            let mut actual = original.clone();
            for edit in repair.edits.iter().rev() {
                actual.splice(edit.start.index()..edit.end.index(), edit.text.bytes());
            }
            let mut body = b"35=D|55=GOOD-LONG|58=".to_vec();
            body.extend_from_slice(payload);
            body.push(b'|');
            let mut expected = format!("8=FIX.4.4|9={}|", body.len()).into_bytes();
            expected.extend_from_slice(&body);
            let checksum: u32 = expected
                .iter()
                .map(|&byte| u32::from(if byte == b'|' { 1 } else { byte }))
                .sum();
            expected.extend_from_slice(format!("10={:03}|", checksum % 256).as_bytes());
            assert_eq!(actual, expected);
            assert!(repair_line(
                &actual,
                None,
                &ctx,
                &registry,
                &names,
                &AtomicBool::new(false)
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn repairs_keep_unicode_edit_boundaries_and_checked_inserted_slices() {
        let current = "x🙂y";
        for boundary in 0..=current.len() {
            let mut edits = vec![Edit {
                start: byte(boundary as u32),
                end: byte(boundary as u32),
                text: "x".into(),
            }];
            assert_eq!(
                normalize_edits(current, &mut edits).is_ok(),
                !matches!(boundary, 2..=4)
            );
        }
        let mut pieces = Pieces(vec![Piece::Original(0..current.len())]);
        pieces
            .apply(
                &[Edit {
                    start: byte(5),
                    end: byte(6),
                    text: "é".into(),
                }],
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(pieces.text(current), "x🙂é");
        assert_eq!(
            pieces
                .apply(
                    &[Edit {
                        start: byte(6),
                        end: byte(6),
                        text: "x".into()
                    }],
                    &AtomicBool::new(false)
                )
                .unwrap_err(),
            invalid("A repair splits inserted Unicode text")
        );
    }

    struct SymbolFinalizer {
        requires_bad: bool,
    }

    impl Rule for SymbolFinalizer {
        fn id(&self) -> &'static str {
            "symbolFinalizer"
        }
        fn precondition(&self) -> Precondition {
            Precondition::Fields
        }
        fn default_severity(&self) -> Severity {
            Severity::Warning
        }
        fn repair_policy(&self) -> RepairPolicy {
            RepairPolicy::Safe(Provider {
                repair: |_| None,
                eligible: if self.requires_bad {
                    |line| symbol_field(line).is_some_and(|field| field.value(line.bytes) == b"BAD")
                } else {
                    |line| {
                        symbol_field(line)
                            .is_some_and(|field| field.value(line.bytes) == b"GOOD-LONG")
                    }
                },
                claims: |_, _| false,
                dependencies: &[],
                finalizer: true,
            })
        }
        fn check(&self, _: &LineCtx, _: Severity) -> Vec<Diag> {
            Vec::new()
        }
    }

    #[test]
    fn every_enabled_finalizer_gates_body_repairs_before_and_after() {
        let original = "8=FIX.4.4|35=D|55=BAD|";
        let initially_ineligible = registry(vec![
            &Symbol,
            &SymbolFinalizer {
                requires_bad: false,
            },
        ]);
        assert!(repaired(original, &initially_ineligible).unwrap().is_none());
        let invalidated = registry(vec![&Symbol, &SymbolFinalizer { requires_bad: true }]);
        assert_eq!(
            repaired(original, &invalidated).unwrap_err(),
            invalid("A body repair made a finalizer ineligible")
        );
        let disabled = registry(vec![&Symbol]);
        let repair = repaired(original, &disabled).unwrap().unwrap();
        assert_eq!(apply(original, &repair), "8=FIX.4.4|35=D|55=GOOD-LONG|");
    }

    #[test]
    fn cancelled_work_returns_no_partial_result() {
        let ctx = Ctx::for_test("FIX.4.4");
        let names = DictionaryNames::new(ctx.registry(), ctx.choice());
        let registry = Registry::from_config(&Diagnostics::default());
        assert!(matches!(
            repair_line(
                b"8=FIX.4.4|35=D|",
                None,
                &ctx,
                &registry,
                &names,
                &AtomicBool::new(true)
            ),
            Err(RepairError::Cancelled)
        ));
    }

    #[test]
    fn composition_retains_separate_changes_across_insertions_and_deletions() {
        let original = "α:one:middle:two:ω";
        let mut pieces = Pieces(vec![Piece::Original(0..original.len())]);
        let first = vec![Edit {
            start: byte(3),
            end: byte(6),
            text: "long-value".into(),
        }];
        pieces.apply(&first, &AtomicBool::new(false)).unwrap();
        let current = pieces.text(original);
        let start = current.find("two").unwrap();
        pieces
            .apply(
                &[Edit {
                    start: ByteOffset::from_len(start).unwrap(),
                    end: ByteOffset::from_len(start + 3).unwrap(),
                    text: String::new(),
                }],
                &AtomicBool::new(false),
            )
            .unwrap();
        let edits = pieces.edits(original).unwrap();
        assert_eq!(edits.len(), 2);
        let composed = LineRepair {
            title: String::new(),
            edits,
            diags: Vec::new(),
        };
        assert_eq!(apply(original, &composed), "α:long-value:middle::ω");
    }
}
