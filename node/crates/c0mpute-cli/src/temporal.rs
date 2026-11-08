//! `c0mpute infernet temporal serve|train|predict|models`: Infernet's
//! temporal policies (`infernet.temporal.train`, `infernet.temporal.infer`)
//! over c0mpute's job/receipt protocol. Everything else under
//! `c0mpute infernet` still goes to the infernet binary.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use c0mpute_core::temporal::caps::Requirements;
use c0mpute_core::temporal::client::{self, OpenRequest, SessionFile, TrainRequest};
use c0mpute_core::temporal::node::{self, NetOptions, ServeOptions};
use c0mpute_core::temporal::worker::ModelStore;
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(name = "c0mpute infernet temporal", about = "Temporal policies (CfC/LTC liquid networks, GRU baseline) on c0mpute workers")]
struct TemporalCli {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Args, Debug, Clone)]
struct NetArgs {
    /// Extra bootstrap peer (multiaddr). Repeatable.
    #[arg(long = "bootstrap")]
    bootstrap: Vec<String>,
    /// Don't dial c0mpute.com's seed nodes (isolated / LAN-only runs).
    #[arg(long)]
    no_default_bootstrap: bool,
    /// libp2p listen multiaddr (default /ip4/0.0.0.0/tcp/0).
    #[arg(long)]
    listen: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Sub {
    /// Serve inference for models and/or accept training jobs on this node.
    Serve {
        /// Model dir or sha256:<hash> from the local store. Repeatable.
        #[arg(long)]
        model: Vec<String>,
        /// Also accept infernet.temporal.train jobs (one at a time).
        #[arg(long)]
        train: bool,
        #[arg(long, default_value_t = 64)]
        max_sessions: u32,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Train on a remote worker: one single-node job per family.
    Train {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "cfc")]
        family: String,
        /// Baseline families on the same splits (comma separated), e.g. gru.
        #[arg(long, value_delimiter = ',')]
        baseline: Vec<String>,
        /// Only accept bids from these worker peer ids (trusted workers).
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 1.0)]
        max_price: f64,
        #[arg(long)]
        gpu: bool,
        #[arg(long)]
        min_ram_gb: Option<u64>,
        #[arg(long)]
        runtime_version: Option<String>,
        /// How the worker reaches this machine's transfer server (use https off-LAN).
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value_t = 3600)]
        timeout_s: u64,
        #[arg(long, default_value_t = 1)]
        retries: u32,
        #[arg(long)]
        epochs: Option<u64>,
        #[arg(long)]
        seed: Option<u64>,
        #[arg(long)]
        hidden: Option<u64>,
        #[arg(long, default_value_t = 8)]
        bid_window_s: u64,
        /// Consent: send the redacted dataset to the winning worker.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Send one observed step to a session (opened on first use, sticky to its worker).
    Predict {
        #[arg(long)]
        session: String,
        /// Step JSON: a PredictRequestV1 or {observation, dt_ms?, prev_action?, task?}.
        #[arg(long)]
        input: Option<PathBuf>,
        /// Model hash; needed to open a new session.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 600)]
        ttl: u64,
        #[arg(long, default_value = "default")]
        scope: String,
        #[arg(long, default_value_t = 0.05)]
        max_price: f64,
        #[arg(long)]
        reset: bool,
        #[arg(long)]
        close: bool,
        #[arg(long, default_value_t = 4)]
        bid_window_s: u64,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Local model store: list, or import a trained model dir.
    Models {
        #[arg(long)]
        import: Option<PathBuf>,
    },
}

fn data_dir() -> PathBuf {
    c0mpute_core::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("temporal")
}

fn infernet_bin() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("C0MPUTE_INFERNET_BIN") {
        return Ok(PathBuf::from(p));
    }
    crate::which_on_path("infernet").context("infernet is not on PATH (install it: c0mpute plugin install infernet)")
}

fn net_opts(a: &NetArgs, identity: &str) -> NetOptions {
    NetOptions { identity_dir: data_dir().join(identity), listen: a.listen.clone(), bootstrap: a.bootstrap.clone(), default_bootstrap: !a.no_default_bootstrap }
}

pub async fn run(args: &[String]) -> Result<()> {
    let cli = TemporalCli::try_parse_from(std::iter::once("c0mpute infernet temporal".to_string()).chain(args.iter().cloned())).unwrap_or_else(|e| e.exit());
    let store = ModelStore::new(data_dir().join("models"));
    match cli.cmd {
        Sub::Models { import } => {
            if let Some(dir) = import {
                println!("{}", store.import(&dir)?);
                return Ok(());
            }
            if let Ok(rd) = std::fs::read_dir(&store.root) {
                for e in rd.flatten() {
                    let m: Value = std::fs::read(e.path().join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
                    println!("sha256:{}  {}  {}", e.file_name().to_string_lossy(), m["architecture"]["family"].as_str().unwrap_or("?"), m["created_at"].as_str().unwrap_or(""));
                }
            }
            Ok(())
        }
        Sub::Serve { model, train, max_sessions, net } => {
            let o = ServeOptions { net: net_opts(&net, "serve"), infernet: infernet_bin()?, models: model, train, max_sessions, store, cache_root: data_dir().join("cache") };
            let s = node::serve(&o).await?;
            eprintln!("serving as {}  (runtime {}, cuda {})", s.net.peer_id().to_base58(), s.caps.runtime.version, s.caps.devices.cuda);
            for a in s.net.listen_addrs().await.unwrap_or_default() {
                eprintln!("  listening {a}/p2p/{}", s.net.peer_id().to_base58());
            }
            eprintln!("  tags {}", s.tags.join(" "));
            tokio::signal::ctrl_c().await?;
            if let Some(i) = s.infer {
                i.runtime.shutdown().await;
            }
            Ok(())
        }
        Sub::Train { dataset, output, family, baseline, worker, max_price, gpu, min_ram_gb, runtime_version, public_url, bind, timeout_s, retries, epochs, seed, hidden, bid_window_s, yes, json: as_json, net } => {
            let mut families = vec![family];
            families.extend(baseline.into_iter().filter(|b| !b.is_empty()));
            families.dedup();
            let mut hp = serde_json::Map::new();
            if let Some(v) = epochs {
                hp.insert("epochs".into(), json!(v));
            }
            if let Some(v) = seed {
                hp.insert("seed".into(), json!(v));
            }
            if let Some(v) = hidden {
                hp.insert("hidden_units".into(), json!(v));
            }
            let req = TrainRequest {
                dataset_path: dataset,
                output,
                families,
                workers: worker,
                max_price_usd: max_price,
                requirements: Requirements { gpu, min_ram_gb, runtime_version, model_hash: None },
                public_url,
                bind,
                timeout_s,
                retries,
                hyperparameters: Value::Object(hp),
                bid_window: Duration::from_secs(bid_window_s),
            };
            let p = client::preview(&req)?;
            if !yes {
                eprintln!("About to send dataset {} ({} episodes, {} bytes) for training {} on {} (max ${} per job).\n{}\nRe-run with --yes to send it.", p.dataset_hash, p.episodes, p.bytes, p.families.join(","), if p.workers.is_empty() { "any eligible worker".to_string() } else { p.workers.join(",") }, p.max_price_usd, p.note);
                if as_json {
                    println!("{}", json!({"preview": p}));
                }
                std::process::exit(3);
            }
            let n = node::spawn_node(&net_opts(&net, "client")).await?;
            settle(&n).await;
            let out = client::train(n, &req).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for r in out["results"].as_array().into_iter().flatten() {
                    println!("{} {}  episode_success {}  worker {}", r["family"].as_str().unwrap_or(""), r["model_hash"].as_str().unwrap_or(""), r["eval"]["metrics"]["episode_success"], r["worker_peer_id"].as_str().unwrap_or(""));
                }
                println!("selected: {} (a liquid family wins only if it beats the GRU baseline)", out["selected"].as_str().unwrap_or("?"));
            }
            Ok(())
        }
        Sub::Predict { session, input, model, worker, ttl, scope, max_price, reset, close, bid_window_s, net } => {
            let dir = data_dir().join("sessions");
            let n = node::spawn_node(&net_opts(&net, "client")).await?;
            settle(&n).await;
            let mut sf = match SessionFile::load(&dir, &session)? {
                Some(sf) => {
                    if model.as_ref().is_some_and(|m| m != &sf.model_hash) {
                        bail!("session {session} serves {}, not {}", sf.model_hash, model.unwrap());
                    }
                    sf
                }
                None => {
                    let model = model.context("--model sha256:<hash> is needed to open a new session")?;
                    let sf = client::open_session(n.clone(), &OpenRequest { model_hash: model, session_id: session.clone(), ttl_s: ttl, account_scope: scope, workers: worker, max_price_usd: max_price, bid_window: Duration::from_secs(bid_window_s) }).await?;
                    sf.save(&dir)?;
                    eprintln!("session {} opened on worker {}", sf.session_id, sf.worker_peer_id);
                    sf
                }
            };
            let t = Duration::from_secs(30);
            if reset {
                let r = client::call(n.clone(), &sf, json!({"op": "reset"}), t).await?;
                sf.next_seq = 0;
                sf.save(&dir)?;
                println!("{r}");
            }
            if let Some(p) = input {
                let step: Value = serde_json::from_slice(&std::fs::read(&p)?)?;
                let req = client::predict_request(&step, &sf)?;
                let r = client::call(n.clone(), &sf, json!({"op": "predict", "request": req}), t).await?;
                if r["ok"] == json!(true) {
                    sf.next_seq = req["sequence_index"].as_i64().unwrap_or(sf.next_seq) + 1;
                    sf.save(&dir)?;
                }
                println!("{r}");
            }
            if close {
                let r = client::call(n.clone(), &sf, json!({"op": "close_session"}), t).await?;
                let _ = std::fs::remove_file(SessionFile::path(&dir, &sf.session_id));
                println!("{r}");
            }
            Ok(())
        }
    }
}

/// Give a fresh node's gossip mesh a moment before publishing (as `transcode submit` does).
async fn settle<T>(_n: &T) {
    tokio::time::sleep(Duration::from_secs(2)).await;
}
