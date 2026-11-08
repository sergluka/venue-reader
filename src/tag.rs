//! Tag numbers, and the few tag values, that the code itself reasons about.
//!
//! A dictionary supplies every label; these name only what is wired into a
//! rule, a summary or a hover, so that each number is written once.

pub const BEGIN_STRING: u32 = 8;
pub const BODY_LENGTH: u32 = 9;
pub const CHECK_SUM: u32 = 10;
pub const MSG_TYPE: u32 = 35;
pub const POSS_DUP_FLAG: u32 = 43;
pub const SENDER_COMP_ID: u32 = 49;
pub const SIDE: u32 = 54;
pub const TARGET_COMP_ID: u32 = 56;
pub const APPL_VER_ID: u32 = 1128;
pub const CSTM_APPL_VER_ID: u32 = 1129;
pub const APPL_EXT_ID: u32 = 1156;

/// The tags that select the dictionaries of a line. Each one may occur once, and
/// only outside repeating groups.
pub const IDENTITY_SELECTORS: [u32; 5] = [
    BEGIN_STRING,
    MSG_TYPE,
    APPL_VER_ID,
    APPL_EXT_ID,
    CSTM_APPL_VER_ID,
];

/// FIX Boolean, as it appears on the wire.
pub const YES: &str = "Y";
