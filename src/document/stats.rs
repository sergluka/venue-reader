//! Hit and miss counts for the line cache. Compiled only under `bench-cache-stats`.
//!
//! Process-wide rather than per-document: they serve one scripted benchmark
//! sequence run with nothing else in the process, and per-document counters
//! would be state on `Document` that outlives the measurement.
//!
//! A count rather than a timing, for work-order item 12's reason: an
//! instruction is immune to core class and neighbours, and a wall-clock figure
//! taken on this machine is not.

use std::sync::atomic::{AtomicU64, Ordering};

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

pub(super) fn record(hit: bool) {
    let counter = if hit { &HITS } else { &MISSES };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Hits and misses since the last call, which this resets — so a caller can
/// bracket one phase of a sequence and read that phase alone.
pub fn take() -> (u64, u64) {
    (
        HITS.swap(0, Ordering::Relaxed),
        MISSES.swap(0, Ordering::Relaxed),
    )
}
