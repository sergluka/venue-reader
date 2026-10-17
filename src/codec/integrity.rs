//! BodyLength and CheckSum computed from the field spans.
//!
//! Two deliberate properties: the walk is over fields, so anything the
//! tokenizer left outside one (a skipped prefix, trailing junk) is ignored; and
//! every separator counts as exactly one byte whichever byte it is, because the
//! canonical wire form uses SOH. That is what makes a `|`-separated log and its
//! SOH twin produce the same numbers.
//!
//! The rule gates arithmetic separately from envelope presence checks.

use super::Field;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Integrity {
    /// Index into `LineAnalysis.fields` of the first BeginString(8),
    /// BodyLength(9) and CheckSum(10).
    pub begin_string: Option<u32>,
    pub body_length_field: Option<u32>,
    pub check_sum_field: Option<u32>,
    /// What BodyLength should be.
    pub body_length: u32,
    /// What CheckSum should be; render as `{:03}`.
    pub check_sum: u8,
}

const SEPARATOR_BYTES: u32 = 1;

pub(crate) fn compute(bytes: &[u8], fields: &[Field]) -> Integrity {
    let at = |tag: u32| fields.iter().position(|field| field.tag == tag);
    let begin_string = at(8);
    let body_length_field = at(9);
    let check_sum_field = at(10);

    let body_first = body_length_field
        .or(begin_string)
        .map_or(0, |index| index + 1);
    let body_end = check_sum_field.unwrap_or(fields.len());

    let mut body_length = 0u32;
    for field in fields.iter().take(body_end).skip(body_first) {
        let span = field.span();
        body_length += span.end - span.start + SEPARATOR_BYTES;
    }

    let body_text = body_length.to_string();
    // With BodyLength(9) absent the checksum still has to account for the `9=<len>|`
    // that a repaired line would carry.
    let synthetic_9 = if body_length_field.is_none() {
        sum_bytes(b"9=") + sum_bytes(body_text.as_bytes()) + SEPARATOR_BYTES as u64
    } else {
        0
    };

    let mut sum = 0u64;
    for (index, field) in fields.iter().take(body_end).enumerate() {
        if index == body_first {
            sum += synthetic_9;
        }
        if Some(index) == body_length_field {
            // The corrected length, not the one written on the line.
            sum += sum_bytes(&bytes[field.tag_start.index()..field.value_start().index()]);
            sum += sum_bytes(body_text.as_bytes());
        } else {
            let span = field.span();
            sum += sum_bytes(&bytes[span.start.index()..span.end.index()]);
        }
        sum += SEPARATOR_BYTES as u64;
    }
    if body_first >= body_end {
        sum += synthetic_9;
    }

    Integrity {
        begin_string: begin_string.map(|index| index as u32),
        body_length_field: body_length_field.map(|index| index as u32),
        check_sum_field: check_sum_field.map(|index| index as u32),
        body_length,
        check_sum: (sum % 256) as u8,
    }
}

fn sum_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&byte| u64::from(byte)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::decode;
    use crate::dictionary::{bundled, Dict, Version};
    use std::sync::Arc;

    fn integ(line: &str) -> Integrity {
        let dict = Dict::Static(bundled(Version::Fix44));
        let version: Arc<str> = Arc::from("FIX.4.4");
        match decode(line.as_bytes(), &dict, &version) {
            crate::codec::LineAnalysis::Message(message) => message.integrity(line.as_bytes()),
            other => panic!("expected a message, got {:?}", other.kind()),
        }
    }

    const VALID_HEARTBEAT: &str =
        "8=FIX.4.4|9=57|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|";
    const VALID_LOGON: &str = "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:00.000|98=0|108=30|10=214|";
    const BAD_BOTH: &str = "8=FIX.4.4|9=7|35=D|34=2|49=CLIENT1|56=BROKER1|\
52=20251026-09:00:02.000|11=ORDER123|21=1|55=BTCUSD|54=1|38=10|40=2|44=65000|59=0|\
60=20251026-09:00:02.000|10=001|";

    #[test]
    fn a_valid_message_agrees_with_its_own_fields() {
        // validation/valid.fix line 0 has no diagnostics, so the computed
        // values must equal the ones written on the line.
        let facts = integ(VALID_HEARTBEAT);
        assert_eq!(facts.body_length, 57);
        assert_eq!(format!("{:03}", facts.check_sum), "171");
        assert_eq!(
            (
                facts.begin_string,
                facts.body_length_field,
                facts.check_sum_field
            ),
            (Some(0), Some(1), Some(7))
        );
    }

    #[test]
    fn bad_body_length_and_checksum_are_both_corrected() {
        // validation/bad-both.fix: "expected 139" and "expected 213".
        let facts = integ(BAD_BOTH);
        assert_eq!(facts.body_length, 139);
        assert_eq!(format!("{:03}", facts.check_sum), "213");
    }

    #[test]
    fn a_wrong_body_length_does_not_disturb_the_checksum() {
        // validation/bad-bodylength.fix carries 9=61 and a CORRECT 10=171: the
        // checksum is computed over the corrected length, not the written one.
        let facts = integ(
            "8=FIX.4.4|9=61|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|",
        );
        assert_eq!(facts.body_length, 57);
        assert_eq!(format!("{:03}", facts.check_sum), "171");
    }

    #[test]
    fn a_missing_body_length_is_synthesised() {
        let facts =
            integ("8=FIX.4.4|35=0|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:02:00.000|10=171|");
        assert!(facts.body_length_field.is_none());
        assert_eq!(facts.body_length, 57);
        assert_eq!(format!("{:03}", facts.check_sum), "171");
    }

    #[test]
    fn a_missing_checksum_is_computed_over_the_whole_line() {
        // validation/missing-checksum.fix — the repair appends "10=214|",
        // matching the CheckSum the valid Logon fixture carries.
        let facts = integ(
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30|",
        );
        assert!(facts.check_sum_field.is_none());
        assert_eq!(format!("{:03}", facts.check_sum), "214");
        assert_eq!(integ(VALID_LOGON).check_sum, facts.check_sum);
    }

    #[test]
    fn a_missing_trailing_separator_does_not_change_the_numbers() {
        // validation/missing-checksum-no-trailing.fix — same expected 214.
        let facts = integ(
            "8=FIX.4.4|9=69|35=A|34=1|49=CLIENT1|56=BROKER1|52=20251026-09:00:00.000|98=0|108=30",
        );
        assert_eq!(format!("{:03}", facts.check_sum), "214");
    }

    #[test]
    fn soh_and_pipe_give_the_same_numbers() {
        let soh = VALID_HEARTBEAT.replace('|', "\u{1}");
        assert_eq!(integ(&soh).body_length, integ(VALID_HEARTBEAT).body_length);
        assert_eq!(integ(&soh).check_sum, integ(VALID_HEARTBEAT).check_sum);
    }

    #[test]
    fn a_skipped_log_prefix_does_not_enter_the_arithmetic() {
        let prefixed = format!("2026-06-30 07:55:12 INFO {VALID_HEARTBEAT}");
        assert_eq!(integ(&prefixed).body_length, 57);
        assert_eq!(integ(&prefixed).check_sum, integ(VALID_HEARTBEAT).check_sum);
    }

    #[test]
    fn a_line_without_tag_8_still_gets_numbers() {
        let facts = integ("35=0|49=CLIENT1|56=BROKER1|52=20260101-00:00:00.000|10=001|");
        assert!(facts.begin_string.is_none());
        assert!(facts.body_length > 0);
    }
}
