//! `c0mpute bench` — measure what this node can actually do.
//!
//! The capability ad (`c0mpute/cap/v1`) says *what* a worker has: roles and
//! one hardware tag. It never said *how fast*. This crate runs three CPU
//! workloads at increasing thread counts and reduces them to one number,
//! the **bench score**, that schedulers and buyers can compare across nodes.
//!
//! Workloads (all pure Rust, no external binaries, deterministic input):
//!
//!   * `fib`    — recursive fork-join fibonacci. Scheduler + call overhead.
//!   * `matmul` — dense f64 matrix multiply, rows partitioned. FPU + cache.
//!   * `hash`   — blake3 over a fixed buffer in 1 MiB chunks. Memory
//!                bandwidth, and the exact work the verify/storage roles do.
//!
//! Each workload runs at thread counts 1, 2, 4, … up to the machine's
//! parallelism. Per sample we record the duration, the speedup over the
//! single-thread run and `scaled` (duration relative to the fastest sample
//! of that workload). The JSON layout deliberately mirrors
//! <https://fleetcode.com/runtime-benchmarks/> (`metadata` + `results`
//! keyed by runtime → workload → per-thread samples) so anyone who already
//! reads those files can read ours.
//!
//! The report is written to `<data_dir>/bench.json`; the supervisor reads
//! the score from there when it builds the capability ad.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// File name under the c0mpute data dir.
pub const FILE_NAME: &str = "bench.json";

/// Key under `results` that holds our samples. fleetcode keys by C++ runtime
/// (`libfork`, `tbb`, …); we have exactly one runtime, the c0mpute binary.
pub const RUNTIME: &str = "c0mpute";

/// Workloads in the order they run and print.
pub const WORKLOADS: [&str; 3] = ["fib", "matmul", "hash"];

// ────────────────────────────────────────────────────────────────────────
// Report types (serialised to bench.json)
// ────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchReport {
    pub metadata: Metadata,
    /// runtime → workload → samples (one per thread count).
    pub results: BTreeMap<String, BTreeMap<String, Vec<Sample>>>,
    /// The one number the network compares. See [`score`].
    pub score: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Metadata {
    /// RFC 3339 UTC.
    pub start_time: String,
    pub cpu: String,
    pub cores: usize,
    pub kernel: String,
    pub compiler: String,
    /// c0mpute version that produced the report.
    pub version: String,
    /// `quick` runs use smaller problem sizes; their scores are not
    /// comparable with full runs and are never advertised.
    pub quick: bool,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    /// Problem size, e.g. `n=34` or `n=512` or `64 MiB`.
    pub params: String,
    pub threads: usize,
    pub result: SampleResult,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SampleResult {
    /// Human form, `"1590139 us"`, matching fleetcode.
    pub duration: String,
    pub duration_us: u64,
    /// duration / fastest duration for this workload (1.0 = best sample).
    pub scaled: f64,
    /// duration(1 thread) / duration(this sample).
    pub speedup: f64,
    /// Work units per second: fib node visits, matmul flops, hashed bytes.
    pub throughput: f64,
}

// ────────────────────────────────────────────────────────────────────────
// Options
// ────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Smaller problem sizes; finishes in a second or two. Not advertised.
    pub quick: bool,
    /// Explicit thread counts; default is 1, 2, 4, … , available_parallelism.
    pub threads: Option<Vec<usize>>,
    /// Called before each sample runs, for progress output.
    pub progress: Option<fn(&str, usize)>,
}

/// Default thread ladder: powers of two up to `max`, then `max` itself.
pub fn thread_ladder(max: usize) -> Vec<usize> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut t = 1;
    while t < max {
        out.push(t);
        t *= 2;
    }
    out.push(max);
    out
}

// ────────────────────────────────────────────────────────────────────────
// Run
// ────────────────────────────────────────────────────────────────────────

/// Run every workload at every thread count and build the report.
pub fn run(opts: &Options) -> BenchReport {
    let started = Instant::now();
    let start_time = rfc3339_now();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let threads = opts.threads.clone().unwrap_or_else(|| thread_ladder(cores));

    let mut per_workload: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for name in WORKLOADS {
        let mut samples = Vec::with_capacity(threads.len());
        for &t in &threads {
            if let Some(cb) = opts.progress {
                cb(name, t);
            }
            let (params, work_units, dur) = run_workload(name, t, opts.quick);
            samples.push(Sample {
                params,
                threads: t,
                result: SampleResult {
                    duration: format!("{} us", dur.as_micros()),
                    duration_us: dur.as_micros() as u64,
                    scaled: 0.0,
                    speedup: 0.0,
                    throughput: work_units / dur.as_secs_f64().max(1e-9),
                },
            });
        }
        finish_samples(&mut samples);
        per_workload.insert(name.to_string(), samples);
    }

    let mut results = BTreeMap::new();
    results.insert(RUNTIME.to_string(), per_workload);

    let (cpu, kernel) = host_info();
    let mut report = BenchReport {
        metadata: Metadata {
            start_time,
            cpu,
            cores,
            kernel,
            compiler: env!("C0MPUTE_RUSTC_VERSION").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            quick: opts.quick,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        results,
        score: 0,
    };
    report.score = score(&report);
    report
}

/// Fill `scaled` and `speedup` once every thread count has run.
fn finish_samples(samples: &mut [Sample]) {
    let Some(best) = samples.iter().map(|s| s.result.duration_us).min() else {
        return;
    };
    let single = samples
        .iter()
        .find(|s| s.threads == 1)
        .or_else(|| samples.first())
        .map(|s| s.result.duration_us)
        .unwrap_or(best);
    for s in samples {
        let d = s.result.duration_us.max(1) as f64;
        s.result.scaled = round3(d / best.max(1) as f64);
        s.result.speedup = round3(single as f64 / d);
        s.result.throughput = (s.result.throughput * 10.0).round() / 10.0;
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Returns (params, work units, wall time).
fn run_workload(name: &str, threads: usize, quick: bool) -> (String, f64, Duration) {
    match name {
        "fib" => {
            let n = if quick { 29 } else { 36 };
            let t0 = Instant::now();
            let (value, visits) = fib_parallel(n, threads);
            let dur = t0.elapsed();
            debug_assert_eq!(value, fib_serial_value(n));
            (format!("n={n}"), visits as f64, dur)
        }
        "matmul" => {
            let n = if quick { 192 } else { 512 };
            let (a, b) = matmul_inputs(n);
            let t0 = Instant::now();
            let c = matmul_parallel(&a, &b, n, threads);
            let dur = t0.elapsed();
            std::hint::black_box(&c);
            // 2 flops (mul + add) per inner iteration.
            (format!("n={n}"), 2.0 * (n as f64).powi(3), dur)
        }
        "hash" => {
            let mib = if quick { 16 } else { 128 };
            let buf = hash_input(mib << 20);
            let t0 = Instant::now();
            let digest = hash_parallel(&buf, threads);
            let dur = t0.elapsed();
            std::hint::black_box(digest);
            (format!("{mib} MiB"), buf.len() as f64, dur)
        }
        other => panic!("unknown workload {other}"),
    }
}

// ─── fib ────────────────────────────────────────────────────────────────

fn fib_serial_value(n: u32) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        let t = a + b;
        a = b;
        b = t;
    }
    a
}

/// Naive recursive fib, counting node visits so throughput is meaningful.
fn fib_rec(n: u32, visits: &mut u64) -> u64 {
    *visits += 1;
    if n < 2 {
        return n as u64;
    }
    fib_rec(n - 1, visits) + fib_rec(n - 2, visits)
}

/// Fork the tree to depth `split` (2^split leaves), then let `threads`
/// workers pull leaves off a shared counter. Returns (fib(n), node visits).
fn fib_parallel(n: u32, threads: usize) -> (u64, u64) {
    // Enough leaves to load-balance: 64 or more per thread.
    let split = ((threads.max(1) * 64) as f64).log2().ceil() as u32;
    let split = split.min(n.saturating_sub(1)).max(1);

    // Enumerate leaves (subproblem sizes) by expanding the top `split` levels.
    let mut leaves: Vec<u32> = vec![n];
    let mut visits_top: u64 = 0;
    for _ in 0..split {
        let mut next = Vec::with_capacity(leaves.len() * 2);
        for &m in &leaves {
            if m < 2 {
                // Terminal: keep it as a leaf; the worker counts its visit.
                next.push(m);
            } else {
                visits_top += 1;
                next.push(m - 1);
                next.push(m - 2);
            }
        }
        leaves = next;
    }

    let idx = AtomicUsize::new(0);
    let (sum, visits) = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                let idx = &idx;
                let leaves = &leaves;
                s.spawn(move || {
                    let mut sum = 0u64;
                    let mut visits = 0u64;
                    loop {
                        let i = idx.fetch_add(1, Ordering::Relaxed);
                        if i >= leaves.len() {
                            break;
                        }
                        sum += fib_rec(leaves[i], &mut visits);
                    }
                    (sum, visits)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("fib worker panicked"))
            .fold((0u64, 0u64), |(s, v), (s2, v2)| (s + s2, v + v2))
    });
    (sum, visits + visits_top)
}

// ─── matmul ─────────────────────────────────────────────────────────────

fn matmul_inputs(n: usize) -> (Vec<f64>, Vec<f64>) {
    // Deterministic, well-conditioned values.
    let a: Vec<f64> = (0..n * n).map(|i| ((i % 17) as f64) * 0.25 + 1.0).collect();
    let b: Vec<f64> = (0..n * n).map(|i| ((i % 13) as f64) * 0.5 - 3.0).collect();
    (a, b)
}

/// Row-partitioned C = A × B with a transposed copy of B for cache-friendly
/// inner loops. Workers pull rows off a shared counter.
fn matmul_parallel(a: &[f64], b: &[f64], n: usize, threads: usize) -> Vec<f64> {
    let mut bt = vec![0.0; n * n];
    for i in 0..n {
        for j in 0..n {
            bt[j * n + i] = b[i * n + j];
        }
    }
    let mut c = vec![0.0f64; n * n];
    let next_row = AtomicUsize::new(0);
    // Hand each thread a raw pointer to C; rows are disjoint so writes never
    // overlap. Kept in one place so the unsafe is auditable.
    let c_ptr = c.as_mut_ptr() as usize;
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            let next_row = &next_row;
            let bt = &bt;
            s.spawn(move || {
                loop {
                    let i = next_row.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let arow = &a[i * n..(i + 1) * n];
                    // SAFETY: row `i` is claimed by exactly one thread via the
                    // atomic counter, and `c` outlives the scope.
                    let crow = unsafe {
                        std::slice::from_raw_parts_mut((c_ptr as *mut f64).add(i * n), n)
                    };
                    for j in 0..n {
                        let bcol = &bt[j * n..(j + 1) * n];
                        let mut acc = 0.0;
                        for k in 0..n {
                            acc += arow[k] * bcol[k];
                        }
                        crow[j] = acc;
                    }
                }
            });
        }
    });
    c
}

// ─── hash ───────────────────────────────────────────────────────────────

fn hash_input(len: usize) -> Vec<u8> {
    // xorshift fill — cheap, deterministic, not compressible.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = vec![0u8; len];
    for chunk in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

/// Hash 1 MiB chunks in parallel, then hash the chunk digests in order so the
/// result is independent of scheduling.
fn hash_parallel(buf: &[u8], threads: usize) -> blake3::Hash {
    const CHUNK: usize = 1 << 20;
    let chunks: Vec<&[u8]> = buf.chunks(CHUNK).collect();
    let mut digests = vec![[0u8; 32]; chunks.len()];
    let next = AtomicUsize::new(0);
    let d_ptr = digests.as_mut_ptr() as usize;
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            let next = &next;
            let chunks = &chunks;
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= chunks.len() {
                        break;
                    }
                    let h = blake3::hash(chunks[i]);
                    // SAFETY: slot `i` is claimed by exactly one thread and the
                    // vec outlives the scope.
                    unsafe { *(d_ptr as *mut [u8; 32]).add(i) = *h.as_bytes() };
                }
            });
        }
    });
    let mut hasher = blake3::Hasher::new();
    for d in &digests {
        hasher.update(d);
    }
    hasher.finalize()
}

// ────────────────────────────────────────────────────────────────────────
// Score
// ────────────────────────────────────────────────────────────────────────

/// Reference throughput per workload, in work units per second. The score is
/// the geometric mean of (best throughput / reference) × 1000, so **1000 means
/// the node matches one reference core on every workload**. The references
/// are round numbers just under the single-thread results of the box the
/// benchmark was written on (one DigitalOcean Premium Intel vCPU, 2026-09:
/// 581 Mvis/s, 1.54 GFLOP/s, 2.73 GiB/s) and stay fixed across releases so
/// scores remain comparable. That 8-vCPU droplet scores about 7 000; a
/// 16-core desktop that scales well lands around 20 000.
pub const REFERENCE: [(&str, f64); 3] = [
    ("fib", 500_000_000.0),      // node visits / s
    ("matmul", 1_500_000_000.0), // flops / s
    ("hash", 2_500_000_000.0),   // bytes / s
];

/// Geometric mean over workloads of best throughput relative to [`REFERENCE`].
pub fn score(report: &BenchReport) -> u32 {
    let Some(ours) = report.results.get(RUNTIME) else {
        return 0;
    };
    let mut log_sum = 0.0;
    let mut n = 0usize;
    for (name, reference) in REFERENCE {
        let Some(samples) = ours.get(name) else {
            continue;
        };
        let best = samples
            .iter()
            .map(|s| s.result.throughput)
            .fold(0.0, f64::max);
        if best <= 0.0 {
            continue;
        }
        log_sum += (best / reference).ln();
        n += 1;
    }
    if n == 0 {
        return 0;
    }
    ((log_sum / n as f64).exp() * 1000.0)
        .round()
        .clamp(0.0, u32::MAX as f64) as u32
}

// ────────────────────────────────────────────────────────────────────────
// Persistence
// ────────────────────────────────────────────────────────────────────────

pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

pub fn save(report: &BenchReport, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

/// `Ok(None)` when no report has been written yet.
pub fn load(path: &Path) -> Result<Option<BenchReport>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Score to advertise on the network: only full runs count.
pub fn advertised_score(path: &Path) -> Option<u32> {
    load(path)
        .ok()
        .flatten()
        .filter(|r| !r.metadata.quick && r.score > 0)
        .map(|r| r.score)
}

// ────────────────────────────────────────────────────────────────────────
// Text rendering (CLI)
// ────────────────────────────────────────────────────────────────────────

/// Plain-text table with unicode bars: one block per workload, one row per
/// thread count, bar length = parallel efficiency (speedup / threads), so
/// ideal scaling fills the bar.
pub fn render_text(report: &BenchReport) -> String {
    let m = &report.metadata;
    let mut out = String::new();
    out.push_str(&format!(
        "c0mpute bench  ·  score {}{}\n",
        report.score,
        if m.quick {
            "  (quick run — not advertised)"
        } else {
            ""
        }
    ));
    out.push_str(&format!("  cpu      {}\n", m.cpu));
    out.push_str(&format!("  cores    {}\n", m.cores));
    out.push_str(&format!("  kernel   {}\n", m.kernel));
    out.push_str(&format!("  compiler {}\n", m.compiler));
    out.push_str(&format!(
        "  when     {}  ({} ms)\n",
        m.start_time, m.elapsed_ms
    ));

    let Some(ours) = report.results.get(RUNTIME) else {
        return out;
    };
    const BAR: usize = 24;
    for name in WORKLOADS {
        let Some(samples) = ours.get(name) else {
            continue;
        };
        let params = samples.first().map(|s| s.params.as_str()).unwrap_or("");
        out.push_str(&format!("\n  {name} ({params})\n"));
        out.push_str("  thr   duration      speedup  eff   throughput\n");
        for s in samples {
            let eff = (s.result.speedup / s.threads.max(1) as f64).clamp(0.0, 1.0);
            let filled = (eff * BAR as f64).round() as usize;
            let bar: String = "█".repeat(filled) + &"░".repeat(BAR.saturating_sub(filled));
            out.push_str(&format!(
                "  {:>3}  {:>9} us  {:>6.2}x  {:>3}%  {:>13}  {bar}\n",
                s.threads,
                s.result.duration_us,
                s.result.speedup,
                (eff * 100.0).round() as u32,
                human_rate(name, s.result.throughput),
            ));
        }
    }
    out
}

/// `1.2 Gvis/s`, `3.4 GFLOP/s`, `2.1 GiB/s`.
pub fn human_rate(workload: &str, per_sec: f64) -> String {
    match workload {
        "hash" => {
            let gib = per_sec / (1u64 << 30) as f64;
            if gib >= 1.0 {
                format!("{gib:.2} GiB/s")
            } else {
                format!("{:.0} MiB/s", per_sec / (1u64 << 20) as f64)
            }
        }
        "matmul" => format!("{:.2} GFLOP/s", per_sec / 1e9),
        _ => {
            if per_sec >= 1e9 {
                format!("{:.2} Gvis/s", per_sec / 1e9)
            } else {
                format!("{:.0} Mvis/s", per_sec / 1e6)
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Host info
// ────────────────────────────────────────────────────────────────────────

fn host_info() -> (String, String) {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_cpu_all();
    let cpu = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let kernel = format!(
        "{} {}",
        System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
        System::kernel_version().unwrap_or_default()
    )
    .trim()
    .to_string();
    (cpu, kernel)
}

fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant), avoids pulling in chrono.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_is_powers_of_two_then_max() {
        assert_eq!(thread_ladder(1), vec![1]);
        assert_eq!(thread_ladder(2), vec![1, 2]);
        assert_eq!(thread_ladder(6), vec![1, 2, 4, 6]);
        assert_eq!(thread_ladder(16), vec![1, 2, 4, 8, 16]);
        assert_eq!(thread_ladder(0), vec![1]);
    }

    #[test]
    fn fib_parallel_matches_closed_form_at_any_thread_count() {
        for threads in [1, 2, 3, 8] {
            for n in [1u32, 2, 5, 20, 25] {
                let (v, visits) = fib_parallel(n, threads);
                assert_eq!(v, fib_serial_value(n), "n={n} threads={threads}");
                assert!(visits > 0);
            }
        }
    }

    #[test]
    fn fib_visit_count_is_schedule_independent() {
        let (_, one) = fib_parallel(22, 1);
        let (_, four) = fib_parallel(22, 4);
        let mut serial = 0;
        fib_rec(22, &mut serial);
        assert_eq!(one, serial);
        assert_eq!(four, serial);
    }

    #[test]
    fn matmul_parallel_matches_naive() {
        let n = 37;
        let (a, b) = matmul_inputs(n);
        let mut expect = vec![0.0; n * n];
        for i in 0..n {
            for j in 0..n {
                let mut acc = 0.0;
                for k in 0..n {
                    acc += a[i * n + k] * b[k * n + j];
                }
                expect[i * n + j] = acc;
            }
        }
        for threads in [1, 3, 8] {
            let got = matmul_parallel(&a, &b, n, threads);
            for (g, e) in got.iter().zip(&expect) {
                assert!((g - e).abs() < 1e-9, "threads={threads}");
            }
        }
    }

    #[test]
    fn hash_parallel_is_schedule_independent() {
        let buf = hash_input(5 * (1 << 20) + 123);
        let one = hash_parallel(&buf, 1);
        assert_eq!(one, hash_parallel(&buf, 4));
        assert_eq!(one, hash_parallel(&buf, 7));
    }

    #[test]
    fn quick_run_produces_full_report_and_round_trips() {
        let report = run(&Options {
            quick: true,
            threads: Some(vec![1, 2]),
            progress: None,
        });
        let ours = &report.results[RUNTIME];
        for w in WORKLOADS {
            let s = &ours[w];
            assert_eq!(s.len(), 2);
            assert_eq!(s[0].threads, 1);
            assert_eq!(s[0].result.speedup, 1.0);
            assert!(
                s.iter().any(|x| x.result.scaled == 1.0),
                "one sample is the best"
            );
            assert!(s.iter().all(|x| x.result.throughput > 0.0));
        }
        assert!(report.metadata.quick);
        assert!(report.score > 0);
        assert_eq!(
            report.metadata.cores,
            std::thread::available_parallelism().unwrap().get()
        );

        let dir = std::env::temp_dir().join(format!("c0mpute-bench-test-{}", std::process::id()));
        let path = path_in(&dir);
        save(&report, &path).unwrap();
        let back = load(&path).unwrap().expect("saved report loads");
        assert_eq!(back, report);
        // Quick reports are never advertised.
        assert_eq!(advertised_score(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_is_none_and_score_of_reference_is_1000() {
        assert!(
            load(Path::new("/nonexistent/c0mpute/bench.json"))
                .unwrap()
                .is_none()
        );

        let mut per = BTreeMap::new();
        for (name, reference) in REFERENCE {
            per.insert(
                name.to_string(),
                vec![Sample {
                    params: String::new(),
                    threads: 1,
                    result: SampleResult {
                        duration: "1 us".into(),
                        duration_us: 1,
                        scaled: 1.0,
                        speedup: 1.0,
                        throughput: reference,
                    },
                }],
            );
        }
        let mut results = BTreeMap::new();
        results.insert(RUNTIME.to_string(), per);
        let report = BenchReport {
            metadata: Metadata {
                start_time: String::new(),
                cpu: String::new(),
                cores: 1,
                kernel: String::new(),
                compiler: String::new(),
                version: String::new(),
                quick: false,
                elapsed_ms: 0,
            },
            results,
            score: 0,
        };
        assert_eq!(score(&report), 1000);
        let text = render_text(&report);
        assert!(text.contains("score 0"));
        assert!(text.contains("fib"));
        assert!(text.contains("GFLOP/s"));
    }

    #[test]
    fn rfc3339_has_the_right_shape() {
        let s = rfc3339_now();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.starts_with("20"));
        assert!(s.ends_with('Z'));
    }
}
