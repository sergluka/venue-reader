//! Semantic-token classification over a viewport: `analyze` does not reach it, and the per-field
//! header and trailer check (`Scope::reaches`) is paid here.

mod common;

use common::{context, corpus};

use criterion::{criterion_group, criterion_main, Criterion};
use fix_lsp::codec::LineAnalysis;
use fix_lsp::ctx::analyze;
use fix_lsp::render::tokens::classify;

fn tokens(c: &mut Criterion) {
    let ctx = context();
    let bytes = corpus();
    let lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').take(150).collect();
    let analyses: Vec<LineAnalysis> = lines.iter().map(|line| analyze(line, &ctx)).collect();
    let mut out = Vec::new();
    c.bench_function("classify_viewport", |b| {
        b.iter(|| {
            out.clear();
            for (line, analysis) in lines.iter().zip(&analyses) {
                classify(line, analysis, true, &mut out);
            }
            out.len()
        })
    });
}

criterion_group!(benches, tokens);
criterion_main!(benches);
