//! The worst case: a reader who scrolls the whole file.
//!
//! `rss_viewport` with the viewport count raised from 150 lines to every line,
//! and the difference between the two figures is what the cache costs when the
//! document has been read end to end. **Kept apart from `rss_sequence` on
//! purpose** — combining a forty-screen workload with whole-file retention
//! describes a session nobody has.
//!
//! **A budget, since work-order item 16 bounded the cache.** Before the bound
//! this read retained 4.60× the file (2026-09-11, 46.7 MB of cache on a 16 MB
//! log), which the 3× acceptance did not cover — it is scoped to `didOpen`
//! plus the first viewport — but which contradicted the invariant that the
//! cache is bounded. Now the loop sweeps after every request, modelled here
//! after every screen: second chance lets each population survive one sweep,
//! so the peak is two bounds of lines, about 3.5 MB at the default. The gate
//! is set from that arithmetic plus the measured no-cache floor, not from a
//! run, so that a cache which stops being swept fails here.
//!
//! Its own target for the reason `common` gives.

mod common;

use common::{context, corpus, report, rss};
use fix_lsp::config::Config;
use fix_lsp::document::Document;

/// What a screen is to the loop: one request, then one sweep.
const VIEWPORT: usize = 50;

/// The no-cache floor is 1.71×; two bounds of lines at ~440 bytes each are
/// 0.22× of this corpus; the rest is headroom for a sweep that lands late.
const GATE: f64 = 2.1;

fn main() {
    let ctx = context();
    let bytes = corpus();
    let file = bytes.len();
    println!(
        "corpus: {file} bytes, {} lines",
        bytes.iter().filter(|&&b| b == b'\n').count()
    );

    let before = rss();
    let bound = Config::default().limits.cache_bound();
    let doc = Document::open(&bytes, 1, ctx.clone());
    for screen in doc.lines.chunks(VIEWPORT) {
        for line in screen {
            let analysis = line.cached_analysis(&doc.ctx, &doc.population);
            std::hint::black_box(analysis.message().is_some());
        }
        doc.evict_over(bound);
    }
    let after = rss();
    println!(
        "cached at the end: {} lines, bound {bound}",
        doc.population.count()
    );
    report("whole file scrolled", before, after, file, GATE);
}
