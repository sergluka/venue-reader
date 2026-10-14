//! What the realistic sequence *retains*, with the cache and without it.
//!
//! The other half of work-order item 16: `cache_sequence` counts what the cache
//! is hit for, this measures what it costs to have been hit. The viewport
//! ranges are the same ones that bench sends, so the two describe one sequence.
//!
//! **Measured against the no-cache build, not modelled from it.** Run it twice:
//!
//! ```sh
//! cargo bench --bench rss_sequence
//! cargo bench --features bench-no-line-cache --bench rss_sequence
//! ```
//!
//! The difference is what the cache costs on a sequence someone performs.
//! Modelling that difference from `cache_cost`'s bytes-per-line is what this
//! project got wrong in the payload bench, and the arithmetic looked fine.
//!
//! Driven through `Document` rather than the protocol, as `rss_viewport` is: an
//! RSS baseline has to be taken before anything is allocated, and a server
//! thread plus its channels sit in between.
//!
//! Its own target for the reason `common` gives.

mod common;

use common::{context, corpus, report, rss};
use fix_lsp::config::Config;
use fix_lsp::document::Document;

/// The same shape `cache_sequence` drives: forty screens of fifty lines, read
/// down and then back.
const VIEWPORT: usize = 50;
const SCREENS: usize = 40;

/// A regression baseline, not a budget — the precedent is `rss_diagnostics`.
/// Nothing says what this sequence *ought* to retain; item 16 decides that
/// after 4(c), and until then the only regression available is "worse than it
/// was". Set above three measured runs.
const GATE: f64 = 2.1;

fn main() {
    let ctx = context();
    let bytes = corpus();
    let file = bytes.len();
    println!(
        "corpus: {file} bytes, {} lines",
        bytes.iter().filter(|&&b| b == b'\n').count()
    );

    // The loop sweeps after every request; the sequence stays under the
    // default bound, so this is fidelity rather than a change in the figure.
    let bound = Config::default().limits.cache_bound();
    let before = rss();
    let doc = Document::open(&bytes, 1, ctx.clone());
    // Down, then back — the second pass is where a cache pays and where a
    // build without one does the work again.
    for pass in 0..2 {
        for screen in 0..SCREENS {
            let from = screen * VIEWPORT;
            for line in doc.lines.iter().skip(from).take(VIEWPORT) {
                let analysis = line.cached_analysis(&doc.ctx, &doc.population);
                // Touched so the analysis cannot be optimised away in a build
                // that does not keep it.
                std::hint::black_box(analysis.message().is_some());
            }
            doc.evict_over(bound);
        }
        let _ = pass;
    }
    let after = rss();

    let label = if cfg!(feature = "bench-no-line-cache") {
        "sequence (no cache)"
    } else {
        "sequence"
    };
    report(label, before, after, file, GATE);
}
