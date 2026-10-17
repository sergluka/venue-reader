//! Properties that hold over every line of every sample log in
//! `tests/fixtures/`.
//!
//! The four below need no reference implementation to be true, which is why
//! they are stated over sample logs rather than over hand-written lines.
//! Breadth is the point — 33 lines across four FIX versions, well-formed and
//! malformed, is more shapes than any single test file assembles by hand.

mod harness;

use fix_lsp::codec::LineAnalysis;
use fix_lsp::config::Diagnostics;
use fix_lsp::ctx::analyze;
use fix_lsp::rules::{integrity, LineCtx, Registry, Scope};

/// The one rule a repair answers to.
const INTEGRITY: Scope = Scope {
    labels: fix_lsp::ctx::Labels {
        transport: None,
        application: None,
    },
    count_from: None,
    only: Some("integrity"),
};

/// Totals, so a loop that quietly walks nothing cannot pass. They are
/// tokenizer and rule facts — no dictionary decides them, so a dictionary
/// update does not move them.
const MESSAGES: usize = 33;
const FIELDS: usize = 450;
const REPAIRABLE: usize = 18;
/// Three fragments lack BeginString(8); a lone BeginString(8) cannot be repaired.
const UNREPAIRABLE: usize = 4;

#[test]
fn every_field_span_is_ordered_and_inside_its_line() {
    let ctx = harness::test_ctx();
    let (mut messages, mut fields) = (0, 0);

    for (at, bytes) in harness::fixture_lines() {
        let LineAnalysis::Message(message) = analyze(&bytes, &ctx) else {
            continue;
        };
        messages += 1;
        let mut end_of_previous = 0;
        for field in &message.fields {
            let (tag_start, tag_end) = (field.tag_start.index(), field.tag_end().index());
            let (value_start, value_end) = (field.value_start().index(), field.value_end().index());
            assert!(
                end_of_previous <= tag_start
                    && tag_start < tag_end
                    && tag_end <= value_start
                    && value_start <= value_end
                    && value_end <= bytes.len(),
                "{at}: tag {} spans {tag_start}..{tag_end}, value {value_start}..{value_end}, \
                 after {end_of_previous} in {} bytes",
                field.tag,
                bytes.len()
            );
            end_of_previous = value_end;
            fields += 1;
        }
    }

    assert_eq!((messages, fields), (MESSAGES, FIELDS));
}

#[test]
fn repairs_require_findings_but_presence_findings_can_be_unrepairable() {
    let ctx = harness::test_ctx();
    let rules = Registry::from_config(&Diagnostics::default());
    let (mut repairable, mut unrepairable) = (0, 0);

    for (at, bytes) in harness::fixture_lines() {
        let analysis = analyze(&bytes, &ctx);
        let reports = rules.check(&bytes, &analysis, &INTEGRITY).diags.len();
        let repair = analysis
            .message()
            .and_then(|message| integrity::repair(&LineCtx::new(&bytes, message)));
        if repair.is_some() {
            assert!(reports > 0, "{at}: repair without a finding");
            repairable += 1;
        } else if reports > 0 {
            let message = analysis.message().expect("findings require fields");
            assert!(
                message.fields.len() < 2 || message.first(8).is_none(),
                "{at}: fixture has {reports} findings but no repair"
            );
            unrepairable += 1;
        }
    }

    assert_eq!((repairable, unrepairable), (REPAIRABLE, UNREPAIRABLE));
}

#[test]
fn a_repaired_line_has_nothing_left_to_repair() {
    let ctx = harness::test_ctx();
    let rules = Registry::from_config(&Diagnostics::default());
    let mut applied = 0;

    for (at, bytes) in harness::fixture_lines() {
        let analysis = analyze(&bytes, &ctx);
        let Some(message) = analysis.message() else {
            continue;
        };
        let Some(repair) = integrity::repair(&LineCtx::new(&bytes, message)) else {
            continue;
        };

        let repaired = harness::apply_edits(&bytes, &repair.edits);
        assert_ne!(repaired, bytes, "{at}: the repair changed nothing");

        let analysis = analyze(&repaired, &ctx);
        let left: Vec<&str> = rules
            .check(&repaired, &analysis, &INTEGRITY)
            .diags
            .into_iter()
            .map(|diag| diag.code)
            .collect();
        assert!(
            left.is_empty(),
            "{at}: {left:?} left in {}",
            String::from_utf8_lossy(&repaired)
        );
        let message = analysis.message().expect("still a message");
        assert_eq!(
            integrity::repair(&LineCtx::new(&repaired, message)),
            None,
            "{at}"
        );
        applied += 1;
    }

    assert_eq!(applied, REPAIRABLE);
}

/// A line nothing reports on must agree with its own arithmetic. Stated the
/// other way round from the rule — the rule decides whether to report, this
/// checks that silence was earned.
#[test]
fn a_line_nothing_reports_on_agrees_with_its_own_trailer() {
    let ctx = harness::test_ctx();
    let rules = Registry::from_config(&Diagnostics::default());
    let mut clean = 0;

    for (at, bytes) in harness::fixture_lines() {
        let analysis = analyze(&bytes, &ctx);
        let Some(message) = analysis.message() else {
            continue;
        };
        if !rules.check(&bytes, &analysis, &INTEGRITY).diags.is_empty() {
            continue;
        }
        let written = |tag: u32| {
            message
                .first(tag)
                .map(|field| String::from_utf8_lossy(field.value(&bytes)).into_owned())
        };
        assert!(message.first(8).is_some(), "{at}: missing BeginString(8)");
        let length = written(9).expect("silent line has BodyLength(9)");
        let sum = written(10).expect("silent line has CheckSum(10)");
        let facts = message.integrity(&bytes);
        assert_eq!(length, facts.body_length.to_string(), "{at}");
        assert_eq!(sum, format!("{:03}", facts.check_sum), "{at}");
        clean += 1;
    }

    assert_eq!(clean, MESSAGES - REPAIRABLE - UNREPAIRABLE);
}
