//! Codec throughput. The memory figures live in their own targets —
//! `rss_viewport`, `rss_transient` and `cache_cost` — for the reasons `common`
//! gives.

#[path = "support/allocations.rs"]
mod allocations;
mod common;

#[global_allocator]
static ALLOCATOR: allocations::CountingAllocator = allocations::CountingAllocator;

use common::{context, corpus};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use venue_reader::codec::LineAnalysis;
use venue_reader::ctx::analyze;
use venue_reader::render::{hover, summary, Options};

fn throughput(c: &mut Criterion) {
    let ctx = context();
    let bytes = corpus();
    let lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();

    // One benchmark per generated message shape, over a batch of lines. What
    // this replaces was `heartbeat` plus `first` — and `first` was not a shape
    // at all, just whichever line the generator happened to put at index 0, so
    // a change in the RNG silently changed what it measured. Four named shapes
    // say which one moved.
    //
    // Batching is for attribution, not stability: these numbers swing by a
    // factor of two on a loaded machine whatever the iteration size, so
    // criterion's verdicts are only worth reading when the box is quiet. The
    // spec asks for fixed hardware for exactly this reason.
    const BATCH: usize = 200;
    let mut group = c.benchmark_group("analyze");
    for (label, msg_type) in [
        ("heartbeat", &b"|35=0|"[..]),
        ("new_order_single", b"|35=D|"),
        ("execution_report", b"|35=8|"),
        ("market_data", b"|35=W|"),
    ] {
        let shape: Vec<&[u8]> = lines
            .iter()
            .filter(|line| line.windows(msg_type.len()).any(|w| w == msg_type))
            .take(BATCH)
            .copied()
            .collect();
        assert_eq!(shape.len(), BATCH, "{label}: too few lines in the corpus");
        group.throughput(criterion::Throughput::Elements(BATCH as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let mut fields = 0usize;
                for line in &shape {
                    fields += analyze(line, &ctx).message().map_or(0, |m| m.fields.len());
                }
                fields
            })
        });
    }
    group.finish();

    let mut fixt = c.benchmark_group("analyze_fixt");
    for (code, version) in [("7", "FIX.5.0"), ("8", "FIX.5.0SP1"), ("9", "FIX.5.0SP2")] {
        let messages: Vec<_> = lines
            .iter()
            .filter(|line| !line.windows(6).any(|value| value == b"|35=0|"))
            .take(BATCH)
            .map(|line| {
                String::from_utf8_lossy(line)
                    .replacen("8=FIX.4.4|", &format!("8=FIXT.1.1|1128={code}|"), 1)
                    .into_bytes()
            })
            .collect();
        assert_eq!(messages.len(), BATCH);
        for line in &messages {
            let analysis = analyze(line, &ctx);
            let message = analysis.message().expect("synthetic FIXT message");
            assert_eq!(message.resolution.issue, None);
            if message.application_known() {
                assert_eq!(message.version.as_ref(), version);
            }
        }
        fixt.throughput(criterion::Throughput::Elements(BATCH as u64));
        fixt.bench_function(version, |bench| {
            bench.iter(|| {
                messages
                    .iter()
                    .map(|line| {
                        analyze(line, &ctx)
                            .message()
                            .map_or(0, |message| message.fields.len())
                    })
                    .sum::<usize>()
            })
        });
    }
    fixt.finish();

    summary_cost(c, &ctx, &lines);

    // Cold walk: a fresh document per iteration, or only the first is cold.
    c.bench_function("cold_walk_transient", |b| {
        b.iter_batched(
            || bytes.clone(),
            |bytes| {
                let mut total = 0usize;
                for line in bytes.split(|&byte| byte == b'\n') {
                    total += analyze(line, &ctx).message().map_or(0, |m| m.fields.len());
                }
                total
            },
            BatchSize::LargeInput,
        )
    });

    // The viewport case Stage 4 multiplies by ~150.
    let viewport: Vec<_> = lines
        .iter()
        .take(150)
        .filter_map(|line| match analyze(line, &ctx) {
            LineAnalysis::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    let options = Options::default();
    c.bench_function("summary_viewport", |b| {
        b.iter(|| {
            let mut length = 0usize;
            for (line, message) in lines.iter().zip(&viewport) {
                length += summary::render(line, message, &options.template).len();
            }
            length
        })
    });

    // §4's acceptance for hover is single-digit milliseconds, and nothing
    // measured it until this stage. Every field of one line, because a single
    // field is too fast to time cleanly: 8 fields in 11.2 µs on 2026-09-03,
    // so ~1.4 µs a request against a budget three orders of magnitude larger.
    // Read it against work-order item 12 before believing the absolute number —
    // a wall-clock figure taken inside an agent session is worth little; what
    // it is here for is to notice the day it becomes milliseconds.
    let message = viewport.first().expect("the corpus has messages");
    let line = lines.first().expect("and its bytes");
    c.bench_function("hover_one_field", |b| {
        b.iter(|| {
            let mut length = 0usize;
            for at in 0..message.fields.len() {
                length += hover(line, message, at, &options, None).map_or(0, |text| text.len());
            }
            length
        })
    });
}

criterion_group!(benches, throughput);
criterion_main!(benches);

fn summary_cost(c: &mut Criterion, ctx: &venue_reader::ctx::Ctx, lines: &[&[u8]]) {
    use std::hint::black_box;
    use venue_reader::render::{summary::structured, summary_joined};
    let mut group = c.benchmark_group("summary_cost");
    for (name, pattern) in [
        ("heartbeat", "|35=0|"),
        ("order", "|35=D|"),
        ("execution", "|35=8|"),
    ] {
        let line = lines
            .iter()
            .find(|line| {
                line.windows(pattern.len())
                    .any(|part| part == pattern.as_bytes())
            })
            .unwrap();
        let analysis = analyze(line, ctx);
        let message = analysis.message().unwrap();
        let options = Options::default();
        // Exclude one-time dictionary/template initialization from per-message allocations.
        drop(structured(line, message, &options, true));
        drop(summary_joined(line, message, &options));
        group.bench_function(format!("{name}/analysis"), |bench| {
            bench.iter(|| black_box(analyze(black_box(line), ctx)))
        });
        group.bench_function(format!("{name}/plain"), |bench| {
            bench.iter(|| black_box(summary_joined(black_box(line), message, &options)))
        });
        group.bench_function(format!("{name}/combined"), |bench| {
            bench.iter(|| {
                let analysis = analyze(black_box(line), ctx);
                black_box(structured(
                    line,
                    analysis.message().unwrap(),
                    &options,
                    true,
                ))
            })
        });
        for semantic in [false, true] {
            group.bench_function(format!("{name}/parts_{semantic}"), |bench| {
                bench.iter(|| black_box(structured(black_box(line), message, &options, semantic)))
            });
            let counts = allocations::measure(|| {
                drop(structured(line, message, &options, semantic));
            });
            assert_eq!(
                counts.live, 0,
                "rendered output must release all allocations"
            );
            println!("summary allocations {name}, metadata={semantic}: {counts:?}");
        }
        let counts = allocations::measure(|| {
            drop(summary_joined(line, message, &options));
        });
        assert_eq!(counts.live, 0);
        println!("summary allocations {name}, plain: {counts:?}");
    }
    // Vary lookup work independently of input size, including absent references.
    for fields in [32, 4096] {
        let mut line = String::from("8=FIX.4.4|35=0|");
        for index in 0..fields {
            line.push_str(&format!("{}=value|", 100_000 + index));
        }
        line.push_str("58=last|");
        let analysis = analyze(line.as_bytes(), ctx);
        let message = analysis.message().unwrap();
        lookup_cost(
            &mut group,
            &format!("large/{fields}"),
            line.as_bytes(),
            message,
        );
    }
    for entries in [32, 4096] {
        let mut line = format!("8=FIX.4.4|35=W|55=BTCUSD|268={entries}|");
        for _ in 0..entries {
            line.push_str("269=0|270=100|271=10|");
        }
        line.push_str("58=last|");
        let analysis = analyze(line.as_bytes(), ctx);
        let message = analysis.message().unwrap();
        assert_eq!(message.groups.len(), entries);
        assert_eq!(message.fields.len(), 5 + 3 * entries);
        lookup_cost(
            &mut group,
            &format!("large_groups/{entries}"),
            line.as_bytes(),
            message,
        );
    }
    group.finish();
}

/// One and many references to the last field and to an absent one: the linear
/// first-occurrence lookup's cost, independent of how the line got long.
fn lookup_cost(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    prefix: &str,
    line: &[u8],
    message: &venue_reader::codec::MessageAnalysis,
) {
    use std::hint::black_box;
    use venue_reader::render::{summary::structured, Template};
    for references in [1, 32] {
        for (kind, reference) in [("late", "{58}"), ("absent", "{999999}")] {
            let options = Options {
                template: Template::parse(&reference.repeat(references)).unwrap(),
                details: false,
                ..Options::default()
            };
            group.bench_function(format!("{prefix}/{references}/{kind}"), |bench| {
                bench.iter(|| black_box(structured(black_box(line), message, &options, true)))
            });
        }
    }
}
