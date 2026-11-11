//! `c0mpute` — the c0mpute.com umbrella CLI.
//!
//! Top-level surface:
//!
//!   c0mpute doctor
//!   c0mpute bench [run|show|path] [--quick] [--json]
//!   c0mpute worker register|start|stop|status
//!   c0mpute job submit|status|logs|cancel
//!   c0mpute modules list|install|enable|disable
//!   c0mpute version
//!
//! Plugin (module) subcommands. Each one delegates / dispatches into the
//! relevant module — built-in modules run in-process, peer-CLI modules
//! shell out to their binary on PATH (per DIP-0006):
//!
//!   c0mpute transcode <sub>     # in-process FFmpeg workload
//!   c0mpute coinpay   <args…>   # delegates to `coinpay`
//!   c0mpute infernet  <args…>   # delegates to `infernet`
//!   c0mpute webhook   <sub>     # paid, E2E encrypted webhook delivery
//!
//! The plugin form mirrors the URL namespace: c0mpute.com/transcode,
//! c0mpute.com/coinpay, c0mpute.com/infernet.

mod apps_mcp;
mod container_cli;
mod ai_chat;
mod chat_connect;
mod chat_history;
mod chat_plugins;
mod chat_push;
mod crowdgpt;
mod dm_cli;
use dm_cli::run_chat;
mod gameserver;
mod git_cli;
mod peers;
mod private_storage;
mod promo;
mod storage;
#[cfg(all(target_os = "linux", feature = "fuse"))]
mod mount;
mod node_status;
mod temporal;
mod text;
mod train;
mod webhook;

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use c0mpute_core::{
    Config, JobAuction, Supervisor, TranscodeJobInline, config, init_tracing,
    run_register,
};
use c0mpute_proto::Role;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute",
    version,
    about = "c0mpute.com — decentralized compute network",
    long_about = "c0mpute.com CLI. Submit jobs, run a worker, manage modules.\n\nBuilt-in workloads:\n  transcode  (FFmpeg)\n  tts        (Piper, Kokoro, or Pocket, CPU)\n  stt        (faster-whisper, CPU)\n  workflow-step (JMESPath transforms + guarded HTTP calls, for meshhook)\n  ocr        (Tesseract on the worker, images + PDFs)\n  embed      (fastembed bge-small-en-v1.5, CPU)\n  captcha    (ddddocr image/slider/gap/detect, CPU; audio + math via speech serve)\n  render     (Blender frames, Cycles on CPU or GPU)\n  coinpay    (DID + payments, peer CLI)\n  inference  (chat from your Ollama/llama-server, p2p)\n  infernet   (temporal + LoRA runtimes, peer CLI)\n\n  c0mpute coinpay reputation did claim\n  c0mpute transcode preset list\n  c0mpute tts setup --engine all\n  c0mpute stt setup\n  c0mpute speech serve\n  c0mpute ocr scan.pdf --lang eng\n  c0mpute embed \"first text\" \"second text\"\n  c0mpute captcha code.png --charset alnum --plain\n  c0mpute render scene.blend --frames 1-120 --out frames --mp4\n  C0MPUTE_JOBS_TOKEN=<secret> c0mpute jobs serve\n  c0mpute worker start --roles tts,stt\n  c0mpute chat --p2p \"hello\"     (peer to peer, no account)\n  c0mpute chat login\n  c0mpute chat \"hello\"\n  c0mpute infernet chat --model qwen2.5:7b \"hello\"\n  c0mpute infernet model list --node <id>\n  c0mpute infernet reservation pricing\n  c0mpute infernet text serve\n  c0mpute infernet text train --job job.json --dataset corpus.jsonl --yes\n  c0mpute infernet text chat --adapter ./adapter \"How should I price my SaaS?\"\n  c0mpute train lora --base Qwen/Qwen2.5-0.5B-Instruct --dataset data.jsonl --yes\n  c0mpute gameserver start minecraft --hours 4 --ram 4g --accept-eula\n  c0mpute infernet temporal serve --model sha256:<hash> --train\n  c0mpute infernet temporal predict --session s1 --input step.json\n  c0mpute webhook serve --price 0.001\n  c0mpute webhook send https://example.com/hook -d '{\"event\":\"paid\"}' --secret whsec_..."
)]
struct Cli {
    /// Override the config file location.
    #[arg(long, env = "C0MPUTE_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Print c0mpute + every installed plugin/peer-CLI version, then exit.
    #[arg(short = 'v', long = "versions", global = true)]
    versions: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run full-stack diagnostic checks, and list every c0mpute worker
    /// process, systemd unit and identity lock on this machine (duplicates,
    /// stale locks, wedged workers, units on old binaries).
    Doctor {
        /// Disable the extra worker units it may (your own user units; system
        /// units only as root, otherwise it prints the sudo command) and add
        /// the restart-loop guard to units that lack it.
        #[arg(long)]
        fix: bool,
    },
    /// Benchmark this machine (fib / matmul / hash at 1…N threads) and record
    /// a score the worker advertises to the network. Report lands in
    /// `<data-dir>/bench.json`, same shape as fleetcode's runtime-benchmarks.
    #[command(alias = "benchmark")]
    Bench {
        #[command(subcommand)]
        cmd: Option<BenchCmd>,
        /// Smaller problem sizes; done in a second or two. Not advertised.
        #[arg(long, global = true)]
        quick: bool,
        /// Print the report as JSON instead of the table.
        #[arg(long, global = true)]
        json: bool,
        /// Thread counts to run, e.g. `1,4,16`. Default: 1, 2, 4, … , all cores.
        #[arg(long, value_delimiter = ',', global = true)]
        threads: Option<Vec<usize>>,
    },
    /// Your listing on c0mpute.com/market: how this node prices its bids.
    ///
    /// Dynamic (default) undercuts the buyer's cap when idle and rises as the
    /// queue fills; fixed always asks one price. Neither goes under `--min`.
    /// A running worker picks a change up on restart.
    Pricing {
        #[command(subcommand)]
        cmd: Option<PricingCmd>,
    },
    /// Your node's nickname on c0mpute.com/nodes (who runs it).
    ///
    /// `c0mpute name` prints it, `c0mpute name <handle>` sets it, `c0mpute
    /// name --clear` removes it. Use the same handle on every machine you run
    /// and the board groups them under it. Lowercase a-z, 0-9, `-` and `_`, up
    /// to 32 characters; public and unverified. Stored in `~/.c0mpute/name`
    /// (`C0MPUTE_NAME` overrides it). A running worker picks a change up on
    /// restart.
    Name {
        /// The nickname to set, e.g. `profullstack`.
        handle: Option<String>,
        /// Remove the nickname.
        #[arg(long, conflicts_with = "handle")]
        clear: bool,
    },
    /// Private mode: serve only your own machines, stay off the public list.
    ///
    /// A private node only bids on jobs whose signed publisher is one of the
    /// identities on this machine or a peer-id you allow (your other boxes),
    /// and it stops advertising itself to the network. A running worker
    /// picks a change up on restart. `C0MPUTE_PRIVATE=1` also turns it on.
    Private {
        #[command(subcommand)]
        cmd: Option<PrivateCmd>,
    },
    /// Worker lifecycle.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Job lifecycle.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Object storage (DIP-0012), private by default.
    ///
    /// `put` encrypts on this machine and the network only ever holds
    /// ciphertext; `--public` stores plaintext. Run `c0mpute storage tiers`
    /// for the redundancy and price table.
    #[command(after_long_help = storage::unimplemented_note())]
    Storage {
        #[command(subcommand)]
        cmd: storage::StorageCmd,
    },
    /// Plugin management (list / install / enable / disable / uninstall).
    #[command(alias = "plugins")]
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },

    /// Transcode plugin (built-in FFmpeg workload).
    Transcode {
        #[command(subcommand)]
        cmd: TranscodeCmd,
    },
    /// Install and inspect local CPU TTS voices.
    Tts {
        #[command(subcommand)]
        cmd: TtsCmd,
    },
    /// Install local CPU speech recognition models.
    Stt {
        #[command(subcommand)]
        cmd: SttCmd,
    },
    /// Donate idle compute to CrowdGPT (an open LLM trained by volunteers).
    /// Off by default, earns nothing, runs only while this node has no paid
    /// work and stops the moment it does: `c0mpute crowdgpt on|off|status`.
    Crowdgpt {
        #[command(subcommand)]
        cmd: crowdgpt::CrowdgptCmd,
    },
    /// This node at a glance: the worker, auto-update, bench, idle donation
    /// (same as `c0mpute worker status`).
    Status {
        /// One JSON object instead of the text report (for the desktop app,
        /// `c0mpute tui` and scripts): version, worker pid, peer id, nickname,
        /// config, and every role, serving right now or gated and why.
        #[arg(long)]
        json: bool,
    },
    /// Video generation (Wan 2.2) in the runner container.
    Video {
        #[command(subcommand)]
        cmd: VideoCmd,
    },
    /// Run the authenticated HTTP buyer bridge for P2P speech inference.
    Speech {
        #[command(subcommand)]
        cmd: SpeechCmd,
    },
    /// Run the authenticated HTTP buyer bridge for generic jobs
    /// (`workflow.step`, `crawl.fetch`, DIP 0030): POST /v1/jobs, GET /healthz.
    #[command(after_help = JOBS_EXAMPLES)]
    Jobs {
        #[command(subcommand)]
        cmd: JobsCmd,
    },
    /// Crawl plugin: fetch a public web page through a c0mpute worker
    /// (`crawl.fetch`: static, robots.txt-aware, private addresses refused).
    #[command(after_help = CRAWL_EXAMPLES)]
    Crawl {
        #[command(subcommand)]
        cmd: CrawlCmd,
    },
    /// Hosting plugin: put a static site on the network. Hosting nodes keep
    /// three copies and heal themselves; every gateway serves it at
    /// /s/<site_id>/ and on custom domains confirmed by DNS.
    Hosting {
        #[command(subcommand)]
        cmd: HostingCmd,
    },
    /// Apps plugin: run a backend (API, MCP server, worker, database app) on
    /// the network from a container image. Three copies, one primary,
    /// state snapshotted to the standbys, failover with no coordinator;
    /// every gateway serves it at /a/<app_id>/ and on custom domains.
    Apps {
        #[command(subcommand)]
        cmd: AppsCmd,
    },
    /// Train plugin: fine-tune a LoRA adapter on a c0mpute worker from a
    /// JSONL dataset (`c0mpute train lora --base <model> --dataset <jsonl>`;
    /// runs as infernet.text.train).
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Train {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Game server plugin: rent a Minecraft, Factorio or Valheim server for a
    /// few hours (`c0mpute gameserver start minecraft --hours 4 --ram 4g --accept-eula`).
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Gameserver {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Git hosting: repos kept by the network, signed by your key, push to
    /// deploy (`c0mpute git init my-repo`, `git push origin main`,
    /// `git clone c0mpute://<owner>/<repo>`). Full guide: `c0mpute git --help`.
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Git {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Build a Dockerfile on a trusted builder on the network and get the
    /// image back (`container.run`, DIP 0033). No git: any directory or
    /// tarball. `c0mpute build . --load -t app:latest`
    Build(container_cli::BuildArgs),
    /// Run a command in a container on a trusted builder and bring files
    /// back: `c0mpute run --image node@sha256:... --in . --out dist -- npm run build`
    Run(container_cli::RunArgs),
    /// The `containers` role and builders: trust buyers, add builders, print
    /// this machine's build identity (`c0mpute containers show`).
    Containers {
        #[command(subcommand)]
        cmd: container_cli::ContainersCmd,
    },
    /// OCR plugin: extract the text of an image (PNG, JPEG, TIFF, WebP) or a
    /// PDF on a c0mpute worker running Tesseract (`ocr.extract`).
    Ocr {
        /// The image or PDF to read.
        file: std::path::PathBuf,
        /// Tesseract language(s), e.g. `eng`, `deu`, `eng+deu`.
        #[arg(long, default_value = "eng")]
        lang: String,
        /// Highest price in USD you will pay for the job.
        #[arg(long, default_value_t = 0.05)]
        max_price: f64,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 900)]
        timeout_secs: u64,
        /// Print the whole result JSON instead of just the text.
        #[arg(long)]
        json: bool,
    },
    /// Render plugin: render frames of a `.blend` across c0mpute workers
    /// running Blender (`blender.render-frame`). Frames are split into jobs,
    /// rendered in parallel, retried on other workers when one fails, checked
    /// (hash, image header, matching dimensions) and written to `--out`.
    Render {
        /// The `.blend` scene (up to 512 MiB; pack external textures into it).
        file: std::path::PathBuf,
        /// Frames: `1-120`, `7`, or `1-10,20,30-32`.
        #[arg(long, default_value = "1")]
        frames: String,
        /// CYCLES (CPU or GPU), EEVEE or WORKBENCH (GPU workers only).
        #[arg(long, default_value = "CYCLES")]
        engine: String,
        /// AUTO (GPU where the worker has one), CPU, or GPU (GPU workers only).
        #[arg(long, default_value = "AUTO")]
        device: String,
        /// PNG, JPEG or EXR.
        #[arg(long, default_value = "PNG")]
        format: String,
        /// Scale the scene's resolution (1-200 percent).
        #[arg(long)]
        resolution: Option<u32>,
        /// Override the scene's sample count.
        #[arg(long)]
        samples: Option<u32>,
        /// Where frames are written (`frame_0001.png`, ...).
        #[arg(long, default_value = "frames")]
        out: std::path::PathBuf,
        /// Frames per job (1-10). Bigger batches fetch the scene fewer times.
        #[arg(long, default_value_t = 1)]
        batch: u32,
        /// Jobs in flight at once.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Tries per job, each on a different worker.
        #[arg(long, default_value_t = 3)]
        attempts: usize,
        /// Highest price in USD you will pay per job.
        #[arg(long, default_value_t = 0.10)]
        max_price: f64,
        /// Hard render limit per frame, in seconds.
        #[arg(long, default_value_t = 1800)]
        frame_timeout_secs: u64,
        /// Also stitch the frames into this MP4 with the local ffmpeg
        /// (default `<out>/render.mp4`; PNG/JPEG, contiguous frames).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        mp4: Option<std::path::PathBuf>,
        /// Frame rate for `--mp4`.
        #[arg(long, default_value_t = 24)]
        fps: u32,
    },
    /// Embeddings plugin: embed texts on a c0mpute worker (`embed.text`,
    /// fastembed on CPU) and print `{model, dims, vectors}`.
    /// `c0mpute embed setup` provisions the runtime on this machine.
    #[command(args_conflicts_with_subcommands = true)]
    Embed {
        #[command(subcommand)]
        cmd: Option<EmbedCmd>,
        /// Texts to embed (one vector each). Reads lines from stdin when none
        /// are given.
        texts: Vec<String>,
        /// Model: bge-small-en-v1.5 (default, 384 dims) or
        /// nomic-embed-text-v1.5 (768 dims).
        #[arg(long)]
        model: Option<String>,
        /// Highest price in USD you will pay for the job.
        #[arg(long, default_value_t = 0.01)]
        max_price: f64,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },
    /// Captcha plugin: read a captcha on a c0mpute worker (`captcha.solve`,
    /// ddddocr on CPU). Image text by default; `--type slider|gap` with
    /// `--background`, `--type detect` for click-the-text boxes, or
    /// `--question "3 + 4?"` for math (answered locally). Audio captchas go
    /// through `POST /v1/captcha` on `c0mpute speech serve`.
    /// `c0mpute captcha setup` provisions the solver on this machine.
    #[command(args_conflicts_with_subcommands = true)]
    Captcha {
        #[command(subcommand)]
        cmd: Option<CaptchaCmd>,
        /// The captcha image (PNG, JPEG, GIF, WebP, BMP); the puzzle piece
        /// for `slider`, the backdrop with the gap for `gap`.
        file: Option<std::path::PathBuf>,
        /// image (default), slider, gap, detect or math.
        #[arg(long = "type", default_value = "image")]
        kind: String,
        /// Background image for slider / gap.
        #[arg(long)]
        background: Option<std::path::PathBuf>,
        /// Limit image answers to a charset: digits, lower, upper, letters,
        /// alnum, lower_digits, upper_digits, or the literal characters.
        #[arg(long)]
        charset: Option<String>,
        /// Force the answer's case: lower or upper.
        #[arg(long)]
        case: Option<String>,
        /// A math question to answer locally ("What is 3 + 4?").
        #[arg(long)]
        question: Option<String>,
        /// Print only the answer (text, or "x,y" for slider/gap).
        #[arg(long)]
        plain: bool,
        /// Highest price in USD you will pay for the job.
        #[arg(long, default_value_t = 0.001)]
        max_price: f64,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
    /// Podcasting plugin: publish a podcast (RSS 2.0 + Podcasting 2.0 feed,
    /// episode page, audio) as a self-healing hosting site.
    Podcasting {
        #[command(subcommand)]
        cmd: PodcastingCmd,
    },
    /// Podcasts plugin: subscribe to c0mpute shows or any RSS podcast feed,
    /// list, download and play episodes.
    Podcasts {
        #[command(subcommand)]
        cmd: PodcastsCmd,
    },
    /// Ads (ads-manager, ctv-ads, podcast-ads): signed campaigns gossiped to
    /// every gateway, creatives on hosting, VAST/VMAP and podcast ad
    /// insertion served by gateways, receipts counted here. No billing yet.
    Ads {
        #[command(subcommand)]
        cmd: AdsCmd,
    },
    /// Live-stream plugin: p2p HLS live streaming. `go` broadcasts (needs
    /// ffmpeg); every gateway serves /live/<stream_id>/index.m3u8 and peers
    /// swarm the segments.
    Stream {
        #[command(subcommand)]
        cmd: StreamCmd,
    },
    /// Coinpay plugin — delegates to the `coinpay` peer CLI.
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Coinpay {
        /// Arguments forwarded to `coinpay`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Paid, end-to-end encrypted webhook delivery on c0mpute workers
    /// (`c0mpute webhook serve|send`, DIP-0028).
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Webhook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Infernet plugin — delegates to the `infernet` peer CLI.
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Infernet {
        /// Arguments forwarded to `infernet`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Sign in to every c0mpute network (coinpay + infernet), one at a time.
    Login,

    /// Chat with open models on the c0mpute network (`c0mpute chat login`,
    /// then `c0mpute chat "prompt"`, or no prompt for a session). Also
    /// `models`, `whoami`, `logout`, `mcp`. Run `c0mpute chat --help`.
    #[command(disable_help_flag = true)]
    Chat {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Make a 20-40 s cartoon promo video (MP4 with narration, music and
    /// captions) of a website on the c0mpute network:
    /// `c0mpute promo https://example.com --style pixar [--vertical]`.
    /// Needs `c0mpute chat login`.
    Promo(promo::PromoArgs),

    /// Secure DMs — E2E encrypted p2p messaging (DIP-0018). Was `c0mpute chat`.
    Dm {
        #[command(subcommand)]
        cmd: ChatCmd,
    },

    /// The terminal app: chat (with /image, /video, /music, /album,
    /// /promo, /push), your jobs, this node's roles and why any are gated,
    /// the network, and settings.
    ///
    /// Subprocess-launches `c0mpute-tui` (on @profullstack/hqtui, run through
    /// Bun), installed and kept at this release's version on demand. See
    /// apps/tui in the repo.
    /// Long-term we move to Perry once their CLI surface ships.
    // -h/--help go to the plugin's own parser, which prints the real help.
    #[command(disable_help_flag = true)]
    Tui {
        /// Arguments forwarded to the TUI binary.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Check for and install a newer c0mpute release.
    #[command(alias = "upgrade")]
    Update {
        /// Only check; don't apply the upgrade even if available.
        #[arg(long)]
        check: bool,
        /// Override the release-feed URL (defaults to the c0mpute.com one).
        #[arg(long)]
        feed: Option<String>,
    },

    /// Uninstall the c0mpute binary (and optionally peer binaries).
    #[command(alias = "remove")]
    Uninstall {
        /// Also remove `coinpay` and `infernet` from `~/.c0mpute/bin`.
        #[arg(long)]
        all: bool,
        /// Also remove the c0mpute config dir (`~/.config/c0mpute`).
        #[arg(long)]
        purge: bool,
        /// Skip the y/N confirmation.
        #[arg(long)]
        yes: bool,
    },

    /// Run the read-only status aggregator (DIP-0014).
    ///
    /// Boots an observer libp2p node that crawls the Kad-DHT and listens on
    /// the public gossipsub topics, then serves aggregate network health as
    /// JSON over HTTP. No roles are advertised — it never poses as a worker,
    /// and no private data is ever exposed. This is the service behind
    /// c0mpute.com/status; anyone can run their own and get the same numbers.
    #[command(name = "status-aggregator", alias = "aggregator")]
    StatusAggregator {
        /// Address to serve the status JSON on (`GET /`, plus `/healthz`).
        #[arg(long, env = "C0MPUTE_STATUS_BIND", default_value = "0.0.0.0:8080")]
        bind: std::net::SocketAddr,
    },

    /// Print the c0mpute binary version.
    Version,

    /// Show the live infernet distribution pool: how many nodes are serving
    /// each model right now (from the control plane's peer list). Pass a model
    /// id to see just that model + which nodes, or --node for the models one
    /// node advertises. To reserve a node: `c0mpute infernet reservation pricing`.
    Pool {
        /// Model id to filter on (substring match, e.g. `9b`). Omit for all.
        model: Option<String>,
        /// List the models one node advertises (its node_id or row id), live or not.
        #[arg(long, conflicts_with = "model")]
        node: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum WorkerCmd {
    /// Register this machine as a worker (requires a CoinPay worker DID).
    Register,
    /// Start the worker daemon and accept jobs.
    Start {
        /// Comma-separated roles: storage, transcode, tts, stt, gateway,
        /// verifier, workflow-step, video, diffusion, music. Default: the config's
        /// roles plus tts, stt, workflow-step, diffusion and music. Every node
        /// is also a disk provider (storage + hosting, listed on d1sks) on top
        /// of whatever is listed here, unless turned off below or in config.
        #[arg(long, value_delimiter = ',')]
        roles: Option<Vec<String>>,
        #[arg(long)]
        storage: Option<String>,
        /// Do not be a disk provider: no storage role, not listed on d1sks
        /// (same as `[storage] enabled = false` or `C0MPUTE_STORAGE=0`).
        #[arg(long)]
        no_storage: bool,
        /// Do not hold other people's sites (same as `[hosting] enabled =
        /// false` or `C0MPUTE_HOSTING=0`).
        #[arg(long)]
        no_hosting: bool,
        #[arg(long)]
        gpu: bool,
        /// Private mode for this run: only your own buyers, not advertised.
        /// (`c0mpute private on` makes it permanent.)
        #[arg(long)]
        private: bool,
        /// Start the worker as a background daemon, then stay attached and
        /// stream its log. Press Ctrl-D or Ctrl-C to detach — the worker keeps
        /// running (use `worker stop` to stop it). Attaches to an already
        /// running worker if there is one.
        #[arg(short = 'a', long, conflicts_with = "daemon")]
        attach: bool,
        /// Detach and run in the background as a daemon (writes a PID file and
        /// redirects output to a log). Use `worker stop` / `worker status`.
        #[arg(short = 'd', long)]
        daemon: bool,
    },
    /// Stop a running worker.
    Stop,
    /// Install the worker as a systemd service (a user unit by default) that
    /// starts at boot. Refuses when another unit already runs a worker for
    /// this user, so one machine never runs one identity twice.
    Install {
        /// A system unit (`/etc/systemd/system`, needs root) running as the
        /// invoking user instead of a user unit.
        #[arg(long)]
        system: bool,
        /// Do not start it now.
        #[arg(long)]
        no_start: bool,
    },
    /// Show worker status.
    Status {
        /// One JSON object instead of the text report (roles serving or
        /// gated and why included).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum JobCmd {
    /// Submit a job manifest JSON.
    Submit { manifest: PathBuf },
    /// Show status for a job ID.
    Status { id: String },
    /// Tail logs for a job ID.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
    },
    /// Cancel a queued/running job.
    Cancel { id: String },
}

#[derive(Subcommand, Debug)]
enum PluginCmd {
    /// List installed plugins.
    List,
    /// Install a plugin by id (from the c0mpute marketplace) or by URL.
    ///
    /// Examples:
    ///   c0mpute plugin install transcode
    ///   c0mpute plugin install https://example.com/my-plugin/install.sh
    Install { target: String },
    /// Enable a previously disabled plugin.
    Enable { id: String },
    /// Disable a plugin without uninstalling.
    Disable { id: String },
    /// Uninstall a plugin (alias: `delete`, `remove`).
    #[command(alias = "delete", alias = "remove")]
    Uninstall { id: String },
}

#[derive(Subcommand, Debug)]
enum TranscodeCmd {
    /// Submit an FFmpeg transcode job.
    Submit {
        input: PathBuf,
        #[arg(long, default_value = "video-1080p")]
        preset: String,
        #[arg(long)]
        max_price: Option<f64>,
        /// The job's id in the system billing it (an Infernet job id, a
        /// CoinPay invoice id). Sent on the offer and echoed on the
        /// receipt; billing keys on it, not on the per-auction job_id.
        #[arg(long)]
        external_ref: Option<String>,
        /// How the winning worker is picked: `smart` (most capable worker
        /// that is not overloaded, the default) or `cheapest` (lowest bid).
        #[arg(long, default_value = "smart")]
        policy: c0mpute_core::routing::SelectionPolicy,
        /// Download the transcoded output to this file (streamed to disk,
        /// checked against the receipt). Without it only the hash is printed.
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// How long to wait in the queue for a free worker before giving up,
        /// in seconds (default 2 h). While waiting it prints
        /// "queued, waiting for a worker (12 min)".
        #[arg(long, default_value_t = 7200)]
        max_wait: u64,
    },
    /// Manage transcode presets.
    Preset {
        #[command(subcommand)]
        cmd: PresetCmd,
    },
    /// Run local diagnostics for the transcode module.
    Doctor,
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    /// List available presets.
    List,
}

#[derive(Subcommand, Debug)]
enum TtsCmd {
    /// Install CPU TTS runtime and voice models for this worker.
    Setup {
        #[arg(long, default_value = "all", value_parser = ["all", "piper", "kokoro", "pocket"])]
        engine: String,
    },
    /// Show supported voices and the recommended OpenProfile voice map.
    Voices,
    /// Turn automatic Kokoro provisioning on or off for this node.
    ///
    /// Every node serves text-to-speech by default: a worker installs the CPU
    /// engines on first start and keeps retrying Kokoro in the background.
    /// `off` leaves TTS to nodes that opted in, and removes nothing already
    /// installed.
    Auto {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
}

#[derive(Subcommand, Debug)]
enum SttCmd {
    /// Install multilingual faster-whisper base and its CPU runtime.
    Setup,
}

#[derive(Subcommand, Debug)]
enum VideoCmd {
    /// Pull the runner image so this node can serve video jobs, building it
    /// from source if the registry will not serve it.
    Setup {
        /// Build from source without trying the registry first.
        #[arg(long)]
        build: bool,
        /// No GPU: install CPU text-to-video instead (Wan 2.1 1.3B on
        /// stable-diffusion.cpp, ~5.4 GB, slow batch clips). Opts this node in.
        #[arg(long)]
        cpu: bool,
    },
    /// Report whether this node can serve video, and why not if it cannot.
    Check {
        /// Also run the runner's own selftest inside the container. Needs the
        /// image, but neither a GPU nor model weights.
        #[arg(long)]
        selftest: bool,
    },
    /// List the model options the runner image ships with.
    Models,
}

#[derive(Subcommand, Debug)]
enum SpeechCmd {
    /// Serve /v1/tts, /v1/stt and /v1/images as a P2P buyer gateway.
    Serve {
        #[arg(long, env = "C0MPUTE_SPEECH_BIND", default_value = "127.0.0.1:8787")]
        bind: std::net::SocketAddr,
        #[arg(long, env = "C0MPUTE_SPEECH_MAX_PRICE_USD", default_value_t = 0.0)]
        max_price_usd: f64,
    },
}

/// `c0mpute dm` subcommands, still accepted after `c0mpute chat` for old scripts.
const SECURE_CHAT_CMDS: &[&str] = &[
    "keygen", "key", "restore", "send", "pull", "inbox", "lookup", "contacts", "block", "unblock",
];

#[derive(Parser, Debug)]
#[command(name = "c0mpute dm")]
struct DmCli {
    #[command(subcommand)]
    cmd: ChatCmd,
}

#[derive(Subcommand, Debug)]
enum AppsCmd {
    /// Deploy (or redeploy) an app.
    ///
    /// The image is built here (`docker build`), saved, and published to the
    /// network as a hosting site signed by this machine, so no registry is
    /// involved; or pass --pull with a digest-pinned registry image. Waits
    /// until a primary answers on the new version.
    ///
    ///   c0mpute apps deploy ./api --name api --port 8080 --state /data
    ///   c0mpute apps deploy --pull ghcr.io/me/api@sha256:... --name api
    ///   c0mpute apps deploy ./api        # settings from ./api/c0mpute.toml
    #[command(verbatim_doc_comment)]
    Deploy {
        /// Directory with a Dockerfile (and optionally c0mpute.toml).
        dir: Option<std::path::PathBuf>,
        /// App name: 1-59 of a-z, 0-9 and '-'. Same name again = new version.
        #[arg(long)]
        name: Option<String>,
        /// Deploy an image already in the local docker instead of building.
        #[arg(long, conflicts_with = "pull")]
        image: Option<String>,
        /// A registry image pinned by digest (name@sha256:...); holders pull it.
        #[arg(long)]
        pull: Option<String>,
        /// Port the container serves HTTP on.
        #[arg(long)]
        port: Option<u16>,
        /// Health check path (any answer below 500 is healthy).
        #[arg(long)]
        health: Option<String>,
        /// Directory inside the container that holds state (snapshotted to
        /// the standbys, restored on failover). Encrypted with the app key.
        #[arg(long)]
        state: Option<String>,
        /// Seconds between state snapshots (10-3600). Recovery point on failover.
        #[arg(long)]
        snapshot_secs: Option<u32>,
        /// Public environment variable KEY=VALUE (repeatable). Secrets go
        /// through `c0mpute apps secrets set`, never here.
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(long)]
        memory_mb: Option<u32>,
        /// CPU cores (0.1-8).
        #[arg(long)]
        cpus: Option<f64>,
        /// Copies the network keeps (2-5): one primary, the rest standbys.
        #[arg(long)]
        replicas: Option<u8>,
        /// Custom domain (repeatable): point it at gateways and add TXT
        /// `_c0mpute-app.<domain>` = `app=<app_id>`.
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// Peer id allowed to hold and run the app (repeatable). Without it
        /// any apps node may, and every holder receives the app's secrets.
        #[arg(long = "host")]
        hosts: Vec<String>,
        /// Give up waiting for a healthy primary after this many seconds.
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
    },
    /// List the apps a gateway knows, with holders, primary and health.
    Ls {
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
        #[arg(long)]
        json: bool,
    },
    /// One app's state on a gateway: holders, primary, epoch, last snapshot.
    Status {
        /// App name (deployed from here) or app id.
        app: String,
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
    /// The primary's container logs (owner only; asked over libp2p).
    Logs {
        app: String,
        #[arg(long, default_value_t = 200)]
        tail: u32,
    },
    /// Secret environment variables, sealed to each holder at the next deploy.
    Secrets {
        #[command(subcommand)]
        cmd: AppsSecretsCmd,
    },
    /// MCP server on stdio for agents (`claude mcp add c0mpute-apps -- c0mpute apps mcp`).
    Mcp {
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
    /// Delete an app: holders stop it and drop its copies and snapshots.
    Destroy {
        name: String,
        /// Required: this cannot be undone (the state goes with it).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AppsSecretsCmd {
    /// Set KEY=VALUE for an app (takes effect on the next deploy).
    Set { app: String, pairs: Vec<String> },
    /// Remove secrets by name.
    Unset { app: String, keys: Vec<String> },
    /// Show secret names (never values).
    Ls { app: String },
}

#[derive(Subcommand, Debug)]
enum HostingCmd {
    /// Publish (or update) a site from a directory of static files.
    ///
    /// The site is signed with this machine's identity, so only this machine
    /// can update it; publishing the same --name again ships a new version.
    /// Stays up until three hosting nodes hold copies, then exits.
    /// Sites over 1 GiB (up to 64 GiB free, 4 TiB with --disks) are striped
    /// over the network in 64 MiB groups (bigger past 512 GiB); files are served from the folder
    /// itself while the network copies them, so keep it running, and run the
    /// same command again to resume if it stops.
    #[command(verbatim_doc_comment)]
    Publish {
        /// Directory to publish (hidden files are skipped, .well-known kept).
        dir: std::path::PathBuf,
        /// Site name: 1-63 of a-z, 0-9 and '-'.
        #[arg(long)]
        name: String,
        /// A custom domain for the site (repeatable). Point it at gateways and
        /// add TXT `_c0mpute-hosting.<domain>` = `site=<site_id>`.
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// Give up waiting for network copies after this many seconds (for a
        /// striped site: this many seconds without progress).
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
        /// Pay hosting nodes from the d1sks OpenDisk marketplace to hold the
        /// site: `auto`, or a d1sks query such as
        /// `country=DE,FI&max_price=0.01&min_standing=50`. Without it the
        /// site is free and the network keeps 3 copies as usual.
        #[arg(long)]
        disks: Option<String>,
        /// Highest price per GiB-month (USD) a preferred host may ask.
        #[arg(long)]
        max_price: Option<String>,
        /// Only hosts in these countries (ISO codes, comma separated).
        #[arg(long)]
        country: Option<String>,
        /// How many preferred (paid) hosts to pick, 1-5.
        #[arg(long, default_value_t = 3)]
        copies: usize,
        /// The OpenDisk marketplace to pick hosts from (discovery only).
        #[arg(long, default_value = c0mpute_core::opendisk::DEFAULT_D1SKS_URL)]
        d1sks: String,
        /// Publish a private site: encrypted on this machine, served only to
        /// people with its link (the browser decrypts; gateways see only
        /// ciphertext). Sites are public by default.
        #[arg(long)]
        private: bool,
        /// With --private: draw a new key, killing every link given out so far.
        #[arg(long, requires = "private")]
        rekey: bool,
        /// The gateway the printed link points at (public and --private sites).
        #[arg(long, default_value = c0mpute_core::sealed_store::DEFAULT_GATEWAY)]
        gateway: String,
    },
    /// Show the c0mpute hosting nodes d1sks lists, ranked the way
    /// `publish --disks` would pick them.
    Disks {
        /// `auto` or a d1sks query (see `publish --disks`).
        #[arg(long, default_value = "auto")]
        query: String,
        #[arg(long)]
        max_price: Option<String>,
        #[arg(long)]
        country: Option<String>,
        #[arg(long, default_value = c0mpute_core::opendisk::DEFAULT_D1SKS_URL)]
        d1sks: String,
        /// Also join the hosting gossip (about a minute) and mark which
        /// listed disks are announcing themselves as live hosts right now.
        #[arg(long)]
        live: bool,
    },
    /// Take a site down: publish an empty version of it. Holders switch to
    /// it and delete the old files; gateways answer 404.
    Rm {
        /// Site name (as published from this machine).
        #[arg(long)]
        name: String,
        /// Seconds to keep telling the network.
        #[arg(long, default_value_t = 90)]
        wait_secs: u64,
    },
    /// Download one file of a site from the network (as a gateway would),
    /// showing the transfer rate.
    Get {
        /// Site id.
        site: String,
        /// Path in the site, e.g. `movies/a.mp4`.
        path: String,
        /// Where to write it (default: the file name).
        #[arg(short, long)]
        out: Option<std::path::PathBuf>,
        /// One chunk at a time from one peer (how releases up to 0.3.2
        /// fetched), to compare.
        #[arg(long)]
        sequential: bool,
        /// Only fetch from these peers: peer ids or full multiaddrs
        /// (repeatable). They are asked for every chunk.
        #[arg(long)]
        from: Vec<String>,
    },
    /// List the sites a gateway knows, with live copy counts.
    Ls {
        /// Gateway to ask.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
        /// Only the sites this machine published (signed with its key).
        #[arg(long)]
        mine: bool,
        /// Print the gateway's JSON (`{target_copies, sites: [...]}`).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum CaptchaCmd {
    /// Install ddddocr so this node serves `captcha.solve` (a worker does
    /// this by itself in the background).
    Setup,
    /// Show whether this machine can serve captcha jobs.
    Status,
}

#[derive(Subcommand, Debug)]
enum EmbedCmd {
    /// Install fastembed and download the model(s) so this node serves
    /// `embed.text` (a worker does this by itself in the background).
    Setup {
        /// Also install nomic-embed-text-v1.5 (~550 MB) besides bge-small.
        #[arg(long)]
        all: bool,
    },
    /// Show which embedding models this machine can serve.
    Status,
}

#[derive(Subcommand, Debug)]
enum PodcastingCmd {
    /// Build a show and publish it (a new version each time).
    ///
    /// Input is a show folder (every audio file in it, plus optional
    /// show.toml and episodes.toml) or a list of audio files. The show is
    /// written to <data_dir>/podcasting/<name>/site and published as hosting
    /// site <name>: feed at /s/<site_id>/feed.xml on every gateway.
    #[command(verbatim_doc_comment)]
    Publish {
        /// Show folder, or audio files (mp3, m4a, aac, ogg, opus, flac, wav).
        #[arg(required = true)]
        inputs: Vec<std::path::PathBuf>,
        /// Show (= hosting site) name: 1-63 of a-z, 0-9 and '-'.
        #[arg(long)]
        name: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        author: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Cover art (jpg, png or webp; 1400-3000 px square for Apple).
        #[arg(long)]
        image: Option<std::path::PathBuf>,
        #[arg(long)]
        language: Option<String>,
        /// iTunes category, e.g. "Technology".
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        explicit: bool,
        /// Owner email (itunes:owner, podcast:locked owner).
        #[arg(long)]
        email: Option<String>,
        /// Write podcast:locked "no" (allow other hosts to import the show).
        #[arg(long)]
        unlocked: bool,
        /// Public URL of the site, for absolute enclosure URLs (defaults to
        /// https://<first --domain>/; without either, URLs are relative).
        #[arg(long)]
        base_url: Option<String>,
        /// Custom domain (repeatable), as in `c0mpute hosting publish`.
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// podcast:value recipient address (e.g. a Lightning node pubkey).
        /// No value block is written without it.
        #[arg(long)]
        value_address: Option<String>,
        #[arg(long, default_value = "lightning")]
        value_type: String,
        #[arg(long, default_value = "keysend")]
        value_method: String,
        /// Opt in to dynamic ad insertion (podcast-ads) for these breaks:
        /// comma list of pre, mid, post. Same as an [ads] table in show.toml.
        /// Without it (or [ads]) no gateway ever inserts ads.
        #[arg(long, value_name = "BREAKS")]
        ads: Option<String>,
        /// Where the show's share of ad revenue goes, <COIN>:<address>
        /// (e.g. SOL:9xQe...). Same as `payout` in [ads]. Without it the show
        /// runs ads unpaid.
        #[arg(long, value_name = "COIN:ADDRESS")]
        ads_payout: Option<String>,
        /// Build the show folder only; do not publish.
        #[arg(long)]
        dry_run: bool,
        /// Give up waiting for network copies after this many seconds.
        #[arg(long, default_value_t = 600)]
        wait_secs: u64,
    },
}

#[derive(Subcommand, Debug)]
enum AdsCmd {
    /// Create, change, pause and list your campaigns.
    Campaign {
        #[command(subcommand)]
        cmd: AdsCampaignCmd,
    },
    /// Count a campaign's impressions from the gateways' signed receipts.
    Report {
        /// Campaign name (local) or id.
        campaign: String,
        /// Also fetch receipts straight from these gateways (repeatable).
        #[arg(long = "gateway")]
        gateways: Vec<String>,
        /// How long to collect receipts over gossip.
        #[arg(long, default_value_t = 20)]
        wait_secs: u64,
        #[arg(long)]
        json: bool,
    },
    /// Verify gateways' payment claims for a campaign and (with --pay) settle
    /// and pay them through the node's payment rail ([payments]). Without
    /// --pay it only reports.
    Settle {
        /// Campaign name (local) or id.
        campaign: String,
        /// Sign settlements and send payouts. Also needs `[ads] payouts =
        /// true` and `[payments] enabled = true` in config.toml: real money
        /// moves.
        #[arg(long)]
        pay: bool,
        /// How long to collect claims and receipts over gossip.
        #[arg(long, default_value_t = 20)]
        wait_secs: u64,
        #[arg(long)]
        json: bool,
    },
    /// Ask a gateway which campaign it would serve, and why the others lost.
    Select {
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
        /// podcast_pre|podcast_mid|podcast_post|ctv_preroll|ctv_midroll|ctv_postroll|display
        #[arg(long)]
        placement: String,
        #[arg(long)]
        category: Vec<String>,
        #[arg(long)]
        keyword: Vec<String>,
        #[arg(long)]
        country: Option<String>,
    },
}

/// Targeting and money flags shared by create and update.
#[derive(clap::Args, Debug, Default)]
struct AdsTargetArgs {
    /// Creative files or folders (e.g. an ad-generator-agent output folder
    /// with an optional ad-set.json). On update, replaces the creatives.
    #[arg(long = "creative")]
    creatives: Vec<std::path::PathBuf>,
    /// Where it runs (comma list): pre, mid, post (podcast), ctv_preroll,
    /// ctv_midroll, ctv_postroll, display.
    #[arg(long)]
    placement: Option<String>,
    /// Highest CPM in USD, e.g. 2.50. The winner pays the second price (best
    /// losing bid or the show's floor), never more than this.
    #[arg(long)]
    max_cpm: Option<String>,
    /// Total budget in USD. With a budget, gateways claim payment for
    /// verified impressions and `c0mpute ads settle` pays them; without one
    /// the campaign runs unpaid.
    #[arg(long)]
    budget: Option<String>,
    /// Most the campaign pays for one UTC day of impressions, USD.
    #[arg(long)]
    daily_budget: Option<String>,
    /// Your paying CoinPay wallet, <COIN>:<address> (claims paying out to it
    /// are refused as self-dealing).
    #[arg(long, value_name = "COIN:ADDRESS")]
    payer: Option<String>,
    /// A CoinPay escrow or payment id that shows the budget is funded.
    #[arg(long)]
    funding_ref: Option<String>,
    /// Where a click goes.
    #[arg(long)]
    landing: Option<String>,
    /// Categories (repeatable or comma list). None = any.
    #[arg(long)]
    category: Vec<String>,
    #[arg(long)]
    keyword: Vec<String>,
    /// Countries, ISO 3166-1 alpha-2 (US,CA). None = anywhere.
    #[arg(long)]
    geo: Vec<String>,
    /// Start (YYYY-MM-DD, RFC 3339 or unix seconds).
    #[arg(long)]
    start: Option<String>,
    #[arg(long)]
    end: Option<String>,
    /// Frequency cap per listener: N/hour, N/day, N/week or N/<seconds>.
    #[arg(long)]
    freq_cap: Option<String>,
    /// Duration for audio/video creatives when ffprobe is not installed.
    #[arg(long)]
    duration_secs: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum AdsCampaignCmd {
    /// Publish a new campaign: creatives go to a hosting site you own, the
    /// signed record is gossiped to every gateway.
    Create {
        /// Campaign name: 1-63 of a-z, 0-9 and '-'.
        #[arg(long)]
        name: String,
        #[command(flatten)]
        t: AdsTargetArgs,
        #[arg(long, default_value_t = 300)]
        wait_secs: u64,
    },
    /// Publish a new version with changed fields.
    Update {
        campaign: String,
        #[command(flatten)]
        t: AdsTargetArgs,
        #[arg(long, default_value_t = 300)]
        wait_secs: u64,
    },
    /// Stop serving (a signed new version with paused = true).
    Pause { campaign: String },
    /// Serve again.
    Resume { campaign: String },
    /// Your campaigns (or, with --gateway, every campaign a gateway knows).
    Ls {
        #[arg(long)]
        gateway: Option<String>,
    },
    /// The signed record of one of your campaigns.
    Show { campaign: String },
}

#[derive(Subcommand, Debug)]
enum PodcastsCmd {
    /// Subscribe to a show: a c0mpute site id (or c0mpute://site/<id>) or
    /// any http(s) RSS feed URL.
    Subscribe {
        source: String,
        #[command(flatten)]
        via: PodcastsVia,
    },
    /// Remove a subscription.
    Unsubscribe { show: String },
    /// List subscriptions.
    Ls,
    /// List a show's episodes, newest first (refreshes the feed).
    Episodes {
        /// Subscription id, number from `ls`, site id or title fragment.
        show: String,
        /// Use the stored copy of the feed; do not fetch.
        #[arg(long)]
        offline: bool,
        #[command(flatten)]
        via: PodcastsVia,
    },
    /// Download an episode (default: the newest).
    Download {
        show: String,
        /// Number from `episodes`, guid, or title fragment.
        episode: Option<String>,
        /// Directory to save into (default <data_dir>/podcasts/episodes/<show>).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        #[command(flatten)]
        via: PodcastsVia,
    },
    /// Download and play an episode with mpv or ffplay (prints the path when
    /// neither is installed).
    Play {
        show: String,
        episode: Option<String>,
        #[command(flatten)]
        via: PodcastsVia,
    },
}

#[derive(clap::Args, Debug)]
struct PodcastsVia {
    /// Gateway that serves c0mpute shows (/s/<site_id>/feed.xml).
    #[arg(long, default_value = c0mpute_core::podcast::listen::DEFAULT_GATEWAY)]
    gateway: String,
    /// Read c0mpute shows straight from the network: run a hosting client on
    /// this machine's buyer identity instead of asking a gateway.
    #[arg(long)]
    p2p: bool,
    /// With --p2p: how long to wait to learn the show from gossip.
    #[arg(long, default_value_t = 360)]
    wait_secs: u64,
}

#[derive(Subcommand, Debug)]
enum StreamCmd {
    /// Broadcast a live stream to the network.
    ///
    /// INPUT is a file (played at native speed), an ffmpeg URL (rtmp://,
    /// srt://, udp://, http://...) or `-` for stdin. Needs ffmpeg on PATH;
    /// RTMP/SRT ingest is ffmpeg's. The stream is signed with this machine's
    /// identity, so its id stays the same for the same --name.
    ///
    ///   c0mpute stream go show.mp4 --name my-show
    ///   c0mpute stream go rtmp://0.0.0.0:1935/live/key --listen --name my-show
    ///   c0mpute stream go testsrc=size=1280x720:rate=30 --format lavfi --name test
    #[command(verbatim_doc_comment)]
    Go {
        input: String,
        /// Stream name: 1-63 of a-z, 0-9 and '-'.
        #[arg(long)]
        name: String,
        /// ffmpeg input format (-f), e.g. lavfi, flv, mpegts.
        #[arg(long)]
        format: Option<String>,
        /// Target segment length in seconds (2-10).
        #[arg(long, default_value_t = 4)]
        segment_secs: u32,
        /// Segments per rendition in the live window.
        #[arg(long, default_value_t = 6)]
        window: usize,
        /// Also encode a 480p rendition (libx264) next to the source.
        #[arg(long)]
        low: bool,
        /// Source rendition: copy (no re-encode), x264, or auto
        /// (x264 for lavfi/raw input, copy otherwise).
        #[arg(long, default_value = "auto")]
        codec: String,
        /// Receive a push (ffmpeg -listen 1), e.g. on rtmp://0.0.0.0:1935/live/key.
        #[arg(long)]
        listen: bool,
        /// Also serve the stream locally for a preview, e.g. 127.0.0.1:7778.
        #[arg(long)]
        bind: Option<std::net::SocketAddr>,
        /// After the input ends, stay up this many seconds for relays.
        #[arg(long, default_value_t = 30)]
        linger_secs: u64,
        /// Do not restart ffmpeg when a network input drops.
        #[arg(long)]
        no_reconnect: bool,
    },
    /// Print a stream's playlist URL on a gateway and play it with mpv or
    /// ffplay when one is installed.
    Watch {
        stream_id: String,
        /// Gateway to watch through.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
        /// Only print the URL.
        #[arg(long)]
        no_play: bool,
    },
    /// List the live streams a gateway knows.
    Ls {
        /// Gateway to ask.
        #[arg(long, default_value = "http://127.0.0.1:7777")]
        gateway: String,
    },
}

const CRAWL_EXAMPLES: &str = "\
Examples:
  # one page, fetched from a worker's IP (static GET, robots.txt honoured)
  c0mpute crawl fetch https://example.com/ --max-price 0.01
  c0mpute crawl fetch https://mysite.example/ --ignore-robots   # only a site you own

  # many pages, spread over the network: one crawl.fetch job per URL
  # through the jobs bridge (each URL is its own auction)
  export C0MPUTE_JOBS_TOKEN=$(openssl rand -hex 24)
  c0mpute jobs serve --max-price 0.01 &
  grep -v '^#' urls.txt | xargs -P 8 -I{} curl -s http://127.0.0.1:8788/v1/jobs \\
    -H \"Authorization: Bearer $C0MPUTE_JOBS_TOKEN\" \\
    -d '{\"workload\":\"crawl.fetch\",\"spec\":{\"v\":1,\"url\":\"{}\"}}'

Full guide: https://c0mpute.com/getting-started#distributed-scraper
Runnable script: examples/distributed-scraper in github.com/profullstack/c0mpute";

const JOBS_EXAMPLES: &str = "\
Examples:
  export C0MPUTE_JOBS_TOKEN=$(openssl rand -hex 24)
  c0mpute jobs serve --listen 127.0.0.1:8788 --max-price 0.01

  # distributed scraper: one crawl.fetch per URL, each its own auction
  curl -s http://127.0.0.1:8788/v1/jobs -H \"Authorization: Bearer $C0MPUTE_JOBS_TOKEN\" \\
    -d '{\"workload\":\"crawl.fetch\",\"spec\":{\"v\":1,\"url\":\"https://example.com/\"},\"max_price\":0.005}'

  # webhook workflow step (meshhook)
  curl -s http://127.0.0.1:8788/v1/jobs -H \"Authorization: Bearer $C0MPUTE_JOBS_TOKEN\" \\
    -d '{\"workload\":\"workflow.step\",\"spec\":{\"v\":1,\"kind\":\"transform\",
         \"config\":{\"expression\":\"items[].name\"},\"input\":{\"items\":[{\"name\":\"a\"}]}}}'

Full guide: https://c0mpute.com/docs#crawl";

#[derive(Subcommand, Debug)]
enum CrawlCmd {
    /// Fetch one URL from a worker and print the result JSON
    /// (`{v, ok, output: {url, status, headers, body}}`).
    #[command(after_help = CRAWL_EXAMPLES)]
    Fetch {
        url: String,
        /// Highest price in USD you will pay for the fetch.
        #[arg(long, default_value_t = 0.01)]
        max_price: f64,
        /// Skip the robots.txt check (only for a site you own).
        #[arg(long)]
        ignore_robots: bool,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
}

#[derive(Subcommand, Debug)]
enum JobsCmd {
    /// Serve POST /v1/jobs as a P2P buyer bridge (meshhook workflow steps).
    ///
    /// Requires C0MPUTE_JOBS_TOKEN (24-256 chars); callers send it as
    /// `Authorization: Bearer <token>`. Binds loopback by default; put it
    /// behind HTTPS or a private network if callers are remote.
    /// C0MPUTE_JOBS_ALLOW_PEERS=peerA,peerB limits which workers may bid.
    ///
    /// Step payloads (inputs, URLs, headers, bodies) are visible to the
    /// winning worker: keep secrets off the network or pin trusted workers.
    #[command(verbatim_doc_comment, after_help = JOBS_EXAMPLES)]
    Serve {
        #[arg(long, env = "C0MPUTE_JOBS_LISTEN", default_value = c0mpute_core::jobs_gateway::DEFAULT_BIND)]
        listen: std::net::SocketAddr,
        /// Highest price per job in USD; a request's max_price may only lower it.
        #[arg(long, env = "C0MPUTE_JOBS_MAX_PRICE_USD", default_value_t = 0.0)]
        max_price: f64,
        /// How long each auction collects bids, in milliseconds.
        #[arg(long, env = "C0MPUTE_JOBS_BID_WINDOW_MS", default_value_t = 2000)]
        bid_window_ms: u64,
    },
}

#[derive(Subcommand, Debug)]
enum ChatCmd {
    /// Generate a new keypair and write the encrypted keyfile.
    Keygen,
    /// Key management subcommands.
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Restore a keypair from a backup JSON file.
    Restore {
        #[arg(long = "from-backup")]
        from_backup: PathBuf,
    },
    /// Send an end-to-end encrypted message. TO is a contact name, an
    /// address, a fingerprint, or a `c0dm1.` identity string. Delivered
    /// directly to the recipient's node when it is online, otherwise held on
    /// three mailbox nodes for up to --ttl-days.
    Send {
        to: String,
        message: String,
        /// Days mailboxes keep the message for an offline recipient (max 30).
        #[arg(long, default_value_t = 30)]
        ttl_days: u64,
        /// Accepted for old scripts; every message is sealed now.
        #[arg(long, hide = true)]
        sealed: bool,
    },
    /// Fetch new messages (this machine's node spool and the mailboxes),
    /// decrypt, verify, and delete them from the network.
    Pull,
    /// Show received messages.
    Inbox {
        /// Most recent N messages.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Resolve a fingerprint or address to a full public key.
    Lookup { query: String },
    /// Contacts (stored locally, encrypted under your key). Lists by default.
    Contacts {
        #[command(subcommand)]
        cmd: Option<ContactsCmd>,
    },
    /// Block a sender: their messages are dropped (and deleted) on receipt.
    Block { who: String },
    /// Unblock a sender.
    Unblock { who: String },
}

#[derive(Subcommand, Debug)]
enum ContactsCmd {
    /// Add a contact by address, fingerprint, or `c0dm1.` identity string.
    Add {
        who: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List contacts.
    Ls,
    /// Remove a contact.
    Rm { who: String },
}

#[derive(Subcommand, Debug)]
enum KeyCmd {
    /// Print your address and fingerprint (and, with --full, the shareable identity).
    Show {
        /// Also print the `c0dm1.` identity string others can paste.
        #[arg(long)]
        full: bool,
    },
    /// Re-export the encrypted backup JSON (pipe to a file for safe storage).
    Export,
    /// How to replace your keypair.
    Rotate,
}

/// Opportunistic self-update run at the start of any command. Throttled to once
/// per 60 seconds (a marker under the data dir); when a newer release exists it
/// downloads + verifies + swaps the binary and re-executes the same command on
/// it. This keeps `c0mpute` current even when no worker is running — the
/// worker's poll loop only updates while the worker is up. Opt out with
/// `C0MPUTE_NO_AUTO_UPDATE=1`.
#[cfg(unix)]
fn maybe_self_update(cli: &Cli) {
    // `update` handles this itself; don't double up. Opt-out for CI/scripts.
    if matches!(cli.command, Some(Cmd::Update { .. })) {
        return;
    }
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    let configured = Config::load_or_default(&config_path)
        .map(|cfg| cfg.update_auto)
        .unwrap_or(true);
    if !c0mpute_update::auto_update_enabled(configured) {
        return;
    }
    let marker = config::data_dir().map(|d| d.join(".update-check"));
    if let Some(m) = &marker {
        if let Ok(Ok(elapsed)) = std::fs::metadata(m)
            .and_then(|md| md.modified())
            .map(|t| t.elapsed())
        {
            if elapsed < std::time::Duration::from_secs(config::UPDATE_CHECK_INTERVAL_SECS) {
                return;
            }
        }
        if let Some(parent) = m.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(m, b"");
    }

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    if let Ok(c0mpute_update::UpgradeOutcome::Upgraded { .. }) = rt.block_on(
        c0mpute_update::upgrade_now(current, c0mpute_update::DEFAULT_RELEASE_FEED),
    ) {
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("c0mpute"));
        let args: Vec<String> = std::env::args().skip(1).collect();
        // exec only returns on failure; if it fails, fall through and run the
        // requested command on the current (pre-swap) image.
        let _ = std::process::Command::new(exe).args(args).exec();
    }
}

#[cfg(not(unix))]
fn maybe_self_update(_cli: &Cli) {}

/// A server or worker (`... serve`, `worker start`, `status-aggregator`): a
/// process that must survive a peer closing a pipe on it, so it keeps Rust's
/// ignored SIGPIPE (writes fail with EPIPE instead of killing it).
fn long_running(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .any(|a| matches!(a.as_str(), "serve" | "start" | "status-aggregator"))
}

#[cfg(test)]
mod sigpipe_tests {
    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn daemons_keep_sigpipe_ignored() {
        assert!(super::long_running(&argv("c0mpute worker start")));
        assert!(super::long_running(&argv("c0mpute worker start --roles tts,stt")));
        assert!(super::long_running(&argv("c0mpute speech serve --bind 0.0.0.0:8787")));
        assert!(super::long_running(&argv("c0mpute jobs serve")));
        assert!(super::long_running(&argv("c0mpute status-aggregator --bind 127.0.0.1:8091")));
        assert!(!super::long_running(&argv("c0mpute storage ls")));
        assert!(!super::long_running(&argv("c0mpute captcha code.png --plain")));
        assert!(!super::long_running(&argv("c0mpute embed -- start")));
    }
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so writing to a closed pipe returns EPIPE and the
    // stdlib panics on it. For a CLI that prints lists that is a crash on
    // `c0mpute storage ls | head`, which is ordinary shell usage. Restore the
    // default disposition so the process exits quietly instead.
    //
    // Not for daemons, though: with the default disposition one write to a
    // closed pipe kills `worker start` / `speech serve` outright, and systemd
    // counts death by SIGPIPE as a clean exit, so `Restart=on-failure` never
    // brings the node back. That took down dev2's gateway (and every Moshpit
    // name behind edge.c0mpute.com) during the 0.2.100 self-update restart.
    #[cfg(unix)]
    unsafe {
        let args: Vec<String> = std::env::args().collect();
        if !long_running(&args) {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    // Run as `git-remote-c0mpute` (a link to this binary): the git remote helper.
    if let Some(code) = git_cli::as_remote_helper() {
        std::process::exit(code);
    }

    let cli = Cli::parse();

    // Opportunistic self-update on any command (throttled), so c0mpute stays
    // current even when no worker is running. May re-exec into the new binary.
    maybe_self_update(&cli);

    // `-v` / `--versions`: full version report, no async runtime needed.
    if cli.versions {
        print_all_versions();
        return Ok(());
    }

    // Attach mode: launch the worker as a detached daemon (or find the one
    // already running) and follow its log until Ctrl-D / Ctrl-C. This is a
    // plain supervisor + file tail — it never builds the async runtime, so it
    // can hand off to the daemon and return your shell without killing it.
    if let Some(Cmd::Worker {
        cmd:
            WorkerCmd::Start {
                attach: true,
                roles,
                storage,
                no_storage,
                no_hosting,
                gpu,
                private,
                daemon: _,
            },
    }) = &cli.command
    {
        let mut passthrough = Vec::new();
        if *private {
            passthrough.push("--private".to_string());
        }
        if *no_storage {
            passthrough.push("--no-storage".to_string());
        }
        if *no_hosting {
            passthrough.push("--no-hosting".to_string());
        }
        if let Some(rs) = roles {
            passthrough.push("--roles".to_string());
            passthrough.push(rs.join(","));
        }
        if let Some(s) = storage {
            passthrough.push("--storage".to_string());
            passthrough.push(s.clone());
        }
        if *gpu {
            passthrough.push("--gpu".to_string());
        }
        if let Some(cfg) = &cli.config {
            passthrough.push("--config".to_string());
            passthrough.push(cfg.display().to_string());
        }
        return run_attached_worker(passthrough);
    }

    // One worker per identity on this machine: take the identity lock now,
    // replacing a worker that already runs it (newest start wins), before
    // any setup and before a daemon forks (the child keeps the lock).
    if let Some(Cmd::Worker {
        cmd: WorkerCmd::Start { attach: false, .. },
    }) = &cli.command
    {
        let dir = c0mpute_core::config::config_dir().unwrap_or_else(|| PathBuf::from("."));
        if let Err(e) = c0mpute_core::instance::claim(&dir) {
            exit_if_duplicate(&e);
            return Err(e);
        }
    }

    // Daemonize BEFORE the async runtime starts. Forking is unsafe once the
    // multi-threaded Tokio runtime has spawned worker threads, so this must
    // happen while the process is still single-threaded. Only `worker start
    // --daemon` detaches; every other command runs in the foreground.
    if let Some(Cmd::Worker {
        cmd: WorkerCmd::Start { daemon: true, .. },
    }) = &cli.command
    {
        daemonize_worker()?;
        // The daemon child keeps the lock taken above; record its own pid
        // (and restart the takeover watch: threads do not survive a fork).
        let dir = c0mpute_core::config::config_dir().unwrap_or_else(|| PathBuf::from("."));
        c0mpute_core::instance::claim(&dir)?;
    }

    if matches!(&cli.command, Some(Cmd::Git { .. })) {
        c0mpute_core::init_tracing_with("warn")?;
    } else {
        init_tracing()?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let out = runtime.block_on(run_app(cli));
    if let Err(e) = &out {
        exit_if_duplicate(e);
    }
    out
}

/// A worker that could not take over the identity exits with a code every
/// c0mpute unit lists in `RestartPreventExitStatus`, so systemd does not
/// restart it in a loop.
fn exit_if_duplicate(e: &anyhow::Error) {
    if e.downcast_ref::<c0mpute_core::instance::DuplicateWorker>().is_some() {
        eprintln!("c0mpute worker: not started: {e}");
        std::process::exit(c0mpute_core::instance::EXIT_DUPLICATE);
    }
}

fn start_auto_update_poller(configured: bool) {
    if !c0mpute_update::auto_update_enabled(configured) {
        return;
    }
    let current = env!("CARGO_PKG_VERSION").to_string();
    let feed = c0mpute_update::DEFAULT_RELEASE_FEED.to_string();
    let interval = std::time::Duration::from_secs(config::UPDATE_CHECK_INTERVAL_SECS);
    tokio::spawn(c0mpute_update::poll_loop(current, feed, interval));
}

async fn run_app(cli: Cli) -> Result<()> {
    let config_path = cli.config.unwrap_or_else(config::default_config_path);

    // Private mode is process-wide: every worker loop (transcode, speech,
    // text, temporal, webhook) checks it before bidding. An unreadable
    // config still honours C0MPUTE_PRIVATE.
    let private_cfg = Config::load_or_default(&config_path)
        .map(|c| c.private)
        .unwrap_or_default();
    c0mpute_core::private::install_from(&private_cfg);

    // No subcommand: open the interactive menu in a terminal, else print help
    // (keeps piped/non-TTY and CI usage unchanged).
    let Some(command) = cli.command else {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return run_menu();
        }
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Cmd::Version => {
            print_all_versions();
            Ok(())
        }
        Cmd::Pool { node: Some(id), .. } => run_pool_node(&id),
        Cmd::Pool { model, node: None } => run_pool(model),
        Cmd::Doctor { fix } => run_doctor(fix).await,
        Cmd::Bench {
            cmd,
            quick,
            json,
            threads,
        } => run_bench(cmd, quick, json, threads),
        Cmd::StatusAggregator { bind } => {
            let cfg = Config::load_or_default(&config_path)?;
            start_auto_update_poller(cfg.update_auto);
            c0mpute_core::status_aggregator::run(bind).await
        }
        Cmd::Pricing { cmd } => run_pricing(cmd, &config_path),
        Cmd::Name { handle, clear } => run_name(handle, clear),
        Cmd::Private { cmd } => run_private(cmd, &config_path),
        Cmd::Worker { cmd } => run_worker(cmd, &config_path).await,
        Cmd::Job { cmd } => run_job(cmd).await,
        Cmd::Storage { cmd } => storage::run(cmd, &config_path).await,
        Cmd::Plugin { cmd } => run_plugin(cmd),

        Cmd::Transcode { cmd } => run_transcode(cmd).await,
        Cmd::Tts { cmd } => run_tts(cmd, &config_path).await,
        Cmd::Stt { cmd } => run_stt(cmd).await,
        Cmd::Video { cmd } => run_video(cmd).await,
        Cmd::Crowdgpt { cmd } => crowdgpt::run(cmd, &config_path).await,
        Cmd::Status { json } => run_worker(WorkerCmd::Status { json }, &config_path).await,
        Cmd::Speech { cmd } => {
            let cfg = Config::load_or_default(&config_path)?;
            start_auto_update_poller(cfg.update_auto);
            run_speech(cmd).await
        }
        Cmd::Jobs { cmd } => {
            let cfg = Config::load_or_default(&config_path)?;
            start_auto_update_poller(cfg.update_auto);
            run_jobs(cmd).await
        }
        Cmd::Crawl { cmd } => run_crawl(cmd).await,
        Cmd::Hosting { cmd } => run_hosting(cmd).await,
        Cmd::Apps { cmd } => run_apps(cmd).await,
        Cmd::Build(args) => container_cli::run_build(args, &config_path).await,
        Cmd::Run(args) => container_cli::run_run(args, &config_path).await,
        Cmd::Containers { cmd } => container_cli::run_containers(cmd, &config_path),
        Cmd::Train { args } => train::run(&args).await,
        Cmd::Gameserver { args } => gameserver::run(&args).await,
        Cmd::Git { args } => git_cli::run(&args).await,
        Cmd::Stream { cmd } => run_stream(cmd).await,
        Cmd::Ocr {
            file,
            lang,
            max_price,
            timeout_secs,
            json,
        } => run_ocr(file, lang, max_price, timeout_secs, json).await,
        Cmd::Render {
            file,
            frames,
            engine,
            device,
            format,
            resolution,
            samples,
            out,
            batch,
            concurrency,
            attempts,
            max_price,
            frame_timeout_secs,
            mp4,
            fps,
        } => {
            run_render(RenderArgs {
                file,
                frames,
                engine,
                device,
                format,
                resolution,
                samples,
                out,
                batch,
                concurrency,
                attempts,
                max_price,
                frame_timeout_secs,
                mp4,
                fps,
            })
            .await
        }
        Cmd::Embed {
            cmd,
            texts,
            model,
            max_price,
            timeout_secs,
        } => run_embed(cmd, texts, model, max_price, timeout_secs).await,
        Cmd::Captcha {
            cmd,
            file,
            kind,
            background,
            charset,
            case,
            question,
            plain,
            max_price,
            timeout_secs,
        } => {
            run_captcha(CaptchaArgs {
                cmd,
                file,
                kind,
                background,
                charset,
                case,
                question,
                plain,
                max_price,
                timeout_secs,
            })
            .await
        }
        Cmd::Podcasting { cmd } => run_podcasting(cmd).await,
        Cmd::Podcasts { cmd } => run_podcasts(cmd).await,
        Cmd::Ads { cmd } => run_ads(cmd).await,
        Cmd::Dm { cmd } => run_chat(cmd).await,
        // The old secure-chat forms (`c0mpute chat keygen|send|...`) keep working.
        Cmd::Chat { args } if args.first().is_some_and(|a| SECURE_CHAT_CMDS.contains(&a.as_str())) => {
            let dm = DmCli::try_parse_from(std::iter::once("c0mpute dm".to_string()).chain(args))
                .unwrap_or_else(|e| e.exit());
            run_chat(dm.cmd).await
        }
        Cmd::Chat { args } => ai_chat::run(&args).await,
        Cmd::Promo(args) => promo::run(args).await,
        Cmd::Tui { args } => run_tui(&args),
        Cmd::Update { check, feed } => run_update(check, feed).await,
        Cmd::Uninstall { all, purge, yes } => run_uninstall(all, purge, yes),
        Cmd::Coinpay { args } => delegate("coinpay", &args),
        Cmd::Login => run_login(),
        Cmd::Webhook { args } => webhook::run(&args).await,
        Cmd::Infernet { args } if args.first().map(String::as_str) == Some("text") => {
            text::run(&args[1..]).await
        }
        Cmd::Infernet { args } if args.first().map(String::as_str) == Some("temporal") => {
            temporal::run(&args[1..]).await
        }
        Cmd::Infernet { mut args } => {
            // Default the network to c0mpute when caller didn't specify.
            if matches!(args.first().map(String::as_str), Some("run"))
                && !args.iter().any(|a| a == "--network")
            {
                args.push("--network".into());
                args.push("c0mpute".into());
            }
            delegate("infernet", &args)
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// doctor
// ────────────────────────────────────────────────────────────────────────

async fn run_doctor(fix: bool) -> Result<()> {
    let local = c0mpute_doctor::run().await;
    for r in &local {
        println!("{:5} {} — {:?}", status_label(&r.status), r.name, r.status);
    }
    print_worker_census(fix);

    println!("{:5} c0mpute — Ok (this binary)", "OK");
    println!(
        "{:5} coinpay — {}",
        peer_label("coinpay"),
        peer_status_text("coinpay")
    );
    println!(
        "{:5} infernet — {}",
        peer_label("infernet"),
        peer_status_text("infernet")
    );

    Ok(())
}

#[cfg(not(unix))]
fn print_worker_census(_fix: bool) {
    println!("INFO  workers: the duplicate-worker census needs Linux (systemd, /proc)");
}

#[cfg(not(unix))]
fn install_worker_unit(_system: bool, _no_start: bool) -> Result<()> {
    anyhow::bail!("`worker install` writes a systemd unit; it is Linux only")
}

/// `c0mpute doctor`: every worker process, unit and identity lock on this
/// machine, what is wrong, and (`--fix`) the fixes it may apply.
#[cfg(unix)]
fn print_worker_census(fix: bool) {
    use c0mpute_core::instance;
    let report = instance::scan();
    println!();
    println!("workers on this machine:");
    if report.processes.is_empty() {
        println!("  processes: none running");
    }
    for p in &report.processes {
        println!(
            "  process pid {} as {} in {}{}",
            p.pid,
            p.user,
            p.unit
                .as_ref()
                .map(|u| u.describe())
                .or_else(|| p.container.as_ref().map(|c| format!("container {c}")))
                .unwrap_or_else(|| "no systemd unit".into()),
            if p.exe_deleted { " (binary replaced on disk)" } else { "" }
        );
    }
    for u in &report.units {
        println!(
            "  unit {} ({}) runs as {}, {}, restart-loop guard {}, starts {}",
            u.unit.describe(),
            u.path.display(),
            u.runs_as,
            if u.enabled { "enabled" } else { "disabled" },
            if u.restart_prevented { "on" } else { "OFF" },
            u.exec.as_deref().unwrap_or("?")
        );
    }
    for l in &report.locks {
        let who = match (&l.holder, l.held) {
            (Some(h), true) => format!("held by {}", h.describe()),
            (_, true) => "held".to_string(),
            (Some(h), false) => format!("free (last holder pid {})", h.pid),
            (None, false) => "free".to_string(),
        };
        println!(
            "  identity {} (user {}): {who}{}{}",
            l.dir.display(),
            l.owner,
            l.heartbeat_age_s.map(|a| format!(", heartbeat {a}s ago")).unwrap_or_default(),
            if l.takeover_pending { ", takeover pending" } else { "" }
        );
    }
    if report.findings.is_empty() {
        println!("OK    workers: one per identity, no duplicate units");
        return;
    }
    for f in &report.findings {
        let level = match f.kind {
            instance::FindingKind::StaleLock => "INFO",
            instance::FindingKind::DuplicateProcesses
            | instance::FindingKind::DuplicateUnits
            | instance::FindingKind::Wedged => "FAIL",
            _ => "WARN",
        };
        println!("{level:5} {}", f.message);
        if let Some(cmd) = &f.fix {
            println!("        fix: {cmd}");
        }
    }
    if fix {
        for line in instance::fix(&report) {
            println!("FIXED {line}");
        }
    } else if report.findings.iter().any(|f| f.disable.is_some() || f.add_dropin.is_some()) {
        println!("        run `c0mpute doctor --fix` to apply the fixes this user may make");
    }
}

/// `c0mpute worker install`: one systemd unit for this user's worker.
#[cfg(unix)]
fn install_worker_unit(system: bool, no_start: bool) -> Result<()> {
    use c0mpute_core::instance;
    // SAFETY: getuid has no preconditions.
    let me = instance::user_name(unsafe { libc::getuid() });
    let report = instance::scan();
    if let Some(other) = report
        .units
        .iter()
        .find(|u| u.runs_as == me && u.enabled && u.unit.user == system)
    {
        anyhow::bail!(
            "{} already runs a c0mpute worker as {me} ({}). One identity, one worker: remove it first ({}) \
             or keep using it.",
            other.unit.describe(),
            other.path.display(),
            other.unit.disable_command()
        );
    }
    let exe = std::env::current_exe()?.display().to_string();
    let exe = exe.trim_end_matches(" (deleted)").to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let bin_dir = std::path::Path::new(&exe)
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let user_line = if system { format!("User={me}\n") } else { String::new() };
    let unit = format!(
        "[Unit]\nDescription=c0mpute worker\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=simple\n{user_line}Environment=HOME={home}\n\
         Environment=PATH={bin_dir}:{home}/.local/bin:/usr/local/bin:/usr/bin:/bin\n\
         ExecStart={exe} worker start\nRestart=on-failure\nRestartSec=5\n\
         # 90: replaced by a newer start of this identity; 91: a duplicate that stood down.\n\
         RestartPreventExitStatus={}\nTimeoutStopSec=30\n\n[Install]\nWantedBy={}\n",
        instance::RESTART_PREVENT_EXIT_STATUS,
        if system { "multi-user.target" } else { "default.target" }
    );
    let (path, ctl): (PathBuf, Vec<&str>) = if system {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            anyhow::bail!(
                "a system unit needs root: sudo -E {exe} worker install --system (or drop --system for a user unit)"
            );
        }
        (PathBuf::from("/etc/systemd/system/c0mpute-worker.service"), vec![])
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".config"));
        (base.join("systemd/user/c0mpute-worker.service"), vec!["--user"])
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, unit)?;
    println!("wrote {}", path.display());
    let run = |args: &[&str]| {
        let ok = std::process::Command::new("systemctl")
            .args(&ctl)
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("systemctl {} {} failed", ctl.join(" "), args.join(" "));
        }
        ok
    };
    run(&["daemon-reload"]);
    if no_start {
        run(&["enable", "c0mpute-worker.service"]);
    } else {
        // A daemon from `worker start -d` would be replaced by the service
        // anyway; stop it cleanly first.
        let _ = stop_worker();
        run(&["enable", "--now", "c0mpute-worker.service"]);
    }
    if !system {
        println!("starts at boot once lingering is on: loginctl enable-linger {me}");
    }
    Ok(())
}

fn status_label(s: &c0mpute_doctor::Status) -> &'static str {
    match s {
        c0mpute_doctor::Status::Ok => "OK",
        c0mpute_doctor::Status::Warn(_) => "WARN",
        c0mpute_doctor::Status::Fail(_) => "FAIL",
    }
}

fn peer_label(bin: &str) -> &'static str {
    if which_on_path(bin).is_some() {
        "OK"
    } else {
        "WARN"
    }
}

/// The installed version of a peer CLI, from the first line of `<bin> --version`.
/// Only that line is read: `infernet --version` goes on to check npm for updates,
/// so the child is killed rather than waited on.
fn peer_version(bin: &str) -> Option<String> {
    use std::io::BufRead;
    let path = which_on_path(bin)?;
    let mut child = Command::new(path)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    let read = child
        .stdout
        .take()
        .map(|out| std::io::BufReader::new(out).read_line(&mut line));
    let _ = child.kill();
    let _ = child.wait();
    read?.ok()?;
    parse_version(&line)
}

/// The first dotted version in a line: "infernet v0.1.58" and "0.15.0" both work.
fn parse_version(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

fn peer_status_text(bin: &str) -> String {
    match which_on_path(bin) {
        Some(p) => format!("Ok ({})", p.display()),
        None => {
            format!("not on PATH — install via `curl -fsSL https://c0mpute.com/install.sh | sh`")
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// worker
// ────────────────────────────────────────────────────────────────────────

/// Ensure this worker has published a full benchmark from its current binary.
/// Older c0mpute releases may update themselves without knowing about the
/// post-update hook, so check the saved report on every worker start as a
/// compatibility backstop. The report version also prevents repeat runs on
/// ordinary restarts.
fn ensure_current_version_benchmark() {
    let report = c0mpute_bench::load(&bench_report_path()).ok().flatten();
    let current_version = env!("CARGO_PKG_VERSION");
    let current_report = report.as_ref().is_some_and(|report| {
        !report.metadata.quick && report.score > 0 && report.metadata.version == current_version
    });
    if current_report {
        return;
    }

    tracing::info!(
        version = current_version,
        "worker benchmark is missing, quick, or stale; running a full benchmark before advertising"
    );
    match run_bench(None, false, false, None) {
        Ok(()) => tracing::info!(version = current_version, "worker benchmark refreshed"),
        Err(error) => tracing::warn!(
            version = current_version,
            err = %error,
            "worker benchmark failed; continuing worker startup"
        ),
    }
}

/// `c0mpute status --json`: everything the text report says, as one object.
/// Reads only: no identity is generated, no network is touched.
fn worker_status_json(config_path: &std::path::Path) -> Result<serde_json::Value> {
    let cfg = Config::load_or_default(config_path)?;
    let pid = read_worker_pid();
    let running = pid.is_some_and(pid_alive);
    let (pid_file, log_file) = worker_runtime_paths();
    let worker_peer = config::config_dir().and_then(|d| c0mpute_net::identity::existing_peer_id(&d));
    let publisher_peer = config::data_dir()
        .and_then(|d| c0mpute_net::identity::existing_peer_id(&d.join("buyer")));
    let bench = c0mpute_bench::load(&bench_report_path()).ok().flatten().map(|r| {
        serde_json::json!({
            "score": r.score,
            "quick": r.metadata.quick,
            "cores": r.metadata.cores,
            "at": r.metadata.start_time,
            "version": r.metadata.version,
        })
    });
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "worker": {
            "running": running,
            "pid": pid,
            "stale_pid": pid.is_some() && !running,
            "pid_file": pid_file,
            "log_file": log_file,
        },
        "peer_id": worker_peer.map(|p| p.to_string()),
        // The key sites are signed with (`hosting ls --mine` matches on it).
        "publisher": publisher_peer.as_ref().and_then(c0mpute_net::identity::ed25519_public_hex),
        "name": c0mpute_core::capabilities::node_name(),
        "private": cfg.private.enabled || c0mpute_core::private::env_forces_private(),
        "auto_update": cfg.update_auto,
        "storage": cfg.storage_enabled(),
        "hosting": cfg.hosting_enabled(),
        "roles": cfg.roles,
        "gateway_bind": cfg.gateway.bind,
        "payments": {
            "enabled": cfg.payments.enabled,
            "rail": cfg.payments.rail,
            "api_key_env": cfg.payments.coinpay.api_key_env,
            "api_key_set": std::env::var(&cfg.payments.coinpay.api_key_env).is_ok_and(|v| !v.is_empty()),
        },
        "bench": bench,
        "config_path": config_path,
        "config_dir": config::config_dir(),
        "data_dir": config::data_dir(),
    }))
}

async fn run_worker(cmd: WorkerCmd, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        WorkerCmd::Register => {
            let r = run_register(Some(config_path))?;
            println!("registered as worker:");
            println!("  peer-id   : {}", r.peer_id);
            println!(
                "  identity  : {}{}",
                r.identity_path.display(),
                if r.created_identity { "  (new)" } else { "" }
            );
            println!(
                "  config    : {}{}",
                r.config_path.display(),
                if r.created_config { "  (new)" } else { "" }
            );
            println!("  storage   : {}", r.storage_root.display());
            println!();
            // Auto-mint the payable DID so the operator never has to run a
            // separate coinpay command. Idempotent (coinpay returns the
            // existing DID) and best-effort — a worker runs fine without it,
            // so a missing/logged-out coinpay must not fail registration.
            ensure_coinpay_did();
            println!();
            println!("next:");
            println!(
                "  c0mpute login          # sign in to coinpay + infernet (ties this node to your accounts)"
            );
            println!("  c0mpute worker start   # join the swarm");
            Ok(())
        }
        WorkerCmd::Status { json: true } => {
            let mut status = worker_status_json(config_path)?;
            // Roles serving or gated (and why), tags, host load: what the
            // running worker would do, from its own command line.
            let mut cfg = Config::load_or_default(config_path)?;
            let worker = node_status::find_worker(read_worker_pid());
            let flags = worker.as_ref().and_then(|(_, argv)| node_status::worker_flags(argv));
            let source = match (&worker, flags.as_ref().and_then(|f| f.roles.as_ref())) {
                (Some(_), Some(_)) => "worker --roles",
                (Some(_), None) => "worker defaults",
                (None, _) => "defaults (worker not running)",
            };
            let f = flags.unwrap_or_default();
            if f.private {
                cfg.private.enabled = true;
            }
            effective_worker_roles(&mut cfg, f.roles.as_deref(), f.gpu, f.no_storage, f.no_hosting);
            let roles = cfg.roles.clone();
            let gates = node_status::report(&cfg, &roles, worker, source).await;
            if let (Some(base), serde_json::Value::Object(extra)) = (status.as_object_mut(), gates) {
                base.extend(extra);
            }
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        WorkerCmd::Status { json: false } => {
            println!("version: c0mpute {}", env!("CARGO_PKG_VERSION"));
            match read_worker_pid() {
                Some(pid) if pid_alive(pid) => println!("worker: running (pid {pid})"),
                Some(pid) => println!("worker: not running (stale pid {pid})"),
                None => println!("worker: not running"),
            }
            let cfg = Config::load_or_default(config_path)?;
            if cfg.update_auto {
                let feed = cfg
                    .update_feed_url
                    .clone()
                    .unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
                println!(
                    "auto-update: on — checks {} every {}s, applies in place",
                    feed,
                    config::UPDATE_CHECK_INTERVAL_SECS
                );
            } else {
                println!("auto-update: off");
            }
            match c0mpute_bench::load(&bench_report_path()).ok().flatten() {
                Some(r) if r.metadata.quick => println!(
                    "bench: {} (quick run {} — not advertised; run `c0mpute bench`)",
                    r.score, r.metadata.start_time
                ),
                Some(r) => println!(
                    "bench: score {} ({} cores, {})",
                    r.score, r.metadata.cores, r.metadata.start_time
                ),
                None => println!("bench: not run — `c0mpute bench` publishes a capacity score"),
            }
            println!("{}", crowdgpt::status_line(config_path));
            print_served_models_eligibility();
            print_house_models();
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
        WorkerCmd::Stop => stop_worker(),
        WorkerCmd::Install { system, no_start } => install_worker_unit(system, no_start),
        WorkerCmd::Start {
            roles,
            storage: _,
            no_storage,
            no_hosting,
            gpu,
            private,
            // Both handled in `main` before the runtime started; at this point
            // we are the (possibly detached) worker process itself.
            attach: _,
            daemon: _,
        } => {
            let mut cfg = Config::load_or_default(config_path)?;
            if private {
                cfg.private.enabled = true;
            }
            let policy = c0mpute_core::private::install_from(&cfg.private);
            if policy.enabled {
                eprintln!(
                    "private mode: serving {} trusted buyer identit{} only; not advertised to the network",
                    policy.trusted.len(),
                    if policy.trusted.len() == 1 { "y" } else { "ies" }
                );
            }
            ensure_current_version_benchmark();
            let default_speech_roles = roles.is_none();
            effective_worker_roles(&mut cfg, roles.as_deref(), gpu, no_storage, no_hosting);
            if cfg.lists_on_d1sks() {
                eprintln!(
                    "disk provider: storing shards and holding sites for the network, listed on {} \
                     (opt out: --no-storage / --no-hosting, [storage] enabled = false, \
                     [storage] list_on_d1sks = false, C0MPUTE_D1SKS=0)",
                    cfg.storage.d1sks_url
                );
            }
            // Speech is a nice-to-have, not a precondition for joining: a box
            // that cannot build a TTS runtime should still serve storage,
            // gateway and verifier work. So this warns rather than aborting,
            // and the supervisor retries Kokoro in the background, only
            // advertising TTS once it actually works.
            if cfg.roles.contains(&Role::Tts) && cfg.speech.auto_install_enabled() {
                if let Err(error) = c0mpute_core::speech::setup_tts("all").await {
                    eprintln!("could not install every CPU TTS engine: {error:#}");
                }
            }
            if cfg.roles.contains(&Role::Stt) {
                c0mpute_core::speech::setup_stt()
                    .await
                    .context("install the default CPU STT model before starting the worker")?;
            }
            if default_speech_roles {
                eprintln!(
                    "TTS, STT, workflow-step, diffusion and music (CPU images and songs, on boxes with the RAM) are enabled by default; pass --roles to choose a specific role set"
                );
            }
            // Only gateway workers run the Infernet provider bootstrap. A
            // speech-only or storage-only worker should not start an unrelated
            // inference agent as a side effect of joining c0mpute.
            let auto = cfg.update_auto;
            let interval_secs = config::UPDATE_CHECK_INTERVAL_SECS;
            // Private mode never joins Infernet as a public provider or RPC
            // slice: that would share this machine with the whole network.
            let serves_infernet = cfg.roles.contains(&Role::Gateway) && !policy.enabled;
            if policy.enabled && cfg.roles.contains(&Role::Gateway) {
                eprintln!("private mode: not registering with Infernet as a public provider");
            }
            if serves_infernet {
                // First run: auto-configure the infernet peer (init + register,
                // plus token login if INFERNET_TOKEN is set), then ensure its
                // node daemon is available for managed inference jobs.
                bootstrap_infernet_first_run();
                ensure_infernet_daemon();
                reap_stale_infernet_daemons();
                // Opt-in: serve configured models over infernet RPC.
                let _ = tokio::task::spawn_blocking(bootstrap_infernet_rpc);
            }
            // Check every surface for upgrades on the fixed 60-second cadence:
            // c0mpute self-updates in the supervisor's poll loop; here we refresh
            // the plugins and (re)attempt RPC serving on the same interval. First
            // tick fires immediately.
            if auto && serves_infernet {
                let interval = std::time::Duration::from_secs(interval_secs);
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    loop {
                        ticker.tick().await;
                        let _ = tokio::task::spawn_blocking(refresh_plugins).await;
                        let _ = tokio::task::spawn_blocking(bootstrap_infernet_rpc).await;
                        let _ = tokio::task::spawn_blocking(reap_stale_infernet_daemons).await;
                    }
                });
            }
            let sup = Supervisor::boot(cfg).await?;
            sup.run().await
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// bench
// ────────────────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
enum PricingCmd {
    /// Print the current listing (the default).
    Show,
    /// Set the floor and the mode.
    Set {
        /// Lowest price per job you accept, in USD.
        #[arg(long)]
        min: Option<f64>,
        /// Always ask this price per job (fixed mode).
        #[arg(long, conflicts_with = "dynamic")]
        fixed: Option<f64>,
        /// Price by demand: undercut when idle, up to the buyer's cap when busy.
        #[arg(long)]
        dynamic: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PrivateCmd {
    /// Print the mode, this machine's identities and the allowed buyers (the default).
    Show,
    /// Turn private mode on.
    On,
    /// Turn private mode off (back to the open market).
    Off,
    /// Allow a buyer peer-id (another machine of yours) to hire this node.
    Allow { peer_id: String },
    /// Remove a buyer peer-id from the list.
    Deny { peer_id: String },
}

fn run_private(cmd: Option<PrivateCmd>, config_path: &std::path::Path) -> Result<()> {
    let mut cfg = Config::load_or_default(config_path)?;
    let changed = match cmd.unwrap_or(PrivateCmd::Show) {
        PrivateCmd::Show => false,
        PrivateCmd::On => {
            cfg.private.enabled = true;
            true
        }
        PrivateCmd::Off => {
            cfg.private.enabled = false;
            true
        }
        PrivateCmd::Allow { peer_id } => {
            if !c0mpute_core::private::valid_peer_id(&peer_id) {
                anyhow::bail!("{peer_id:?} is not a libp2p peer-id (12D3KooW...)");
            }
            if !cfg.private.allow(&peer_id) {
                println!("{} is already allowed", peer_id.trim());
            }
            true
        }
        PrivateCmd::Deny { peer_id } => {
            if !cfg.private.deny(&peer_id) {
                println!("{} was not on the list", peer_id.trim());
            }
            true
        }
    };
    if changed {
        cfg.save(config_path)?;
        println!(
            "saved to {} (apply: c0mpute worker stop && c0mpute worker start -d)",
            config_path.display()
        );
    }
    let forced = c0mpute_core::private::env_forces_private();
    let mode = match (cfg.private.enabled, forced) {
        (true, _) => "private",
        (false, true) => "private (C0MPUTE_PRIVATE is set)",
        (false, false) => "public (open market)",
    };
    println!("mode     : {mode}");
    let local = c0mpute_core::private::local_identities();
    println!("this machine ({}, always allowed):", local.len());
    for id in &local {
        println!("  {id}");
    }
    println!("allowed buyers ({}):", cfg.private.buyers.len());
    for id in &cfg.private.buyers {
        println!("  {id}");
    }
    if local.is_empty() && cfg.private.buyers.is_empty() {
        println!("  (none yet: run `c0mpute worker register` or allow a peer-id)");
    }
    Ok(())
}

fn run_name(handle: Option<String>, clear: bool) -> Result<()> {
    use c0mpute_core::capabilities::{node_name, node_name_path, sanitize_ref};
    let path = node_name_path().context("cannot find a home directory for ~/.c0mpute")?;
    let apply = "(apply: c0mpute worker stop && c0mpute worker start -d)";
    if clear {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("removed {} {apply}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("no nickname was set"),
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
    } else if let Some(raw) = handle {
        let name = sanitize_ref(&raw).with_context(|| {
            format!("{raw:?} is not a valid nickname: use a-z, 0-9, - or _, up to 32 characters")
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, format!("{name}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        println!("saved to {} {apply}", path.display());
    }
    match node_name() {
        Some(n) => {
            let via = if std::env::var_os("C0MPUTE_NAME").is_some() { " (from C0MPUTE_NAME)" } else { "" };
            println!("name     : {n}{via}");
            println!("shown on : https://c0mpute.com/nodes");
        }
        None => println!("name     : (none; set one with `c0mpute name <handle>`)"),
    }
    Ok(())
}

fn run_pricing(cmd: Option<PricingCmd>, config_path: &std::path::Path) -> Result<()> {
    use c0mpute_core::pricing::PricingMode;
    let mut cfg = Config::load_or_default(config_path)?;
    if let Some(PricingCmd::Set {
        min,
        fixed,
        dynamic,
    }) = cmd
    {
        if let Some(m) = min {
            cfg.pricing.min_usd = m;
        }
        if let Some(f) = fixed {
            cfg.pricing.mode = PricingMode::Fixed;
            cfg.pricing.fixed_usd = Some(f);
        } else if dynamic {
            cfg.pricing.mode = PricingMode::Dynamic;
            cfg.pricing.fixed_usd = None;
        }
        cfg.pricing.validate().map_err(anyhow::Error::msg)?;
        cfg.save(config_path)?;
        println!(
            "saved to {} (apply: c0mpute worker stop && c0mpute worker start -d)",
            config_path.display()
        );
    }
    let p = &cfg.pricing;
    println!("mode     : {}", p.mode.as_str());
    println!("min      : ${} per job", p.min_usd);
    if let (PricingMode::Fixed, Some(f)) = (p.mode, p.fixed_usd) {
        println!("price    : ${f} per job");
    }
    if let Some(q) = p.quote(1.0, 0.0) {
        println!("example  : on a $1.00 job you bid ${q:.2} when idle");
    } else {
        println!("example  : you skip jobs capped at $1.00 (under your floor)");
    }
    Ok(())
}

#[derive(Subcommand, Debug)]
enum BenchCmd {
    /// Run the benchmark and save the report (the default).
    Run,
    /// Print the last saved report without running anything.
    Show,
    /// Print where the report is stored.
    Path,
}

fn bench_report_path() -> PathBuf {
    c0mpute_bench::path_in(&config::data_dir().unwrap_or_else(|| PathBuf::from(".")))
}

fn run_bench(
    cmd: Option<BenchCmd>,
    quick: bool,
    json: bool,
    threads: Option<Vec<usize>>,
) -> Result<()> {
    let path = bench_report_path();
    match cmd.unwrap_or(BenchCmd::Run) {
        BenchCmd::Path => {
            println!("{}", path.display());
            Ok(())
        }
        BenchCmd::Show => match c0mpute_bench::load(&path)? {
            Some(report) => {
                print_bench(&report, json);
                Ok(())
            }
            None => anyhow::bail!(
                "no bench report at {} — run `c0mpute bench` first",
                path.display()
            ),
        },
        BenchCmd::Run => {
            if let Some(t) = &threads {
                if t.is_empty() || t.contains(&0) {
                    anyhow::bail!("--threads needs one or more counts greater than zero");
                }
            }
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            if !json {
                eprintln!(
                    "c0mpute bench: {} run on {cores} cores — {} workloads × {} thread counts",
                    if quick { "quick" } else { "full" },
                    c0mpute_bench::WORKLOADS.len(),
                    threads
                        .as_ref()
                        .map(|t| t.len())
                        .unwrap_or_else(|| c0mpute_bench::thread_ladder(cores).len()),
                );
            }
            fn progress(workload: &str, threads: usize) {
                eprint!("\r  {workload:<7} @ {threads:>3} threads…   ");
            }
            let opts = c0mpute_bench::Options {
                quick,
                threads,
                progress: if json { None } else { Some(progress) },
            };
            let report = c0mpute_bench::run(&opts);
            if !json {
                eprint!("\r{:40}\r", "");
            }
            c0mpute_bench::save(&report, &path)?;
            print_bench(&report, json);
            if !json {
                println!("saved {}", path.display());
                if report.metadata.quick {
                    println!(
                        "quick runs are not advertised — run `c0mpute bench` without --quick to publish a score"
                    );
                } else {
                    println!(
                        "the worker advertises score {} on its next start",
                        report.score
                    );
                }
            }
            Ok(())
        }
    }
}

fn print_bench(report: &c0mpute_bench::BenchReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_default()
        );
    } else {
        print!("{}", c0mpute_bench::render_text(report));
    }
}

// ────────────────────────────────────────────────────────────────────────
// worker daemon: detach, PID file, stop/status
// ────────────────────────────────────────────────────────────────────────

/// `(pid_file, log_file)` for the background worker, under the data dir
/// (`~/.local/share/c0mpute` on Linux), falling back to the cwd.
fn worker_runtime_paths() -> (PathBuf, PathBuf) {
    let dir = config::data_dir().unwrap_or_else(|| PathBuf::from("."));
    (dir.join("worker.pid"), dir.join("worker.log"))
}

fn read_worker_pid() -> Option<i32> {
    let (pid_file, _) = worker_runtime_paths();
    std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
}

/// Fork into the background, redirect stdout/stderr to the worker log, and
/// write a locked PID file. Runs before the Tokio runtime starts, so the
/// process is still single-threaded when it forks.
#[cfg(unix)]
fn daemonize_worker() -> Result<()> {
    use std::fs::OpenOptions;

    let (pid_file, log_file) = worker_runtime_paths();
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)?;
    let stderr = stdout.try_clone()?;

    // Print to the launching terminal *before* forking — once detached,
    // stdout points at the log file.
    println!("c0mpute worker starting in background");
    println!("  pid file: {}", pid_file.display());
    println!("  logs:     {}", log_file.display());
    println!("  stop:     c0mpute worker stop");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    daemonize::Daemonize::new()
        .pid_file(&pid_file)
        .working_directory(cwd)
        .stdout(stdout)
        .stderr(stderr)
        .start()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to start worker daemon: {e} \
                 (already running? check `c0mpute worker status`)"
            )
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn daemonize_worker() -> Result<()> {
    anyhow::bail!("`worker start --daemon` is only supported on Unix platforms")
}

/// Attach mode (`worker start -a`): make sure a background worker is running
/// (launch one via `worker start -d` if not), then stream its log to the
/// terminal until the user detaches with Ctrl-D (stdin EOF) or Ctrl-C. The
/// worker is a standalone daemon the whole time, so detaching just stops the
/// viewer — it never touches the worker.
#[cfg(unix)]
fn run_attached_worker(passthrough: Vec<String>) -> Result<()> {
    use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static DETACH: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_sig: libc::c_int) {
        DETACH.store(true, Ordering::SeqCst);
    }

    let (_pid_file, log_file) = worker_runtime_paths();

    // Attach to an already-running worker, or spawn a detached one. The spawned
    // child does the fork-before-runtime itself, so nothing unsafe happens here.
    let pid = if let Some(p) = read_worker_pid().filter(|p| pid_alive(*p)) {
        println!("worker already running (pid {p}); attaching");
        Some(p)
    } else {
        let exe = std::env::current_exe()?;
        let mut args = vec!["worker".to_string(), "start".to_string(), "-d".to_string()];
        args.extend(passthrough);
        let status = Command::new(&exe)
            .args(&args)
            .stdout(std::process::Stdio::null()) // we print our own banner
            .status()?;
        if !status.success() {
            anyhow::bail!("worker failed to start");
        }
        // The daemon writes its PID file just after forking — poll briefly.
        let mut pid = None;
        for _ in 0..50 {
            pid = read_worker_pid().filter(|p| pid_alive(*p));
            if pid.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        pid
    };

    match pid {
        Some(p) => println!(
            "attached to worker (pid {p}) — Ctrl-D or Ctrl-C to detach; the worker keeps running"
        ),
        None => {
            println!("attached to worker — Ctrl-D or Ctrl-C to detach; the worker keeps running")
        }
    }

    // Replay recent history so attaching isn't a blank screen — a settled
    // worker is nearly silent, so tailing only *new* output shows nothing.
    const HISTORY_BYTES: u64 = 16 * 1024;
    let end = std::fs::metadata(&log_file).map(|m| m.len()).unwrap_or(0);
    if end == 0 {
        println!(
            "(no output at {} yet — if the worker runs under systemd, follow it with: \
             journalctl --user -u c0mpute-worker -f)",
            log_file.display()
        );
    } else if let Ok(mut f) = std::fs::File::open(&log_file) {
        let from = end.saturating_sub(HISTORY_BYTES);
        if f.seek(SeekFrom::Start(from)).is_ok() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                // Drop a partial first line when we started mid-file.
                let slice: &[u8] = match (from > 0, buf.iter().position(|&b| b == b'\n')) {
                    (true, Some(i)) => &buf[i + 1..],
                    _ => &buf,
                };
                let out = std::io::stdout();
                let _ = out.lock().write_all(slice);
            }
        }
    }

    // Detach on Ctrl-D (0x04) or Ctrl-C (0x03). Put the terminal in raw mode so
    // we catch the keystroke itself rather than relying on canonical-mode EOF
    // (which only fires on an empty line and proved unreliable across terminals).
    // Only c_lflag is touched, so OPOST newline translation still renders the
    // streamed log correctly.
    struct RawGuard {
        fd: libc::c_int,
        orig: libc::termios,
    }
    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
        }
    }
    let mut raw_guard: Option<RawGuard> = None;
    if std::io::stdin().is_terminal() {
        unsafe {
            let fd = libc::STDIN_FILENO;
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) == 0 {
                let mut raw = orig;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) == 0 {
                    raw_guard = Some(RawGuard { fd, orig });
                }
            }
        }
        std::thread::spawn(|| {
            let mut b = [0u8; 1];
            loop {
                let n = unsafe {
                    libc::read(libc::STDIN_FILENO, b.as_mut_ptr() as *mut libc::c_void, 1)
                };
                // EOF/error, Ctrl-D, or Ctrl-C → detach.
                if n <= 0 || b[0] == 0x04 || b[0] == 0x03 {
                    DETACH.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
    }
    // Safety net for `kill -INT/-TERM` (and Ctrl-C when raw mode was unavailable).
    let handler = on_signal as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, handler as usize as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as usize as libc::sighandler_t);
    }

    // Follow the log until detached.
    let drain = |from: u64| -> u64 {
        let mut at = from;
        if let Ok(mut f) = std::fs::File::open(&log_file) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(at);
            if len > at && f.seek(SeekFrom::Start(at)).is_ok() {
                let mut chunk = Vec::new();
                if f.take(len - at).read_to_end(&mut chunk).is_ok() {
                    let out = std::io::stdout();
                    let mut h = out.lock();
                    let _ = h.write_all(&chunk);
                    let _ = h.flush();
                    at = len;
                }
            }
        }
        at
    };

    let mut pos = end;
    while !DETACH.load(Ordering::SeqCst) {
        pos = drain(pos);
        std::thread::sleep(Duration::from_millis(150));
    }
    drain(pos); // final flush
    drop(raw_guard); // restore cooked terminal before the final line + shell prompt

    match read_worker_pid().filter(|p| pid_alive(*p)) {
        Some(p) => {
            println!("\ndetached — worker still running (pid {p}); stop with: c0mpute worker stop")
        }
        None => println!("\ndetached"),
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_attached_worker(_passthrough: Vec<String>) -> Result<()> {
    anyhow::bail!("`worker start --attach` is only supported on Unix platforms")
}

/// True if a process with this PID exists (signal 0 probes without delivering).
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

#[cfg(unix)]
fn stop_worker() -> Result<()> {
    match read_worker_pid() {
        Some(pid) if pid_alive(pid) => {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                println!("sent SIGTERM to worker (pid {pid})");
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "failed to signal worker pid {pid}: {}",
                    std::io::Error::last_os_error()
                ))
            }
        }
        Some(pid) => {
            // Stale PID file — the flock is released, so the next `start`
            // will reclaim it. Clean it up so `status` reads true.
            let (pid_file, _) = worker_runtime_paths();
            let _ = std::fs::remove_file(pid_file);
            println!("no running worker (cleared stale pid {pid})");
            Ok(())
        }
        None => {
            println!("no running worker");
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn stop_worker() -> Result<()> {
    anyhow::bail!("`worker stop` is only supported on Unix platforms")
}

// ────────────────────────────────────────────────────────────────────────
// job
// ────────────────────────────────────────────────────────────────────────

async fn run_job(cmd: JobCmd) -> Result<()> {
    match cmd {
        JobCmd::Submit { manifest } => {
            println!("[stub] would POST {} to coordinator", manifest.display());
            Ok(())
        }
        JobCmd::Status { id } => {
            println!("[stub] status for {id}");
            Ok(())
        }
        JobCmd::Logs { id, follow } => {
            println!("[stub] logs for {id} (follow={follow})");
            Ok(())
        }
        JobCmd::Cancel { id } => {
            println!("[stub] cancel {id}");
            Ok(())
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// transcode plugin (in-process)
// ────────────────────────────────────────────────────────────────────────

async fn run_transcode(cmd: TranscodeCmd) -> Result<()> {
    match cmd {
        TranscodeCmd::Submit {
            input,
            preset,
            max_price,
            external_ref,
            policy,
            output,
            max_wait,
        } => {
            // Phase 1: only HTTP(S) URLs are supported as inputs (the
            // worker fetches via reqwest). Local-file submission needs
            // chunk-store + libp2p request-response, which is Phase 2.
            let input_url = input.to_string_lossy().to_string();
            if !(input_url.starts_with("http://") || input_url.starts_with("https://")) {
                anyhow::bail!(
                    "Phase 1 requires an http(s) URL for --input (local file submission \
                     uses the chunk store and lands in Phase 2). Got: {input_url}"
                );
            }
            let spec = preset_to_spec(&preset)?;
            let inline = TranscodeJobInline {
                input_url: input_url.clone(),
                preset: preset.clone(),
                spec,
            };
            let spec_inline = serde_json::to_value(&inline)?;

            // Boot a buyer-mode libp2p node — this CLI invocation is
            // ephemeral, no roles, no advertise.
            let cfg = Config::default();
            let sup = Supervisor::boot_buyer(cfg).await?;
            let net = sup.libp2p.clone();
            // Tiny grace period so peer discovery (mDNS) sees us.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // No worker free right now: wait in the queue (up to --max-wait)
            // instead of failing.
            let outcome = c0mpute_core::buyer::run_auction_queued(
                net.clone(),
                || {
                    JobAuction::new("ffmpeg.transcode", spec_inline.clone())
                        .with_required_capabilities(vec!["c0mpute:role:transcode".into()])
                        .with_max_price_usd(max_price.unwrap_or(1.0))
                        .with_external_ref(external_ref.clone())
                        .with_policy(policy)
                },
                std::time::Duration::from_secs(max_wait),
                |waited| {
                    eprintln!(
                        "queued, waiting for a worker ({})",
                        c0mpute_core::buyer_queue::human(waited)
                    )
                },
            )
            .await?;
            println!("job_id        : {}", outcome.job_id);
            if let Some(r) = &outcome.external_ref {
                println!("external_ref  : {r}");
            }
            println!("winner        : {}", outcome.accepted_bid.bidder_peer_id);
            println!("price_usd     : {}", outcome.accepted_bid.price_usd);
            println!("status        : {:?}", outcome.receipt.status);
            if let Some(h) = &outcome.receipt.output_hash {
                // The worker stored the output in its chunk store under this
                // hash and announced it on the DHT.
                println!("output_hash   : {h}");
                if let Some(path) = &output {
                    // Pieced outputs (anything over 1 MiB from a current
                    // worker) are written a piece at a time.
                    // Sealed to this buyer; opened as it is written.
                    let written = c0mpute_core::gateway_util::fetch_result_to_file(
                        &net,
                        &outcome,
                        c0mpute_core::runner::MAX_OUTPUT_BYTES as usize,
                        std::time::Duration::from_secs(30),
                        path,
                    )
                    .await?;
                    println!("output        : {} ({written} bytes)", path.display());
                }
            } else if output.is_some() {
                anyhow::bail!("the worker returned no output to download");
            }
            Ok(())
        }
        TranscodeCmd::Preset {
            cmd: PresetCmd::List,
        } => {
            // Keep this list in sync with preset_to_spec. The wider set of
            // audio/HLS presets is a roadmap, not executable today.
            for p in ["video-720p", "video-1080p", "video-4k"] {
                println!("{p}");
            }
            Ok(())
        }
        TranscodeCmd::Doctor => {
            println!("OK   ffmpeg presence (delegated to top-level doctor)");
            Ok(())
        }
    }
}

async fn run_tts(cmd: TtsCmd, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        TtsCmd::Auto { state } => {
            let mut cfg = Config::load_or_default(config_path)?;
            cfg.speech.auto_install = state == "on";
            cfg.save(config_path)?;
            println!(
                "automatic Kokoro provisioning is {state} (apply: c0mpute worker stop && c0mpute worker start -d)"
            );
            if state == "off" {
                println!(
                    "already-installed voices are left alone; remove them by deleting {}",
                    c0mpute_core::speech::speech_root().display()
                );
            }
            Ok(())
        }
        TtsCmd::Setup { engine } => {
            c0mpute_core::speech::setup_tts(&engine).await?;
            let ready = c0mpute_core::speech::available_tts_engines();
            println!("ready TTS engines: {}", ready.join(", "));
            println!(
                "voices and models under {}",
                c0mpute_core::speech::speech_root().display()
            );
            println!(
                "start a worker with `c0mpute worker start --roles tts` to advertise TTS jobs"
            );
            Ok(())
        }
        TtsCmd::Voices => {
            println!("Kokoro: af_sarah, af_bella, af_nicole, am_onyx, am_michael, am_echo");
            println!("Piper:  en_US-joe-medium, en_US-norman-medium");
            println!(
                "Pocket: alba, anna, caro_davy, marius, javert, bill_boerst, peter_yearsley, stuart_bell"
            );
            println!("\nOpenProfile role map:");
            for profile in c0mpute_core::speech_profiles::AGENTS {
                println!(
                    "@{}: {} / {}:{} (fallback {}:{})",
                    profile.slug,
                    profile.persona,
                    profile.engine,
                    profile.voice,
                    profile.fallback_engine,
                    profile.fallback_voice
                );
            }
            Ok(())
        }
    }
}

async fn run_stt(cmd: SttCmd) -> Result<()> {
    match cmd {
        SttCmd::Setup => {
            c0mpute_core::speech::setup_stt().await?;
            println!(
                "installed multilingual faster-whisper base for CPU under {}",
                c0mpute_core::speech::speech_root().display()
            );
            println!(
                "start a worker with `c0mpute worker start --roles stt` to advertise STT jobs"
            );
            Ok(())
        }
    }
}

/// `c0mpute video` — the surface `plugins/video/module.toml` declares.
///
/// Everything here talks to the runner image rather than to the network: a
/// provider's first question is "can this box earn on video", and the answer
/// is a GPU, a container runtime and a pulled image.
async fn run_video(cmd: VideoCmd) -> Result<()> {
    use c0mpute_core::video;

    match cmd {
        VideoCmd::Setup { cpu: true, .. } => {
            use c0mpute_core::video_cpu;
            println!(
                "installing CPU video: stable-diffusion.cpp + Wan 2.1 T2V 1.3B ({:.1} GB of weights)",
                video_cpu::download_bytes() as f64 / 1e9
            );
            video_cpu::install().await?;
            println!("installed under {}", c0mpute_core::diffusion::diffusion_root().display());
            if c0mpute_core::supervisor::which_ffmpeg().is_none() {
                println!("ffmpeg is not on PATH: install it, or this node cannot encode clips");
            }
            println!(
                "this node now serves {} text-to-video on CPU (at most {} frames of 480x272, {} steps; many minutes per clip)",
                video_cpu::MODEL_ID,
                video_cpu::MAX_FRAMES,
                video_cpu::MAX_STEPS
            );
            println!("start (or restart) a worker with the video role: `c0mpute worker start`");
            Ok(())
        }
        VideoCmd::Setup { build, .. } => {
            let how = video::setup(build, &std::env::temp_dir(), &|line: &str| println!("{line}")).await?;
            println!("runner image ready as {} ({how:?})", video::RUNNER_IMAGE);
            match video::readiness(video::RUNNER_IMAGE) {
                None => {
                    println!("this node can serve video jobs");
                    println!(
                        "start a worker with `c0mpute worker start --roles video` to advertise them"
                    );
                }
                Some(not_ready) => {
                    // The image is in place but something else is missing, so
                    // say so now rather than at the first auction.
                    println!("the runner image is ready, but this node still cannot serve video:");
                    println!("  {}", not_ready.reason());
                }
            }
            Ok(())
        }
        VideoCmd::Check { selftest } => {
            if let Ok(cfg) = Config::load_or_default(&config::default_config_path()) {
                video::set_configured_runner(cfg.video.runner);
            }
            let local = video::runner_mode() == c0mpute_core::config::VideoRunner::Local;
            match video::readiness(video::RUNNER_IMAGE) {
                None => {
                    if local {
                        println!("ready (local runner): GPU, python+torch+CUDA and runner.py are all present");
                    } else {
                        println!("ready: GPU, container runtime and runner image are all present");
                    }
                    match video::nvidia_vram_mib() {
                        Some(mib) => println!(
                            "card: {mib} MiB; models it can run: {}",
                            video::models_for_vram(mib).join(", ")
                        ),
                        None => println!("card memory unknown (nvidia-smi reported none)"),
                    }
                }
                Some(not_ready) => {
                    println!("GPU video not ready: {}", not_ready.reason());
                    let cfg = Config::load_or_default(&config::default_config_path()).unwrap_or_default();
                    if c0mpute_core::video_cpu::installed() {
                        let serving = match c0mpute_core::video_cpu::gate_reason(&cfg.video) {
                            None => "yes".to_string(),
                            Some(why) => format!("no ({why})"),
                        };
                        println!(
                            "CPU video: installed ({}); can take a clip right now: {serving}",
                            c0mpute_core::video_cpu::MODEL_ID
                        );
                    } else {
                        println!("CPU video: not installed (`c0mpute video setup --cpu` opts in)");
                    }
                    return Ok(());
                }
            }
            if selftest && local {
                println!("running the runner selftest locally");
                let dir = std::env::temp_dir().join(format!("c0mpute-video-selftest-{}", std::process::id()));
                let status = tokio::process::Command::new(video::local_python())
                    .arg(video::local_runner_py())
                    .args(["selftest", "--job"])
                    .arg(dir.join("job.json"))
                    .status()
                    .await
                    .context("could not run the local runner")?;
                let _ = std::fs::remove_dir_all(&dir);
                if !status.success() {
                    anyhow::bail!("runner selftest failed with {status}");
                }
                return Ok(());
            }
            if selftest {
                println!("running the runner selftest in the container");
                let status = c0mpute_core::container::command()
                    .args(["run", "--rm", video::RUNNER_IMAGE, "selftest"])
                    .status()
                    .await
                    .context("could not run the runner image")?;
                if !status.success() {
                    anyhow::bail!("runner selftest failed with {status}");
                }
            }
            Ok(())
        }
        VideoCmd::Models => {
            // The image carries the manifest it was built from, so this is
            // the model set that node would actually serve — not what this
            // CLI happened to be compiled with.
            let out = c0mpute_core::container::command()
                .args([
                    "run",
                    "--rm",
                    "--entrypoint",
                    "python3",
                    video::RUNNER_IMAGE,
                    "-c",
                    "import tomllib,sys;\
m=tomllib.load(open('/opt/c0mpute-video/module.toml','rb'))['module']['models'];\
[print(f\"{k:18} {v.get('params','?'):9} {v.get('vram_gib','?')} GiB  {v.get('repo','')}\") for k,v in m.items()]",
                ])
                .output()
                .await
                .context("could not run the runner image; `c0mpute video setup` first")?;
            if !out.status.success() {
                anyhow::bail!(
                    "could not read the model table: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            print!("{}", String::from_utf8_lossy(&out.stdout));
            Ok(())
        }
    }
}

async fn run_speech(cmd: SpeechCmd) -> Result<()> {
    match cmd {
        SpeechCmd::Serve {
            bind,
            max_price_usd,
        } => {
            let token = std::env::var("C0MPUTE_SPEECH_TOKEN")
                .context("set C0MPUTE_SPEECH_TOKEN to a random secret (at least 24 characters)")?;
            c0mpute_core::speech_gateway::serve(bind, token, max_price_usd).await
        }
    }
}

async fn run_podcasting(cmd: PodcastingCmd) -> Result<()> {
    use c0mpute_core::podcast::show;
    match cmd {
        PodcastingCmd::Publish {
            inputs,
            name,
            title,
            author,
            description,
            image,
            language,
            category,
            explicit,
            email,
            unlocked,
            base_url,
            domains,
            value_address,
            value_type,
            value_method,
            ads,
            ads_payout,
            dry_run,
            wait_secs,
        } => {
            let mut input = show::load_input(&inputs)?;
            if let Some(breaks) = ads {
                let meta = input.show.ads.get_or_insert_with(Default::default);
                meta.enabled = Some(true);
                meta.placements = breaks.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            if let Some(p) = ads_payout {
                let Some(meta) = input.show.ads.as_mut() else {
                    bail!("--ads-payout needs --ads (or an [ads] table in show.toml)");
                };
                meta.payout = Some(p);
            }
            let m = &mut input.show;
            m.title = title.or(m.title.take());
            m.author = author.or(m.author.take());
            m.description = description.or(m.description.take());
            m.language = language.or(m.language.take());
            m.category = category.or(m.category.take());
            m.email = email.or(m.email.take());
            if explicit {
                m.explicit = Some(true);
            }
            if unlocked {
                m.locked = Some(false);
            }
            if let Some(address) = value_address {
                m.value = Some(show::ValueMeta {
                    kind: value_type,
                    method: value_method,
                    address,
                    name: None,
                    recipient_type: "node".into(),
                    suggested: None,
                });
            }
            if let Some(img) = image {
                input.image = Some(img);
            }
            let domains: Vec<String> = domains.into_iter().map(|d| d.to_ascii_lowercase()).collect();
            if dry_run {
                let keypair = c0mpute_core::hosting::publisher_keypair()?;
                let owner = c0mpute_core::hosting::site::owner_hex(&keypair).context("ed25519 identity")?;
                let site_id = c0mpute_core::hosting::site::site_id_for(&owner, &name);
                let base = base_url
                    .or_else(|| input.show.base_url.clone())
                    .or_else(|| domains.first().map(|d| format!("https://{d}/")));
                let out = show::build_dir(&name);
                let built = show::build_show(&input, &name, &site_id, base.as_deref(), &out)?;
                println!("built:   {} ({} episodes, {} bytes)", out.display(), built.episodes.len(), built.total_bytes);
                println!("site:    {site_id} (not published: --dry-run)");
                println!("guid:    {}", built.podcast_guid);
                return Ok(());
            }
            let (record, copies, built) = show::publish(
                &input,
                &name,
                domains.clone(),
                base_url,
                std::time::Duration::from_secs(wait_secs),
            )
            .await?;
            println!("show:    {} ({} episodes)", built.title, built.episodes.len());
            println!("site:    {}", record.site_id);
            println!("version: {}", record.seq);
            println!("copies:  {copies}/{}", c0mpute_core::hosting::TARGET_REPLICAS);
            println!("guid:    {}", built.podcast_guid);
            println!("feed:    /s/{}/feed.xml   (on any c0mpute gateway)", record.site_id);
            if let Some(b) = &built.base_url {
                println!("public:  {b}feed.xml");
            } else {
                eprintln!(
                    "note: enclosure URLs are relative (fine for gateways and `c0mpute podcasts`); \
                     pass --base-url or --domain for absolute URLs that every podcast app accepts"
                );
            }
            println!("listen:  c0mpute podcasts subscribe {}", record.site_id);
            if built.ads {
                println!(
                    "ads:     opted in (c0mpute-ads.json); gateways with ffmpeg splice ads into /s/{}/episodes/<file>?ads=1",
                    record.site_id
                );
            }
            for d in &domains {
                println!(
                    "domain:  {d}  -> add TXT {}.{d} \"site={}\" and point {d} at gateway nodes",
                    c0mpute_core::hosting::dns::TXT_PREFIX,
                    record.site_id
                );
            }
            // A striped site already said where its copies stand (and why
            // any are short) when the wait ended.
            if copies < c0mpute_core::hosting::TARGET_REPLICAS
                && !c0mpute_core::hosting::stripe::is_striped(record.size)
            {
                eprintln!(
                    "note: fewer than {} hosting nodes took a copy before the wait ran out; \
                     re-run publish (same --name) if no copies appear",
                    c0mpute_core::hosting::TARGET_REPLICAS
                );
            }
            Ok(())
        }
    }
}

/// A podcasts client for `source`: through the gateway, or (`--p2p`, c0mpute
/// shows only) through a hosting client run here. Keep the second value alive.
async fn podcasts_client(
    via: &PodcastsVia,
    source: &c0mpute_core::podcast::listen::Source,
) -> Result<(
    c0mpute_core::podcast::listen::Client,
    Option<c0mpute_core::podcast::listen::LocalGateway>,
)> {
    use c0mpute_core::podcast::listen::{Client, LocalGateway, Source};
    if via.p2p
        && let Source::Site(id) = source
    {
        eprintln!("joining the network to find site {id} ...");
        let lg = LocalGateway::start(id, std::time::Duration::from_secs(via.wait_secs)).await?;
        let c = Client::new(&lg.url)?;
        return Ok((c, Some(lg)));
    }
    Ok((Client::new(&via.gateway)?, None))
}

/// Refresh subscription `i` from the network and save.
async fn podcasts_refresh(
    store: &mut c0mpute_core::podcast::listen::Store,
    i: usize,
    via: &PodcastsVia,
) -> Result<(
    reqwest::Url,
    c0mpute_core::podcast::listen::Client,
    Option<c0mpute_core::podcast::listen::LocalGateway>,
)> {
    let source = store.subs[i].source.clone();
    let (client, lg) = podcasts_client(via, &source).await?;
    let (feed_url, feed) = client.fetch_feed(&source).await?;
    store.upsert(source, feed_url.to_string(), feed);
    store.save()?;
    Ok((feed_url, client, lg))
}

async fn run_podcasts(cmd: PodcastsCmd) -> Result<()> {
    use c0mpute_core::podcast::{dates, listen};
    let mut store = listen::Store::open_default()?;
    match cmd {
        PodcastsCmd::Subscribe { source, via } => {
            let source = listen::Source::parse(&source)?;
            let (client, _lg) = podcasts_client(&via, &source).await?;
            let (feed_url, feed) = client.fetch_feed(&source).await?;
            let n = feed.episodes.len();
            let title = feed.title.clone();
            let id = store.upsert(source, feed_url.to_string(), feed);
            store.save()?;
            println!("subscribed: {title}  ({n} episodes)  id: {id}");
            Ok(())
        }
        PodcastsCmd::Unsubscribe { show } => {
            let gone = store.remove(&show)?;
            store.save()?;
            println!("unsubscribed: {} ({})", gone.title, gone.id);
            Ok(())
        }
        PodcastsCmd::Ls => {
            if store.subs.is_empty() {
                println!("no subscriptions yet: c0mpute podcasts subscribe <feed-url|site_id>");
            }
            for (i, s) in store.subs.iter().enumerate() {
                let n = s.feed.as_ref().map(|f| f.episodes.len()).unwrap_or(0);
                println!("{:>3}  {:<28} {:<40} {n:>4} eps  {}", i + 1, s.id, s.title, s.source.label());
            }
            Ok(())
        }
        PodcastsCmd::Episodes { show, offline, via } => {
            let i = store.find(&show)?;
            if !offline && let Err(e) = podcasts_refresh(&mut store, i, &via).await {
                eprintln!("warning: could not refresh ({e:#}); showing the stored copy");
            }
            let sub = &store.subs[i];
            let feed = sub.feed.as_ref().context("no stored copy of this feed yet")?;
            println!("{}", feed.title);
            for (n, e) in listen::sorted_episodes(feed).iter().enumerate() {
                let date = e.published.map(dates::fmt_ymd).unwrap_or_else(|| "          ".into());
                let dur = e.duration_secs.map(dates::fmt_duration).unwrap_or_default();
                let audio = if e.enclosure.is_some() { "" } else { "  (no audio)" };
                println!("{:>4}  {date}  {dur:>8}  {}{audio}", n + 1, e.title);
            }
            Ok(())
        }
        PodcastsCmd::Download { show, episode, dir, via } => {
            let i = store.find(&show)?;
            let (feed_url, client, _lg) = podcasts_refresh(&mut store, i, &via).await?;
            let sub = &store.subs[i];
            let feed = sub.feed.as_ref().context("no feed")?;
            let ep = listen::find_episode(feed, episode.as_deref())?;
            let dir = dir.unwrap_or_else(|| listen::podcasts_dir().join("episodes").join(&sub.id));
            eprintln!("downloading {} ...", ep.title);
            let path = client.download(&feed_url, ep, &dir).await?;
            println!("{}", path.display());
            Ok(())
        }
        PodcastsCmd::Play { show, episode, via } => {
            let i = store.find(&show)?;
            let (feed_url, client, _lg) = podcasts_refresh(&mut store, i, &via).await?;
            let sub = &store.subs[i];
            let feed = sub.feed.as_ref().context("no feed")?;
            let ep = listen::find_episode(feed, episode.as_deref())?;
            let dir = listen::podcasts_dir().join("episodes").join(&sub.id);
            eprintln!("fetching {} ...", ep.title);
            let path = client.download(&feed_url, ep, &dir).await?;
            match listen::player() {
                Some((bin, args)) => {
                    eprintln!("playing {} with {bin} (q to stop)", path.display());
                    let status = std::process::Command::new(bin).args(args).arg(&path).status()?;
                    if !status.success() {
                        bail!("{bin} exited with {status}");
                    }
                }
                None => {
                    eprintln!("no player found (install mpv or ffmpeg's ffplay); the episode is at:");
                    println!("{}", path.display());
                }
            }
            Ok(())
        }
    }
}

async fn run_stream(cmd: StreamCmd) -> Result<()> {
    use c0mpute_core::live::broadcast;
    match cmd {
        StreamCmd::Go {
            input,
            name,
            format,
            segment_secs,
            window,
            low,
            codec,
            listen,
            bind,
            linger_secs,
            no_reconnect,
        } => {
            broadcast::go(broadcast::GoOptions {
                input,
                name,
                format,
                segment_secs,
                window,
                low,
                codec: codec.parse()?,
                listen,
                bind,
                linger: std::time::Duration::from_secs(linger_secs),
                reconnect: !no_reconnect,
            })
            .await
        }
        StreamCmd::Watch {
            stream_id,
            gateway,
            no_play,
        } => {
            if !c0mpute_core::live::record::valid_stream_id(&stream_id) {
                anyhow::bail!("a stream id is 20 lowercase hex characters (from `c0mpute stream ls`)");
            }
            let url = format!("{}/live/{stream_id}/index.m3u8", gateway.trim_end_matches('/'));
            println!("{url}");
            if no_play {
                return Ok(());
            }
            let player = ["mpv", "ffplay"].into_iter().find(|p| {
                std::env::var_os("PATH")
                    .map(|path| std::env::split_paths(&path).any(|d| d.join(p).is_file()))
                    .unwrap_or(false)
            });
            match player {
                Some(p) => {
                    let mut cmd = std::process::Command::new(p);
                    if p == "ffplay" {
                        cmd.arg("-hide_banner");
                    }
                    let status = cmd.arg(&url).status().with_context(|| format!("run {p}"))?;
                    if !status.success() {
                        anyhow::bail!("{p} exited with {status}");
                    }
                }
                None => eprintln!("(no mpv or ffplay on PATH; open the URL in any HLS player: VLC, Safari, hls.js)"),
            }
            Ok(())
        }
        StreamCmd::Ls { gateway } => {
            let url = format!("{}/live/v1/streams", gateway.trim_end_matches('/'));
            let v: serde_json::Value = reqwest::get(&url)
                .await
                .with_context(|| format!("GET {url}"))?
                .json()
                .await?;
            let streams = v["streams"].as_array().cloned().unwrap_or_default();
            if streams.is_empty() {
                println!("no live streams known to {gateway} right now");
            }
            for s in streams {
                let rends: Vec<String> = s["renditions"]
                    .as_array()
                    .map(|r| {
                        r.iter()
                            .map(|x| {
                                format!(
                                    "{}@{}kbps",
                                    x["name"].as_str().unwrap_or("?"),
                                    x["bandwidth"].as_u64().unwrap_or(0) / 1000
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                println!(
                    "{:<22} {:<24} {:<6} {:>4} holders  {}  {}",
                    s["stream_id"].as_str().unwrap_or(""),
                    s["name"].as_str().unwrap_or(""),
                    if s["ended"].as_bool().unwrap_or(false) { "ended" } else { "live" },
                    s["holders"],
                    rends.join(","),
                    s["path"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

fn parse_freq_cap(s: &str) -> Result<c0mpute_core::ads::campaign::FreqCap> {
    let (n, per) = s.split_once('/').context("frequency cap is N/hour, N/day, N/week or N/<seconds>")?;
    let max: u32 = n.trim().parse().context("frequency cap count")?;
    let per_secs = match per.trim() {
        "hour" | "h" => 3600,
        "day" | "d" => 86_400,
        "week" | "w" => 7 * 86_400,
        x => x.parse().context("frequency cap period")?,
    };
    Ok(c0mpute_core::ads::campaign::FreqCap { max, per_secs })
}

fn parse_placements(s: &str) -> Result<Vec<c0mpute_core::ads::campaign::Placement>> {
    use c0mpute_core::ads::campaign::Placement;
    let mut out = Vec::new();
    for p in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let pl = Placement::parse(p).with_context(|| format!("unknown placement {p:?}"))?;
        if !out.contains(&pl) {
            out.push(pl);
        }
    }
    if out.is_empty() {
        bail!("give at least one placement");
    }
    Ok(out)
}

/// Apply flags to a spec (create starts from an empty one, update from the
/// current record).
fn apply_ads_args(spec: &mut c0mpute_core::ads::manage::CampaignSpec, t: &AdsTargetArgs) -> Result<()> {
    use c0mpute_core::ads::{campaign, manage};
    if let Some(p) = &t.placement {
        spec.placements = parse_placements(p)?;
    }
    if let Some(m) = &t.max_cpm {
        spec.max_cpm_micros = campaign::parse_usd_micros(m)?;
    }
    if let Some(l) = &t.landing {
        spec.landing_url = l.clone();
    }
    if !t.category.is_empty() {
        spec.categories = campaign::norm_terms(&t.category);
    }
    if !t.keyword.is_empty() {
        spec.keywords = campaign::norm_terms(&t.keyword);
    }
    if !t.geo.is_empty() {
        spec.geo = campaign::norm_countries(&t.geo);
    }
    if let Some(s) = &t.start {
        spec.start = Some(manage::parse_time(s)?);
    }
    if let Some(e) = &t.end {
        spec.end = Some(manage::parse_time(e)?);
    }
    if let Some(f) = &t.freq_cap {
        spec.freq_cap = Some(parse_freq_cap(f)?);
    }
    if t.budget.is_some() || t.daily_budget.is_some() || t.payer.is_some() || t.funding_ref.is_some() {
        let mut b = spec.budget.clone().unwrap_or(c0mpute_core::ads::billing::Budget {
            total_micros: 0,
            daily_micros: None,
            rail: "coinpay".into(),
            payer: None,
            funding_ref: None,
        });
        if let Some(v) = &t.budget {
            b.total_micros = campaign::parse_usd_micros(v)?;
        }
        if let Some(v) = &t.daily_budget {
            b.daily_micros = Some(campaign::parse_usd_micros(v)?);
        }
        if let Some(v) = &t.payer {
            b.payer = Some(v.clone());
        }
        if let Some(v) = &t.funding_ref {
            b.funding_ref = Some(v.clone());
        }
        if b.total_micros == 0 {
            bail!("give --budget <usd> for a billed campaign");
        }
        b.check()?;
        spec.budget = Some(b);
    }
    Ok(())
}

fn print_campaign(r: &c0mpute_core::ads::campaign::CampaignRecord) {
    use c0mpute_core::ads::campaign::fmt_usd;
    let now = c0mpute_core::ads::now_unix();
    let state = if r.paused {
        "paused"
    } else if !r.schedule.contains(now) {
        "outside schedule"
    } else {
        "active"
    };
    println!("campaign:   {} ({})", r.name, r.campaign_id);
    println!("state:      {state}   version {}   signature {}", r.seq, if r.verify() { "ok" } else { "INVALID" });
    println!(
        "placements: {}",
        r.targeting.placements.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(",")
    );
    println!("max cpm:    {} (pays the second price, never more)", fmt_usd(r.max_cpm_micros));
    match &r.budget {
        Some(b) => {
            let spent = c0mpute_core::ads::billing::Ledger::load(&c0mpute_core::ads::manage::ledger_path(&r.name), &r.campaign_id)
                .map(|l| l.spent_micros)
                .unwrap_or(0);
            println!(
                "budget:     {} total, {} spent{}  rail {}",
                fmt_usd(b.total_micros),
                fmt_usd(spent),
                b.daily_micros.map(|d| format!(", {} per day", fmt_usd(d))).unwrap_or_default(),
                b.rail
            );
            if let Some(p) = &b.payer {
                println!("payer:      {p}");
            }
            if let Some(f) = &b.funding_ref {
                println!("funding:    {f}");
            }
        }
        None => println!("budget:     none (runs unpaid; add one with --budget)"),
    }
    println!("landing:    {}", r.landing_url);
    if !r.targeting.categories.is_empty() {
        println!("categories: {}", r.targeting.categories.join(","));
    }
    if !r.targeting.keywords.is_empty() {
        println!("keywords:   {}", r.targeting.keywords.join(","));
    }
    if !r.targeting.geo.is_empty() {
        println!("geo:        {}", r.targeting.geo.join(","));
    }
    if let Some(f) = r.freq_cap {
        println!("freq cap:   {} per {} s per listener", f.max, f.per_secs);
    }
    println!("creatives:  site {}", r.creative_site);
    for c in &r.creatives {
        let mut extra = String::new();
        if let Some(d) = c.duration_ms {
            extra.push_str(&format!(" {:.1}s", d as f64 / 1000.0));
        }
        if let (Some(w), Some(h)) = (c.width, c.height) {
            extra.push_str(&format!(" {w}x{h}"));
        }
        println!(
            "  {:<6} {:<32} {} bytes{extra}  {}",
            format!("{:?}", c.kind).to_lowercase(),
            c.path,
            c.bytes,
            &c.hash[..16]
        );
    }
}

async fn set_campaign_paused(which: &str, pause: bool) -> Result<()> {
    use c0mpute_core::ads::manage;
    let mut record = manage::load_local(which)?;
    record.paused = pause;
    record.seq = manage::now_seq();
    let keypair = c0mpute_core::hosting::publisher_keypair()?;
    let record = record.sign(&keypair)?;
    manage::save_local(&record)?;
    manage::publish(&record, None, std::time::Duration::from_secs(20)).await?;
    println!("{} {}", record.name, if pause { "paused" } else { "resumed" });
    Ok(())
}

async fn run_ads(cmd: AdsCmd) -> Result<()> {
    use c0mpute_core::ads::{campaign, manage};
    let wait = |s: u64| std::time::Duration::from_secs(s);
    match cmd {
        AdsCmd::Campaign { cmd } => match cmd {
            AdsCampaignCmd::Create { name, t, wait_secs } => {
                if manage::campaign_dir(&name).join("campaign.json").exists() {
                    bail!("campaign {name} exists; use `c0mpute ads campaign update {name}`");
                }
                if t.creatives.is_empty() {
                    bail!("give --creative <file|folder> (repeatable)");
                }
                let input = manage::collect_creatives(&t.creatives)?;
                let mut spec = manage::CampaignSpec {
                    name: name.clone(),
                    landing_url: input.meta.landing_url.clone().unwrap_or_default(),
                    ..Default::default()
                };
                apply_ads_args(&mut spec, &t)?;
                if spec.placements.is_empty() {
                    bail!("give --placement (e.g. pre,post or ctv_preroll or display)");
                }
                if spec.max_cpm_micros == 0 {
                    bail!("give --max-cpm in USD (e.g. 2.50)");
                }
                if spec.landing_url.is_empty() {
                    bail!("give --landing <url> (or landing_url in ad-set.json)");
                }
                let site_dir = manage::campaign_dir(&name).join("site");
                let creatives = manage::build_creative_dir(&input.files, &site_dir, t.duration_secs.map(|s| s * 1000))?;
                let keypair = c0mpute_core::hosting::publisher_keypair()?;
                let owner = c0mpute_core::hosting::site::owner_hex(&keypair).context("ed25519 identity")?;
                let record = manage::record_from_spec(&spec, &owner, creatives, manage::now_seq()).sign(&keypair)?;
                manage::save_local(&record)?;
                let copies = manage::publish(&record, Some(&site_dir), wait(wait_secs)).await?;
                print_campaign(&record);
                println!("copies:     {copies}/{} of the creative site", c0mpute_core::hosting::TARGET_REPLICAS);
                println!("vast:       http://<gateway>:7777/ads/v1/vast?placement=ctv_preroll");
                println!("report:     c0mpute ads report {name}");
                Ok(())
            }
            AdsCampaignCmd::Update { campaign: which, t, wait_secs } => {
                let current = manage::load_local(&which)?;
                let mut spec = manage::CampaignSpec {
                    name: current.name.clone(),
                    placements: current.targeting.placements.clone(),
                    categories: current.targeting.categories.clone(),
                    keywords: current.targeting.keywords.clone(),
                    geo: current.targeting.geo.clone(),
                    start: current.schedule.start,
                    end: current.schedule.end,
                    freq_cap: current.freq_cap,
                    max_cpm_micros: current.max_cpm_micros,
                    landing_url: current.landing_url.clone(),
                    budget: current.budget.clone(),
                };
                apply_ads_args(&mut spec, &t)?;
                let site_dir = manage::campaign_dir(&current.name).join("site");
                let (creatives, republish) = if t.creatives.is_empty() {
                    (current.creatives.clone(), false)
                } else {
                    let input = manage::collect_creatives(&t.creatives)?;
                    (
                        manage::build_creative_dir(&input.files, &site_dir, t.duration_secs.map(|s| s * 1000))?,
                        true,
                    )
                };
                let keypair = c0mpute_core::hosting::publisher_keypair()?;
                let owner = c0mpute_core::hosting::site::owner_hex(&keypair).context("ed25519 identity")?;
                let mut record = manage::record_from_spec(&spec, &owner, creatives, manage::now_seq());
                record.paused = current.paused;
                let record = record.sign(&keypair)?;
                manage::save_local(&record)?;
                manage::publish(&record, republish.then_some(site_dir.as_path()), wait(wait_secs)).await?;
                print_campaign(&record);
                Ok(())
            }
            AdsCampaignCmd::Pause { campaign: which } => set_campaign_paused(&which, true).await,
            AdsCampaignCmd::Resume { campaign: which } => set_campaign_paused(&which, false).await,
            AdsCampaignCmd::Ls { gateway } => {
                if let Some(g) = gateway {
                    let url = format!("{}/ads/v1/campaigns", g.trim_end_matches('/'));
                    let v: serde_json::Value = reqwest::get(&url)
                        .await
                        .with_context(|| format!("GET {url}"))?
                        .json()
                        .await?;
                    let list = v["campaigns"].as_array().cloned().unwrap_or_default();
                    if list.is_empty() {
                        println!("{g} knows no campaigns yet");
                    }
                    for c in list {
                        println!(
                            "{:<22} {:<28} {:<7} cpm ${:<6} {}",
                            c["campaign_id"].as_str().unwrap_or(""),
                            c["name"].as_str().unwrap_or(""),
                            if c["active"].as_bool() == Some(true) { "active" } else { "idle" },
                            c["max_cpm_usd"],
                            c["placements"]
                                .as_array()
                                .map(|p| p.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                                .unwrap_or_default()
                        );
                    }
                    return Ok(());
                }
                let list = manage::list_local()?;
                if list.is_empty() {
                    println!(
                        "no campaigns yet: c0mpute ads campaign create --name <n> --creative <dir> --placement pre --max-cpm 2 --landing <url>"
                    );
                }
                for r in list {
                    println!(
                        "{:<22} {:<28} {:<7} {} {}",
                        r.campaign_id,
                        r.name,
                        if r.paused { "paused" } else { "live" },
                        campaign::fmt_usd(r.max_cpm_micros),
                        r.targeting.placements.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(",")
                    );
                }
                Ok(())
            }
            AdsCampaignCmd::Show { campaign: which } => {
                print_campaign(&manage::load_local(&which)?);
                Ok(())
            }
        },
        AdsCmd::Report {
            campaign: which,
            gateways,
            wait_secs,
            json,
        } => {
            let id = manage::load_local(&which).map(|r| r.campaign_id).unwrap_or(which);
            let rep = manage::report(&id, &gateways, wait(wait_secs)).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
                return Ok(());
            }
            println!("campaign:     {}", rep.campaign_id);
            println!("impressions:  {}", rep.impressions());
            println!("listeners:    {} unique (per gateway hashing)", rep.unique_listeners);
            for (ev, n) in &rep.events {
                println!("  {ev:<14} {n}");
            }
            for (p, n) in &rep.by_placement {
                println!("  placement {p:<14} {n}");
            }
            for (g, n) in &rep.by_gateway {
                println!("  gateway {}  {n}", &g[..16.min(g.len())]);
            }
            for (s, n) in &rep.by_site {
                println!("  show /s/{s}/  {n}");
            }
            println!("batches:      {} verified, {} rejected", rep.batches, rep.rejected_batches);
            for (g, seq) in &rep.gaps {
                println!(
                    "  gap: gateway {} is missing batch {seq} (offline, or not yet re-gossiped)",
                    &g[..16.min(g.len())]
                );
            }
            Ok(())
        }
        AdsCmd::Settle {
            campaign: which,
            pay,
            wait_secs,
            json,
        } => {
            use c0mpute_core::ads::billing;
            let record = manage::load_local(&which)?;
            let cfg = Config::load_or_default(&config::default_config_path())?;
            let fee_payout = cfg.ads.fee_payout.clone().unwrap_or_default();
            if !fee_payout.is_empty() && !billing::valid_payout(&fee_payout) {
                bail!("[ads] fee_payout must be <COIN>:<address>, e.g. \"USDC_POL:0x...\"");
            }
            // Real money needs both switches: `[ads] payouts` (this feature)
            // and `[payments] enabled` (the node's one payment rail).
            let rail: Option<std::sync::Arc<dyn c0mpute_core::payments::Rail>> = if pay {
                if !cfg.ads.payouts || !cfg.payments.enabled {
                    bail!(
                        "--pay sends real money; it needs both `[ads] payouts = true` and `[payments] enabled = true` in {}",
                        config::default_config_path().display()
                    );
                }
                Some(
                    c0mpute_core::payments::rail_from_config(&cfg.payments)?
                        .context("[payments] is enabled but no rail was configured")?,
                )
            } else {
                None
            };
            let (round, ledger) =
                manage::settle(&record, &fee_payout, rail.as_deref(), cfg.payments.round_cap(), wait(wait_secs)).await?;
            if json {
                let settled: Vec<_> = round
                    .settled
                    .iter()
                    .map(|(s, v)| serde_json::json!({"settlement": s, "verified": v}))
                    .collect();
                let refused: Vec<_> = round
                    .refused
                    .iter()
                    .map(|(c, e)| serde_json::json!({"claim": c, "error": e}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "campaign_id": record.campaign_id,
                        "paid": pay,
                        "claims": round.claims,
                        "batches": round.batches,
                        "settled": settled,
                        "refused": refused,
                        "spent_micros": ledger.spent_micros,
                        "unpaid_micros": ledger.unpaid_micros(),
                        "lines": ledger.lines,
                    }))?
                );
                return Ok(());
            }
            let budget = record.budget.as_ref().map(|b| b.total_micros).unwrap_or(0);
            println!("campaign:   {} ({})", record.name, record.campaign_id);
            println!("heard:      {} claims, {} receipt batches", round.claims, round.batches);
            for (s, v) in &round.settled {
                println!(
                    "{} gateway {} batches {}-{}: {} impressions, {}",
                    if pay { "settled" } else { "would pay" },
                    &s.gateway[..16.min(s.gateway.len())],
                    s.from_seq,
                    s.to_seq,
                    v.impressions,
                    campaign::fmt_usd(s.gross_micros)
                );
                for (why, n) in &v.excluded {
                    println!("    not paid: {n} {}", serde_json::to_string(why)?.trim_matches('"'));
                }
                for p in &s.payouts {
                    println!(
                        "    {:<9} {:>10}  {}",
                        serde_json::to_string(&p.role)?.trim_matches('"'),
                        format!("${}", billing::usd_decimal(p.micros)),
                        if p.address.is_empty() { "(owed: no address)" } else { &p.address }
                    );
                }
            }
            for (c, e) in &round.refused {
                println!("refused gateway {}: {e}", &c.gateway[..16.min(c.gateway.len())]);
            }
            let spent = if pay {
                ledger.spent_micros
            } else {
                ledger.spent_micros + round.settled.iter().map(|(s, _)| s.gross_micros).sum::<u64>()
            };
            println!(
                "budget:     {} of {} spent{}",
                campaign::fmt_usd(spent),
                campaign::fmt_usd(budget),
                if pay { "" } else { " (if paid)" }
            );
            if pay {
                for l in ledger.lines.iter().filter(|l| l.error.is_some()) {
                    println!("  unpaid line {}/{}: {}", l.settlement, l.line, l.error.as_deref().unwrap_or(""));
                }
            } else if !round.settled.is_empty() {
                println!("dry run: nothing signed or paid. Pay with --pay ([ads] payouts = true).");
            }
            Ok(())
        }
        AdsCmd::Select {
            gateway,
            placement,
            category,
            keyword,
            country,
        } => {
            let mut url = reqwest::Url::parse(&format!("{}/ads/v1/select", gateway.trim_end_matches('/')))?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("placement", &placement);
                if !category.is_empty() {
                    q.append_pair("categories", &category.join(","));
                }
                if !keyword.is_empty() {
                    q.append_pair("keywords", &keyword.join(","));
                }
                if let Some(c) = &country {
                    q.append_pair("country", c);
                }
            }
            let v: serde_json::Value = reqwest::get(url.clone())
                .await
                .with_context(|| format!("GET {url}"))?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
    }
}

/// `--disks` plus the `--max-price` / `--country` shorthands as one query.
fn disk_query(
    spec: &str,
    max_price: Option<&str>,
    country: Option<&str>,
) -> Result<c0mpute_core::hosting::market::DiskQuery> {
    let mut q = c0mpute_core::hosting::market::DiskQuery::parse(spec)?;
    if let Some(p) = max_price {
        q.max_price = Some(c0mpute_core::payments::parse_usd(p).with_context(|| format!("--max-price {p:?}"))?);
    }
    if let Some(c) = country {
        q.add_countries(c)?;
    }
    Ok(q)
}

fn parse_kv(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut m = std::collections::BTreeMap::new();
    for p in pairs {
        let (k, v) = p.split_once('=').with_context(|| format!("{p:?} is not KEY=VALUE"))?;
        m.insert(k.trim().to_string(), v.to_string());
    }
    Ok(m)
}

fn app_id_from(app: &str) -> String {
    match c0mpute_core::apps::deploy::load_owner(app).record {
        Some(r) => r.app_id,
        None => app.to_string(),
    }
}

async fn run_apps(cmd: AppsCmd) -> Result<()> {
    use c0mpute_core::apps::deploy;
    match cmd {
        AppsCmd::Deploy {
            dir,
            name,
            image,
            pull,
            port,
            health,
            state,
            snapshot_secs,
            env,
            memory_mb,
            cpus,
            replicas,
            domains,
            hosts,
            wait_secs,
        } => {
            let wait = std::time::Duration::from_secs(wait_secs);
            let manifest = match &dir {
                Some(d) => deploy::read_manifest(d)?,
                None => None,
            };
            let mut opts = match (&manifest, &dir) {
                (Some(m), Some(d)) => m.options(d, wait),
                _ => deploy::DeployOptions {
                    name: String::new(),
                    source: match &dir {
                        Some(d) => deploy::Source::Build(d.clone()),
                        None => deploy::Source::Local(String::new()),
                    },
                    port: 8080,
                    health_path: "/".into(),
                    env: Default::default(),
                    state_path: None,
                    snapshot_secs: c0mpute_core::apps::record::DEFAULT_SNAPSHOT_SECS,
                    memory_mb: 256,
                    millicpus: 500,
                    replicas: c0mpute_core::apps::record::DEFAULT_REPLICAS,
                    domains: vec![],
                    hosts: vec![],
                    wait,
                },
            };
            if let Some(n) = name {
                opts.name = n;
            }
            if opts.name.is_empty() {
                anyhow::bail!("--name is required (or a c0mpute.toml with [app] name)");
            }
            if let Some(i) = image {
                opts.source = deploy::Source::Local(i);
            }
            if let Some(p) = pull {
                opts.source = deploy::Source::Pull(p);
            }
            if matches!(&opts.source, deploy::Source::Local(i) if i.is_empty()) {
                anyhow::bail!("give a directory to build, --image, or --pull");
            }
            if let Some(p) = port {
                opts.port = p;
            }
            if let Some(h) = health {
                opts.health_path = h;
            }
            if state.is_some() {
                opts.state_path = state;
            }
            if let Some(s) = snapshot_secs {
                opts.snapshot_secs = s;
            }
            opts.env.extend(parse_kv(&env)?);
            if let Some(m) = memory_mb {
                opts.memory_mb = m;
            }
            if let Some(c) = cpus {
                opts.millicpus = (c * 1000.0).round() as u32;
            }
            if let Some(r) = replicas {
                opts.replicas = r;
            }
            opts.domains.extend(domains);
            opts.hosts.extend(hosts);
            opts.wait = wait;
            let rec = deploy::deploy(opts).await?;
            println!("{}", rec.app_id);
            for d in &rec.domains {
                eprintln!("  domain {d}: TXT _c0mpute-app.{d} = \"app={}\"", rec.app_id);
            }
            Ok(())
        }
        AppsCmd::Ls { gateway, json } => {
            let url = format!("{}/apps/v1/apps", gateway.trim_end_matches('/'));
            let v: serde_json::Value = reqwest::get(&url).await.with_context(|| format!("GET {url}"))?.json().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let apps = v["apps"].as_array().cloned().unwrap_or_default();
            if apps.is_empty() {
                println!("no apps known to {gateway} yet");
            }
            for a in apps {
                let holders = a["holders"].as_array().map(|h| h.len()).unwrap_or(0);
                println!(
                    "{:<22} {:<24} {}/{} copies  primary {}  epoch {}  {}",
                    a["app_id"].as_str().unwrap_or(""),
                    a["name"].as_str().unwrap_or(""),
                    holders,
                    a["replicas"],
                    a["primary"].as_str().map(|p| &p[p.len().saturating_sub(8)..]).unwrap_or("none"),
                    a["epoch"],
                    if a["healthy"].as_bool() == Some(true) { "healthy" } else { "down" }
                );
            }
            Ok(())
        }
        AppsCmd::Status { app, gateway } => {
            let id = app_id_from(&app);
            let url = format!("{}/apps/v1/apps/{id}", gateway.trim_end_matches('/'));
            let v: serde_json::Value = reqwest::get(&url).await.with_context(|| format!("GET {url}"))?.json().await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        AppsCmd::Logs { app, tail } => {
            print!("{}", deploy::logs(&app, tail).await?);
            Ok(())
        }
        AppsCmd::Secrets { cmd } => match cmd {
            AppsSecretsCmd::Set { app, pairs } => {
                for (k, v) in parse_kv(&pairs)? {
                    deploy::set_secret(&app, &k, Some(&v))?;
                }
                eprintln!("saved; run `c0mpute apps deploy` to hand them to the holders");
                Ok(())
            }
            AppsSecretsCmd::Unset { app, keys } => {
                for k in keys {
                    deploy::set_secret(&app, &k, None)?;
                }
                Ok(())
            }
            AppsSecretsCmd::Ls { app } => {
                if let Some(s) = deploy::load_owner(&app).secrets {
                    for k in s.env.keys() {
                        println!("{k}");
                    }
                }
                Ok(())
            }
        },
        AppsCmd::Mcp { gateway } => apps_mcp::serve(gateway).await,
        AppsCmd::Destroy { name, yes } => {
            if !yes {
                anyhow::bail!("this deletes {name} and its state everywhere; pass --yes to confirm");
            }
            let id = deploy::destroy(&name).await?;
            println!("deleted {name} ({id})");
            Ok(())
        }
    }
}

async fn run_hosting(cmd: HostingCmd) -> Result<()> {
    match cmd {
        HostingCmd::Publish {
            dir,
            name,
            domains,
            wait_secs,
            disks,
            max_price,
            country,
            copies,
            d1sks,
            private,
            rekey,
            gateway,
        } => {
            if private {
                if !domains.is_empty() || disks.is_some() {
                    anyhow::bail!("--private sites are reached by their link only: drop --domain and --disks");
                }
                return private_storage::publish_site(&dir, &name, rekey, &gateway, std::time::Duration::from_secs(wait_secs)).await;
            }
            let domains: Vec<String> = domains.into_iter().map(|d| d.to_ascii_lowercase()).collect();
            let market = match disks {
                Some(spec) => Some(c0mpute_core::hosting::MarketOptions {
                    query: disk_query(&spec, max_price.as_deref(), country.as_deref())?,
                    copies: copies.clamp(1, c0mpute_core::hosting::site::MAX_PREFERRED_HOSTS),
                    d1sks_url: d1sks,
                }),
                None => {
                    if max_price.is_some() || country.is_some() {
                        anyhow::bail!("--max-price and --country pick marketplace hosts; add --disks auto");
                    }
                    None
                }
            };
            let (record, copies) = c0mpute_core::hosting::publish_dir_with(
                &dir,
                &name,
                domains.clone(),
                std::time::Duration::from_secs(wait_secs),
                market,
            )
            .await?;
            println!("site:    {}", record.site_id);
            println!("version: {}", record.seq);
            println!("copies:  {copies}/{}", c0mpute_core::hosting::TARGET_REPLICAS);
            if !record.preferred_hosts.is_empty() {
                println!(
                    "paid:    {} preferred host(s) at ${}/GiB-month, paid by {}",
                    record.preferred_hosts.len(),
                    record.price_per_gib_month.as_deref().unwrap_or("?"),
                    record.payer_key()
                );
                println!(
                    "         keep a c0mpute node running on this machine: it challenges the \
                     holders and settles (dry run until [payments] is enabled)"
                );
            }
            println!("url:     {}/s/{}/", gateway.trim_end_matches('/'), record.site_id);
            println!("path:    /s/{}/   (works on any c0mpute gateway, including http://127.0.0.1:7777 on a node)", record.site_id);
            for d in &domains {
                println!(
                    "domain:  {d}  -> add TXT {}.{d} \"site={}\" and point {d} at gateway nodes",
                    c0mpute_core::hosting::dns::TXT_PREFIX,
                    record.site_id
                );
            }
            // A striped site already said where its copies stand (and why
            // any are short) when the wait ended.
            if copies < c0mpute_core::hosting::TARGET_REPLICAS
                && !c0mpute_core::hosting::stripe::is_striped(record.size)
            {
                eprintln!(
                    "note: fewer than {} hosting nodes took a copy before the wait ran out; \
                     the network keeps replicating as hosting nodes see the record, but \
                     re-run publish (same --name) if no copies appear",
                    c0mpute_core::hosting::TARGET_REPLICAS
                );
            }
            // Some copies held = published (the network heals the rest);
            // none held = nothing is serving the site yet, so fail loudly.
            if copies == 0 {
                eprintln!(
                    "status:  not yet held by any hosting node after {wait_secs}s (exit 1); \
                     re-run publish with the same --name"
                );
                std::process::exit(1);
            }
            Ok(())
        }
        HostingCmd::Disks {
            query,
            max_price,
            country,
            d1sks,
            live,
        } => {
            let q = disk_query(&query, max_price.as_deref(), country.as_deref())?;
            let m = c0mpute_core::hosting::MarketOptions {
                query: q.clone(),
                copies: 1,
                d1sks_url: d1sks.clone(),
            };
            let disks = if live {
                eprintln!(
                    "listening to the hosting gossip for up to {}s ...",
                    c0mpute_core::hosting::GOSSIP_CHECK.as_secs()
                );
                let sup = c0mpute_core::Supervisor::boot_buyer(c0mpute_core::Config::default()).await?;
                let h = c0mpute_core::hosting::Hosting::start(
                    sup.libp2p.clone(),
                    sup.store.clone(),
                    c0mpute_core::hosting::Options::default(),
                )
                .await?;
                c0mpute_core::hosting::market_disks(&m, Some(&h)).await?
            } else {
                c0mpute_core::hosting::market_disks(&m, None).await?
            };
            let ranked = c0mpute_core::hosting::market::rank(&disks, &q, 0);
            if ranked.is_empty() {
                println!("no c0mpute hosting disks on {d1sks} match ({} listed)", disks.len());
            } else {
                println!(
                    "{:<54} {:<10} {:>10}  {:<8} {:<8} {:<8} {:<6} NAME",
                    "PEER", "$/GiB-MO", "FREE", "REACH", "SEEN", "GOSSIP", "WHERE"
                );
            }
            for d in ranked {
                let seen = match d.heartbeat_age_secs {
                    Some(s) if s < 120 => format!("{s}s"),
                    Some(s) => format!("{}m", s / 60),
                    None => "-".into(),
                };
                let gossip = if d.live { "live" } else if live { "silent" } else { "-" };
                println!(
                    "{:<54} {:<10} {:>6.1} GiB  {:<8} {:<8} {:<8} {:<6} {}",
                    d.peer_id,
                    c0mpute_core::payments::format_usd(d.price),
                    d.free_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    d.reachable.as_deref().unwrap_or("-"),
                    seen,
                    gossip,
                    if d.countries.is_empty() { "-".to_string() } else { d.countries.join(",") },
                    d.name
                );
            }
            Ok(())
        }
        HostingCmd::Rm { name, wait_secs } => {
            let rec = c0mpute_core::hosting::unpublish(&name, std::time::Duration::from_secs(wait_secs)).await?;
            println!("site:    {} ({name}) is empty now, version {}", rec.site_id, rec.seq);
            Ok(())
        }
        HostingCmd::Get {
            site,
            path,
            out,
            sequential,
            from,
        } => {
            let out = out.unwrap_or_else(|| {
                std::path::PathBuf::from(path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("index.html"))
            });
            let d = c0mpute_core::hosting::download(&site, &path, &out, sequential, from).await?;
            let secs = d.took.as_secs_f64().max(0.001);
            println!(
                "{} bytes in {:.1} s = {:.1} MB/s ({}) -> {}",
                d.bytes,
                secs,
                d.bytes as f64 / secs / 1e6,
                if sequential { "sequential" } else { "parallel" },
                out.display()
            );
            for (p, b) in d.by_peer {
                println!("  {p}  {:.1} MiB", b as f64 / 1048576.0);
            }
            Ok(())
        }
        HostingCmd::Ls { gateway, mine, json } => {
            let url = format!("{}/hosting/v1/sites", gateway.trim_end_matches('/'));
            let mut v: serde_json::Value = reqwest::get(&url)
                .await
                .with_context(|| format!("GET {url}"))?
                .json()
                .await?;
            if mine {
                // Read the key without creating one: no key, no sites.
                let owner = config::data_dir()
                    .and_then(|d| c0mpute_net::identity::existing_peer_id(&d.join("buyer")))
                    .as_ref()
                    .and_then(c0mpute_net::identity::ed25519_public_hex)
                    .unwrap_or_else(|| "-".into());
                if let Some(list) = v["sites"].as_array_mut() {
                    list.retain(|s| s["owner"].as_str() == Some(owner.as_str()));
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let sites = v["sites"].as_array().cloned().unwrap_or_default();
            if sites.is_empty() {
                println!("no sites known to {gateway} yet");
            }
            for s in sites {
                // Striped sites: copies = fewest copies of any group.
                let striped = if s["striped"].as_bool() == Some(true) {
                    format!(
                        "  striped: {} groups, {} short, {} without a copy",
                        s["groups"], s["short_groups"], s["missing_groups"]
                    )
                } else {
                    String::new()
                };
                println!(
                    "{:<22} {:<24} copies {}/{}  {} bytes{}  {}",
                    s["site_id"].as_str().unwrap_or(""),
                    s["name"].as_str().unwrap_or(""),
                    s["copies"],
                    v["target_copies"],
                    s["size"],
                    striped,
                    s["domains"]
                        .as_array()
                        .map(|d| d.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                        .unwrap_or_default()
                );
                // Who holds a striped site, and how much (copies are counted
                // one per machine; this lists every node).
                for hd in s["holders"].as_array().into_iter().flatten() {
                    let room = hd["room"]
                        .as_u64()
                        .map(|r| format!("{:.1} GiB free", r as f64 / (1u64 << 30) as f64))
                        .unwrap_or_else(|| "room ?".into());
                    println!(
                        "    held by {}  {}/{} groups  {}",
                        hd["peer"].as_str().unwrap_or("?"),
                        hd["groups"],
                        s["groups"],
                        room
                    );
                }
            }
            Ok(())
        }
    }
}

async fn run_crawl(cmd: CrawlCmd) -> Result<()> {
    match cmd {
        CrawlCmd::Fetch {
            url,
            max_price,
            ignore_robots,
            timeout_secs,
        } => {
            let spec = serde_json::json!({ "v": 1, "url": url, "ignore_robots": ignore_robots });
            c0mpute_core::crawl::parse_spec(&spec).map_err(|e| anyhow::anyhow!(e))?;
            let result = c0mpute_core::jobs_gateway::submit_once(
                c0mpute_core::crawl::WORKLOAD,
                spec,
                max_price,
                std::time::Duration::from_secs(timeout_secs.clamp(5, 300)),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

async fn run_ocr(
    file: std::path::PathBuf,
    lang: String,
    max_price: f64,
    timeout_secs: u64,
    json: bool,
) -> Result<()> {
    use c0mpute_core::ocr;
    if !ocr::valid_lang(&lang) {
        anyhow::bail!("--lang must match [a-z_+]{{3,32}}, e.g. eng or eng+deu");
    }
    let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
    if bytes.is_empty() || bytes.len() > ocr::MAX_INPUT_BYTES {
        anyhow::bail!("the document must be between 1 byte and 32 MiB");
    }
    let format = ocr::detect_format(&bytes)
        .context("not a PNG, JPEG, TIFF, WebP image or a PDF")?;
    let result = c0mpute_core::jobs_gateway::submit_once_with_input(
        ocr::WORKLOAD,
        Some(bytes),
        |blob| {
            let (pieces, peer) = blob.expect("the document was stored");
            let mut spec = serde_json::json!({
                "v": 1,
                "input_provider_peer_id": peer,
                "format": format,
                "lang": lang,
            });
            if let Some(obj) = spec.as_object_mut() {
                obj.extend(pieces.spec_fields("input"));
            }
            spec
        },
        max_price,
        std::time::Duration::from_secs(timeout_secs.clamp(10, 1800)),
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", result["result"]["text"].as_str().unwrap_or_default());
    }
    Ok(())
}

struct RenderArgs {
    file: std::path::PathBuf,
    frames: String,
    engine: String,
    device: String,
    format: String,
    resolution: Option<u32>,
    samples: Option<u32>,
    out: std::path::PathBuf,
    batch: u32,
    concurrency: usize,
    attempts: usize,
    max_price: f64,
    frame_timeout_secs: u64,
    mp4: Option<std::path::PathBuf>,
    fps: u32,
}

/// Most frames one `c0mpute render` run takes.
const MAX_RENDER_FRAMES: usize = 100_000;

async fn run_render(args: RenderArgs) -> Result<()> {
    use c0mpute_core::render;
    let frames = render::parse_frames(&args.frames, MAX_RENDER_FRAMES).map_err(anyhow::Error::msg)?;
    let engine = render::Engine::parse(&args.engine)
        .context("--engine must be CYCLES, EEVEE or WORKBENCH")?;
    let device = render::Device::parse(&args.device).context("--device must be AUTO, CPU or GPU")?;
    let format = render::Format::parse(&args.format).context("--format must be PNG, JPEG or EXR")?;
    if !args.max_price.is_finite() || args.max_price < 0.0 {
        anyhow::bail!("--max-price must be a non-negative number");
    }
    let mp4 = args.mp4.map(|p| {
        if p.as_os_str().is_empty() {
            args.out.join("render.mp4")
        } else {
            p
        }
    });
    if mp4.is_some() {
        if format == render::Format::Exr {
            anyhow::bail!("--mp4 needs PNG or JPEG frames");
        }
        if frames.windows(2).any(|w| w[1] != w[0] + 1) {
            anyhow::bail!("--mp4 needs a contiguous frame range");
        }
        if c0mpute_core::supervisor::which_ffmpeg().is_none() {
            anyhow::bail!("--mp4 needs ffmpeg on PATH");
        }
    }
    let size = std::fs::metadata(&args.file)
        .with_context(|| format!("read {}", args.file.display()))?
        .len();
    if size == 0 || size > render::MAX_INPUT_BYTES {
        anyhow::bail!("the .blend must be between 1 byte and 512 MiB");
    }
    // Check the per-job spec (resolution, samples, engine/device, timeout)
    // before the network comes up.
    let probe = serde_json::json!({
        "v": 1,
        "input_hash": c0mpute_proto::Hash::of(b"").to_hex(),
        "input_size": size,
        "input_pieces": vec![c0mpute_proto::Hash::of(b"").to_hex(); render::piece_count(size)],
        "frame_start": frames[0],
        "frame_end": frames[0],
        "engine": engine.name(),
        "device": device.name(),
        "format": format.name(),
        "resolution_pct": args.resolution,
        "samples": args.samples,
        "frame_timeout_secs": args.frame_timeout_secs,
    });
    render::parse_spec(&probe).map_err(anyhow::Error::msg)?;

    let supervisor = c0mpute_core::Supervisor::boot_buyer(c0mpute_core::Config::default()).await?;
    // Give the swarm a moment to reach peers before the first auction.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let job = render::RenderJob {
        blend: args.file,
        frames: frames.clone(),
        batch: args.batch,
        engine,
        device,
        format,
        resolution_pct: args.resolution,
        samples: args.samples,
        frame_timeout: std::time::Duration::from_secs(args.frame_timeout_secs),
        max_price_usd: args.max_price,
        concurrency: args.concurrency.clamp(1, 256),
        attempts: args.attempts.clamp(1, 10),
        out_dir: args.out.clone(),
        allowed_bidders: Vec::new(),
        bid_window: c0mpute_core::jobs_gateway::DEFAULT_BID_WINDOW,
    };
    eprintln!("rendering {} frame(s) across c0mpute workers", frames.len());
    let report = render::render_frames(supervisor.libp2p.clone(), &supervisor.store, job, |line| {
        eprintln!("{line}")
    })
    .await?;
    let workers: std::collections::BTreeSet<&str> =
        report.written.iter().map(|f| f.worker.as_str()).collect();
    eprintln!(
        "{} of {} frame(s) written to {} by {} worker(s)",
        report.written.len(),
        frames.len(),
        args.out.display(),
        workers.len()
    );
    if !report.failed.is_empty() {
        let list: Vec<String> = report.failed.iter().map(|(f, _)| f.to_string()).collect();
        anyhow::bail!("frames not rendered: {}", list.join(","));
    }
    if let Some(mp4) = mp4 {
        let ffmpeg = c0mpute_core::supervisor::which_ffmpeg().context("ffmpeg not found")?;
        let status = Command::new(ffmpeg)
            .args(render::mp4_args(&args.out, frames[0], format, args.fps, &mp4)?)
            .status()
            .context("run ffmpeg")?;
        if !status.success() {
            anyhow::bail!("ffmpeg failed ({status})");
        }
        println!("{}", mp4.display());
    } else {
        for f in &report.written {
            println!("{}", f.path.display());
        }
    }
    Ok(())
}

async fn run_embed(
    cmd: Option<EmbedCmd>,
    texts: Vec<String>,
    model: Option<String>,
    max_price: f64,
    timeout_secs: u64,
) -> Result<()> {
    use c0mpute_core::embed;
    match cmd {
        Some(EmbedCmd::Setup { all }) => {
            let models: Vec<&'static embed::Model> = embed::MODELS
                .iter()
                .filter(|m| all || m.default_install)
                .collect();
            embed::setup(&models).await?;
            for m in models {
                println!("ready: {} ({} dims)", m.id, m.dims);
            }
            Ok(())
        }
        Some(EmbedCmd::Status) => {
            println!("runtime: {}", embed::embed_root().display());
            for m in embed::MODELS {
                let state = if embed::model_ready(m) { "ready" } else { "not installed" };
                println!("{:<32} {:>4} dims  {state}", m.id, m.dims);
            }
            Ok(())
        }
        None => {
            let texts = if texts.is_empty() {
                use std::io::{BufRead, IsTerminal};
                if std::io::stdin().is_terminal() {
                    anyhow::bail!("give texts to embed as arguments, or pipe them one per line");
                }
                std::io::stdin()
                    .lock()
                    .lines()
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|l| !l.trim().is_empty())
                    .collect()
            } else {
                texts
            };
            let spec = serde_json::json!({ "v": 1, "model": model, "texts": texts });
            embed::parse_spec(&spec).map_err(|e| anyhow::anyhow!(e))?;
            let result = c0mpute_core::jobs_gateway::submit_once(
                embed::WORKLOAD,
                spec,
                max_price,
                std::time::Duration::from_secs(timeout_secs.clamp(5, 600)),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
    }
}

struct CaptchaArgs {
    cmd: Option<CaptchaCmd>,
    file: Option<std::path::PathBuf>,
    kind: String,
    background: Option<std::path::PathBuf>,
    charset: Option<String>,
    case: Option<String>,
    question: Option<String>,
    plain: bool,
    max_price: f64,
    timeout_secs: u64,
}

/// The jobs bridge answers `{job_id, worker, price, output_hash, result}`;
/// print the answer itself (as `POST /v1/captcha` does) with the job fields
/// beside it.
fn flatten_job_result(envelope: serde_json::Value) -> serde_json::Value {
    let mut answer = match envelope.get("result") {
        Some(serde_json::Value::Object(o)) => serde_json::Value::Object(o.clone()),
        _ => return envelope,
    };
    for (from, to) in [("job_id", "job_id"), ("worker", "worker_peer_id"), ("price", "price")] {
        if let Some(v) = envelope.get(from) {
            answer[to] = v.clone();
        }
    }
    answer
}

#[cfg(test)]
mod captcha_cli_tests {
    #[test]
    fn job_envelope_is_flattened() {
        let env = serde_json::json!({
            "job_id": "j", "worker": "w", "price": 0.0006, "output_hash": "h",
            "result": {"v": 1, "ok": true, "kind": "image", "text": "4831"}
        });
        let out = super::flatten_job_result(env);
        assert_eq!(out["text"], "4831");
        assert_eq!(out["worker_peer_id"], "w");
        assert_eq!(out["job_id"], "j");
        assert!(out.get("result").is_none());
        let bare = serde_json::json!({"text": "7"});
        assert_eq!(super::flatten_job_result(bare.clone()), bare);
    }
}

async fn run_captcha(a: CaptchaArgs) -> Result<()> {
    use base64::Engine as _;
    use c0mpute_core::captcha;
    match a.cmd {
        Some(CaptchaCmd::Setup) => {
            let version = captcha::setup().await?;
            println!("ready: ddddocr {version} ({})", captcha::captcha_root().display());
            return Ok(());
        }
        Some(CaptchaCmd::Status) => {
            println!("runtime: {}", captcha::captcha_root().display());
            println!("captcha.solve: {}", if captcha::ready() { "ready" } else { "not installed" });
            return Ok(());
        }
        None => {}
    }
    if a.case.as_deref().is_some_and(|c| !matches!(c, "lower" | "upper" | "any")) {
        anyhow::bail!("--case must be lower, upper or any");
    }
    let kind = if a.question.is_some() { "math".to_string() } else { a.kind.to_ascii_lowercase() };
    let mut result = if kind == "math" {
        let question = a.question.as_deref().context("--type math needs --question")?;
        let answer = captcha::solve_math(question).context("no arithmetic found in the question")?;
        serde_json::json!({"v": 1, "ok": true, "kind": "math", "text": answer, "engine": "local"})
    } else {
        if !captcha::WORKER_KINDS.contains(&kind.as_str()) {
            anyhow::bail!(captcha::unsupported_kind_message(&kind));
        }
        let b64 = |path: &std::path::Path| -> Result<String> {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
        };
        let file = a.file.as_deref().context("give the captcha image file")?;
        let mut spec = serde_json::json!({ "v": 1, "kind": kind, "image": b64(file)? });
        if let Some(bg) = a.background.as_deref() {
            spec["background"] = serde_json::json!(b64(bg)?);
        }
        if let Some(cs) = a.charset.as_deref() {
            spec["charset"] = serde_json::json!(captcha::resolve_charset(cs).map_err(anyhow::Error::msg)?);
        }
        captcha::parse_spec(&spec).map_err(anyhow::Error::msg)?;
        let envelope = c0mpute_core::jobs_gateway::submit_once(
            captcha::WORKLOAD,
            spec,
            a.max_price,
            std::time::Duration::from_secs(a.timeout_secs.clamp(5, 600)),
        )
        .await?;
        flatten_job_result(envelope)
    };
    if let Some(text) = result.get("text").and_then(|t| t.as_str()).map(str::to_string) {
        result["text"] = serde_json::json!(captcha::apply_case(&text, a.case.as_deref()));
    }
    if a.plain {
        match kind.as_str() {
            "slider" | "gap" => println!("{},{}", result["x"], result["y"]),
            "detect" => println!("{}", result["boxes"]),
            _ => println!("{}", result["text"].as_str().unwrap_or("")),
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

async fn run_jobs(cmd: JobsCmd) -> Result<()> {
    match cmd {
        JobsCmd::Serve {
            listen,
            max_price,
            bid_window_ms,
        } => {
            let token = std::env::var("C0MPUTE_JOBS_TOKEN")
                .context("set C0MPUTE_JOBS_TOKEN to a random secret (24 to 256 characters)")?;
            let allow_peers = c0mpute_core::jobs_gateway::parse_allow_peers(
                &std::env::var("C0MPUTE_JOBS_ALLOW_PEERS").unwrap_or_default(),
            );
            if !(200..=30_000).contains(&bid_window_ms) {
                anyhow::bail!("--bid-window-ms must be between 200 and 30000");
            }
            c0mpute_core::jobs_gateway::serve(
                listen,
                token,
                max_price,
                allow_peers,
                std::time::Duration::from_millis(bid_window_ms),
            )
            .await
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// secure-chat plugin (in-process)
// ────────────────────────────────────────────────────────────────────────

fn prompt_new_password() -> Result<String> {
    loop {
        let pw = rpassword::prompt_password("New password (min 10 chars): ")?;
        if pw.len() < 10 {
            eprintln!("password too short — must be at least 10 characters");
            continue;
        }
        let confirm = rpassword::prompt_password("Confirm password: ")?;
        if pw != confirm {
            eprintln!("passwords do not match, try again");
            continue;
        }
        return Ok(pw);
    }
}

// ────────────────────────────────────────────────────────────────────────
// plugin registry stub
// ────────────────────────────────────────────────────────────────────────

fn run_plugin(cmd: PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::List => {
            println!(
                "transcode  v{:<8}  in-process  built-in",
                env!("CARGO_PKG_VERSION")
            );
            println!(
                "tts        v{:<8}  in-process  CPU (Piper / Kokoro)",
                env!("CARGO_PKG_VERSION")
            );
            println!(
                "stt        v{:<8}  in-process  CPU (faster-whisper)",
                env!("CARGO_PKG_VERSION")
            );
            for bin in ["coinpay", "infernet"] {
                let v = peer_version(bin)
                    .map(|v| format!("v{v}"))
                    .unwrap_or_else(|| "-".into());
                println!("{bin:<9}  {v:<9}  subprocess  {}", peer_status_text(bin));
            }
            Ok(())
        }
        PluginCmd::Install { target } => install_plugin(&target),
        PluginCmd::Enable { id } => {
            println!("[stub] enable {id}");
            Ok(())
        }
        PluginCmd::Disable { id } => {
            println!("[stub] disable {id}");
            Ok(())
        }
        PluginCmd::Uninstall { id } => {
            println!("[stub] uninstall {id}");
            Ok(())
        }
    }
}

/// Phase-1 preset → TranscodeSpec. Three are wired so the round-trip
/// demo works — the full preset library lands when the marketplace
/// flow is mature enough to be worth fleshing out.
fn preset_to_spec(preset: &str) -> Result<c0mpute_proto::TranscodeSpec> {
    use c0mpute_proto::{Codec, TranscodeSpec};
    let s = match preset {
        "video-720p" => TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 2_500_000,
            width: 1280,
            height: 720,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        "video-1080p" => TranscodeSpec {
            codec: Codec::H264,
            bitrate_bps: 5_000_000,
            width: 1920,
            height: 1080,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        "video-4k" => TranscodeSpec {
            codec: Codec::Hevc,
            bitrate_bps: 15_000_000,
            width: 3840,
            height: 2160,
            keyframe_interval: 60,
            hardware_pref: None,
            extra_ffmpeg_args: vec![],
        },
        other => anyhow::bail!(
            "preset '{other}' isn't wired in Phase 1 (try video-720p, video-1080p, video-4k)"
        ),
    };
    Ok(s)
}

/// Resolve a `c0mpute plugin install <target>` argument and dispatch.
///
/// Accepts either:
///   - a plugin **id** registered in the c0mpute marketplace, in which
///     case we chain to `https://c0mpute.com/plugins/<id>/install.sh`
///   - or any **install.sh URL** (third-party plugins published at
///     their own URL — e.g. a GitHub raw URL)
///
/// Both flows pipe `curl -fsSL <url>` into `sh`. Future: minisign
/// signature verification before execution (per DIP-0006).
fn install_plugin(target: &str) -> Result<()> {
    let url = resolve_plugin_target(target);
    println!("→ installing plugin via {url}");
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {url} | sh"))
        .status()?;
    if !status.success() {
        anyhow::bail!("plugin install failed (exit {status})");
    }
    Ok(())
}

fn resolve_plugin_target(target: &str) -> String {
    let t = target.trim();
    if t.starts_with("http://") || t.starts_with("https://") {
        return t.to_string();
    }
    // Treat anything else as a plugin id and resolve through the
    // c0mpute marketplace. The route at c0mpute.com/plugins/<id>/install.sh
    // serves the manifest-checked-in install script for that plugin.
    format!("https://c0mpute.com/plugins/{t}/install.sh")
}

// ────────────────────────────────────────────────────────────────────────
// update / uninstall (alias: upgrade / remove)
// ────────────────────────────────────────────────────────────────────────

async fn run_update(check_only: bool, feed: Option<String>) -> Result<()> {
    use c0mpute_update::UpgradeOutcome;
    let feed = feed.unwrap_or_else(|| c0mpute_update::DEFAULT_RELEASE_FEED.to_string());
    let current = env!("CARGO_PKG_VERSION");

    if check_only {
        match c0mpute_update::try_upgrade(current, &feed).await? {
            UpgradeOutcome::AlreadyLatest { current } => {
                println!("c0mpute {current} — already latest")
            }
            UpgradeOutcome::Available { current, latest } => {
                println!("update available: {current} → {latest} (run `c0mpute update` to install)")
            }
            UpgradeOutcome::Upgraded { .. } => unreachable!("try_upgrade never swaps"),
        }
        return Ok(());
    }

    match c0mpute_update::upgrade_now(current, &feed).await {
        Ok(UpgradeOutcome::AlreadyLatest { current }) => {
            println!("c0mpute {current} — already latest");
        }
        Ok(UpgradeOutcome::Upgraded { from, to }) => {
            println!("upgraded {from} → {to}");
            println!(
                "restart any running worker to apply: c0mpute worker stop && c0mpute worker start -d"
            );
        }
        Ok(UpgradeOutcome::Available { .. }) => unreachable!("upgrade_now applies or errors"),
        Err(e) => {
            println!("update failed: {e:#}");
            println!(
                "reinstall manually: curl -fsSL https://c0mpute.com/install.sh | sh -s -- --force"
            );
        }
    }
    // `c0mpute update` always brings the plugins along too.
    upgrade_plugins_foreground();
    Ok(())
}

fn run_uninstall(all: bool, purge: bool, yes: bool) -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let bin_dir = std::path::PathBuf::from(&home).join(".c0mpute/bin");

    let mut targets: Vec<std::path::PathBuf> = vec![bin_dir.join("c0mpute")];
    if all {
        for peer in ["coinpay", "infernet", "c0mpute-tui"] {
            targets.push(bin_dir.join(peer));
        }
    }
    if purge {
        targets.push(std::path::PathBuf::from(&home).join(".config/c0mpute"));
    }

    println!("Will remove:");
    for t in &targets {
        println!("  {}", t.display());
    }
    if !yes {
        print!("Proceed? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let a = answer.trim();
        if a != "y" && a != "Y" {
            println!("aborted");
            return Ok(());
        }
    }

    for t in &targets {
        if t.is_file() || t.is_symlink() {
            std::fs::remove_file(t).ok();
        } else if t.is_dir() {
            std::fs::remove_dir_all(t).ok();
        }
    }
    println!("uninstalled. (PATH entries in shell rc files left in place)");
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// helpers
// ────────────────────────────────────────────────────────────────────────

/// The roles `worker start` runs with: `--roles` when given, otherwise the
/// config's roles plus the defaults; then transcode for `--gpu`, and storage
/// and hosting unless opted out. Shared with `c0mpute status --json`, so the
/// TUI reports the role set the worker actually has.
fn effective_worker_roles(cfg: &mut Config, roles: Option<&[String]>, gpu: bool, no_storage: bool, no_hosting: bool) {
    if let Some(rs) = roles {
        cfg.roles = rs.iter().filter_map(|s| parse_role(s)).collect();
    } else {
        for role in [
            Role::Storage,
            Role::Tts,
            Role::Stt,
            Role::WorkflowStep,
            Role::Diffusion,
            Role::Music,
            Role::Video,
            Role::Crawl,
            Role::Hosting,
            Role::Ocr,
            Role::Embed,
            Role::Captcha,
            Role::LiveStream,
            Role::Render,
            Role::Inference,
            Role::Mailbox,
        ] {
            if !cfg.roles.contains(&role) {
                cfg.roles.push(role);
            }
        }
        // Transcoding needs nothing but ffmpeg: a node that has it
        // serves it, CPU encoders included, without `--gpu`.
        if c0mpute_core::supervisor::which_ffmpeg().is_some() && !cfg.roles.contains(&Role::Transcode) {
            cfg.roles.push(Role::Transcode);
        }
    }
    if gpu && !cfg.roles.contains(&Role::Transcode) {
        cfg.roles.push(Role::Transcode);
    }
    // Every NVIDIA node serves the expressive GPU voices, even with an explicit
    // --roles list (Anthony 2026-11-11: "install on all gpus by default").
    // `worker start` then provisions Chatterbox; C0MPUTE_TTS_GPU=0 opts out.
    if c0mpute_core::speech::has_cuda_gpu() && !cfg.roles.contains(&Role::Tts) {
        cfg.roles.push(Role::Tts);
    }
    if no_storage {
        cfg.storage.enabled = false;
    }
    if no_hosting {
        cfg.hosting.enabled = false;
    }
    // Every node is a disk provider unless it opted out: storage and
    // hosting join even an explicit --roles list.
    cfg.apply_disk_provider_defaults();
}

fn parse_role(s: &str) -> Option<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "storage" => Some(Role::Storage),
        "transcode" => Some(Role::Transcode),
        "tts" => Some(Role::Tts),
        "stt" => Some(Role::Stt),
        "gateway" => Some(Role::Gateway),
        "verifier" => Some(Role::Verifier),
        "workflow-step" | "workflow_step" | "workflow" => Some(Role::WorkflowStep),
        "video" => Some(Role::Video),
        "diffusion" | "image" | "images" => Some(Role::Diffusion),
        "music" | "song" | "songs" => Some(Role::Music),
        "crawl" | "fetch" => Some(Role::Crawl),
        "hosting" | "host" | "sites" => Some(Role::Hosting),
        "ocr" => Some(Role::Ocr),
        "embed" | "embeddings" => Some(Role::Embed),
        "captcha" | "captchas" => Some(Role::Captcha),
        "gameserver" | "gameservers" | "games" => Some(Role::Gameserver),
        "apps" | "app" | "backends" | "backend" => Some(Role::Apps),
        "live-stream" | "live_stream" | "livestream" | "live" | "stream" => Some(Role::LiveStream),
        "render" | "blender" => Some(Role::Render),
        "inference" | "chat" | "llm" => Some(Role::Inference),
        "mailbox" | "dm" | "secure-chat" | "chat-mailbox" => Some(Role::Mailbox),
        "containers" | "container" | "build" | "builds" | "builder" => Some(Role::Containers),
        other => {
            eprintln!("warning: unknown role '{other}' ignored");
            None
        }
    }
}

/// Interactive command menu shown when `c0mpute` is run with no subcommand in a
/// TTY: browse the command tree, drill into subcommands, and run — spawning a
/// child `c0mpute` so the real handlers run unchanged.
fn run_menu() -> Result<()> {
    use clap::CommandFactory;
    use dialoguer::{Select, theme::ColorfulTheme};

    let root = Cli::command();
    let mut path: Vec<String> = Vec::new();

    loop {
        // Resolve the current node by descending `path` from the root each pass
        // (avoids holding a borrow of `root` across mutations of `path`).
        let mut node = &root;
        for p in &path {
            match node.find_subcommand(p) {
                Some(sc) => node = sc,
                None => break,
            }
        }

        let subs: Vec<(String, String)> = node
            .get_subcommands()
            .filter(|c| c.get_name() != "help" && !c.is_hide_set())
            .map(|c| {
                (
                    c.get_name().to_string(),
                    c.get_about().map(|s| s.to_string()).unwrap_or_default(),
                )
            })
            .collect();

        if subs.is_empty() {
            break; // leaf — run the accumulated path
        }

        let mut labels: Vec<String> = subs
            .iter()
            .map(|(n, a)| {
                if a.is_empty() {
                    n.clone()
                } else {
                    format!("{n:<12} {a}")
                }
            })
            .collect();
        let has_extra = !path.is_empty();
        if has_extra {
            labels.push(format!("▶ run: c0mpute {}", path.join(" ")));
            labels.push("↩ back".to_string());
        }

        let prompt = if path.is_empty() {
            "c0mpute — pick a command".to_string()
        } else {
            format!("c0mpute {}", path.join(" "))
        };

        let Some(idx) = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .items(&labels)
            .default(0)
            .interact_opt()?
        else {
            return Ok(()); // Esc / Ctrl-C
        };

        if has_extra && idx == subs.len() {
            break; // "▶ run" the current path
        }
        if has_extra && idx == subs.len() + 1 {
            path.pop(); // "↩ back"
            continue;
        }
        path.push(subs[idx].0.clone());
    }

    if path.is_empty() {
        return Ok(());
    }

    // Re-exec `c0mpute <path…>` so the real command handlers run.
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(exe).args(&path).status()?;
    std::process::exit(status.code().unwrap_or(0));
}

/// `c0mpute tui` — launch the terminal UI, installing it on demand the first
/// time. The TUI is a Bun app (apps/tui, on @profullstack/hqtui), not part of
/// the Rust binary, so `c0mpute tui` fetches the source, runs `bun install`,
/// and writes a launcher — the user never has to know about the underlying
/// `c0mpute-tui` binary. After an update it reinstalls the TUI from this
/// release's tag, so the CLI and the TUI move together.
fn run_tui(args: &[String]) -> Result<()> {
    let stale = installed_tui_version().is_some_and(|v| tui_is_stale(&v));
    if tui_launcher().is_none() {
        install_tui()?;
    } else if stale {
        if let Err(e) = install_tui() {
            eprintln!("c0mpute tui: could not update the terminal UI ({e:#}); starting the installed one");
        }
    }
    // The launcher lives in ~/.c0mpute/bin, which is not on PATH when the
    // CLI itself was installed elsewhere (e.g. /usr/local/bin).
    let launcher = tui_launcher().ok_or_else(|| anyhow::anyhow!("c0mpute-tui was not installed; run `c0mpute tui` again"))?;
    let status = Command::new(launcher).args(args).status()?;
    if !status.success() {
        anyhow::bail!("c0mpute-tui exited {status}");
    }
    Ok(())
}

/// `c0mpute-tui` on PATH, else the launcher `install_tui` writes.
fn tui_launcher() -> Option<PathBuf> {
    which_on_path("c0mpute-tui").or_else(|| {
        let p = PathBuf::from(std::env::var_os("HOME")?).join(".c0mpute/bin/c0mpute-tui");
        p.is_file().then_some(p)
    })
}

/// The version in ~/.c0mpute/tui/package.json, when the TUI was installed there.
fn installed_tui_version() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(PathBuf::from(home).join(".c0mpute/tui/package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("version")?.as_str().map(str::to_string)
}

/// Reinstall unless it matches this binary, or the operator pinned a ref
/// (`C0MPUTE_TUI_REF`) or opted out (`C0MPUTE_TUI_NO_UPDATE=1`).
fn tui_is_stale(installed: &str) -> bool {
    std::env::var_os("C0MPUTE_TUI_REF").is_none()
        && std::env::var_os("C0MPUTE_TUI_NO_UPDATE").is_none()
        && installed != env!("CARGO_PKG_VERSION")
}

/// Locate a usable `bun`, installing it via mise if needed. bun may be on PATH,
/// under ~/.bun, or managed by mise (which the c0mpute installer uses, sometimes
/// with a relocated data dir) — so a plain `which bun` isn't enough.
fn ensure_bun() -> Option<PathBuf> {
    if let Some(p) = which_on_path("bun") {
        return Some(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".bun/bin/bun");
        if p.exists() {
            return Some(p);
        }
    }
    let mise = which_on_path("mise")?;
    let mise_which = |m: &std::path::Path| -> Option<PathBuf> {
        let out = Command::new(m).args(["which", "bun"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        p.exists().then_some(p)
    };
    if let Some(p) = mise_which(&mise) {
        return Some(p);
    }
    // Not installed yet — install it via mise.
    println!("installing bun via mise…");
    let _ = Command::new(&mise)
        .args(["use", "--global", "bun@latest"])
        .status();
    mise_which(&mise).or_else(|| which_on_path("bun"))
}

fn install_tui() -> Result<()> {
    println!("c0mpute tui: installing the terminal UI for v{}…", env!("CARGO_PKG_VERSION"));
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let bun = ensure_bun().ok_or_else(|| {
        anyhow::anyhow!(
            "bun is required for the TUI and couldn't be installed via mise. \
             Install it, then rerun `c0mpute tui`:\n  curl -fsSL https://bun.sh/install | bash"
        )
    })?;

    let tui_dir = home.join(".c0mpute/tui");
    let wrapper = home.join(".c0mpute/bin/c0mpute-tui");
    let tmp = std::env::temp_dir().join("c0mpute-tui-install");
    // This release's tag, so the TUI matches the CLI; master if the tag is
    // not there (a dev build). C0MPUTE_TUI_REF picks a branch instead.
    let archive = match std::env::var("C0MPUTE_TUI_REF") {
        Ok(branch) if !branch.is_empty() => format!("https://github.com/profullstack/c0mpute/archive/refs/heads/{branch}.tar.gz"),
        _ => format!("https://github.com/profullstack/c0mpute/archive/refs/tags/v{}.tar.gz", env!("CARGO_PKG_VERSION")),
    };
    let fallback = "https://github.com/profullstack/c0mpute/archive/refs/heads/master.tar.gz";

    // Fetch the repo, copy apps/tui into ~/.c0mpute/tui, and materialise deps.
    let script = format!(
        "set -e\n\
         rm -rf '{tmp}' && mkdir -p '{tmp}'\n\
         curl -fsSL '{archive}' | tar -xz -C '{tmp}' || {{ rm -rf '{tmp}' && mkdir -p '{tmp}' && curl -fsSL '{fallback}' | tar -xz -C '{tmp}'; }}\n\
         src=$(find '{tmp}' -type d -path '*/apps/tui' | head -1)\n\
         [ -d \"$src/src\" ] || {{ echo 'TUI source not found' >&2; exit 1; }}\n\
         rm -rf '{tui}' && mkdir -p '{tui}'\n\
         cp -R \"$src/src\" \"$src/package.json\" \"$src/tsconfig.json\" '{tui}/'\n\
         cd '{tui}' && '{bun}' install --production --no-save >/dev/null 2>&1\n\
         rm -rf '{tmp}'\n",
        tmp = tmp.display(),
        archive = archive,
        fallback = fallback,
        tui = tui_dir.display(),
        bun = bun.display(),
    );
    let ok = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        anyhow::bail!("failed to fetch/build the TUI (need network + bun)");
    }

    // Launcher — runs the Bun source. Kept as source rather than a
    // `bun --compile` binary so `c0mpute tui` picks up the repo's copy on
    // reinstall without a release cycle.
    let body = format!(
        "#!/usr/bin/env sh\n\
         # c0mpute-tui launcher (installed on demand by `c0mpute tui`).\n\
         BUN=\"$(command -v bun 2>/dev/null || true)\"\n\
         [ -z \"$BUN\" ] && [ -x \"$HOME/.bun/bin/bun\" ] && BUN=\"$HOME/.bun/bin/bun\"\n\
         [ -z \"$BUN\" ] && command -v mise >/dev/null 2>&1 && BUN=\"$(mise which bun 2>/dev/null || true)\"\n\
         [ -n \"$BUN\" ] || {{ echo 'bun not found; run: mise use --global bun@latest' >&2; exit 1; }}\n\
         exec \"$BUN\" run \"{}/src/index.ts\" \"$@\"\n",
        tui_dir.display()
    );
    if let Some(parent) = wrapper.parent() {
        std::fs::create_dir_all(parent).context("create launcher directory")?;
    }
    std::fs::write(&wrapper, body).map_err(|e| anyhow::anyhow!("write launcher: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Infernet control-plane base URL from its config, or the default.
fn infernet_control_plane_url() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".config/infernet/config.json");
        if let Ok(text) = std::fs::read_to_string(p) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(url) = json
                    .get("controlPlane")
                    .and_then(|c| c.get("url"))
                    .and_then(|u| u.as_str())
                {
                    return url.trim_end_matches('/').to_string();
                }
            }
        }
    }
    "https://infernetprotocol.com".to_string()
}

/// `c0mpute pool [model]` — how many nodes are serving each model right now, per
/// the control plane's live peer list (specs.served_models). This is the number
/// that actually decides how a fits-one-node model distributes.
fn run_pool(model: Option<String>) -> Result<()> {
    let base = infernet_control_plane_url();
    // The control plane caps /api/peers at 100.
    let url = format!("{base}/api/peers?limit=100");
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "12", &url])
        .output()
        .map_err(|e| anyhow::anyhow!("run curl: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("failed to fetch {url}");
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("parse peers from {url}: {e}"))?;
    let peers: Vec<serde_json::Value> = json
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .or_else(|| json.as_array().cloned())
        .unwrap_or_default();

    println!(
        "infernet pool @ {base} — {} dialable nodes (public address, seen in the last 15 min)",
        peers.len()
    );
    // /api/peers only lists nodes another node can dial, so NAT'd nodes that
    // still take jobs are missing from it. The overview has the full count.
    if let Some((online, registered)) = fetch_online_nodes(&base) {
        println!("{online} online of {registered} registered (incl. nodes behind NAT)");
    }

    let served = |p: &serde_json::Value| -> Vec<String> {
        p.get("served_models")
            .and_then(|s| s.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    match model {
        Some(filter) => {
            let f = filter.to_lowercase();
            let matched: Vec<&serde_json::Value> = peers
                .iter()
                .filter(|p| served(p).iter().any(|m| m.to_lowercase().contains(&f)))
                .collect();
            println!(
                "nodes serving a model matching \"{filter}\": {}",
                matched.len()
            );
            for p in matched {
                let pk = p.get("pubkey").and_then(|v| v.as_str()).unwrap_or("?");
                let short = pk.get(..12).unwrap_or(pk);
                let gpu = p.get("gpu_model").and_then(|v| v.as_str()).unwrap_or("");
                let seen = p.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
                println!("  {short}…  {:<30.30}  last_seen {seen}", gpu);
            }
        }
        None => {
            let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
            for p in &peers {
                for m in served(p) {
                    *counts.entry(m).or_default() += 1;
                }
            }
            let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            println!("nodes per model:");
            for (m, n) in rows {
                println!("  {n:>3}  {m}");
            }
        }
    }
    println!(
        "one node's models: c0mpute pool --node <id>   reserve one: c0mpute infernet reservation pricing"
    );
    Ok(())
}

/// `c0mpute pool --node <id>` — one node's advertised models, from the control
/// plane's public GET /api/nodes/<id>/models (the same data as
/// `infernet model list --node`, without needing infernet installed).
fn run_pool_node(id: &str) -> Result<()> {
    let base = infernet_control_plane_url();
    let url = format!("{base}/api/nodes/{}/models", url_path_segment(id));
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "12", "-w", "\n%{http_code}", &url])
        .output()
        .map_err(|e| anyhow::anyhow!("run curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", text.trim()));
    match code {
        "200" => {}
        "404" => anyhow::bail!("node {id} not found (or not public) at {base}"),
        _ => anyhow::bail!("failed to fetch {url} (HTTP {code})"),
    }
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("parse node models from {url}: {e}"))?;
    let node = json.get("data").unwrap_or(&json);
    let field = |k: &str| node.get(k).and_then(|v| v.as_str()).unwrap_or("-");
    let models: Vec<&str> = node
        .get("served_models")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(|m| m.as_str()).collect())
        .unwrap_or_default();
    println!(
        "node {} ({})  status {}  last_seen {}",
        field("node_id"),
        field("name"),
        field("status"),
        field("last_seen")
    );
    println!("advertised models: {}", models.len());
    for m in models {
        println!("  {m}");
    }
    Ok(())
}

/// Percent-encode everything but unreserved characters, so an id can't add
/// path segments or a query string.
fn url_path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// (online, registered) from the control plane's /api/overview "Online nodes"
/// card, whose note reads "of N registered". Best-effort: None on any failure.
fn fetch_online_nodes(base: &str) -> Option<(u64, u64)> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "8", &format!("{base}/api/overview")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    parse_online_card(&json)
}

fn parse_online_card(json: &serde_json::Value) -> Option<(u64, u64)> {
    let card = json
        .get("cards")?
        .as_array()?
        .iter()
        .find(|c| c.get("label").and_then(|l| l.as_str()) == Some("Online nodes"))?;
    let online = card.get("value")?.as_u64()?;
    let registered = card
        .get("note")?
        .as_str()?
        .split_whitespace()
        .find_map(|w| w.parse::<u64>().ok())?;
    Some((online, registered))
}

fn delegate(bin: &str, args: &[String]) -> Result<()> {
    let path = which_on_path(bin).ok_or_else(|| {
        anyhow::anyhow!(
            "{bin} not found on PATH. Install with:\n  curl -fsSL https://c0mpute.com/install.sh | sh"
        )
    })?;
    let status = Command::new(path).args(args).status()?;
    if !status.success() {
        anyhow::bail!("{bin} exited {status}");
    }
    Ok(())
}

/// Ensure the operator has a payable CoinPay DID, as part of `worker register`.
///
/// Runs `coinpay reputation did setup`, which discovers an existing DID and
/// confirms its use, or offers to create one — interactively when a terminal
/// is attached, and non-interactively (sensible default) under `curl | sh`.
/// Best-effort: a worker runs without a DID, so a missing or logged-out
/// coinpay must never fail registration — we just point the operator at it.
fn ensure_coinpay_did() {
    let Some(coinpay) = which_on_path("coinpay") else {
        println!(
            "payable DID: `coinpay` not installed — run `c0mpute coinpay reputation did setup` later to set one up."
        );
        return;
    };
    println!("payable DID:");
    match Command::new(coinpay)
        .args(["reputation", "did", "setup"])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(_) => println!(
            "  (not set up — are you logged in? run `c0mpute coinpay login`, then `c0mpute coinpay reputation did setup`)"
        ),
        Err(e) => println!("  (couldn't run coinpay: {e})"),
    }
}

/// `c0mpute login` — sign in to every network this node participates in, one
/// at a time: coinpay (payments / payable DID) then infernet (ties the node to
/// your infernetprotocol.com account). Each is a browser device-code flow.
/// Best-effort per service — one failing/declined login doesn't abort the rest.
fn run_login() -> Result<()> {
    println!("Signing in to your c0mpute accounts (coinpay + infernet)...\n");
    login_one("coinpay", "payments + payable DID");
    // infernet login needs a configured control plane — init + register first,
    // otherwise `infernet login` errors with "no control plane configured".
    if which_on_path("infernet").is_some() && !infernet_initialized() {
        println!("── infernet ── initializing node (init + register)…");
        ensure_infernet_initialized();
    }
    login_one(
        "infernet",
        "ties this node to your infernetprotocol.com account",
    );
    // Bind this node to the account so it shows on /dashboard and can be
    // assigned models/jobs (best-effort; no-op if not installed or not authed).
    if which_on_path("infernet").is_some() {
        println!("── infernet ── linking node to your account…");
        let _ = infernet_cmd(&["pubkey", "link"]);
    }
    login_hf();
    println!("Done. Next: c0mpute worker register  →  c0mpute worker start");
    Ok(())
}

/// HuggingFace sign-in — for model downloads (only strictly required for gated
/// models; public ones need nothing). Runs like the other providers: prefer the
/// new `hf auth login`, else legacy `huggingface-cli login`. Best-effort — a
/// skip/Ctrl-C just moves on, and HF_TOKEN in the env counts as signed in.
fn login_hf() {
    println!("── huggingface ── (model downloads; token-based — no OAuth device flow)");
    if std::env::var_os("HF_TOKEN").is_some() {
        println!("  ✓ HF_TOKEN already set in the environment\n");
        return;
    }
    let (bin, args): (&str, &[&str]) = if which_on_path("hf").is_some() {
        ("hf", &["auth", "login"])
    } else if which_on_path("huggingface-cli").is_some() {
        ("huggingface-cli", &["login"])
    } else {
        println!(
            "  ! huggingface CLI not installed — skip (installed with vLLM on NVIDIA nodes)\n"
        );
        return;
    };
    let path = which_on_path(bin).unwrap();
    match Command::new(path).args(args).status() {
        Ok(s) if s.success() => println!("  ✓ huggingface signed in\n"),
        Ok(_) => {
            println!("  ! huggingface login skipped/failed (fine unless you use gated models)\n")
        }
        Err(e) => println!("  ! couldn't run {bin}: {e}\n"),
    }
}

/// Run `<bin> login`, inheriting stdio so its browser device-code flow works.
fn login_one(bin: &str, why: &str) {
    println!("── {bin} ── ({why})");
    match which_on_path(bin) {
        Some(path) => match Command::new(path).arg("login").status() {
            Ok(s) if s.success() => println!("  ✓ {bin} signed in\n"),
            Ok(_) => println!("  ! {bin} login did not complete — re-run `c0mpute login`\n"),
            Err(e) => println!("  ! couldn't run {bin}: {e}\n"),
        },
        None => println!("  ! {bin} not installed — skipping\n"),
    }
}

/// Path to infernet's config (`$XDG_CONFIG_HOME`/`~/.config` + infernet/config.json).
fn infernet_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("infernet").join("config.json"))
}

/// True once infernet has been initialized on this machine.
fn infernet_initialized() -> bool {
    infernet_config_path().map(|p| p.exists()).unwrap_or(false)
}

/// Run `infernet <args…>` inheriting stdio; returns whether it succeeded.
fn infernet_cmd(args: &[&str]) -> bool {
    match which_on_path("infernet") {
        Some(bin) => Command::new(bin)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        None => false,
    }
}

/// A human-readable node name for infernet — the machine hostname, falling
/// back to a constant.
fn infernet_node_name() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "c0mpute-node".to_string())
}

/// Idempotently configure infernet as a provider node: `init` (generates a
/// Nostr identity) + `register` (announces to the control plane). No-op if
/// infernet isn't installed or is already initialized. Login is intentionally
/// NOT run here — it's a device-code flow.
///
/// `init` prompts for URL / name / firewall unless every flag is supplied, so
/// we pass them all and close stdin — a stray prompt must never hang a headless
/// worker. Control-plane URL is overridable via `INFERNET_URL`.
fn ensure_infernet_initialized() -> bool {
    let Some(bin) = which_on_path("infernet") else {
        return false;
    };
    if infernet_initialized() {
        return false;
    }
    let url = std::env::var("INFERNET_URL")
        .unwrap_or_else(|_| "https://infernetprotocol.com".to_string());
    let name = infernet_node_name();
    // "Always accept": every prompt is supplied as a flag, and we pipe a stream
    // of "y" so any remaining prompt (e.g. the firewall rule, which opens the
    // P2P port) is auto-accepted — while never blocking a headless worker.
    if let Ok(mut child) = Command::new(&bin)
        .args(["init", "--url", &url, "--role", "provider", "--name", &name])
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all("y\n".repeat(20).as_bytes());
        }
        let _ = child.wait();
    }
    let _ = infernet_cmd(&["register"]);
    true
}

/// Best-effort version string for a peer CLI on PATH. Runs `<bin> --version`
/// with stdin closed (so tools that otherwise read a prompt can't hang) and
/// reads the first stdout line. Returns None if the binary isn't installed.
fn tool_version(bin: &str) -> Option<String> {
    let path = which_on_path(bin)?;
    let out = Command::new(&path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut line = raw.lines().next().unwrap_or("").trim();
    // Normalize e.g. "infernet v0.1.45" → "0.1.45": drop a redundant leading
    // binary name and a leading "v" before the number.
    if let Some(rest) = line.strip_prefix(bin) {
        line = rest.trim();
    }
    if let Some(rest) = line.strip_prefix('v') {
        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            line = rest;
        }
    }
    Some(if line.is_empty() {
        "installed".to_string()
    } else {
        line.to_string()
    })
}

/// Print c0mpute's version plus every installed plugin (transcode built-in;
/// coinpay + infernet peer CLIs). `c0mpute tui` is a subcommand, not a plugin,
/// so it isn't listed here.
fn print_all_versions() {
    println!("{:<10} {}", "c0mpute", env!("CARGO_PKG_VERSION"));
    println!("{:<10} built-in", "transcode");
    for bin in ["coinpay", "infernet"] {
        match tool_version(bin) {
            Some(v) => println!("{bin:<10} {v}"),
            None => println!("{bin:<10} not installed"),
        }
    }
}

/// First-run infernet bootstrap for `worker start`: init + register, then a
/// non-interactive token login when `INFERNET_TOKEN` is set (device-code login
/// can't run unattended, so it's skipped otherwise). Best-effort and idempotent
/// — a worker runs fine without infernet.
fn bootstrap_infernet_first_run() {
    if which_on_path("infernet").is_none() || infernet_initialized() {
        return;
    }
    tracing::info!("first run: bootstrapping infernet (init + register)");
    ensure_infernet_initialized();
    match std::env::var("INFERNET_TOKEN") {
        Ok(token) if !token.is_empty() => {
            tracing::info!("infernet: logging in with INFERNET_TOKEN + linking node to account");
            let _ = infernet_cmd(&["login", "--token", &token]);
            // Bind the node's pubkey to the account so /dashboard lists it and
            // can route models/jobs to it (registration alone only makes it
            // "available", not owned by the account).
            let _ = infernet_cmd(&["pubkey", "link"]);
        }
        _ => tracing::info!(
            "infernet: login + account-link skipped (set INFERNET_TOKEN, or run `c0mpute login`)"
        ),
    }
}

/// Best-effort: make sure the infernet node daemon is running so the node picks
/// up pending jobs (model pulls, inference). Runs on every `worker start` once
/// infernet is initialized; `infernet start` detaches and is a no-op if the
/// daemon is already up. A worker runs fine if this fails — the infernet daemon
/// is infernet's own process, with its own lifecycle and health.
fn ensure_infernet_daemon() {
    if which_on_path("infernet").is_none() || !infernet_initialized() {
        return;
    }
    if infernet_systemd_managed() {
        tracing::info!("infernet daemon is managed by infernet.service; not starting another");
        return;
    }
    tracing::info!("ensuring infernet node daemon is running");
    let _ = infernet_cmd(&["start"]);
}

// ── infernet federated RPC serving (IPIP-0033) — opt-in ─────────────────────
//
// A node only counts toward "Distribute across all nodes" for a model by
// SERVING it over llama.cpp RPC: ≥2 slices (`infernet inference serve --backend
// rpc`) + a primary holding the GGUF (`infernet inference primary`). infernet
// provisions neither the llama.cpp binaries nor the GGUF, so c0mpute builds the
// binaries and drives the serve — config-driven, since the model + GGUF are
// operator choices:
//
//   C0MPUTE_RPC_MODELS="qwen2.5:72b,llama3:70b"          # serve as RPC slice
//   C0MPUTE_RPC_PRIMARY="qwen2.5:72b=/abs/model.gguf"    # host as primary
//
// The infernet daemon heartbeat advertises specs.rpc / specs.rpc_primary once
// the serve processes are up.

/// Built llama.cpp binary dir under ~/.c0mpute.
fn llama_build_bin_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".c0mpute/llama.cpp/build/bin"))
}

/// Kick off a background llama.cpp (RPC-enabled) build; single-flight via a lock
/// marker. Symlinks rpc-server + llama-server into ~/.c0mpute/bin when done.
fn build_llama_rpc_background() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let lock = PathBuf::from(&home).join(".c0mpute/.llama-build.lock");
    if let Ok(Ok(e)) = std::fs::metadata(&lock)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed())
    {
        if e < std::time::Duration::from_secs(3600) {
            return; // a build was started recently
        }
    }
    if let Some(p) = lock.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(&lock, b"");
    tracing::info!(
        "building llama.cpp (rpc-server/llama-server) in background for infernet RPC serving"
    );
    let script = "set -e\n\
         # Ensure a build toolchain (cmake + C/C++ compiler + git) — not present\n\
         # on a fresh node by default.\n\
         if ! command -v cmake >/dev/null 2>&1; then\n\
           if command -v apt-get >/dev/null 2>&1; then sudo apt-get update -y && sudo apt-get install -y cmake build-essential git\n\
           elif command -v dnf >/dev/null 2>&1; then sudo dnf install -y cmake gcc-c++ make git\n\
           elif command -v yum >/dev/null 2>&1; then sudo yum install -y cmake gcc-c++ make git\n\
           elif command -v pacman >/dev/null 2>&1; then sudo pacman -Sy --noconfirm cmake base-devel git\n\
           elif command -v zypper >/dev/null 2>&1; then sudo zypper install -y cmake gcc-c++ make git\n\
           elif command -v apk >/dev/null 2>&1; then sudo apk add cmake build-base git\n\
           elif command -v brew >/dev/null 2>&1; then brew install cmake git\n\
           fi\n\
         fi\n\
         command -v cmake >/dev/null 2>&1 || { echo 'cmake unavailable and could not be installed automatically; install cmake + a C/C++ toolchain' >&2; exit 1; }\n\
         D=\"$HOME/.c0mpute/llama.cpp\"\n\
         [ -d \"$D/.git\" ] || git clone --depth 1 https://github.com/ggml-org/llama.cpp \"$D\"\n\
         cd \"$D\" && (git pull --ff-only 2>/dev/null || true)\n\
         cmake -B build -DGGML_RPC=ON -DCMAKE_BUILD_TYPE=Release >/dev/null\n\
         cmake --build build -j --target ggml-rpc-server llama-server\n\
         mkdir -p \"$HOME/.c0mpute/bin\"\n\
         # llama.cpp names the RPC server binary `ggml-rpc-server`; infernet\n\
         # looks for `rpc-server` on PATH, so link it under that name.\n\
         ln -sf \"$D/build/bin/ggml-rpc-server\" \"$HOME/.c0mpute/bin/rpc-server\"\n\
         ln -sf \"$D/build/bin/llama-server\" \"$HOME/.c0mpute/bin/llama-server\"\n";
    let logf = std::fs::File::create(PathBuf::from(&home).join(".c0mpute/llama-build.log")).ok();
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
    if let Some(f) = logf {
        if let Ok(err) = f.try_clone() {
            cmd.stdout(f).stderr(err);
        }
    }
    let _ = cmd.spawn();
}

/// True if llama.cpp RPC binaries are ready (symlinking a prior build onto PATH
/// if needed); otherwise starts a background build and returns false.
fn ensure_llama_rpc(want_primary: bool) -> bool {
    let ready = || {
        which_on_path("rpc-server").is_some()
            && (!want_primary || which_on_path("llama-server").is_some())
    };
    if ready() {
        return true;
    }
    // A previous build may be present but not symlinked onto PATH yet. The RPC
    // server binary is `ggml-rpc-server`; infernet expects `rpc-server`.
    if let (Some(build), Ok(home)) = (llama_build_bin_dir(), std::env::var("HOME")) {
        if build.join("ggml-rpc-server").exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let dst = PathBuf::from(&home).join(".c0mpute/bin");
                let _ = std::fs::create_dir_all(&dst);
                for (src, link) in [
                    ("ggml-rpc-server", "rpc-server"),
                    ("llama-server", "llama-server"),
                ] {
                    if build.join(src).exists() {
                        let _ = std::fs::remove_file(dst.join(link));
                        let _ = symlink(build.join(src), dst.join(link));
                    }
                }
            }
            if ready() {
                return true;
            }
        }
    }
    build_llama_rpc_background();
    false
}

/// Models pulled on this node, from `infernet model list` (the NAME column).
fn infernet_models() -> Vec<String> {
    let Some(bin) = which_on_path("infernet") else {
        return Vec::new();
    };
    let Ok(out) = Command::new(bin)
        .args(["model", "list"])
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|s| s.contains(':') && *s != "active:") // model:tag, not the footer
        .map(str::to_string)
        .collect()
}

/// Resolve an Ollama model id (`ns/name:tag` or `name:tag`) to its on-disk GGUF
/// blob so this node can act as an RPC primary. The blobs are world-readable, so
/// no copy/download is needed. None if not found/readable.
fn resolve_ollama_gguf(model: &str) -> Option<PathBuf> {
    let (repo, tag) = model.split_once(':').unwrap_or((model, "latest"));
    let (ns, name) = match repo.split_once('/') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => ("library".to_string(), repo.to_string()),
    };
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(h) = std::env::var("HOME") {
        bases.push(PathBuf::from(h).join(".ollama/models"));
    }
    if let Ok(m) = std::env::var("OLLAMA_MODELS") {
        bases.push(PathBuf::from(m));
    }
    bases.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    for base in bases {
        let manifest = base.join(format!("manifests/registry.ollama.ai/{ns}/{name}/{tag}"));
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        for layer in json
            .get("layers")
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
        {
            if layer
                .get("mediaType")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .contains("model")
            {
                if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
                    let blob = base.join("blobs").join(digest.replace(':', "-"));
                    if std::fs::File::open(&blob).is_ok() {
                        return Some(blob);
                    }
                }
            }
        }
    }
    None
}

/// Models already advertised as a live RPC slice (from the inference state), so
/// the periodic loop doesn't re-spawn rpc-servers it already started.
fn rpc_slice_active_models() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Ok(home) = std::env::var("HOME") else {
        return set;
    };
    let path = PathBuf::from(home).join(".infernet/inference/state.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return set;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return set;
    };
    if let Some(slice) = json.get("rpc_slice") {
        let alive = slice
            .get("pid")
            .and_then(|p| p.as_i64())
            .map(|pid| pid_alive(pid as i32))
            .unwrap_or(false);
        if alive {
            for m in slice
                .get("models")
                .and_then(|m| m.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(s) = m.as_str() {
                    set.insert(s.to_string());
                }
            }
        }
    }
    set
}

/// Fully automatic, no roles: every node serves each model it has over infernet
/// RPC — registers as a primary (the GGUF is a world-readable Ollama blob) AND
/// runs an rpc-server slice. The control plane picks a primary + slices per
/// request, so at any scale nobody assigns masters/slaves. Idempotent (skips
/// models already serving) so the 5-min loop is safe. `infernet inference serve`
/// stays foreground supervising rpc-server, so it's spawned detached.
fn bootstrap_infernet_rpc() {
    if which_on_path("infernet").is_none() || !infernet_initialized() {
        return;
    }
    let mut models = infernet_models();
    // Optional explicit additions (e.g. slice for a model this node hasn't pulled).
    for m in std::env::var("C0MPUTE_RPC_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !models.iter().any(|x| x == m) {
            models.push(m.to_string());
        }
    }
    if models.is_empty() {
        return;
    }

    let vram = gpu_vram_bytes();
    let ram = system_ram_bytes();
    // RPC sharding is ONLY for models too big to serve on a single node. Models
    // that fit (VRAM or RAM) are served single-node and distributed via
    // served_models load-balancing — so they need no llama.cpp build, no
    // rpc-servers, and don't clutter the RPC census.
    let oversized: Vec<(String, PathBuf)> = models
        .iter()
        .filter_map(|m| resolve_ollama_gguf(m).map(|g| (m.clone(), g)))
        .filter(|(_, gguf)| !node_can_run(gguf, vram, ram))
        .collect();
    if oversized.is_empty() {
        // Everything fits a single node → served_models handles distribution.
        // Clear any stale rpc_primary left by earlier versions that RPC-served
        // fitting models.
        prune_rpc_primaries(&[]);
        return;
    }

    // rpc-server is needed to shard now; llama-server (primary request
    // execution) builds in the background without blocking advertisement.
    if !ensure_llama_rpc(false) {
        tracing::info!(
            "infernet RPC sharding deferred until rpc-server finishes building (~/.c0mpute/llama-build.log)"
        );
        return;
    }
    if which_on_path("llama-server").is_none() {
        build_llama_rpc_background();
    }
    let Some(infernet) = which_on_path("infernet") else {
        return;
    };
    let active = rpc_slice_active_models();
    let mut port = 50052;
    let mut primaries: Vec<String> = Vec::new();
    for (model, gguf) in &oversized {
        // Too big for any single node: this node holds the GGUF and shards
        // layers across slices at request time (primary), and donates compute
        // (slice).
        tracing::info!(model, "infernet: RPC-sharding oversized model");
        let _ = Command::new(&infernet)
            .args([
                "inference",
                "primary",
                "--model",
                model,
                "--gguf",
                &gguf.to_string_lossy(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        primaries.push(model.clone());
        if !active.contains(model) {
            let _ = Command::new(&infernet)
                .args([
                    "inference",
                    "serve",
                    "--backend",
                    "rpc",
                    "--model",
                    model,
                    "--port",
                    &port.to_string(),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            port += 1;
        }
    }
    prune_rpc_primaries(&primaries);
}

/// Total system RAM in bytes (Linux /proc/meminfo).
fn system_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Show, per model on the node, whether it's eligible for the infernet
/// distribution pool (`served_models`) — i.e. whether it fits this node's GPU
/// VRAM (fast) or RAM (CPU). The control plane load-balances chat requests
/// across every node whose `served_models` includes the model, so this answers
/// "why is / isn't this model being distributed to me".
fn print_served_models_eligibility() {
    if which_on_path("infernet").is_none() {
        return;
    }
    let models = infernet_models();
    if models.is_empty() {
        return;
    }
    let vram = gpu_vram_bytes();
    let ram = system_ram_bytes();
    let gib = |b: u64| format!("{:.1} GB", b as f64 / 1e9);
    println!(
        "distribution pool (served_models) — GPU VRAM {}, RAM {}:",
        vram.map(gib).unwrap_or_else(|| "none".into()),
        ram.map(gib).unwrap_or_else(|| "?".into()),
    );
    for m in &models {
        let size = resolve_ollama_gguf(m)
            .and_then(|g| std::fs::metadata(g).ok())
            .map(|md| md.len());
        match size {
            Some(sz) => {
                let need = sz + sz / 5; // ~20% headroom
                let verdict = if vram.map(|v| v >= need).unwrap_or(false) {
                    "eligible (GPU)"
                } else if ram.map(|r| r >= need).unwrap_or(false) {
                    "eligible (CPU — slower)"
                } else {
                    "TOO BIG for this node — not in the pool"
                };
                println!("  {:<34} {:>8}  {}", m, gib(sz), verdict);
            }
            None => println!("  {:<34} {:>8}  (size unknown)", m, "?"),
        }
    }
}

/// Infernet house models (infernet >= 0.1.82): the models Infernet publishes
/// at https://infernetprotocol.com/house-models.json, which the infernet daemon
/// pulls and serves when they fit. Read from infernet's config (opt-outs) and
/// its state file, so this works without the daemon answering.
fn print_house_models() {
    let Some(cfg_path) = infernet_config_path() else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(&cfg_path) else {
        return;
    };
    let config: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let state: serde_json::Value = cfg_path
        .parent()
        .map(|d| d.join("house-models-state.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for line in house_models_lines(&config, &state, |k| std::env::var(k).ok()) {
        println!("{line}");
    }
}

/// The house-models lines for `worker status`. Env beats config, as in infernet.
fn house_models_lines(
    config: &serde_json::Value,
    state: &serde_json::Value,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let off = |v: Option<String>| {
        v.map(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(false)
    };
    let one = |v: Option<String>| v.map(|s| s.trim() == "1").unwrap_or(false);
    let hm = &config["houseModels"];
    let enabled = hm["enabled"].as_bool() != Some(false)
        && !off(env("INFERNET_HOUSE_MODELS"))
        && !one(env("INFERNET_NO_HOUSE_MODELS"));
    let uncensored = hm["uncensored"].as_bool() != Some(false)
        && !off(env("INFERNET_UNCENSORED_MODELS"))
        && !one(env("INFERNET_NO_UNCENSORED_MODELS"));
    let cap = env("INFERNET_HOUSE_MODELS_MAX_GB")
        .and_then(|v| v.parse::<f64>().ok())
        .or_else(|| hm["maxDiskGb"].as_f64());
    let gb = |b: f64| format!("{:.1} GB", b / 1024f64.powi(3));
    let mut lines = vec![format!(
        "house models: {}; uncensored: {}; disk cap: {}",
        if enabled { "on" } else { "off" },
        if uncensored { "on" } else { "off" },
        cap.map(|c| format!("{c} GB"))
            .unwrap_or_else(|| "none".into())
    )];
    let mut total = 0f64;
    if let Some(owned) = state["owned"].as_object() {
        for (name, o) in owned {
            let size = o["size_bytes"].as_f64().unwrap_or(0.0);
            total += size;
            let tag = if o["uncensored"].as_bool() == Some(true) {
                "  [uncensored]"
            } else {
                ""
            };
            lines.push(format!("  {:<52} {:>8}{}", name, gb(size), tag));
        }
    }
    if total == 0.0 {
        lines.push("  (none pulled yet)".into());
    }
    lines.push(format!("  disk used by house models: {}", gb(total)));
    if let Some(at) = state["last_sync"]["at"].as_str() {
        let errors = state["last_sync"]["errors"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let note = if errors > 0 {
            format!(" ({errors} error(s); see `infernet house-models`)")
        } else {
            String::new()
        };
        lines.push(format!("  last sync: {at}{note}"));
    }
    lines.push(
        "  opt out: infernet house-models off  (uncensored only: infernet house-models uncensored off)"
            .into(),
    );
    lines
}

/// Total GPU VRAM in bytes (NVIDIA via nvidia-smi), or None if there's no GPU.
fn gpu_vram_bytes() -> Option<u64> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mib: u64 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .sum();
    (mib > 0).then(|| mib * 1024 * 1024)
}

/// Whether the node can hold `gguf` as an RPC primary: it fits in GPU VRAM with
/// ~20% headroom for KV cache/context. No GPU → false (slice only).
fn node_can_run(gguf: &std::path::Path, vram: Option<u64>, ram: Option<u64>) -> bool {
    let Ok(size) = std::fs::metadata(gguf).map(|m| m.len()) else {
        return false;
    };
    let need = size + size / 5; // ~20% headroom
    // Fits in GPU VRAM (fast) OR in system RAM (CPU) — a CPU node can be a
    // primary for a model that fits its RAM, so CPU-only fleets can distribute.
    vram.map(|v| v >= need).unwrap_or(false) || ram.map(|r| r >= need).unwrap_or(false)
}

/// Prune `rpc_primary` in the inference state down to models this node can run,
/// so a node that registered for everything (pre-gate) stops advertising models
/// it can't serve. Best-effort, atomic write.
fn prune_rpc_primaries(keep: &[String]) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let path = PathBuf::from(&home).join(".infernet/inference/state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(primary) = json.get_mut("rpc_primary") else {
        return;
    };
    let keep_set: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
    let mut changed = false;
    if let Some(models) = primary.get_mut("models").and_then(|m| m.as_array_mut()) {
        let before = models.len();
        models.retain(|m| m.as_str().map(|s| keep_set.contains(s)).unwrap_or(false));
        changed |= models.len() != before;
    }
    if let Some(paths) = primary
        .get_mut("gguf_paths")
        .and_then(|p| p.as_object_mut())
    {
        let before = paths.len();
        paths.retain(|k, _| keep_set.contains(k.as_str()));
        changed |= paths.len() != before;
    }
    if changed {
        if let Ok(out) = serde_json::to_string_pretty(&json) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, out).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// (binary, self-update args) for the peer-CLI plugins c0mpute manages.
const PLUGIN_UPDATERS: &[(&str, &[&str])] = &[("coinpay", &["update"]), ("infernet", &["update"])];

/// Upgrade every installed plugin in the foreground, streaming their output.
/// Used by `c0mpute update` so a manual update always brings plugins along.
fn upgrade_plugins_foreground() {
    for (bin, args) in PLUGIN_UPDATERS {
        if let Some(path) = which_on_path(bin) {
            // `infernet update` restarts the daemon from this shell; under infernet.service that
            // starts a second daemon beside systemd's, and the daemon updates itself anyway.
            if *bin == "infernet" && infernet_systemd_managed() {
                println!("→ infernet is managed by infernet.service (it updates itself); skipping");
                continue;
            }
            println!("→ upgrading {bin}…");
            match Command::new(path).args(*args).status() {
                Ok(s) if s.success() => println!("  ✓ {bin} up to date"),
                Ok(_) => println!("  ! {bin} update reported a problem"),
                Err(e) => println!("  ! couldn't run {bin}: {e}"),
            }
        }
    }
}

/// Run each managed plugin's self-update once, waiting for each to finish so the
/// periodic loop never starts two overlapping (potentially heavy) updates.
/// Output is discarded; c0mpute drives the plugins so operators never update
/// nodes by hand. Also self-heals a stuck infernet whose old daemon can't
/// update itself. Runs on the background poll cadence.
fn refresh_plugins() {
    for (bin, args) in PLUGIN_UPDATERS {
        if let Some(path) = which_on_path(bin) {
            // `infernet update` has no version check: every call reinstalls and restarts the
            // daemon. Only run it when npm has something newer than what is installed.
            // A systemd-managed daemon updates itself; `infernet update` would restart it from here.
            if *bin == "infernet" && (infernet_systemd_managed() || !infernet_update_available()) {
                continue;
            }
            tracing::debug!(plugin = bin, "checking plugin for upgrades");
            let _ = Command::new(path)
                .args(*args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// True when a systemd `infernet.service` is running the node daemon, system-wide or as a user
/// unit (`infernet setup` installs the user unit since infernet v0.1.62). That unit owns the
/// daemon (and the daemon auto-updates itself), so the worker must not start, update or restart
/// another one: two daemons on one config dir fight over the socket, port and pidfile.
fn infernet_systemd_managed() -> bool {
    // Enabled, not only active: during the unit's own restart it is briefly "activating", and the
    // worker must not slip a second daemon in then.
    let scopes: [&[&str]; 2] = [&[], &["--user"]];
    scopes.iter().any(|scope| {
        ["is-enabled", "is-active"].iter().any(|verb| {
            Command::new("systemctl")
                .args(scope.iter())
                .args([*verb, "--quiet", "infernet.service"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
    })
}

/// npm package the infernet installer pulls (`npm install -g @infernetprotocol/cli`).
const INFERNET_NPM_LATEST: &str = "https://registry.npmjs.org/@infernetprotocol/cli/latest";

/// True only when npm's latest infernet differs from the installed one. Unknown on either side
/// (offline, registry down, unparseable `--version`) means no: a pointless reinstall restarts the
/// daemon, and an interrupted restart is what leaks old daemons.
fn infernet_update_available() -> bool {
    let Some(installed) = tool_version("infernet") else {
        return false;
    };
    let Some(latest) = npm_latest_version(INFERNET_NPM_LATEST) else {
        return false;
    };
    let newer = latest != installed;
    if newer {
        tracing::info!(
            installed,
            latest,
            "infernet: newer version on npm, updating"
        );
    } else {
        tracing::debug!(installed, "infernet: already latest");
    }
    newer
}

/// `version` from an npm registry `/latest` document. Called from blocking threads.
fn npm_latest_version(url: &str) -> Option<String> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    handle.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .ok()?;
        let doc: serde_json::Value = client
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        doc.get("version")?.as_str().map(str::to_string)
    })
}

/// Stop infernet node daemons that are not the one in `daemon.pid`. A daemon restart (`infernet
/// setup`, run by `infernet update`) asks the old daemon to shut down over IPC, and the old one can
/// release its socket and pidfile without exiting (its offline heartbeat has no timeout), so the
/// next start succeeds and the old process lives on. On dev2 that reached 326 daemons / 22 GB.
#[cfg(target_os = "linux")]
fn reap_stale_infernet_daemons() {
    let Some(pidfile) = infernet_config_path().map(|p| p.with_file_name("daemon.pid")) else {
        return;
    };
    reap_stale_infernet_daemons_from(&pidfile);
}

#[cfg(target_os = "linux")]
fn reap_stale_infernet_daemons_from(pidfile: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let Some(current) = std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return; // no daemon of record: never guess which one to keep
    };
    // SAFETY: kill(pid, 0) only checks that the pid exists.
    if unsafe { libc::kill(current, 0) } != 0 {
        return;
    }
    let me = unsafe { libc::getuid() };
    // Only daemons this worker started (they share its cgroup): a daemon run by infernet.service or
    // by hand is someone else's to manage.
    let Ok(own_cgroup) = std::fs::read_to_string("/proc/self/cgroup") else {
        return;
    };
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut stale = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == current {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if !is_infernet_daemon_cmdline(&cmdline)
            || entry.metadata().map(|m| m.uid()).ok() != Some(me)
            || std::fs::read_to_string(entry.path().join("cgroup"))
                .ok()
                .as_deref()
                != Some(own_cgroup.as_str())
        {
            continue;
        }
        stale.push(pid);
    }
    if stale.is_empty() {
        return;
    }
    tracing::warn!(
        count = stale.len(),
        keep = current,
        "infernet: stopping stale node daemons"
    );
    for &pid in &stale {
        // SAFETY: a plain signal to a process matched by cmdline and uid.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    std::thread::sleep(std::time::Duration::from_secs(5));
    for &pid in &stale {
        if unsafe { libc::kill(pid, 0) } == 0 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_stale_infernet_daemons() {}

/// `node …/@infernetprotocol/cli/index.js start --foreground` (the detached daemon), from a
/// NUL-separated /proc cmdline. The installer's git path runs the same daemon from
/// `~/.infernet/source/apps/cli/index.js` (under bun or node); one of those leaked on bbs for a
/// week because only the npm path matched.
fn is_infernet_daemon_cmdline(raw: &[u8]) -> bool {
    let args: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    let has = |needle: &[u8]| args.iter().any(|a| *a == needle);
    let infernet = args.iter().any(|a| {
        let a = String::from_utf8_lossy(a);
        a.contains("@infernetprotocol/cli")
            || a.ends_with("/infernet")
            || a.ends_with("/.infernet/source/apps/cli/index.js")
    });
    infernet && has(b"start") && has(b"--foreground")
}

pub(crate) fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        let candidate = entry.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod infernet_tests {
    use super::*;

    #[test]
    fn house_models_lines_show_models_and_opt_outs() {
        let state = serde_json::json!({
            "owned": {
                "huihui_ai/qwen3.5-abliterated:4b": { "size_bytes": 3315333475u64, "uncensored": true },
                "hf.co/profullstack/saas-business-strategist-agent-GGUF:latest": { "size_bytes": 531070268u64 }
            },
            "last_sync": { "at": "2026-11-02T12:00:00Z", "errors": [] }
        });
        let none = |_: &str| None;
        let lines = house_models_lines(&serde_json::json!({}), &state, none);
        assert_eq!(lines[0], "house models: on; uncensored: on; disk cap: none");
        assert!(lines.iter().any(|l| l.contains("qwen3.5-abliterated:4b")
            && l.contains("3.1 GB")
            && l.contains("[uncensored]")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("disk used by house models: 3.6 GB"))
        );

        let cfg = serde_json::json!({ "houseModels": { "uncensored": false, "maxDiskGb": 20 } });
        assert_eq!(
            house_models_lines(&cfg, &state, none)[0],
            "house models: on; uncensored: off; disk cap: 20 GB"
        );

        let env_off = |k: &str| (k == "INFERNET_HOUSE_MODELS").then(|| "0".to_string());
        let lines = house_models_lines(&serde_json::json!({}), &serde_json::json!({}), env_off);
        assert!(lines[0].starts_with("house models: off"));
        assert!(lines.iter().any(|l| l.contains("none pulled yet")));
    }

    #[test]
    fn infernet_daemon_cmdline_matching() {
        let daemon = b"/root/.local/share/mise/installs/node/22.23.3/bin/node\0/root/.local/share/mise/installs/node/22.23.3/lib/node_modules/@infernetprotocol/cli/index.js\0start\0--foreground\0";
        assert!(is_infernet_daemon_cmdline(daemon));
        assert!(is_infernet_daemon_cmdline(
            b"/root/.local/share/mise/installs/bun/1.4.2/bin/bun\0/root/.infernet/source/apps/cli/index.js\0start\0--foreground\0"
        ));
        assert!(!is_infernet_daemon_cmdline(
            b"/usr/bin/node\0/x/@infernetprotocol/cli/index.js\0start\0"
        ));
        assert!(!is_infernet_daemon_cmdline(
            b"/usr/bin/node\0/x/@infernetprotocol/cli/index.js\0inference\0serve\0--foreground\0"
        ));
        assert!(!is_infernet_daemon_cmdline(
            b"/usr/bin/node\0/srv/app/index.js\0start\0--foreground\0"
        ));
    }

    /// Real processes: two fake daemons, one recorded in daemon.pid. Only the other one dies.
    #[cfg(target_os = "linux")]
    #[test]
    fn reaper_stops_only_unrecorded_daemons() {
        use std::process::{Command, Stdio};
        let dir = std::env::temp_dir().join(format!("c0mpute-reap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("infernet")).unwrap();
        let fake = |tag: &str| {
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 600; :",
                    &format!("/x/{tag}/@infernetprotocol/cli/index.js"),
                    "start",
                    "--foreground",
                ])
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        };
        // `; :` keeps sh from exec-ing sleep, which would replace the cmdline being matched.
        let mut keep = fake("keep");
        let mut stale = fake("stale");
        for child in [&keep, &stale] {
            let path = format!("/proc/{}/cmdline", child.id());
            let ready = (0..100).any(|_| {
                let ok = std::fs::read(&path).is_ok_and(|c| is_infernet_daemon_cmdline(&c));
                if !ok {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                ok
            });
            assert!(ready, "fake daemon never showed the infernet cmdline");
            // Also wait for its `sleep` child to have exec'd. Between sh's fork and the
            // child's exec, the child carries the same infernet cmdline under its own pid, so a
            // reaper pass in that window stops keep's `sleep` and keep exits on its own: the
            // "recorded daemon was killed" flake on CI.
            let children = format!("/proc/{0}/task/{0}/children", child.id());
            let forked = (0..250).any(|_| {
                let ok = std::fs::read_to_string(&children).is_ok_and(|kids| {
                    kids.split_whitespace().any(|kid| {
                        std::fs::read(format!("/proc/{kid}/cmdline"))
                            .is_ok_and(|c| c.starts_with(b"sleep\0"))
                    })
                });
                if !ok {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                ok
            });
            assert!(forked, "fake daemon never started its sleep child");
        }
        let pidfile = dir.join("infernet/daemon.pid");
        std::fs::write(&pidfile, keep.id().to_string()).unwrap();
        reap_stale_infernet_daemons_from(&pidfile);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            stale.try_wait().unwrap().is_some(),
            "stale daemon still running"
        );
        assert!(
            keep.try_wait().unwrap().is_none(),
            "recorded daemon was killed"
        );
        let _ = keep.kill();
        let _ = keep.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_version_reads_infernet_and_coinpay_lines() {
        assert_eq!(
            parse_version("infernet v0.1.58\n").as_deref(),
            Some("0.1.58")
        );
        assert_eq!(parse_version("0.15.0").as_deref(), Some("0.15.0"));
        assert_eq!(parse_version("update check failed (offline?)"), None);
    }

    #[test]
    fn online_card_parses_value_and_registered() {
        let json = serde_json::json!({ "cards": [
            { "label": "Models served", "value": 3, "note": "a, b, c" },
            { "label": "Online nodes", "value": 2, "note": "of 41 registered" }
        ]});
        assert_eq!(parse_online_card(&json), Some((2, 41)));
        assert_eq!(parse_online_card(&serde_json::json!({})), None);
    }

    #[test]
    fn node_id_is_one_path_segment() {
        assert_eq!(url_path_segment("provider-0f44326c"), "provider-0f44326c");
        assert_eq!(url_path_segment("../x?y"), "..%2Fx%3Fy");
    }
}

#[cfg(test)]
mod json_flag_tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("c0mpute").chain(args.iter().copied())).expect("parses")
    }

    /// The desktop app (apps/desktop) shells out to these; they must keep parsing.
    #[test]
    fn desktop_app_command_lines_parse() {
        assert!(matches!(parse(&["status", "--json"]).command, Some(Cmd::Status { json: true })));
        assert!(matches!(
            parse(&["worker", "status", "--json"]).command,
            Some(Cmd::Worker { cmd: WorkerCmd::Status { json: true } })
        ));
        assert!(matches!(
            parse(&["hosting", "ls", "--mine", "--json", "--gateway", "http://127.0.0.1:7999"]).command,
            Some(Cmd::Hosting { cmd: HostingCmd::Ls { mine: true, json: true, .. } })
        ));
        assert!(matches!(parse(&["status"]).command, Some(Cmd::Status { json: false })));
    }

    #[test]
    fn status_json_reads_without_creating_anything() {
        let dir = std::env::temp_dir().join(format!("c0-status-json-{}", std::process::id()));
        let v = worker_status_json(&dir.join("config.toml")).expect("status json");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["worker"]["running"].is_boolean());
        assert!(v["payments"]["api_key_env"].is_string());
        assert!(!dir.exists(), "status --json must not write a config");
    }
}
