//! `c0mpute train lora`: fine-tune a LoRA adapter on a c0mpute worker from a
//! JSONL dataset. A front door to `infernet.text.train` (the same auction,
//! live progress and adapter hash check as `c0mpute infernet text train`),
//! minus the hand-written job file.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use c0mpute_core::temporal::node::{NetOptions, spawn_node_with_store};
use c0mpute_store::ChunkStore;
use c0mpute_core::text::{self, TrainRequest};
use c0mpute_core::train::{LoraOptions, normalize_dataset};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "c0mpute train", about = "Fine-tune language models (LoRA) on c0mpute workers")]
struct TrainCli {
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
    /// Train a LoRA adapter on a remote worker and download it.
    ///
    /// The dataset is JSONL: {"text"} (optionally "title"), {"prompt","completion"}
    /// or {"messages":[{role,content}]} (multi-turn chats train on every turn,
    /// loss on the assistant's). Workers run `infernet model train` (peft, fp32
    /// LoRA on CPU or CUDA); keep the base model small. The dataset and the
    /// adapter move peer to peer over libp2p.
    Lora {
        /// Base model, a Hugging Face id (e.g. Qwen/Qwen2.5-0.5B-Instruct).
        #[arg(long)]
        base: String,
        /// JSONL dataset (max 512 MiB).
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long, default_value_t = 1)]
        epochs: u32,
        /// LoRA rank (lora_alpha is 2x the rank).
        #[arg(long, default_value_t = 8)]
        rank: u32,
        #[arg(long, default_value_t = 2e-4)]
        lr: f64,
        #[arg(long, default_value_t = 256)]
        seq_len: u32,
        #[arg(long, default_value_t = 4)]
        batch_size: u32,
        /// Stop after this many steps (default: epochs over the dataset).
        #[arg(long)]
        max_steps: Option<u32>,
        /// System prompt baked into the adapter's manifest (default: the dataset's).
        #[arg(long)]
        system_prompt: Option<String>,
        /// Adapter name (default: the output directory's name).
        #[arg(long)]
        name: Option<String>,
        /// Where the adapter lands.
        #[arg(long, default_value = "adapter")]
        out: PathBuf,
        /// Only accept bids from these worker peer ids. Repeatable.
        #[arg(long)]
        worker: Vec<String>,
        #[arg(long, default_value_t = 1.0)]
        max_price: f64,
        /// Require a CUDA worker.
        #[arg(long)]
        gpu: bool,
        #[arg(long)]
        min_ram_gb: Option<u64>,
        /// Also serve workers older than the peer-to-peer transport: the corpus and adapter then go
        /// over HTTP from this machine (the worker must reach it: same LAN, or --public-url).
        #[arg(long)]
        legacy_http: bool,
        /// With --legacy-http: how an old worker reaches this machine's transfer server (an https URL forwarding to --bind).
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
        /// Consent: send the dataset to the winning worker. Without it, only check and preview.
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
    c0mpute_core::config::data_dir().unwrap_or_else(|| PathBuf::from("./c0mpute-data")).join("train")
}

pub async fn run(args: &[String]) -> Result<()> {
    let cli = TrainCli::try_parse_from(std::iter::once("c0mpute train".to_string()).chain(args.iter().cloned())).unwrap_or_else(|e| e.exit());
    let Sub::Lora {
        base,
        dataset,
        epochs,
        rank,
        lr,
        seq_len,
        batch_size,
        max_steps,
        system_prompt,
        name,
        out,
        worker,
        max_price,
        gpu,
        min_ram_gb,
        legacy_http,
        public_url,
        bind,
        timeout_s,
        retries,
        bid_window_s,
        yes,
        json: as_json,
        net,
    } = cli.cmd;
    let name = name.unwrap_or_else(|| {
        out.file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .unwrap_or("adapter")
            .to_string()
    });
    let opts = LoraOptions { name, base_model: base, epochs, rank, lr, seq_len, batch_size, max_steps, system_prompt };
    opts.validate()?;
    let raw = std::fs::read(&dataset).with_context(|| format!("read {}", dataset.display()))?;
    let (corpus, report) = normalize_dataset(&raw).with_context(|| format!("dataset {}", dataset.display()))?;
    let job = opts.job(&report);

    // Stage the corpus and job where `text::train` reads them.
    let corpus_hash = text::sha256_hex(&corpus);
    let stage = data_dir().join("jobs").join(&corpus_hash.trim_start_matches("sha256:")[..16]);
    std::fs::create_dir_all(&stage)?;
    std::fs::write(stage.join("corpus.jsonl"), &corpus)?;
    std::fs::write(stage.join("job.json"), serde_json::to_vec_pretty(&job)?)?;
    let req = TrainRequest {
        job_path: stage.join("job.json"),
        dataset_path: Some(stage.join("corpus.jsonl")),
        output: out.clone(),
        workers: worker,
        max_price_usd: max_price,
        gpu,
        min_ram_gb,
        legacy_http,
        public_url,
        bind,
        timeout_s,
        retries,
        bid_window: Duration::from_secs(bid_window_s),
    };
    let (_, _, preview) = text::load(&req)?;
    eprintln!(
        "dataset ok: {} records ({} text, {} prompt/completion, {} messages), {} bytes -> {} bytes corpus {}",
        report.records, report.text, report.prompt_completion, report.messages, report.bytes_in, report.bytes_out, preview.dataset_hash
    );
    if !yes {
        eprintln!(
            "About to send the corpus to {} to fine-tune {} as {} (rank {}, {} epoch(s), lr {}; max ${}).\nRe-run with --yes to send it.",
            if preview.workers.is_empty() { "any eligible worker".to_string() } else { preview.workers.join(",") },
            opts.base_model,
            opts.name,
            opts.rank,
            opts.epochs,
            opts.lr,
            max_price
        );
        if as_json {
            println!("{}", json!({"preview": preview, "dataset": report, "job": job}));
        }
        std::process::exit(3);
    }

    // The corpus goes out as pieces from this store, served over libp2p.
    let store = ChunkStore::open(data_dir().join("store")).await?;
    let node = spawn_node_with_store(
        &NetOptions {
            identity_dir: data_dir().join("client"),
            listen: net.listen.clone(),
            bootstrap: net.bootstrap.clone(),
            default_bootstrap: !net.no_default_bootstrap,
        },
        store.clone(),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    eprintln!("looking for a worker advertising {} ...", text::required_tags(&corpus).join(" + "));
    let mut total = None;
    let mut on_step = false;
    let result = text::train(node, &store, &req, |ev| {
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
    let _ = std::fs::remove_dir_all(&stage);
    if as_json {
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    println!(
        "adapter {} verified, saved to {}  (worker {}, ${})",
        result["adapter_hash"].as_str().unwrap_or("?"),
        out.display(),
        result["receipt"]["worker_peer_id"].as_str().unwrap_or("?"),
        result["receipt"]["price_usd"]
    );
    println!("use it:  c0mpute infernet text chat --adapter {} \"your question\"", out.display());
    Ok(())
}
