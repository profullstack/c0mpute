//! End-to-end: the CrowdGPT governor against real docker, with a stand-in
//! client (no torch, no network) inside the REAL watchdog wrapper.
//!
//! Proves on an actual container: it starts only after the node has been idle
//! for `idle_secs`; a bid preempts it at once (SIGTERM reaches the client,
//! which stops gracefully inside the grace period); it resumes when idle
//! again; `enabled = false` stops it; and the in-container watchdog stops the
//! client when the heartbeat goes stale (a worker that died).
//!
//! Opt in, since it needs docker and builds a small image:
//!   C0MPUTE_CROWDGPT_E2E=1 cargo test -p c0mpute-core --test crowdgpt_e2e -- --nocapture
//! Run it with its own XDG_DATA_HOME (the governor keeps state under the data
//! dir); the test sets one itself before anything reads it.

use std::path::Path;
use std::time::{Duration, Instant};

use c0mpute_core::crowdgpt as cg;

const IMAGE: &str = "c0mpute/crowdgpt-runner:e2e-fake";

fn docker(args: &[&str]) -> std::process::Output {
    std::process::Command::new("docker").args(args).output().expect("docker")
}

fn container_running() -> bool {
    let o = docker(&["ps", "-q", "--filter", &format!("label={}", cg::LABEL)]);
    !String::from_utf8_lossy(&o.stdout).trim().is_empty()
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let t = Instant::now();
    while t.elapsed() < limit {
        if f() {
            return t.elapsed();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("timed out after {limit:?} waiting for {what}");
}

fn build_fake_image(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("c0mpute-crowdgpt-run"), cg::WATCHDOG).unwrap();
    // A client that logs like the real headless one and stops on SIGTERM.
    std::fs::write(
        dir.join("client.py"),
        r#"import signal, sys, time
stop = False
def on_term(*_):
    global stop
    stop = True
signal.signal(signal.SIGTERM, on_term)
print("Round 1", flush=True)
step = 0
while not stop:
    step += 1
    print(f"INFO [train] step {step}/1000 loss={5.0/step:.4f} tps=100 left=10m", flush=True)
    time.sleep(0.5)
print("Stopped by user. Skipping upload.", flush=True)
sys.exit(0)
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("Dockerfile"),
        format!(
            "FROM python:3.11-slim-bookworm\nLABEL {}={}\nCOPY client.py /app/client.py\nCOPY c0mpute-crowdgpt-run /usr/local/bin/c0mpute-crowdgpt-run\nRUN chmod 0755 /usr/local/bin/c0mpute-crowdgpt-run\nRUN ln -sfn /data/checkpoints /app/checkpoints\nWORKDIR /app\nENTRYPOINT [\"/usr/local/bin/c0mpute-crowdgpt-run\"]\n",
            cg::COMMIT_LABEL,
            cg::CROWDGPT_COMMIT
        ),
    )
    .unwrap();
    let o = docker(&["build", "-q", "-t", IMAGE, dir.to_str().unwrap()]);
    assert!(o.status.success(), "build: {}", String::from_utf8_lossy(&o.stderr));
}

fn write_config(path: &Path, enabled: bool) {
    std::fs::write(path, format!("[crowdgpt]\nenabled = {enabled}\nidle_secs = 3\nstop_grace_secs = 8\ncpus = 1.0\nmemory_gb = 1\n")).unwrap();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn governor_starts_on_idle_and_preempts_on_paid_work() {
    if std::env::var("C0MPUTE_CROWDGPT_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set C0MPUTE_CROWDGPT_E2E=1 (needs docker)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: set before any thread reads them; this test binary has one test.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", tmp.path().join("data"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
        std::env::set_var("C0MPUTE_CROWDGPT_IMAGE", IMAGE);
    }
    build_fake_image(&tmp.path().join("img"));

    // Config loads only from the config dir.
    let config = c0mpute_core::config::default_config_path();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    write_config(&config, true);
    let cfg = c0mpute_core::Config::load_or_default(&config).unwrap();
    assert!(cg::blocked_reason(&cfg.crowdgpt).is_none(), "{:?}", cg::blocked_reason(&cfg.crowdgpt));
    cg::spawn_governor(config.clone(), cfg.crowdgpt.clone(), None);

    // 1. Idle for idle_secs (3 s): the client starts and trains.
    let t = wait_for("the client to start", Duration::from_secs(30), container_running);
    eprintln!("started after {t:?} of idleness");
    assert!(t >= Duration::from_secs(2), "must wait for idle_secs first");
    wait_for("training progress in the status", Duration::from_secs(20), || cg::read_status().is_some_and(|s| s.last_loss.is_some()));
    assert!(cg::donating());

    // 2. Paid work arrives (a bid goes out): stopped well inside the grace.
    let bid_at = Instant::now();
    c0mpute_core::activity::note_bid(now_ms() + 6_000);
    let stopped = wait_for("preemption", Duration::from_secs(20), || !container_running());
    eprintln!("preempted {stopped:?} after the bid");
    assert!(stopped < Duration::from_secs(6), "SIGTERM (graceful stop) must beat the 8 s SIGKILL grace: {stopped:?}");
    let _ = bid_at;
    let s = wait_for("preemption recorded", Duration::from_secs(15), || cg::read_status().is_some_and(|s| s.preemptions == 1));
    let _ = s;

    // 3. Idle again (bid expired + idle_secs): it resumes.
    let resumed = wait_for("resume", Duration::from_secs(40), container_running);
    eprintln!("resumed {resumed:?} after preemption");

    // 4. Turned off: stopped within the config reload interval.
    write_config(&config, false);
    wait_for("stop on disable", Duration::from_secs(30), || !container_running());
    wait_for("disabled state", Duration::from_secs(10), || cg::read_status().is_some_and(|s| s.state == "disabled"));

    // 5. Watchdog: a client whose worker vanished (no heartbeat) stops itself.
    let cache = cg::cache_dir();
    let _ = std::fs::remove_file(cache.join("c0mpute-heartbeat"));
    let o = docker(&[
        "run", "-d", "--rm", "--init", "--label", cg::LABEL, "-e", "C0MPUTE_HEARTBEAT_STALE_S=3",
        "-v", &format!("{}:/data", cache.display()), IMAGE,
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let gone = wait_for("watchdog stop", Duration::from_secs(30), || !container_running());
    eprintln!("watchdog stopped an orphaned client after {gone:?}");

    let _ = docker(&["rmi", IMAGE]);
}
