//! BodyLength(9) and CheckSum(10) against what the line carries.
//!
//! The numbers themselves come from `codec::integrity`, which computed them
//! while walking the field spans; this decides what to say about them.
//!
//! **One rule, two codes.** The two are one property of the line — the trailer
//! arithmetic — computed in one pass and repairable only together, since
//! correcting the length changes the bytes the sum is taken over. Two switches
//! over one property produce a state nothing can answer: half of it silenced,
//! and a repair that must still correct both.
//!
//! The repairs are Stage 4a-2: a diagnostic carries no edits, because an edit
//! computed here would be stale by the time the user invokes it.

use super::{Diag, Edit, LineCtx, Precondition, Repair, Rule};
use crate::codec::tokenize;
use crate::config::Severity;
use crate::tag;

/// Below this the rule declines — too broken to reason about. Anchoring a
/// repair needs a field to put it next to.
const MIN_FIELDS: usize = 2;

pub struct Integrity;

impl Rule for Integrity {
    fn id(&self) -> &'static str {
        "integrity"
    }

    fn precondition(&self) -> Precondition {
        Precondition::FixMessage
    }

    fn default_severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, ctx: &LineCtx, severity: Severity) -> Vec<Diag> {
        let facts = ctx.integrity();
        let mut found = Claim {
            code: "bodyLength",
            name: "BodyLength",
            tag: 9,
            at: facts.body_length_field,
            // A missing BodyLength is anchored on BeginString, where it belongs.
            anchor: facts.begin_string.unwrap_or(0),
            expected: &facts.body_length.to_string(),
            compare: Compare::Value,
            severity,
        }
        .report(ctx);
        found.extend(
            Claim {
                code: "checkSum",
                name: "CheckSum",
                tag: 10,
                at: facts.check_sum_field,
                // A missing CheckSum is anchored on the last field: that is
                // where it would be appended.
                anchor: ctx.message.fields.len().saturating_sub(1) as u32,
                expected: &format!("{:03}", facts.check_sum),
                compare: Compare::Text,
                severity,
            }
            .report(ctx),
        );
        found
    }
}

/// The edits that make the line's trailer arithmetic true, or `None` when
/// there is nothing wrong or nothing to anchor a correction to.
///
/// A free function rather than a method on `Rule`: a rule says what is wrong,
/// and two of the four codes have no repair at all.
///
/// The edits are in field order — which is not the order they may be applied
/// in, since correcting BodyLength moves every span after it.
pub fn repair(ctx: &LineCtx) -> Option<Repair> {
    let fields = &ctx.message.fields;
    // The rule's own `Precondition::FixMessage`, restated because a repair is
    // reached by a free function that no precondition guards. Arithmetic over a
    // line that is not a message is arithmetic about nothing — and offering to
    // rewrite a line nothing was reported on is worse than not offering at all.
    if fields.len() < MIN_FIELDS || ctx.message.first(tag::BEGIN_STRING).is_none() {
        return None;
    }
    let facts = ctx.integrity();
    // Stage 3 dropped `MessageAnalysis::separator` as write-only, and it should
    // stay dropped: a byte on every cached analysis for something only a repair
    // reads. One line is cheap to re-tokenise.
    let separator = tokenize(ctx.bytes).separator.unwrap_or(b'|') as char;

    let mut names = Vec::new();
    let mut edits = Vec::new();

    let expected = facts.body_length.to_string();
    match written(ctx, facts.body_length_field, &expected, Compare::Value) {
        Written::Correct => {}
        Written::Wrong { at, .. } => {
            names.push("BodyLength");
            edits.push(replacing(ctx, at, expected));
        }
        Written::Missing => {
            // Not where the diagnostic points: that is BeginString(8), and the field
            // belongs after it — where the arithmetic already assumes it is.
            let body_first = facts.begin_string.map_or(0, |at| at + 1) as usize;
            // Nothing to insert before: too broken to reason about.
            let anchor = fields.get(body_first)?;
            names.push("BodyLength");
            edits.push(Edit {
                start: anchor.tag_start,
                end: anchor.tag_start,
                text: format!("9={expected}{separator}"),
            });
        }
    }

    let expected = format!("{:03}", facts.check_sum);
    match written(ctx, facts.check_sum_field, &expected, Compare::Text) {
        Written::Correct => {}
        Written::Wrong { at, .. } => {
            names.push("CheckSum");
            edits.push(replacing(ctx, at, expected));
        }
        Written::Missing => {
            names.push("CheckSum");
            let after_last = fields.last().expect("MIN_FIELDS").value_end();
            // The trailer keeps the line's own shape: appended after a trailing
            // separator when there is one, and bringing its own when there is
            // not. Taken from the bytes rather than from the line length, which
            // a log suffix would make wrong.
            edits.push(match ctx.bytes.get(after_last.index()) {
                Some(&byte) if byte == separator as u8 => Edit {
                    start: after_last + 1,
                    end: after_last + 1,
                    text: format!("10={expected}{separator}"),
                },
                _ => Edit {
                    start: after_last,
                    end: after_last,
                    text: format!("{separator}10={expected}"),
                },
            });
        }
    }

    (!edits.is_empty()).then(|| Repair {
        title: format!("Fix {}", names.join(" and ")),
        edits,
    })
}

/// How a written value is held against the computed one.
///
/// Not a stylistic choice: BodyLength has no fixed width, so `9=000156` and
/// `9=156` are the same number and a venue that zero-pads is not writing a
/// broken line. CheckSum is three characters by specification, so anything else
/// is wrong whatever it evaluates to.
#[derive(Clone, Copy)]
enum Compare {
    /// As a number, ignoring leading zeros.
    Value,
    /// Byte for byte, the width being part of what is required.
    Text,
}

/// What the line writes for one of the two quantities, against what it should.
///
/// One predicate for the diagnostic and the repair. They must agree: a problem
/// reported with no way to fix it, or fixed without having been reported, is
/// worse than either rule on its own.
enum Written<'a> {
    /// Nothing to report and nothing to fix.
    Correct,
    /// `at` is the field, `text` what it writes — empty when the value is not
    /// UTF-8, which is a disagreement rather than a reason to give up.
    Wrong { at: u32, text: &'a str },
    /// The tag is absent. Where it is reported and where it would be inserted
    /// are different fields; the callers decide which they need.
    Missing,
}

fn written<'a>(ctx: &LineCtx<'a>, at: Option<u32>, expected: &str, how: Compare) -> Written<'a> {
    let Some(at) = at else {
        return Written::Missing;
    };
    let text = ctx.message.fields[at as usize]
        .text(ctx.bytes)
        .unwrap_or_default();
    let agrees = match how {
        // A value that is not a number disagrees, rather than being a reason
        // to give up.
        Compare::Value => text
            .parse::<u32>()
            .is_ok_and(|value| value.to_string() == expected),
        Compare::Text => text == expected,
    };
    if agrees {
        Written::Correct
    } else {
        Written::Wrong { at, text }
    }
}

fn replacing(ctx: &LineCtx, at: u32, text: String) -> Edit {
    let field = &ctx.message.fields[at as usize];
    Edit {
        start: field.value_start(),
        end: field.value_end(),
        text,
    }
}

/// What the rule claims about one of the two quantities. Named rather than
/// passed positionally because `at` and `anchor` are both field indices:
/// swapping them compiles and produces a diagnostic on the wrong field.
struct Claim<'a> {
    code: &'static str,
    name: &'a str,
    tag: u32,
    /// Where the tag is written, when it is.
    at: Option<u32>,
    /// Field a missing tag is reported over.
    anchor: u32,
    expected: &'a str,
    compare: Compare,
    severity: Severity,
}

impl Claim<'_> {
    /// A missing tag is reported over `anchor`, a wrong value over its own span.
    fn report(self, ctx: &LineCtx) -> Vec<Diag> {
        let fields = &ctx.message.fields;
        if fields.len() < MIN_FIELDS {
            return Vec::new();
        }
        let (code, name, tag, severity) = (self.code, self.name, self.tag, self.severity);
        let rule = Integrity.id();
        match written(ctx, self.at, self.expected, self.compare) {
            Written::Correct => Vec::new(),
            Written::Missing => {
                let anchor = &fields[self.anchor as usize];
                vec![Diag {
                    code,
                    rule,
                    start: anchor.tag_start,
                    end: anchor.value_end(),
                    message: format!("Missing {name} (tag {tag})"),
                    severity,
                    document_key: None,
                }]
            }
            Written::Wrong { at, text } => {
                let field = &fields[at as usize];
                vec![Diag {
                    code,
                    rule,
                    start: field.value_start(),
                    end: field.value_end(),
                    message: format!("{name} is {text}, expected {}", self.expected),
                    severity,
                    document_key: None,
                }]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{codes, diags, repair_of, repaired};
    use super::Edit;
    use crate::codec::byte;

    const VALID_HEARTBEAT: &str =
        "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
    const BAD_BOTH: &str = "8=FIX.4.4|9=7|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=001|";

    #[test]
    fn a_sound_line_reports_nothing() {
        assert!(codes(VALID_HEARTBEAT).is_empty());
    }

    #[test]
    fn both_wrong_values_are_reported_over_their_own_spans() {
        let found = diags(BAD_BOTH);
        let length = found.iter().find(|d| d.code == "bodyLength").unwrap();
        assert_eq!((length.start, length.end), (byte(12), byte(13)));
        assert_eq!(length.message, "BodyLength is 7, expected 139");
        let sum = found.iter().find(|d| d.code == "checkSum").unwrap();
        assert_eq!((sum.start, sum.end), (byte(156), byte(159)));
        assert_eq!(sum.message, "CheckSum is 001, expected 213");
    }

    #[test]
    fn a_wrong_body_length_alone_does_not_disturb_the_checksum() {
        // validation/bad-bodylength.fix carries 9=61 and a correct 10=171.
        let line =
            "8=FIX.4.4|9=61|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        assert_eq!(codes(line), vec!["bodyLength"]);
    }

    #[test]
    fn a_missing_body_length_is_anchored_on_begin_string() {
        let line = "8=FIX.4.4|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        let found = diags(line);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "bodyLength");
        assert_eq!((found[0].start, found[0].end), (byte(0), byte(9)));
        assert_eq!(found[0].message, "Missing BodyLength (tag 9)");
    }

    #[test]
    fn a_missing_checksum_is_anchored_on_the_last_field() {
        let line =
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30|";
        let found = diags(line);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, "checkSum");
        assert_eq!(found[0].message, "Missing CheckSum (tag 10)");
        let last = line.rfind("108=30").unwrap() as u32;
        assert_eq!((found[0].start, found[0].end), (byte(last), byte(last + 6)));
    }

    #[test]
    fn a_line_too_short_to_reason_about_is_left_alone() {
        for line in ["8=FIX.4.4|", "8=FIX.4.4"] {
            assert_eq!(codes(line), ["missingMsgType"]);
            assert!(repair_of(line).is_none());
        }
    }

    #[test]
    fn soh_and_pipe_agree() {
        assert_eq!(codes(&BAD_BOTH.replace('|', "\u{1}")), codes(BAD_BOTH));
    }

    #[test]
    fn a_skipped_log_prefix_does_not_enter_the_arithmetic() {
        let prefixed = format!("2026-06-30 07:55:12 INFO {VALID_HEARTBEAT}");
        assert!(codes(&prefixed).is_empty());
    }

    /// The edits are written out rather than derived. Deriving an expectation
    /// from the same code under test asserts nothing; these say what the bytes
    /// must become.
    #[test]
    fn a_sound_line_has_nothing_to_repair() {
        assert_eq!(repair_of(VALID_HEARTBEAT), None);
    }

    #[test]
    fn a_wrong_value_is_replaced_over_its_own_span() {
        // validation__soh, line 1: (158, 161, "213"), title "Fix CheckSum".
        let line = "8=FIX.4.4|9=139|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=216|";
        let repair = repair_of(line).expect("the sum disagrees");
        assert_eq!(repair.title, "Fix CheckSum");
        assert_eq!(
            repair.edits,
            vec![Edit {
                start: byte(158),
                end: byte(161),
                text: "213".into()
            }]
        );
    }

    #[test]
    fn both_wrong_values_are_one_repair() {
        // validation__bad-both: two edits, title "Fix BodyLength and CheckSum".
        let repair = repair_of(BAD_BOTH).expect("both disagree");
        assert_eq!(repair.title, "Fix BodyLength and CheckSum");
        assert_eq!(
            repair.edits,
            vec![
                Edit {
                    start: byte(12),
                    end: byte(13),
                    text: "139".into()
                },
                Edit {
                    start: byte(156),
                    end: byte(159),
                    text: "213".into()
                },
            ],
            "in field order, which is not the order they must be applied in"
        );
    }

    /// The edit anchor is not the diagnostic anchor: a missing BodyLength is
    /// *reported* over BeginString(8) and *inserted* before the field after it.
    #[test]
    fn a_missing_body_length_is_inserted_after_begin_string() {
        // validation__missing-bodylength: (10, 10, "9=57|"), "Fix BodyLength".
        let line = "8=FIX.4.4|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        let repair = repair_of(line).expect("BodyLength(9) is absent");
        assert_eq!(repair.title, "Fix BodyLength");
        assert_eq!(
            repair.edits,
            vec![Edit {
                start: byte(10),
                end: byte(10),
                text: "9=57|".into()
            }]
        );
    }

    #[test]
    fn a_missing_checksum_goes_after_a_trailing_separator() {
        // validation__missing-checksum: (84, 84, "10=214|"), "Fix CheckSum".
        let line =
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30|";
        let repair = repair_of(line).expect("CheckSum(10) is absent");
        assert_eq!(repair.title, "Fix CheckSum");
        assert_eq!(
            repair.edits,
            vec![Edit {
                start: byte(84),
                end: byte(84),
                text: "10=214|".into()
            }]
        );
    }

    #[test]
    fn a_missing_checksum_brings_its_own_separator_when_there_is_none() {
        // validation__missing-checksum-no-trailing: (83, 83, "|10=214").
        let line =
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30";
        let repair = repair_of(line).expect("CheckSum(10) is absent");
        assert_eq!(
            repair.edits,
            vec![Edit {
                start: byte(83),
                end: byte(83),
                text: "|10=214".into()
            }]
        );
    }

    /// The separator is the line's own, not `|`.
    #[test]
    fn an_inserted_field_uses_the_separator_the_line_uses() {
        let line = "8=FIX.4.4\u{1}35=0\u{1}34=1\u{1}49=CLIENT1\u{1}56=BROKER1\u{1}\
52=20251026-09:02:00.000\u{1}10=171\u{1}";
        let repair = repair_of(line).expect("BodyLength(9) is absent");
        assert_eq!(repair.edits[0].text, "9=57\u{1}");
    }

    /// Tested rather than left to be discovered: `sniff_separator` picks one
    /// byte for the whole line,
    /// most-specific first, so a line carrying both is split one way and an
    /// inserted field takes SOH.
    #[test]
    fn a_line_mixing_separators_inserts_the_one_the_tokenizer_chose() {
        let line = "8=FIX.4.4|58=a\u{1}35=0\u{1}";
        let repair = repair_of(line).expect("neither 9 nor 10 is present");
        assert!(
            repair.edits.iter().all(|edit| edit.text.contains('\u{1}')),
            "{:?}",
            repair.edits
        );
    }

    #[test]
    fn a_zero_padded_body_length_is_the_same_number() {
        let line =
            "8=FIX.4.4|9=057|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        assert!(codes(line).is_empty(), "{:?}", codes(line));
        assert_eq!(repair_of(line), None);
        // Still a number, and still the wrong one.
        let wrong =
            "8=FIX.4.4|9=056|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        assert_eq!(codes(wrong), vec!["bodyLength"]);
        // And a value that is no number at all disagrees rather than crashing.
        let junk =
            "8=FIX.4.4|9=x7|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
        assert_eq!(codes(junk), vec!["bodyLength"]);
    }

    /// CheckSum is three characters by specification, so its width is part of
    /// what is required and `10=1` is wrong even though it is the same number.
    /// This is where the two quantities are deliberately not treated alike.
    #[test]
    fn a_checksum_of_the_wrong_width_is_still_wrong() {
        assert!(codes(VALID_HEARTBEAT).is_empty());
        // CheckSum(10) sits outside the body, so widening it moves no other number:
        // 0171 is 171, and it is still not what a CheckSum may look like.
        let padded = VALID_HEARTBEAT.replace("10=171", "10=0171");
        assert_eq!(codes(&padded), vec!["checkSum"]);
    }

    /// The rule declines when BodyLength is absent and there is no field to
    /// insert it before. A fragment never reaches this — it fails the
    /// FixMessage precondition first — so it takes a message whose BeginString(8) is
    /// last.
    #[test]
    fn a_line_with_nothing_to_anchor_an_insertion_to_is_declined() {
        assert_eq!(repair_of("35=0|8=FIX.4.4|"), None);
    }

    /// The report and the repair must agree about which lines have an
    /// integrity problem: a line this rule leaves alone must not be offered a
    /// fix. Task 5's handler rests on it — no integrity diagnostic, no action.
    ///
    /// One field is such a line whatever that field is, including a lone BodyLength(9).
    /// That case is what `MIN_FIELDS` is for and what the missing-anchor guard
    /// does not catch, BodyLength(9) being present being the branch that needs no
    /// anchor. A line with no BeginString is such a line at any width: the rule
    /// never runs on it.
    /// (`beginString` still reports on some of these; it has no repair.)
    #[test]
    fn a_line_this_rule_leaves_alone_is_not_repaired() {
        for line in [
            "8=FIX.4.4|",
            "8=FIX.4.4",
            "9=5|",
            "10=001|",
            "35=0|34=1|49=CLIENT1|56=BROKER1|10=001|",
        ] {
            let reported: Vec<&str> = diags(line)
                .iter()
                .filter(|diag| diag.rule == "integrity")
                .map(|diag| diag.code)
                .collect();
            assert_eq!(reported, Vec::<&str>::new(), "{line}");
            assert_eq!(repair_of(line), None, "{line}");
        }
    }

    /// The strongest statement about the arithmetic: what the repair produces
    /// has nothing left to repair, and nothing left to report.
    #[test]
    fn a_repaired_line_is_sound() {
        for line in [
            BAD_BOTH,
            "8=FIX.4.4|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|",
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30|",
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30",
            "8=FIX.4.4|9=057|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|",
        ] {
            let fixed = repaired(line);
            assert_eq!(codes(&fixed), Vec::<&str>::new(), "{fixed}");
            assert_eq!(repair_of(&fixed), None, "{fixed}");
        }
    }
}
