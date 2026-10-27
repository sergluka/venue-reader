//! `didOpen` plus the first viewport answer — the scenario the design spec's
//! RSS acceptance is written for, so the baseline is taken before the document
//! exists and the document's own retention counts against the budget.
//!
//! Its own target for the reason `common` gives.

mod common;

use common::{context, corpus, report, rss, BUDGET};
use venue_reader::document::Document;

fn main() {
    let ctx = context();
    let bytes = corpus();
    let file = bytes.len();
    println!(
        "corpus: {file} bytes, {} lines",
        bytes.iter().filter(|&&b| b == b'\n').count()
    );

    let before = rss();
    let doc = Document::open(&bytes, 1, ctx.clone());
    for line in doc.lines.iter().take(150) {
        line.cached_analysis(&doc.ctx, &doc.population);
    }
    let after = rss();
    report("viewport", before, after, file, BUDGET);
}
