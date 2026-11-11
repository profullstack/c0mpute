//! `c0mpute status --json`: this node as data, for `c0mpute tui` and scripts.
//!
//! The text `c0mpute status` says whether the worker runs; this says, role by
//! role, whether the node is serving it right now and, when it is not, why:
//! the same checks the worker makes before it advertises a role (installed,
//! RAM, load, CPU budget, docker, a GPU, a runtime on PATH), worded for the
//! operator. The roles are the ones the running worker was started with (its
//! command line), or the defaults `worker start` would use.
//!
//! Computed in this process, with this user's config and environment. When
//! the worker runs as another user (a system unit as root), its config and
//! environment can differ; `config_path` says which one was read. A job
//! already running inside the worker is not visible from here.

use c0mpute_core::config::Config;
use c0mpute_proto::Role;
use serde_json::{Value, json};

/// The flags of a running `c0mpute worker start` that change its role set.
#[derive(Debug, Default, PartialEq)]
pub struct WorkerFlags {
    pub roles: Option<Vec<String>>,
    pub no_storage: bool,
    pub no_hosting: bool,
    pub gpu: bool,
    pub private: bool,
}

/// Parse the argv of a worker process; `None` unless it is `… worker start …`.
pub fn worker_flags(argv: &[String]) -> Option<WorkerFlags> {
    let at = argv.windows(2).position(|w| w[0] == "worker" && w[1] == "start")?;
    let mut flags = WorkerFlags::default();
    let mut it = argv[at + 2..].iter();
    while let Some(arg) = it.next() {
        let split = |v: &str| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        match arg.as_str() {
            "--roles" => flags.roles = it.next().map(|v| split(v)),
            s if s.starts_with("--roles=") => flags.roles = Some(split(&s["--roles=".len()..])),
            "--no-storage" => flags.no_storage = true,
            "--no-hosting" => flags.no_hosting = true,
            "--gpu" => flags.gpu = true,
            "--private" => flags.private = true,
            _ => {}
        }
    }
    Some(flags)
}

/// A running worker on this machine: (pid, argv). The PID file first, then a
/// scan of /proc for `c0mpute worker start` (systemd units write no PID file).
pub fn find_worker(pid_file_pid: Option<i32>) -> Option<(i32, Vec<String>)> {
    let argv_of = |pid: i32| -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            raw.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect(),
        )
    };
    if let Some(pid) = pid_file_pid
        && let Some(argv) = argv_of(pid)
        && worker_flags(&argv).is_some()
    {
        return Some((pid, argv));
    }
    let me = std::process::id() as i32;
    let mut found: Vec<(i32, Vec<String>)> = std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != me)
        .filter_map(|pid| {
            let argv = argv_of(pid)?;
            let exe = argv.first()?;
            // An in-place update re-execs from /proc/self/exe, whose name
            // then ends in " (deleted)".
            (exe.trim_end_matches(" (deleted)").ends_with("c0mpute") && worker_flags(&argv).is_some()).then_some((pid, argv))
        })
        .collect();
    found.sort_by_key(|(pid, _)| *pid);
    found.into_iter().next()
}

/// One role: is it serving right now, and if not, why.
#[derive(Debug, PartialEq)]
pub struct RoleState {
    pub role: String,
    pub serving: bool,
    pub reason: Option<String>,
    /// Extra detail when serving (engines, models, GPU or CPU).
    pub detail: Option<String>,
}

fn role_name(role: &Role) -> String {
    serde_json::to_value(role)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{role:?}").to_ascii_lowercase())
}

fn serving(role: &Role, detail: Option<String>) -> RoleState {
    RoleState { role: role_name(role), serving: true, reason: None, detail }
}

fn gated(role: &Role, reason: impl Into<String>) -> RoleState {
    RoleState { role: role_name(role), serving: false, reason: Some(reason.into()), detail: None }
}

fn from_gate(role: &Role, reason: Option<String>, detail: &str) -> RoleState {
    match reason {
        None => serving(role, Some(detail.to_string())),
        Some(r) => gated(role, r),
    }
}

/// The verdict for one role on this machine, now. Blocking probes (nvidia-smi,
/// docker, blender, tesseract) are run off the async threads by the caller.
async fn role_state(role: &Role, cfg: &Config) -> RoleState {
    use c0mpute_core::{captcha, diffusion, embed, gameserver, inference, music, ocr, render, speech, video, video_cpu};
    match role {
        Role::Tts => {
            let engines = speech::available_tts_engines();
            if engines.is_empty() {
                gated(role, "no TTS engine installed yet (Kokoro and Piper install in the background; `c0mpute tts setup --engine all`)")
            } else {
                serving(role, Some(engines.join(", ")))
            }
        }
        Role::Stt => {
            if speech::stt_ready() {
                serving(role, Some("faster-whisper".into()))
            } else {
                gated(role, "speech-to-text model not installed (`c0mpute stt setup`)")
            }
        }
        Role::Diffusion => from_gate(role, diffusion::gate_reason(&cfg.diffusion), "z-image-turbo on CPU"),
        Role::Music => from_gate(role, music::gate_reason(&cfg.music), "ace-step-1.5"),
        Role::Video => {
            video::set_configured_runner(cfg.video.runner);
            let gpu = tokio::task::spawn_blocking(|| video::readiness(video::RUNNER_IMAGE)).await.ok().flatten();
            match gpu {
                None => serving(role, Some("GPU (Wan 2.2)".into())),
                Some(not_ready) => match video_cpu::gate_reason(&cfg.video) {
                    None => serving(role, Some(format!("CPU ({})", video_cpu::MODEL_ID))),
                    Some(cpu) => gated(role, format!("GPU: {}; CPU: {cpu}", not_ready.reason())),
                },
            }
        }
        Role::Ocr => {
            if tokio::task::spawn_blocking(|| ocr::runtime().ready()).await.unwrap_or(false) {
                serving(role, Some("tesseract".into()))
            } else {
                gated(role, "tesseract is not installed (apt install tesseract-ocr poppler-utils)")
            }
        }
        Role::Render => {
            if tokio::task::spawn_blocking(|| render::runtime().ready()).await.unwrap_or(false) {
                serving(role, Some("blender".into()))
            } else {
                gated(role, "blender is not installed (apt install blender, or blender.org)")
            }
        }
        Role::Inference => match inference::runtime_url() {
            None => gated(role, format!("turned off ({}=off)", inference::RUNTIME_URL_ENV)),
            Some(base) => match inference::list_models(&base).await {
                Ok(models) if !models.is_empty() => serving(role, Some(models.join(", "))),
                Ok(_) => gated(role, format!("{base} lists no chat models (pull one into Ollama, or set {})", inference::RUNTIME_URL_ENV)),
                Err(_) => gated(role, format!("no Ollama or llama-server at {base} (start one, or set {})", inference::RUNTIME_URL_ENV)),
            },
        },
        Role::Captcha => {
            if captcha::ready() {
                serving(role, Some("ddddocr".into()))
            } else if !cfg.captcha.auto_install_enabled() {
                gated(role, "turned off (C0MPUTE_CAPTCHA_AUTO=0 or [captcha] auto_install = false)")
            } else {
                gated(role, "not installed yet: the worker installs it in the background; see the worker log")
            }
        }
        Role::Embed => {
            let models = embed::ready_models();
            if !models.is_empty() {
                serving(role, Some(models.iter().map(|m| m.slug).collect::<Vec<_>>().join(", ")))
            } else if !cfg.embed.auto_install_enabled() {
                gated(role, "turned off (C0MPUTE_EMBED_AUTO=0 or [embed] auto_install = false)")
            } else {
                gated(role, "not installed yet: the worker installs it in the background; see the worker log")
            }
        }
        Role::Gameserver => {
            let c = cfg.clone();
            let r = tokio::task::spawn_blocking(move || gameserver::probe(&c)).await.unwrap_or_default();
            if r.ready() {
                serving(role, Some(r.games.iter().map(|g| format!("{g:?}").to_ascii_lowercase()).collect::<Vec<_>>().join(", ")))
            } else if !r.docker {
                gated(role, "docker is not reachable")
            } else if r.host.is_none() {
                gated(role, "no public address (set [gameserver] public_host or [gateway] announce)")
            } else {
                gated(role, "no game image pulled (`c0mpute gameserver setup`)")
            }
        }
        Role::Storage if !cfg.storage_enabled() => gated(role, "turned off (--no-storage, [storage] enabled = false or C0MPUTE_STORAGE=0)"),
        Role::Hosting if !cfg.hosting_enabled() => gated(role, "turned off (--no-hosting, [hosting] enabled = false or C0MPUTE_HOSTING=0)"),
        _ => serving(role, None),
    }
}

/// What `c0mpute status --json` adds to `worker_status_json`: every role the
/// worker runs with (`role_states`), serving or gated and why; the tags it
/// advertises; the worker process found; host load. Keys never collide with
/// the base report, which the desktop app reads.
pub async fn report(cfg: &Config, roles: &[Role], worker: Option<(i32, Vec<String>)>, roles_source: &str) -> Value {
    let mut states = Vec::new();
    for role in roles {
        states.push(role_state(role, cfg).await);
    }
    let mut tagged = cfg.clone();
    tagged.roles = roles.to_vec();
    // Render's probe ran above, so role_tags sees it; inference models are a
    // worker-process cache, so they are added from the probe here instead.
    let mut tags = c0mpute_core::capabilities::role_tags(&tagged);
    if let Some(inf) = states.iter().find(|s| s.role == role_name(&Role::Inference) && s.serving)
        && !tags.iter().any(|t| t == c0mpute_core::inference::ROLE_TAG)
    {
        tags.push(c0mpute_core::inference::ROLE_TAG.to_string());
        tags.extend(inf.detail.iter().flat_map(|d| d.split(", ")).map(c0mpute_core::inference::model_tag));
    }
    let hardware = tokio::task::spawn_blocking(c0mpute_core::capabilities::detect_hardware_tag).await.unwrap_or_default();
    tags.push(hardware.clone());
    let host = c0mpute_core::diffusion::HostNow::probe();
    let private = cfg.private.enabled;
    json!({
        "worker_process": worker.as_ref().map(|(pid, argv)| json!({"pid": pid, "argv": argv})),
        "roles_source": roles_source,
        "hardware": hardware,
        "role_states": states.iter().map(|s| json!({
            "role": s.role,
            "serving": s.serving,
            "reason": s.reason,
            "detail": s.detail,
        })).collect::<Vec<_>>(),
        "tags": if private { Vec::new() } else { tags },
        "host": {
            "cpus": host.cpus,
            "job_threads": host.job_threads,
            "load_1m": (host.load_1m * 100.0).round() / 100.0,
            "ram_total_bytes": host.mem.total,
            "ram_free_bytes": host.mem.available,
        },
        "notes": [
            "role_states are computed by this CLI with its own config and environment; a worker running as another user may differ",
            "a job already running inside the worker is not visible here",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_worker_start_flags() {
        assert_eq!(worker_flags(&argv("/usr/bin/c0mpute worker status")), None);
        assert_eq!(worker_flags(&argv("/root/.c0mpute/bin/c0mpute worker start")), Some(WorkerFlags::default()));
        let f = worker_flags(&argv("c0mpute --config /etc/c.toml worker start --roles tts,stt --no-storage -d")).unwrap();
        assert_eq!(f.roles, Some(vec!["tts".to_string(), "stt".to_string()]));
        assert!(f.no_storage && !f.no_hosting && !f.private);
        let f = worker_flags(&argv("c0mpute worker start --roles=music --private --gpu --no-hosting")).unwrap();
        assert_eq!(f.roles, Some(vec!["music".to_string()]));
        assert!(f.private && f.gpu && f.no_hosting);
    }

    #[test]
    fn role_names_match_the_config_spelling() {
        assert_eq!(role_name(&Role::WorkflowStep), "workflow-step");
        assert_eq!(role_name(&Role::Diffusion), "diffusion");
    }
}
