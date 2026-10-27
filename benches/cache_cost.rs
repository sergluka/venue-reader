//! What one cached line costs, and how many the RSS budget therefore allows.
//!
//! Its own `harness = false` target rather than a criterion benchmark: it times
//! nothing, runs once, and ends in an assertion. It needs no process isolation —
//! the figure is modelled from `heap_size()`, not measured from RSS — but it is
//! not a benchmark either, and the criterion group is for benchmarks.

mod common;

use common::{context, corpus, BUDGET};
use venue_reader::codec::LineAnalysis;
use venue_reader::document::{Document, Line};

/// The marginal cost of caching a line, which is what Stage 4 needs: it will
/// cache *some* lines, and the question is how many fit. Modelled from
/// `heap_size()` rather than measured, so it is immune to the ordering problem
/// that sent the RSS scenarios into their own targets.
fn main() {
    let ctx = context();
    let bytes = corpus();
    let file = bytes.len();
    let doc = Document::open(&bytes, 1, ctx.clone());
    let mut heap = 0usize;
    for line in doc.lines.iter() {
        heap += line.cached_analysis(&doc.ctx, &doc.population).heap_size();
    }
    cache_cost(heap, doc.lines.len(), file);
}

/// What one cached line costs, and how many the budget therefore allows.
///
/// Modelled rather than measured: an RSS delta taken part-way through the
/// process cannot separate the cache from the document, because the allocator
/// hands back pages the previous scenario freed. The model is exact for the
/// allocations that dominate and is stated in full below, so it can be checked
/// by hand.
fn cache_cost(heap: usize, lines: usize, file: usize) {
    const ARC_HEADER: usize = 16;

    // Per cached line: the `Field`/`GroupInstance` vectors, the `LineAnalysis` they
    // hang off, and its `Arc` header.
    let cache = heap as f64 / lines as f64 + (size_of::<LineAnalysis>() + ARC_HEADER) as f64;
    // Per line of the open document, whether or not it is cached: the bytes,
    // their `Arc` header, the `Line`, its own `Arc` header, and the slot in the
    // document's pointer vector.
    let document = file as f64 / lines as f64
        + (ARC_HEADER + size_of::<Line>() + ARC_HEADER + size_of::<usize>()) as f64;
    let source = file as f64 / lines as f64;

    let headroom = ((BUDGET * file as f64 - document * lines as f64) / cache).max(0.0);
    println!(
        "cache cost: {cache:.0} bytes per cached line, {:.1}x its {source:.0} bytes of text; \
the open document retains {document:.0} bytes per line regardless, so the {BUDGET:.0}x budget \
leaves room for {headroom:.0} cached lines out of {lines}",
        cache / source
    );
}
