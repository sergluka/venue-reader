//! A whole-document walk that keeps nothing — the property Stage 4a's
//! diagnostics worker leans on.
//!
//! The baseline is taken *after* `Document::open`, unlike the viewport target:
//! what is asserted here is that the walk itself retains nothing, and an open
//! document costs ~250 bytes a line whether or not anything walks it. Measuring
//! from before the open would fold that in and gate the wrong quantity.
//!
//! Its own target for the reason `common` gives.

mod common;

use common::{context, corpus, report, rss};
use venue_reader::ctx::analyze;
use venue_reader::document::Document;

/// The walk may keep nothing, so anything above noise is a regression.
const GATE: f64 = 0.05;

fn main() {
    let ctx = context();
    let bytes = corpus();
    let file = bytes.len();
    let doc = Document::open(&bytes, 1, ctx.clone());

    let before = rss();
    let mut fields = 0usize;
    for line in doc.lines.iter() {
        fields += analyze(&line.bytes, &doc.ctx)
            .message()
            .map_or(0, |message| message.fields.len());
    }
    let after = rss();
    println!("whole document, transient: {fields} fields");
    report("whole_transient", before, after, file, GATE);
}
