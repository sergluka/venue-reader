//! Tag numbers, and the few tag values, that the code itself reasons about.
//!
//! A dictionary supplies every label; these name only what is wired into a
//! rule, a summary or a hover, so that each number is written once.

pub const BEGIN_SEQ_NO: u32 = 7;
pub const BEGIN_STRING: u32 = 8;
pub const CL_ORD_ID: u32 = 11;
pub const END_SEQ_NO: u32 = 16;
pub const EXEC_ID: u32 = 17;
pub const MSG_TYPE: u32 = 35;
pub const NEW_SEQ_NO: u32 = 36;
pub const ORDER_QTY: u32 = 38;
pub const ORD_TYPE: u32 = 40;
pub const POSS_DUP_FLAG: u32 = 43;
pub const PRICE: u32 = 44;
pub const SIDE: u32 = 54;
pub const SYMBOL: u32 = 55;
pub const TEXT: u32 = 58;
pub const TIME_IN_FORCE: u32 = 59;
pub const GAP_FILL_FLAG: u32 = 123;
pub const EXEC_TYPE: u32 = 150;

/// FIX Boolean, as it appears on the wire.
pub const YES: &str = "Y";

/// Compared by value, because dictionaries spell the label differently
/// (`Trade`, `TRADE`).
pub mod exec_type {
    pub const TRADE: &str = "F";
    pub const TRADE_CORRECT: &str = "G";
}
