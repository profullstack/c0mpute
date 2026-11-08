//! `c0mpute webhook serve|send`: paid, end-to-end encrypted webhook delivery
//! on c0mpute workers (`webhook.deliver`, DIP-0028).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use c0mpute_core::temporal::node::NetOptions;
use c0mpute_core::webhook::{self, SendRequest, WebhookCall};
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute webhook",
    about = "Send webhooks from c0mpute workers, end-to-end encrypted and paid per delivery"
)]
struct WebhookCli {
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
    /// Deliver webhooks for buyers on this node, at a fixed price per delivery.
    Serve {
        /// Your price per delivery (one webhook with all its retries), USD.
        #[arg(long, default_value_t = webhook::DEFAULT_PRICE_USD)]
        price: f64,
        /// Deliveries in flight at once.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Deliver one webhook through a c0mpute worker. Only the winning worker
    /// can read the URL, headers, body and secret.
    Send {
        /// Target URL (http or https, public addresses only).
        url: String,
        #[arg(short = 'X', long, default_value = "POST")]
        method: String,
        /// Request header "Name: value". Repeatable.
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Request body (JSON gets content-type application/json by default).
        #[arg(short = 'd', long, conflicts_with = "data_file")]
        data: Option<String>,
        /// Read the body from a file ("-" for stdin).
        #[arg(long)]
        data_file: Option<PathBuf>,
        /// Standard Webhooks signing secret (whsec_...): adds webhook-id,
        /// webhook-timestamp and webhook-signature.
        #[arg(long, env = "C0MPUTE_WEBHOOK_SECRET", hide_env_values = true)]
        secret: Option<String>,
        /// webhook-id (default msg_<job id>); the same on every retry.
        #[arg(long)]
        id: Option<String>,
        /// Attempts in total, 1..=5. Retries on no answer, 408, 425, 429, 5xx.
        #[arg(long, default_value_t = 3)]
        attempts: u32,
        /// Timeout per attempt, ms (1000..=30000).
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u32,
        /// Wait before the first retry, ms; doubles each retry (100..=10000).
        #[arg(long, default_value_t = 1_000)]
        backoff_ms: u32,
        /// Only accept bids from these worker peer ids. Repeatable.
        #[arg(long)]
        worker: Vec<String>,
        /// Most you pay for this delivery, USD.
        #[arg(long, default_value_t = webhook::DEFAULT_MAX_PRICE_USD)]
        max_price: f64,
        /// Billing id carried on offer, accept and receipt (e.g. coinpay:inv_...).
        #[arg(long)]
        external_ref: Option<String>,
        /// Seconds to collect bids.
        #[arg(long, default_value_t = 6)]
        bid_window_s: u64,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        net: NetArgs,
    },
}

fn data_dir() -> PathBuf {
    c0mpute_core::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("./c0mpute-data"))
        .join("webhook")
}

fn net_opts(a: &NetArgs, identity: &str) -> NetOptions {
    NetOptions {
        identity_dir: data_dir().join(identity),
        listen: a.listen.clone(),
        bootstrap: a.bootstrap.clone(),
        default_bootstrap: !a.no_default_bootstrap,
    }
}

fn parse_header(h: &str) -> Result<(String, String)> {
    let (k, v) = h
        .split_once(':')
        .with_context(|| format!("header {h:?} must look like \"Name: value\""))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

pub async fn run(args: &[String]) -> Result<()> {
    let cli = WebhookCli::try_parse_from(
        std::iter::once("c0mpute webhook".to_string()).chain(args.iter().cloned()),
    )
    .unwrap_or_else(|e| e.exit());
    match cli.cmd {
        Sub::Serve {
            price,
            concurrency,
            net,
        } => {
            let s = webhook::serve(&net_opts(&net, "serve"), price, concurrency).await?;
            eprintln!(
                "delivering {} as {} at ${} per delivery",
                webhook::WORKLOAD,
                s.net.peer_id().to_base58(),
                s.price_usd
            );
            for a in s.net.listen_addrs().await.unwrap_or_default() {
                eprintln!("  listening {a}/p2p/{}", s.net.peer_id().to_base58());
            }
            eprintln!("  tags {}", s.tags.join(" "));
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Sub::Send {
            url,
            method,
            headers,
            data,
            data_file,
            secret,
            id,
            attempts,
            timeout_ms,
            backoff_ms,
            worker,
            max_price,
            external_ref,
            bid_window_s,
            json: as_json,
            net,
        } => {
            let body = match (data, data_file) {
                (Some(d), _) => d,
                (None, Some(p)) if p.as_os_str() == "-" => {
                    std::io::read_to_string(std::io::stdin())?
                }
                (None, Some(p)) => {
                    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?
                }
                (None, None) => String::new(),
            };
            let call = WebhookCall {
                url,
                method: method.to_ascii_uppercase(),
                headers: headers
                    .iter()
                    .map(|h| parse_header(h))
                    .collect::<Result<_>>()?,
                body,
                secret,
                id,
            };
            let req = SendRequest {
                call,
                max_attempts: attempts,
                timeout_ms,
                backoff_ms,
                workers: worker,
                max_price_usd: max_price,
                bid_window: Duration::from_secs(bid_window_s),
                external_ref,
            };
            // Validate before joining the network.
            req.call.validate()?;
            let n = c0mpute_core::temporal::node::spawn_node(&net_opts(&net, "client")).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
            eprintln!("looking for a webhook worker (max ${max_price}) ...");
            let out = webhook::send(n, &req).await?;
            if as_json {
                println!("{}", serde_json::to_string(&out)?);
            } else {
                let r = &out.result;
                match r.status {
                    Some(s) => println!(
                        "{s} in {}ms after {} attempt(s){}",
                        r.latency_ms,
                        r.attempts,
                        if r.delivered { "" } else { " (not delivered)" }
                    ),
                    None => println!(
                        "not delivered after {} attempt(s): {}",
                        r.attempts,
                        r.error.as_deref().unwrap_or("no answer")
                    ),
                }
                if !r.response_body.is_empty() {
                    println!(
                        "{}{}",
                        r.response_body,
                        if r.truncated { "\n[truncated]" } else { "" }
                    );
                }
                eprintln!(
                    "(worker {}, ${}, receipt {})",
                    out.worker_peer_id, out.price_usd, out.output_hash
                );
            }
            if !out.result.delivered {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
