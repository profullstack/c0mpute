//! How many chat answers this node runs at once: the runtime's real capacity.
//!
//! The worker used to take a fixed two jobs whatever served them. An Ollama
//! started with `OLLAMA_NUM_PARALLEL=1` (its default) then queued the second
//! job behind the first; the queued one hit the generation timeout ("operation
//! timed out") and the buyer's job went to a slow CPU node instead (Vast RTX
//! 3060, 2026-11-18). Now the slot count follows the runtime:
//!
//! 1. an operator override, `C0MPUTE_INFERENCE_CONCURRENCY` or
//!    `[inference] max_concurrent` (1..=[`MAX_SLOTS`]);
//! 2. else what the runtime says it runs in parallel: llama-server's
//!    `/props` `total_slots`; for an Ollama, the `OLLAMA_NUM_PARALLEL` the
//!    worker started it with (house GPU runtime), or, on this box, the one in
//!    the `ollama serve` environment or the `-np` of its loaded runner (and
//!    Ollama's default of 1 when its environment sets none);
//! 3. else 1.
//!
//! On a CPU-only box the runtime's parallelism is not capacity (two answers
//! split the same cores and both finish late), so a local runtime there gets
//! one slot unless the operator overrides it. A runtime on another host is
//! taken at its word. The count is re-read every minute and the worker's
//! slots follow it, so bids (which never exceed free slots) stay honest.

use std::time::Duration;

use serde_json::Value;

use crate::config::InferenceConfig;

/// Upper bound on concurrent answers, whatever a runtime or operator says.
pub const MAX_SLOTS: usize = 16;
/// Operator override of the slot count (`auto`, empty or 0: detect).
pub const CONCURRENCY_ENV: &str = "C0MPUTE_INFERENCE_CONCURRENCY";

/// The decided slot count and why, for the log and `c0mpute status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    pub n: usize,
    pub source: String,
}

/// Parse an override value: a positive count, or None for "detect".
pub fn parse_override(v: &str) -> Option<usize> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "auto" | "0" => None,
        s => s.parse::<usize>().ok().filter(|n| *n > 0).map(|n| n.min(MAX_SLOTS)),
    }
}

/// The operator's override: the env var first, then `[inference] max_concurrent`.
pub fn operator_override(config: &InferenceConfig) -> Option<usize> {
    if let Ok(v) = std::env::var(CONCURRENCY_ENV) {
        return parse_override(&v);
    }
    config.max_concurrent.filter(|n| *n > 0).map(|n| n.min(MAX_SLOTS))
}

/// Combine the facts into a slot count. `runtime` is what the runtime runs in
/// parallel (and how we know); `cpu_only` is a local runtime on a box with no
/// GPU, which gets one slot.
pub fn decide(override_n: Option<usize>, runtime: Option<(usize, String)>, cpu_only: bool) -> Slots {
    if let Some(n) = override_n {
        return Slots { n: n.clamp(1, MAX_SLOTS), source: "operator override".into() };
    }
    match runtime {
        Some((n, how)) if cpu_only && n > 1 => Slots { n: 1, source: format!("CPU-only box: one at a time ({how})") },
        Some((n, how)) => Slots { n: n.clamp(1, MAX_SLOTS), source: how },
        None => Slots { n: 1, source: "runtime parallelism unknown: one at a time".into() },
    }
}

/// `base` points at this machine.
pub fn is_loopback(base: &str) -> bool {
    let host = base.split("://").nth(1).unwrap_or(base);
    let host = host.split('/').next().unwrap_or("");
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0") || host.starts_with("127.")
}

/// The box has an accelerator the runtime uses: a real GPU, or Apple
/// silicon (Metal, unified memory).
pub fn has_accelerator() -> bool {
    crate::gpu::detect().is_some() || cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// The parallel slot count in a runner's argv: `-np N`, `--parallel N` or `--parallel=N`.
pub fn parse_np(args: &[String]) -> Option<usize> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-np" || a == "--parallel" {
            return it.next().and_then(|v| v.parse().ok()).filter(|n| *n > 0);
        }
        if let Some(v) = a.strip_prefix("--parallel=") {
            return v.parse().ok().filter(|n| *n > 0);
        }
    }
    None
}

/// `OLLAMA_NUM_PARALLEL` in a NUL-separated environment block: Some(n) when
/// set to a positive number, Some(1) when unset or 0 (Ollama's default is one
/// at a time), None when the value is unreadable.
pub fn parse_env_parallel(environ: &[u8]) -> Option<usize> {
    for var in environ.split(|b| *b == 0) {
        if let Some(v) = var.strip_prefix(b"OLLAMA_NUM_PARALLEL=") {
            let v = std::str::from_utf8(v).ok()?.trim();
            return match v.parse::<usize>() {
                Ok(0) => Some(1),
                Ok(n) => Some(n),
                Err(_) if v.is_empty() => Some(1),
                Err(_) => None,
            };
        }
    }
    Some(1)
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// What the local Ollama runs in parallel, read from `/proc`: the `ollama
/// serve` environment when readable (same user), else a loaded runner's
/// `-np` (argv is world-readable).
#[cfg(target_os = "linux")]
pub fn local_ollama_parallel() -> Option<(usize, String)> {
    let mut from_env: Option<usize> = None;
    let mut from_runner: Option<usize> = None;
    let Ok(entries) = std::fs::read_dir("/proc") else { return None };
    for e in entries.flatten() {
        if !e.file_name().to_str().is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let Some(argv0) = args.first() else { continue };
        let is_serve = basename(argv0) == "ollama" && args.iter().any(|a| a == "serve");
        let is_runner = argv0.contains("ollama") && (basename(argv0) == "llama-server" || args.iter().any(|a| a == "runner"));
        if is_serve {
            if let Some(n) = std::fs::read(e.path().join("environ")).ok().and_then(|env| parse_env_parallel(&env)) {
                from_env = Some(from_env.map_or(n, |m: usize| m.max(n)));
            }
        } else if is_runner {
            if let Some(n) = parse_np(&args) {
                from_runner = Some(from_runner.map_or(n, |m: usize| m.max(n)));
            }
        }
    }
    from_env
        .map(|n| (n, format!("Ollama OLLAMA_NUM_PARALLEL={n}")))
        .or_else(|| from_runner.map(|n| (n, format!("Ollama runner -np {n}"))))
}

#[cfg(not(target_os = "linux"))]
pub fn local_ollama_parallel() -> Option<(usize, String)> {
    None
}

/// What the runtime at `base` runs in parallel, as far as it can be told.
pub async fn runtime_parallel(base: &str) -> Option<(usize, String)> {
    let base = base.trim_end_matches('/');
    if base == crate::inference_gpu::OLLAMA_URL {
        if let Some(n) = crate::inference_gpu::house_parallel() {
            return Some((n, format!("house Ollama OLLAMA_NUM_PARALLEL={n}")));
        }
    }
    let c = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().ok()?;
    // llama-server (the house CPU runtime, or an operator's).
    if let Ok(r) = c.get(format!("{base}/props")).send().await {
        if r.status().is_success() {
            if let Ok(v) = r.json::<Value>().await {
                if let Some(n) = v.get("total_slots").and_then(Value::as_u64).filter(|n| *n > 0) {
                    return Some((n as usize, format!("llama-server total_slots={n}")));
                }
            }
        }
    }
    // Ollama: its API does not say, so read the process on this box.
    let is_ollama = c.get(format!("{base}/api/version")).send().await.is_ok_and(|r| r.status().is_success());
    if is_ollama && is_loopback(base) {
        return tokio::task::spawn_blocking(local_ollama_parallel).await.ok().flatten();
    }
    None
}

/// The slot count for the runtime at `base` under `config`.
pub async fn detect(base: &str, config: &InferenceConfig) -> Slots {
    let o = operator_override(config);
    if o.is_some() {
        return decide(o, None, false);
    }
    let runtime = runtime_parallel(base).await;
    decide(None, runtime, is_loopback(base) && !has_accelerator())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn overrides() {
        assert_eq!(parse_override("3"), Some(3));
        assert_eq!(parse_override(" auto "), None);
        assert_eq!(parse_override("0"), None);
        assert_eq!(parse_override("x"), None);
        assert_eq!(parse_override("999"), Some(MAX_SLOTS));
    }

    #[test]
    fn decisions() {
        // The override wins over everything, CPU box or not.
        assert_eq!(decide(Some(4), Some((1, "r".into())), true).n, 4);
        // A GPU runtime gets what it runs in parallel.
        assert_eq!(decide(None, Some((2, "Ollama OLLAMA_NUM_PARALLEL=2".into())), false), Slots { n: 2, source: "Ollama OLLAMA_NUM_PARALLEL=2".into() });
        // The Vast 3060 bug: Ollama at its default of 1 must get one slot, not two.
        assert_eq!(decide(None, Some((1, "Ollama OLLAMA_NUM_PARALLEL=1".into())), false).n, 1);
        // CPU-only: one at a time even when the runtime has more slots.
        let cpu = decide(None, Some((4, "llama-server total_slots=4".into())), true);
        assert_eq!(cpu.n, 1);
        assert!(cpu.source.starts_with("CPU-only"));
        assert_eq!(decide(None, None, false).n, 1);
        assert_eq!(decide(None, Some((64, "r".into())), false).n, MAX_SLOTS);
    }

    #[test]
    fn runner_argv() {
        // Ollama 0.40's runner, as seen on the Vast box.
        let ollama = args("/usr/local/lib/ollama/llama-server --model /x --port 33361 --host 127.0.0.1 --no-webui --offline -c 65536 -np 2 --flash-attn on");
        assert_eq!(parse_np(&ollama), Some(2));
        assert_eq!(parse_np(&args("ollama runner --model /x --parallel 4")), Some(4));
        assert_eq!(parse_np(&args("llama-server --parallel=3")), Some(3));
        assert_eq!(parse_np(&args("llama-server -m x")), None);
        assert_eq!(parse_np(&args("llama-server -np")), None);
    }

    #[test]
    fn ollama_environment() {
        assert_eq!(parse_env_parallel(b"HOME=/h\0OLLAMA_NUM_PARALLEL=2\0OLLAMA_HOST=x\0"), Some(2));
        assert_eq!(parse_env_parallel(b"HOME=/h\0OLLAMA_HOST=x\0"), Some(1), "unset: Ollama's default of one");
        assert_eq!(parse_env_parallel(b"OLLAMA_NUM_PARALLEL=0\0"), Some(1));
        assert_eq!(parse_env_parallel(b"OLLAMA_NUM_PARALLEL=lots\0"), None);
    }

    #[test]
    fn loopback() {
        assert!(is_loopback("http://127.0.0.1:11434"));
        assert!(is_loopback("http://localhost:11434"));
        assert!(is_loopback("http://[::1]:11434"));
        assert!(!is_loopback("http://10.0.0.5:11434"));
        assert!(!is_loopback("https://ollama.example.com"));
    }

    /// A llama-server stand-in that reports its slots.
    #[tokio::test]
    async fn reads_llama_server_slots() {
        use axum::{Json, Router, routing::get};
        let app = Router::new().route("/props", get(|| async { Json(serde_json::json!({"total_slots": 3})) }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let base = format!("http://{addr}");
        assert_eq!(runtime_parallel(&base).await.map(|(n, _)| n), Some(3));
        let c = InferenceConfig { max_concurrent: Some(5), ..InferenceConfig::default() };
        if std::env::var(CONCURRENCY_ENV).is_err() {
            assert_eq!(detect(&base, &c).await, Slots { n: 5, source: "operator override".into() });
        }
        // Nothing listening: unknown, one at a time.
        assert_eq!(runtime_parallel("http://127.0.0.1:1").await, None);
    }
}
