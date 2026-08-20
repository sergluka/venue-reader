//! Shared bench scaffolding: the generated corpus, the analysis context, and
//! the RSS probe the memory targets report against.
//!
//! Each RSS scenario is its own `[[bench]]` target — cargo builds one binary
//! per target, so every baseline is the first thing its process does. An RSS
//! delta taken later is not trustworthy: the allocator keeps the pages an
//! earlier scenario freed, which made the gated viewport figure read 1.71x
//! alone against 1.40x behind the throughput group, and left the transient gate
//! unable to fail at all.
#![allow(dead_code)]

use std::sync::Arc;

use fix_lsp::config::Config;
use fix_lsp::ctx::Ctx;
use fix_lsp::dictionary::Registry;
use xxhash_rust::xxh3::xxh3_64;

const DEFAULT_LINES: usize = 100_000;
/// The RSS acceptance from the design spec.
pub const BUDGET: f64 = 3.0;

pub fn context() -> Arc<Ctx> {
    Ctx::new(&Config::default(), Arc::new(Registry::bundled_only()), None)
}

/// SplitMix64 — twenty lines instead of a dependency, and reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Four shapes: a Heartbeat, a NewOrderSingle, an ExecutionReport, and a
/// MarketDataSnapshot carrying a two-entry repeating group.
/// The generated corpus at an arbitrary length. `corpus()` is the pinned
/// default; a bench that needs a specific line count — an exact cap, say —
/// asks for it here rather than through the environment knob.
pub fn generate_lines(lines: usize) -> Vec<u8> {
    generate(lines)
}

fn generate(lines: usize) -> Vec<u8> {
    let mut rng = Rng(0x5EED);
    let mut out = String::with_capacity(lines * 220);
    for line in 0..lines {
        let seq = line + 1;
        let sending = format!(
            "2026063{}-07:55:{:02}.{:06}",
            line % 10,
            line % 60,
            line % 1000
        );
        match rng.below(4) {
            0 => out.push_str(&format!(
                "8=FIX.4.4|9=57|35=0|34={seq}|49=CLIENT|56=EXCHANGE|52={sending}|10=171|"
            )),
            1 => out.push_str(&format!(
                "8=FIX.4.4|9=139|35=D|34={seq}|49=CLIENT|56=EXCHANGE|52={sending}|\
11=ORDER{seq}|21=1|55=BTCUSD|54={}|38={}|40=2|44={}|59=0|60={sending}|10=213|",
                1 + rng.below(2),
                10 + rng.below(500),
                60000 + rng.below(9000),
            )),
            2 => out.push_str(&format!(
                "8=FIX.4.4|9=210|35=8|34={seq}|49=EXCHANGE|56=CLIENT|52={sending}|\
37=EXEC{seq}|11=ORDER{seq}|17=FILL{seq}|150=F|39=2|55=BTCUSD|54=1|38={}|44={}|\
32={}|31={}|151=0|14={}|6={}|60={sending}|10=074|",
                10 + rng.below(500),
                60000 + rng.below(9000),
                1 + rng.below(50),
                60000 + rng.below(9000),
                1 + rng.below(50),
                60000 + rng.below(9000),
            )),
            _ => out.push_str(&format!(
                "8=FIX.4.4|9=123|35=W|34={seq}|49=EXCHANGE|56=CLIENT|52={sending}|\
55=BTCUSD|268=2|269=0|270={}|271={}|269=1|270={}|271={}|10=118|",
                60000 + rng.below(9000),
                1 + rng.below(900),
                60000 + rng.below(9000),
                1 + rng.below(900),
            )),
        }
        out.push('\n');
    }
    out.into_bytes()
}

pub fn corpus() -> Vec<u8> {
    let lines = std::env::var("FIX_LSP_BENCH_LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_LINES);
    let bytes = generate(lines);
    if lines == DEFAULT_LINES {
        // The knob is meant to change the bytes, so only the default is pinned.
        assert_eq!(
            xxh3_64(&bytes),
            0xBECD_A7D5_04F1_A472,
            "generated corpus changed; update the pin after checking why"
        );
    }
    bytes
}

/// Resident set size in bytes, from the field the design spec's acceptance is
/// written against.
#[cfg(target_os = "linux")]
pub fn rss() -> Option<usize> {
    // VmRSS is already in kB, so this does not have to know the page size —
    // `statm` counts pages, and hardcoding 4096 under-reports 4x on a
    // 16K-page kernel, which would make the gate below pass unconditionally.
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(not(target_os = "linux"))]
pub fn rss() -> Option<usize> {
    None
}

pub fn report(label: &str, before: Option<usize>, after: Option<usize>, file: usize, gate: f64) {
    let (Some(before), Some(after)) = (before, after) else {
        println!("{label}: RSS unavailable on this platform; skipped");
        return;
    };
    let delta = after.saturating_sub(before);
    let ratio = delta as f64 / file as f64;
    println!("{label}: RSS delta {delta} bytes = {ratio:.2}x the file");
    assert!(
        ratio <= gate,
        "{label}: {ratio:.2}x exceeds the {gate:.1}x gate"
    );
}
