//! `c0mpute infernet text serve|train|chat`: LoRA fine-tunes of language
//! models (`infernet.text.train`) on c0mpute workers with live progress, and
//! answers from a trained adapter on a worker (`infernet.text.infer`).

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use c0mpute_core::temporal::node::NetOptions;
use c0mpute_core::text::{self, TrainRequest};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "c0mpute infernet text", about = "Fine-tune language models (LoRA) on c0mpute workers and chat with the result")]
struct TextCli {
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
    /// Accept infernet.text.train jobs on this node (needs `infernet model runtime setup --peft --yes`).
    Serve {
        #[command(flatten)]
        net: NetArgs,
    },
    /// Ask a trained adapter a question on a remote worker (sources from pit.world search).
    Chat {
        /// The adapter dir `text train` wrote (adapter_model.safetensors + manifest.json).
        #[arg(long, default_value = "adapter")]
        adapter: PathBuf,
        /// The question.
        question: Vec<String>,
        /// Skip the pit.world search (answer from the adapter alone).
        #[arg(long)]
        no_search: bool,
        #[arg(long, default_value = text::PIT_WORLD_SEARCH)]
        search_url: String,
        #[arg(long, default_value_t = 350)]
        max_new_tokens: u32,
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 0.05)]
        max_price: f64,
        /// Also serve workers older than the peer-to-peer transport over HTTP from this machine.
        #[arg(long)]
        legacy_http: bool,
        /// With --legacy-http: how an old worker reaches this machine's transfer server (use https off-LAN).
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value_t = 600)]
        timeout_s: u64,
        #[arg(long, default_value_t = 4)]
        bid_window_s: u64,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Fine-tune on a remote worker and download the adapter, with live progress.
    Train {
        /// Job config (@infernetprotocol/training): kind lora, base_model, hyperparams, system_prompt.
        #[arg(long)]
        job: PathBuf,
        /// JSONL corpus of {title, text}; defaults to job.dataset.url next to the job file.
        #[arg(long)]
        dataset: Option<PathBuf>,
        /// Where the adapter lands.
        #[arg(long, default_value = "adapter")]
        output: PathBuf,
        /// Only accept bids from these worker peer ids (trusted workers).
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 1.0)]
        max_price: f64,
        #[arg(long)]
        gpu: bool,
        #[arg(long)]
        min_ram_gb: Option<u64>,
        /// Also serve workers older than the peer-to-peer transport over HTTP from this machine.
        #[arg(long)]
        legacy_http: bool,
        /// With --legacy-http: how an old worker reaches this machine's transfer server (use https off-LAN).
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long, default_value = "0.0.0.0:0")]
        bind: std::net::SocketAddr,
        #[arg(long, default_value_t = 4 * 3600)]
        timeout_s: u64,
        #[arg(long, default_value_t = 1)]
        retries: u32,
        #[arg(long, default_value_t = 8)]
        bid_window_s: u64,
        /// Consent: send the corpus to the winning worker.
        #[arg(long)]
        yes: bool,
        /// Print events as JSON lines instead of the progress bar.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        net: NetArgs,
    },
}

fn data_dir() -> PathBuf {
    c0mpute_core::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("text")
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
    let cli = TextCli::try_parse_from(std::iter::once("c0mpute infernet text".to_string()).chain(args.iter().cloned())).unwrap_or_else(|e| e.exit());
    match cli.cmd {
        Sub::Serve { net } => {
            let s = text::serve(&net_opts(&net, "serve"), infernet_bin()?, data_dir().join("cache")).await?;
            eprintln!(
                "serving infernet.text.train{} as {}  (torch {}, peft {}, cuda {})",
                if s.infer { " + infernet.text.infer" } else { " (chat needs Infernet >= 0.1.80)" },
                s.net.peer_id().to_base58(),
                s.runtime["torch"].as_str().unwrap_or("?"),
                s.runtime["peft"].as_str().unwrap_or("?"),
                s.runtime["cuda"]
            );
            for a in s.net.listen_addrs().await.unwrap_or_default() {
                eprintln!("  listening {a}/p2p/{}", s.net.peer_id().to_base58());
            }
            eprintln!("  tags {}", s.tags.join(" "));
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Sub::Chat { adapter, question, no_search, search_url, max_new_tokens, worker, max_price, legacy_http, public_url, bind, timeout_s, bid_window_s, json: as_json, net } => {
            let question = question.join(" ");
            anyhow::ensure!(!question.trim().is_empty(), "ask a question: c0mpute infernet text chat --adapter ./adapter \"How should I price my SaaS?\"");
            let req = text::ChatRequest {
                adapter_dir: adapter,
                question,
                search: if no_search { None } else { Some(search_url) },
                max_new_tokens,
                workers: worker,
                max_price_usd: max_price,
                legacy_http,
                public_url,
                bind,
                timeout_s,
                bid_window: Duration::from_secs(bid_window_s),
            };
            // The adapter goes out as pieces from this store, served over libp2p.
            let store = c0mpute_store::ChunkStore::open(data_dir().join("store")).await?;
            let n = c0mpute_core::temporal::node::spawn_node_with_store(&net_opts(&net, "client"), store.clone()).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            eprintln!("asking a c0mpute worker ...");
            let out = text::chat(n, &store, &req).await?;
            if as_json {
                println!("{}", serde_json::to_string(&out)?);
            } else {
                println!("{}", out["text"].as_str().unwrap_or(""));
                for (i, s) in out["sources"].as_array().into_iter().flatten().enumerate() {
                    println!("  [{}] {}", i + 1, s["url"].as_str().unwrap_or(""));
                }
                eprintln!("(worker {}, {} tokens in {}s, ${})", out["receipt"]["worker_peer_id"].as_str().unwrap_or("?"), out["tokens"], out["seconds"], out["receipt"]["price_usd"]);
            }
            Ok(())
        }
        Sub::Train { job, dataset, output, worker, max_price, gpu, min_ram_gb, legacy_http, public_url, bind, timeout_s, retries, bid_window_s, yes, json: as_json, net } => {
            let req = TrainRequest { job_path: job, dataset_path: dataset, output, workers: worker, max_price_usd: max_price, gpu, min_ram_gb, legacy_http, public_url, bind, timeout_s, retries, bid_window: Duration::from_secs(bid_window_s) };
            let (_, _, p) = text::load(&req)?;
            if !yes {
                eprintln!(
                    "About to send {} ({} documents, {} bytes, {}) to {} to fine-tune {} as {} (max ${} per job).\nRe-run with --yes to send it.",
                    p.dataset,
                    p.documents,
                    p.bytes,
                    p.dataset_hash,
                    if p.workers.is_empty() { "any eligible worker".to_string() } else { p.workers.join(",") },
                    p.base_model,
                    p.name,
                    p.max_price_usd
                );
                if as_json {
                    println!("{}", json!({"preview": p}));
                }
                std::process::exit(3);
            }
            // The corpus goes out as pieces from this store, served over libp2p.
            let store = c0mpute_store::ChunkStore::open(data_dir().join("store")).await?;
            let n = c0mpute_core::temporal::node::spawn_node_with_store(&net_opts(&net, "client"), store.clone()).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            eprintln!("looking for a worker for {} ...", p.name);
            let mut total = None;
            let mut on_step = false;
            let out = text::train(n, &store, &req, |ev| {
                if as_json {
                    println!("{ev}");
                    return;
                }
                let Some(line) = text::render(ev, &mut total) else { return };
                let mut err = std::io::stderr();
                if ev["type"] == "step" {
                    let _ = write!(err, "\r\x1b[2K{line}");
                    on_step = true;
                } else {
                    if on_step {
                        let _ = writeln!(err);
                        on_step = false;
                    }
                    let _ = writeln!(err, "{line}");
                }
                let _ = err.flush();
            })
            .await?;
            if on_step {
                eprintln!();
            }
            if as_json {
                println!("{}", serde_json::to_string(&out)?);
            } else {
                println!("adapter {}  {}  (worker {}, ${})", out["adapter_hash"].as_str().unwrap_or("?"), out["adapter_dir"].as_str().unwrap_or("?"), out["receipt"]["worker_peer_id"].as_str().unwrap_or("?"), out["receipt"]["price_usd"]);
            }
            Ok(())
        }
    }
}
