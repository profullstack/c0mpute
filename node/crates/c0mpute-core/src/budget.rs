//! The node's resource budget: how many jobs of each role fit on this box at
//! once, and the reservations that keep concurrent jobs across roles (several
//! buyers, several commands) from overbooking it.
//!
//! The node has totals (logical CPU cores, RAM, the largest GPU's VRAM). Every
//! job of a role reserves that role's [`Cost`] for as long as it runs; a
//! reservation succeeds only while what is left covers it, and is handed back
//! when the job ends ([`Reservation`] is RAII). From the same numbers:
//!
//! * each role's slot count is how many of its jobs fit an idle node
//!   ([`slots_for`], at least 1, at most the role's ceiling), unless the
//!   operator set one (`C0MPUTE_<ROLE>_CONCURRENCY` or `[capacity] <role> = n`);
//! * the dispatch loop bids only while one more job of the role fits what the
//!   *other* roles left ([`Budget::free_count`] via `WorkerLoad::free_slots`),
//!   so a box busy with images stops bidding on songs instead of queueing them.
//!
//! RAM and VRAM are hard limits (exceeding them swaps or OOMs). CPU is soft
//! (the scheduler shares cores), so a role's CPU cost is the cores a job
//! keeps busy on average, not the threads it is allowed. A GPU job that fills
//! the card reserves it whole (`gpu_exclusive`).
//!
//! Inference is the exception for slot counts: the runtime already decides
//! how many answers run at once (`inference_slots`), and its model memory is
//! held by the runtime, not per job, so an answer reserves only a little CPU.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;

/// GiB in MiB (all memory here is MiB).
const GIB: u64 = 1024;

/// What one job of a role holds while it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// CPU, in thousandths of a logical core (1000 = one core busy).
    pub cpu_milli: u64,
    pub ram_mib: u64,
    pub vram_mib: u64,
    /// The job fills the GPU: it runs alone on the card.
    pub gpu_exclusive: bool,
}

impl Cost {
    pub const fn cpu(cores_milli: u64, ram_mib: u64) -> Self {
        Self { cpu_milli: cores_milli, ram_mib, vram_mib: 0, gpu_exclusive: false }
    }

    pub const fn gpu(cores_milli: u64, ram_mib: u64, vram_mib: u64) -> Self {
        Self { cpu_milli: cores_milli, ram_mib, vram_mib, gpu_exclusive: false }
    }

    pub const fn gpu_exclusive(cores_milli: u64, ram_mib: u64) -> Self {
        Self { cpu_milli: cores_milli, ram_mib, vram_mib: 0, gpu_exclusive: true }
    }
}

impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cpu={:.2} ram={}MiB", self.cpu_milli as f64 / 1000.0, self.ram_mib)?;
        if self.gpu_exclusive {
            write!(f, " gpu=exclusive")
        } else if self.vram_mib > 0 {
            write!(f, " vram={}MiB", self.vram_mib)
        } else {
            Ok(())
        }
    }
}

/// The whole node, as the budget sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub cpu_milli: u64,
    /// RAM jobs may use: the box's total less what the OS and the worker keep.
    pub ram_mib: u64,
    /// The largest GPU's memory; 0 = no usable GPU.
    pub vram_mib: u64,
}

impl Totals {
    /// From raw hardware facts: `cpus` logical cores, `ram_total_mib` of RAM
    /// (of which [`ram_reserve_mib`] stays out of the budget), `vram_mib`.
    pub fn from_hardware(cpus: f64, ram_total_mib: u64, vram_mib: u64) -> Self {
        Self {
            cpu_milli: ((cpus.max(1.0)) * 1000.0) as u64,
            ram_mib: ram_total_mib.saturating_sub(ram_reserve_mib(ram_total_mib)).max(256),
            vram_mib,
        }
    }

    /// `cost` as this node can actually reserve it: never more than the whole
    /// node in any dimension (so one job always fits an idle node: floor 1),
    /// no GPU part on a box without one, and an exclusive GPU job taking the
    /// whole card.
    pub fn clamp(&self, cost: Cost) -> Cost {
        let has_gpu = self.vram_mib > 0;
        let exclusive = cost.gpu_exclusive && has_gpu;
        Cost {
            cpu_milli: cost.cpu_milli.min(self.cpu_milli),
            ram_mib: cost.ram_mib.min(self.ram_mib),
            vram_mib: if exclusive { self.vram_mib } else if has_gpu { cost.vram_mib.min(self.vram_mib) } else { 0 },
            gpu_exclusive: exclusive,
        }
    }

    /// How many jobs of `cost` fit in `left` (already clamped). Unbounded
    /// (`usize::MAX`) for a cost of nothing.
    fn count_in(cost: Cost, left: Totals) -> usize {
        let per = |have: u64, need: u64| if need == 0 { usize::MAX } else { (have / need) as usize };
        per(left.cpu_milli, cost.cpu_milli)
            .min(per(left.ram_mib, cost.ram_mib))
            .min(per(left.vram_mib, cost.vram_mib))
    }
}

/// RAM kept out of the budget for the OS, the worker itself and page cache:
/// 15% of the box, at least 2 GiB, never more than half.
pub fn ram_reserve_mib(total_mib: u64) -> u64 {
    (total_mib * 15 / 100).max(2 * GIB).min(total_mib / 2)
}

/// A role's place in the budget: what a job costs, and the most jobs at
/// once however big the box (light roles would otherwise scale to hundreds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub cost: Cost,
    pub ceiling: usize,
}

/// Facts about the box that change what a role costs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Facts {
    /// Threads one CPU-bound job is given (`resources::cpu_threads_per_job`).
    pub job_threads: usize,
    /// An NVIDIA GPU (video and music run on it when present).
    pub nvidia: bool,
    /// `[containers] max_cpus` / `max_memory_mb` (0 = default).
    pub container_cpus: f64,
    pub container_memory_mib: u64,
}

impl Facts {
    pub fn detect(config: &crate::config::Config) -> Self {
        Self {
            job_threads: crate::resources::cpu_threads_per_job(),
            nvidia: crate::gpu::detect().is_some_and(|g| g.kind == crate::gpu::GpuKind::Nvidia),
            container_cpus: config.containers.max_cpus,
            container_memory_mib: config.containers.max_memory_mb,
        }
    }
}

/// Short role name for a workload: the key in `[capacity]` and in
/// `C0MPUTE_<ROLE>_CONCURRENCY`.
pub fn role_key(workload: &str) -> &'static str {
    match workload {
        "ffmpeg.transcode" => "transcode",
        "tts.synthesize" => "tts",
        "stt.transcribe" => "stt",
        crate::workflow::WORKLOAD => "workflow",
        crate::crawl::WORKLOAD => "crawl",
        crate::video::WORKLOAD_TXT2VID | crate::video::WORKLOAD_IMG2VID => "video",
        crate::diffusion::WORKLOAD_TXT2IMG => "diffusion",
        crate::music::WORKLOAD_TXT2MUSIC => "music",
        crate::ocr::WORKLOAD => "ocr",
        crate::gameserver::WORKLOAD_START | crate::gameserver::WORKLOAD_STOP => "gameserver",
        crate::embed::WORKLOAD => "embed",
        crate::captcha::WORKLOAD => "captcha",
        crate::inference::WORKLOAD => "inference",
        crate::render::WORKLOAD => "render",
        crate::container_run::WORKLOAD => "containers",
        crate::webhook::WORKLOAD => "webhook",
        _ => "other",
    }
}

/// Every role key, for docs and the startup log.
pub const ROLE_KEYS: &[&str] = &[
    "transcode", "tts", "stt", "workflow", "crawl", "video", "diffusion", "music", "ocr", "gameserver",
    "embed", "captcha", "inference", "render", "containers", "webhook",
];

/// Estimated cost and ceiling per role. These are estimates, measured on
/// fleet boxes where noted; the live gates (free RAM, load average) in each
/// role still refuse an offer the estimate let through.
pub fn profile(workload: &str, facts: &Facts) -> Profile {
    // A CPU-bound job keeps the threads it is given busy.
    let all_threads = facts.job_threads.max(1) as u64 * 1000;
    let p = |cost, ceiling| Profile { cost, ceiling };
    match role_key(workload) {
        // ffmpeg: ~4 cores busy on average (x264 stops scaling well past
        // that at our presets), ~1 GiB with buffers.
        "transcode" => p(Cost::cpu(all_threads.min(4000), GIB), 8),
        // Kokoro: one sentence at a time, ~1 core, ~1 GiB process.
        "tts" => p(Cost::cpu(1000, GIB), 8),
        // whisper.cpp small/base: ~2 cores, ~2 GiB.
        "stt" => p(Cost::cpu(2000, 2 * GIB), 8),
        // Light I/O work: an HTTP fetch, a webhook, a workflow step.
        "workflow" => p(Cost::cpu(250, 128), 64),
        "crawl" => p(Cost::cpu(250, 128), 32),
        "webhook" => p(Cost::cpu(100, 32), 64),
        // gameserver.start/stop are docker control calls; the game server
        // itself is accounted by the gameserver role's own port/memory caps.
        "gameserver" => p(Cost::cpu(250, 128), 8),
        // GPU video (Wan/LTX in docker) fills the card; CPU video (Wan 2.1
        // 1.3B on sd-cli) holds ~8-10 GiB and every thread it is given.
        "video" if facts.nvidia => p(Cost::gpu_exclusive(2000, 8 * GIB), 2),
        "video" => p(Cost::cpu(all_threads, 10 * GIB), 4),
        // sd-cli (SD 1.5/Turbo on CPU): ~4-9 GiB and every thread it is given.
        "diffusion" => p(Cost::cpu(all_threads, 9 * GIB), 4),
        // ACE-Step 1.5: on a GPU ~8 GiB VRAM and ~4 GiB host RAM; on CPU the
        // whole model in RAM (~7-9 GiB) and every thread.
        "music" if facts.nvidia => p(Cost::gpu(2000, 4 * GIB, 8 * GIB), 4),
        "music" => p(Cost::cpu(all_threads, 9 * GIB), 4),
        // Tesseract: a page at a time, ~1 core, ~0.5 GiB.
        "ocr" => p(Cost::cpu(1000, 512), 16),
        // fastembed (small ONNX model): ~1 core, ~1 GiB.
        "embed" => p(Cost::cpu(1000, GIB), 16),
        // ddddocr: tiny model, short bursts.
        "captcha" => p(Cost::cpu(500, 256), 32),
        // The runtime holds the model; an answer adds a request's worth.
        "inference" => p(Cost::cpu(250, 64), crate::inference_slots::MAX_SLOTS),
        // Blender Cycles on CPU: every thread, a few GiB per scene.
        "render" => p(Cost::cpu(all_threads, 4 * GIB), 4),
        // A container job may use up to [containers] max_cpus / max_memory_mb
        // (0 = half the machine / 4 GiB).
        "containers" => {
            let cpus = if facts.container_cpus > 0.0 { (facts.container_cpus * 1000.0) as u64 } else { all_threads };
            let mem = if facts.container_memory_mib > 0 { facts.container_memory_mib } else { 4 * GIB };
            p(Cost::cpu(cpus, mem), 4)
        }
        _ => p(Cost::cpu(1000, 512), 4),
    }
}

/// Slots for a role on a node with `totals`: the operator's number when set
/// (at least 1), else how many of its jobs fit an idle node, at least 1 and
/// at most the role's ceiling. Also returns the cost each job reserves: with
/// an override, scaled down so that many fit (the operator knows the box).
pub fn slots_for(totals: &Totals, profile: Profile, override_n: Option<usize>) -> (usize, Cost) {
    let cost = totals.clamp(profile.cost);
    match override_n {
        Some(n) => {
            let n = n.max(1);
            (n, scaled_to_fit(totals, cost, n))
        }
        None => (Totals::count_in(cost, *totals).clamp(1, profile.ceiling.max(1)), cost),
    }
}

/// `cost` shrunk (per dimension) so `n` jobs fit `totals`; an exclusive GPU
/// job shares the card when the operator asked for more than one.
fn scaled_to_fit(totals: &Totals, cost: Cost, n: usize) -> Cost {
    let n64 = n as u64;
    if n <= 1 {
        return cost;
    }
    Cost {
        cpu_milli: cost.cpu_milli.min(totals.cpu_milli / n64),
        ram_mib: cost.ram_mib.min(totals.ram_mib / n64),
        vram_mib: cost.vram_mib.min(totals.vram_mib / n64),
        gpu_exclusive: false,
    }
}

/// Operator overrides: `C0MPUTE_<ROLE>_CONCURRENCY` (wins), then
/// `[capacity] <role> = n` in config.toml. 0 or unparsable = not set.
pub fn override_for(role: &str, config: &BTreeMap<String, usize>) -> Option<usize> {
    override_from(role, config, |k| std::env::var(k).ok())
}

pub fn override_from(role: &str, config: &BTreeMap<String, usize>, env: impl Fn(&str) -> Option<String>) -> Option<usize> {
    let key = format!("C0MPUTE_{}_CONCURRENCY", role.to_ascii_uppercase());
    env(&key)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .or_else(|| config.get(role).copied().filter(|n| *n > 0))
}

#[derive(Debug, Default)]
struct Used {
    cpu_milli: u64,
    ram_mib: u64,
    vram_mib: u64,
    jobs: usize,
}

/// The node's budget and what running jobs hold of it.
#[derive(Debug)]
pub struct Budget {
    totals: Totals,
    used: Mutex<Used>,
    released: Notify,
}

impl Budget {
    pub fn new(totals: Totals) -> Arc<Self> {
        Arc::new(Self { totals, used: Mutex::new(Used::default()), released: Notify::new() })
    }

    pub fn totals(&self) -> Totals {
        self.totals
    }

    fn left(&self, used: &Used) -> Totals {
        Totals {
            cpu_milli: self.totals.cpu_milli.saturating_sub(used.cpu_milli),
            ram_mib: self.totals.ram_mib.saturating_sub(used.ram_mib),
            vram_mib: self.totals.vram_mib.saturating_sub(used.vram_mib),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Used> {
        self.used.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What is not reserved right now.
    pub fn remaining(&self) -> Totals {
        self.left(&self.lock())
    }

    /// How many more jobs of `cost` fit what is left right now.
    pub fn free_count(&self, cost: Cost) -> usize {
        let cost = self.totals.clamp(cost);
        Totals::count_in(cost, self.left(&self.lock()))
    }

    pub fn fits(&self, cost: Cost) -> bool {
        self.free_count(cost) > 0
    }

    /// Jobs holding a reservation now.
    pub fn jobs(&self) -> usize {
        self.lock().jobs
    }

    /// Share of the busiest dimension in use, 0.0 .. 1.0.
    pub fn utilization(&self) -> f32 {
        let used = self.lock();
        let frac = |u: u64, t: u64| if t == 0 { 0.0 } else { u as f32 / t as f32 };
        frac(used.cpu_milli, self.totals.cpu_milli)
            .max(frac(used.ram_mib, self.totals.ram_mib))
            .max(frac(used.vram_mib, self.totals.vram_mib))
            .clamp(0.0, 1.0)
    }

    /// Reserve `cost` now if it fits.
    pub fn try_reserve(self: &Arc<Self>, cost: Cost) -> Option<Reservation> {
        let cost = self.totals.clamp(cost);
        let mut used = self.lock();
        if Totals::count_in(cost, self.left(&used)) == 0 {
            return None;
        }
        used.cpu_milli += cost.cpu_milli;
        used.ram_mib += cost.ram_mib;
        used.vram_mib += cost.vram_mib;
        used.jobs += 1;
        Some(Reservation { budget: self.clone(), cost })
    }

    /// Reserve `cost`, waiting for running jobs to hand back enough. A cost
    /// bigger than the node is clamped to it, so this always ends once the
    /// node is idle.
    pub async fn reserve(self: &Arc<Self>, cost: Cost) -> Reservation {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Some(r) = self.try_reserve(cost) {
                return r;
            }
            released.await;
        }
    }
}

/// A job's hold on the budget; handed back on drop.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<Budget>,
    cost: Cost,
}

impl Reservation {
    pub fn cost(&self) -> Cost {
        self.cost
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        {
            let mut used = self.budget.lock();
            used.cpu_milli = used.cpu_milli.saturating_sub(self.cost.cpu_milli);
            used.ram_mib = used.ram_mib.saturating_sub(self.cost.ram_mib);
            used.vram_mib = used.vram_mib.saturating_sub(self.cost.vram_mib);
            used.jobs = used.jobs.saturating_sub(1);
        }
        self.budget.released.notify_waiters();
    }
}

/// This node's totals, from the hardware (`C0MPUTE_BUDGET_CPUS`,
/// `C0MPUTE_BUDGET_RAM_MIB` and `C0MPUTE_BUDGET_VRAM_MIB` override each).
pub fn detect_totals() -> Totals {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0);
    let cpus = env("C0MPUTE_BUDGET_CPUS").unwrap_or_else(crate::resources::available_cpu_capacity);
    let ram_total_mib = crate::diffusion::memory().total / (1024 * 1024);
    let vram_mib = crate::gpu::detect().map(|g| g.vram_mib).unwrap_or(0);
    let mut totals = Totals::from_hardware(cpus, ram_total_mib, vram_mib);
    if let Some(ram) = env("C0MPUTE_BUDGET_RAM_MIB") {
        totals.ram_mib = ram as u64;
    }
    if let Some(vram) = env("C0MPUTE_BUDGET_VRAM_MIB") {
        totals.vram_mib = vram as u64;
    }
    totals
}

/// The node-wide budget every role reserves from.
pub fn node() -> Arc<Budget> {
    static NODE: OnceLock<Arc<Budget>> = OnceLock::new();
    NODE.get_or_init(|| Budget::new(detect_totals())).clone()
}

static OVERRIDES: OnceLock<BTreeMap<String, usize>> = OnceLock::new();
static FACTS: OnceLock<Facts> = OnceLock::new();

/// Take the operator's `[capacity]` overrides and the box's facts from the
/// config (once, at worker start).
pub fn configure(config: &crate::config::Config) {
    let mut overrides = config.capacity.clone();
    // `[containers] max_jobs` predates `[capacity]`; 1 was its default and is
    // in many saved configs, so only a larger number is an operator's choice.
    if config.containers.max_jobs > 1 {
        overrides.entry("containers".to_string()).or_insert(config.containers.max_jobs);
    }
    let _ = OVERRIDES.set(overrides);
    let _ = FACTS.set(Facts::detect(config));
}

fn facts() -> Facts {
    *FACTS.get_or_init(|| Facts::detect(&crate::config::Config::default()))
}

/// Slots and per-job cost for `workload` on this node, with the operator's
/// override applied.
pub fn for_workload(workload: &str) -> (usize, Cost) {
    let role = role_key(workload);
    let empty = BTreeMap::new();
    let overrides = OVERRIDES.get().unwrap_or(&empty);
    slots_for(&node().totals(), profile(workload, &facts()), override_for(role, overrides))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_box() -> (Totals, Facts) {
        // 16 cores, 64 GiB, no GPU.
        (
            Totals::from_hardware(16.0, 64 * GIB, 0),
            Facts { job_threads: 8, nvidia: false, container_cpus: 0.0, container_memory_mib: 0 },
        )
    }

    fn gpu_box() -> (Totals, Facts) {
        // RTX 3060 12 GB, 8 cores, 32 GiB.
        (
            Totals::from_hardware(8.0, 32 * GIB, 12 * GIB),
            Facts { job_threads: 4, nvidia: true, container_cpus: 0.0, container_memory_mib: 0 },
        )
    }

    fn small_box() -> (Totals, Facts) {
        // 4 cores, 8 GiB.
        (
            Totals::from_hardware(4.0, 8 * GIB, 0),
            Facts { job_threads: 2, nvidia: false, container_cpus: 0.0, container_memory_mib: 0 },
        )
    }

    fn slots(b: &(Totals, Facts), workload: &str) -> usize {
        slots_for(&b.0, profile(workload, &b.1), None).0
    }

    #[test]
    fn ram_reserve_keeps_room_for_the_os() {
        assert_eq!(ram_reserve_mib(64 * GIB), 64 * GIB * 15 / 100);
        assert_eq!(ram_reserve_mib(8 * GIB), 2 * GIB);
        assert_eq!(ram_reserve_mib(2 * GIB), GIB);
    }

    #[test]
    fn fits_until_spent_then_release_frees() {
        let b = Budget::new(Totals { cpu_milli: 8000, ram_mib: 16 * GIB, vram_mib: 0 });
        let job = Cost::cpu(4000, 6 * GIB);
        assert_eq!(b.free_count(job), 2);
        let a = b.try_reserve(job).expect("first fits");
        let c = b.try_reserve(job).expect("second fits");
        assert!(b.try_reserve(job).is_none(), "third does not fit");
        assert_eq!(b.jobs(), 2);
        assert!((b.utilization() - 1.0).abs() < f32::EPSILON);
        drop(a);
        assert_eq!(b.free_count(job), 1);
        drop(c);
        assert_eq!(b.remaining(), b.totals());
        assert_eq!(b.jobs(), 0);
    }

    #[test]
    fn roles_share_one_budget() {
        let b = Budget::new(Totals { cpu_milli: 16000, ram_mib: 54 * GIB, vram_mib: 0 });
        let image = Cost::cpu(8000, 9 * GIB);
        let tts = Cost::cpu(1000, GIB);
        let _i1 = b.try_reserve(image).unwrap();
        assert_eq!(b.free_count(image), 1);
        let _many: Vec<_> = (0..5).map(|_| b.try_reserve(tts).unwrap()).collect();
        // 5 TTS lines took 5 of the 8 cores left: no room for a second image.
        assert_eq!(b.free_count(image), 0);
        assert_eq!(b.free_count(tts), 3);
    }

    #[test]
    fn a_job_bigger_than_the_node_still_runs_alone() {
        let b = Budget::new(Totals { cpu_milli: 4000, ram_mib: 6 * GIB, vram_mib: 0 });
        let huge = Cost::cpu(8000, 9 * GIB);
        assert_eq!(b.free_count(huge), 1);
        let r = b.try_reserve(huge).unwrap();
        assert_eq!(r.cost(), Cost::cpu(4000, 6 * GIB));
        assert!(b.try_reserve(Cost::cpu(1, 1)).is_none());
    }

    #[test]
    fn gpu_exclusive_takes_the_whole_card() {
        let b = Budget::new(Totals { cpu_milli: 8000, ram_mib: 27 * GIB, vram_mib: 12 * GIB });
        let video = Cost::gpu_exclusive(2000, 8 * GIB);
        let song = Cost::gpu(2000, 4 * GIB, 8 * GIB);
        let cpu = Cost::cpu(1000, GIB);
        let v = b.try_reserve(video).unwrap();
        assert_eq!(v.cost().vram_mib, 12 * GIB);
        assert!(b.try_reserve(video).is_none());
        assert!(b.try_reserve(song).is_none(), "no GPU job beside an exclusive one");
        assert!(b.try_reserve(cpu).is_some(), "CPU work still fits");
        drop(v);
        let s = b.try_reserve(song).unwrap();
        assert!(b.try_reserve(video).is_none(), "exclusive waits for the card to empty");
        drop(s);
        assert!(b.fits(video));
    }

    #[test]
    fn gpu_cost_on_a_box_without_one_is_cpu_only() {
        let t = Totals { cpu_milli: 4000, ram_mib: 6 * GIB, vram_mib: 0 };
        let c = t.clamp(Cost::gpu_exclusive(2000, GIB));
        assert_eq!(c, Cost::cpu(2000, GIB));
    }

    #[tokio::test]
    async fn reserve_waits_for_a_release() {
        let b = Budget::new(Totals { cpu_milli: 2000, ram_mib: GIB, vram_mib: 0 });
        let job = Cost::cpu(2000, 512);
        let held = b.try_reserve(job).unwrap();
        let b2 = b.clone();
        let waiter = tokio::spawn(async move { b2.reserve(job).await.cost() });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!waiter.is_finished());
        drop(held);
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await.unwrap().unwrap();
        assert_eq!(got, job);
    }

    #[test]
    fn overrides_env_beats_config_and_zero_is_unset() {
        let mut cfg = BTreeMap::new();
        cfg.insert("diffusion".to_string(), 3);
        cfg.insert("music".to_string(), 0);
        assert_eq!(override_from("diffusion", &cfg, |_| None), Some(3));
        assert_eq!(
            override_from("diffusion", &cfg, |k| (k == "C0MPUTE_DIFFUSION_CONCURRENCY").then(|| "5".into())),
            Some(5)
        );
        assert_eq!(override_from("music", &cfg, |_| None), None);
        assert_eq!(override_from("music", &cfg, |_| Some("0".into())), None);
        assert_eq!(override_from("tts", &cfg, |_| Some("junk".into())), None);
    }

    #[test]
    fn override_sets_slots_and_scales_cost_to_fit() {
        let (t, f) = small_box();
        let (n, cost) = slots_for(&t, profile(crate::diffusion::WORKLOAD_TXT2IMG, &f), Some(3));
        assert_eq!(n, 3);
        let b = Budget::new(t);
        let held: Vec<_> = (0..3).map(|_| b.try_reserve(cost).expect("override's jobs fit")).collect();
        assert_eq!(held.len(), 3);
        // Floor 1.
        assert_eq!(slots_for(&t, profile("tts.synthesize", &f), Some(0)).0, 1);
        // An overridden exclusive GPU role shares the card.
        let (g, gf) = gpu_box();
        let (n, cost) = slots_for(&g, profile(crate::video::WORKLOAD_TXT2VID, &gf), Some(2));
        assert_eq!(n, 2);
        assert!(!cost.gpu_exclusive);
        assert_eq!(cost.vram_mib, 6 * GIB);
    }

    #[test]
    fn slot_derivation_16_core_64_gb_cpu_box() {
        let b = cpu_box();
        assert_eq!(slots(&b, "ffmpeg.transcode"), 4);
        assert_eq!(slots(&b, "tts.synthesize"), 8);
        assert_eq!(slots(&b, "stt.transcribe"), 8);
        assert_eq!(slots(&b, crate::diffusion::WORKLOAD_TXT2IMG), 2);
        assert_eq!(slots(&b, crate::music::WORKLOAD_TXT2MUSIC), 2);
        assert_eq!(slots(&b, crate::video::WORKLOAD_TXT2VID), 2);
        assert_eq!(slots(&b, crate::ocr::WORKLOAD), 16);
        assert_eq!(slots(&b, crate::embed::WORKLOAD), 16);
        assert_eq!(slots(&b, crate::captcha::WORKLOAD), 32);
        assert_eq!(slots(&b, crate::render::WORKLOAD), 2);
        assert_eq!(slots(&b, crate::container_run::WORKLOAD), 2);
        assert_eq!(slots(&b, crate::workflow::WORKLOAD), 64);
        assert_eq!(slots(&b, crate::crawl::WORKLOAD), 32);
        assert_eq!(slots(&b, crate::gameserver::WORKLOAD_START), 8);
    }

    #[test]
    fn slot_derivation_rtx_3060_8_core_32_gb() {
        let b = gpu_box();
        assert_eq!(slots(&b, "ffmpeg.transcode"), 2);
        assert_eq!(slots(&b, "tts.synthesize"), 8);
        assert_eq!(slots(&b, "stt.transcribe"), 4);
        assert_eq!(slots(&b, crate::diffusion::WORKLOAD_TXT2IMG), 2);
        // ACE-Step on the card: 8 GiB of 12 -> one song at a time.
        assert_eq!(slots(&b, crate::music::WORKLOAD_TXT2MUSIC), 1);
        // GPU video fills the card.
        assert_eq!(slots(&b, crate::video::WORKLOAD_TXT2VID), 1);
        assert_eq!(slots(&b, crate::ocr::WORKLOAD), 8);
        assert_eq!(slots(&b, crate::embed::WORKLOAD), 8);
        assert_eq!(slots(&b, crate::captcha::WORKLOAD), 16);
        assert_eq!(slots(&b, crate::render::WORKLOAD), 2);
        assert_eq!(slots(&b, crate::container_run::WORKLOAD), 2);
        assert_eq!(slots(&b, crate::workflow::WORKLOAD), 32);
        assert_eq!(slots(&b, crate::crawl::WORKLOAD), 32);
        assert_eq!(slots(&b, crate::gameserver::WORKLOAD_START), 8);
    }

    #[test]
    fn slot_derivation_4_core_8_gb_small_box() {
        let b = small_box();
        assert_eq!(b.0.ram_mib, 6 * GIB);
        assert_eq!(slots(&b, "ffmpeg.transcode"), 2);
        assert_eq!(slots(&b, "tts.synthesize"), 4);
        assert_eq!(slots(&b, "stt.transcribe"), 2);
        assert_eq!(slots(&b, crate::diffusion::WORKLOAD_TXT2IMG), 1);
        assert_eq!(slots(&b, crate::music::WORKLOAD_TXT2MUSIC), 1);
        assert_eq!(slots(&b, crate::video::WORKLOAD_TXT2VID), 1);
        assert_eq!(slots(&b, crate::ocr::WORKLOAD), 4);
        assert_eq!(slots(&b, crate::embed::WORKLOAD), 4);
        assert_eq!(slots(&b, crate::captcha::WORKLOAD), 8);
        assert_eq!(slots(&b, crate::render::WORKLOAD), 1);
        assert_eq!(slots(&b, crate::container_run::WORKLOAD), 1);
        assert_eq!(slots(&b, crate::workflow::WORKLOAD), 16);
        assert_eq!(slots(&b, crate::crawl::WORKLOAD), 16);
        assert_eq!(slots(&b, crate::gameserver::WORKLOAD_START), 8);
    }
}
