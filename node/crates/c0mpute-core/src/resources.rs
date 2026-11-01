//! Per-job compute shares for local workload processes.

use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::OnceLock;
use tokio::process::Command;

const CPU_SHARE: f64 = 0.5;

/// CPU thread budget for one job: half of the CPUs available to this process,
/// rounded up for odd counts, with one thread when the node has one CPU (or
/// less than two CPUs of quota).
pub fn cpu_threads_per_job() -> usize {
    for name in ["C0MPUTE_JOB_CPU_THREADS", "C0MPUTE_SPEECH_CPU_THREADS"] {
        if let Ok(configured) = std::env::var(name) {
            if let Ok(threads) = configured.parse::<usize>() {
                if threads > 0 {
                    return threads;
                }
            }
        }
    }

    ((available_cpu_capacity() * CPU_SHARE).ceil() as usize).max(1)
}

/// CPU capacity visible to this process, including Linux cgroup quotas.
pub fn available_cpu_capacity() -> f64 {
    let affinity = std::thread::available_parallelism()
        .map(|count| count.get() as f64)
        .unwrap_or(1.0);
    cgroup_cpu_quota().map_or(affinity, |quota| affinity.min(quota))
}

/// Permit two CPU-bound jobs at a time when capacity allows it. This keeps
/// two half-node jobs from stacking into an oversubscribed worker.
pub fn max_concurrent_cpu_jobs() -> usize {
    ((available_cpu_capacity() / cpu_threads_per_job() as f64).floor() as usize).clamp(1, 2)
}

/// Limit concurrent CUDA work to one job on a single-GPU node. On a node with
/// multiple GPUs, one or two jobs may run according to the CPU budget.
pub fn max_concurrent_jobs(cuda_workload: bool) -> usize {
    let cpu_jobs = max_concurrent_cpu_jobs();
    if cuda_workload && cuda_device_count() == 1 {
        1
    } else {
        cpu_jobs
    }
}

fn cgroup_cpu_quota() -> Option<f64> {
    let cgroup_root = Path::new("/sys/fs/cgroup");
    let mut v2_paths = vec![cgroup_root.join("cpu.max")];
    if let Ok(cgroups) = std::fs::read_to_string("/proc/self/cgroup") {
        if let Some(relative) = cgroups.lines().find_map(|line| line.strip_prefix("0::")) {
            let relative = relative.trim_start_matches('/');
            if !relative.is_empty() {
                v2_paths.insert(0, cgroup_root.join(relative).join("cpu.max"));
            }
        }
    }
    for path in v2_paths {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let mut fields = contents.split_whitespace();
            if let (Some(quota), Some(period)) = (fields.next(), fields.next()) {
                if quota != "max" {
                    if let (Ok(quota), Ok(period)) = (quota.parse::<f64>(), period.parse::<f64>()) {
                        if quota > 0.0 && period > 0.0 {
                            return Some(quota / period);
                        }
                    }
                }
            }
        }
    }

    for (quota_path, period_path) in [
        (
            "/sys/fs/cgroup/cpu/cpu.cfs_quota_us",
            "/sys/fs/cgroup/cpu/cpu.cfs_period_us",
        ),
        (
            "/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_quota_us",
            "/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_period_us",
        ),
    ] {
        let quota = std::fs::read_to_string(quota_path)
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok());
        let period = std::fs::read_to_string(period_path)
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok());
        if let (Some(quota), Some(period)) = (quota, period) {
            if quota > 0.0 && period > 0.0 {
                return Some(quota / period);
            }
        }
    }
    None
}

/// If multiple CUDA devices are visible, expose half to each child job. A
/// single visible GPU stays fully available, as does a single-GPU node.
pub fn cuda_devices_per_job() -> Option<String> {
    let devices = cuda_devices();
    if devices.len() <= 1 {
        None
    } else {
        Some(devices[..devices.len().div_ceil(2)].join(","))
    }
}

pub fn cuda_device_count() -> usize {
    cuda_devices().len()
}

fn cuda_devices() -> &'static Vec<String> {
    static DEVICES: OnceLock<Vec<String>> = OnceLock::new();
    DEVICES.get_or_init(|| {
        if let Ok(visible) = std::env::var("CUDA_VISIBLE_DEVICES") {
            parse_visible_devices(&visible)
        } else {
            detect_nvidia_devices()
        }
    })
}

fn parse_visible_devices(value: &str) -> Vec<String> {
    if value.trim().is_empty() || value.trim() == "-1" {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|device| !device.is_empty())
        .map(str::to_string)
        .collect()
}

fn detect_nvidia_devices() -> Vec<String> {
    let output = StdCommand::new("nvidia-smi")
        .args(["--query-gpu=uuid", "--format=csv,noheader"])
        .output()
        .ok();
    output
        .filter(|result| result.status.success())
        .map(|result| {
            String::from_utf8_lossy(&result.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Apply the default CPU thread budget and CUDA device visibility to a child
/// inference/training process. Explicit `C0MPUTE_JOB_CPU_THREADS` overrides
/// remain available to node operators.
pub fn apply_job_limits(command: &mut Command) -> usize {
    let threads = cpu_threads_per_job();
    let thread_value = threads.to_string();
    command
        .env("C0MPUTE_JOB_CPU_THREADS", &thread_value)
        .env("C0MPUTE_SPEECH_CPU_THREADS", &thread_value)
        .env("OMP_NUM_THREADS", &thread_value)
        .env("MKL_NUM_THREADS", &thread_value)
        .env("OPENBLAS_NUM_THREADS", &thread_value)
        .env("NUMEXPR_NUM_THREADS", &thread_value)
        .env("RAYON_NUM_THREADS", &thread_value);
    let devices = cuda_devices_per_job();
    if let Some(visible_devices) = &devices {
        command.env("CUDA_VISIBLE_DEVICES", visible_devices);
    }
    tracing::debug!(cpu_threads = threads, cuda_devices = ?devices, "applying per-job compute limits");
    threads
}
