//! `c0mpute chat`: chat with models on the c0mpute network, either peer to
//! peer (`--p2p`, and the default when not signed in: an `infernet.inference`
//! auction over libp2p, answered by a worker's own Ollama/llama-server; see
//! `c0mpute_core::inference`) or through c0mpute.com with a signed-in key.
//!
//! ```text
//! c0mpute chat --p2p "explain LoRA"      # a worker answers over libp2p, no account
//! c0mpute chat login                     # device sign-in: approve on c0mpute.com with CoinPay (DID required)
//! c0mpute chat "explain LoRA"            # one answer, streamed
//! c0mpute chat                           # interactive session (TTY)
//! cat notes.md | c0mpute chat --system "summarize"
//! c0mpute chat models | whoami | logout
//! c0mpute chat mcp                       # stdio MCP bridge to https://c0mpute.com/api/mcp
//! c0mpute chat connect hosting --name n1 # publish what you /push hosting from c0mpute.com/chat
//! c0mpute chat push hosting ls           # your pushed sites (also nodes, status, rm, <name> <dir>)
//! c0mpute chat plugins add threatcrush   # chat plugins: remote MCP tools the model can use (ls, add, rm, on, off)
//! ```
//!
//! The key lives in `~/.c0mpute/chat.json` (0600). `C0MPUTE_API_KEY` and
//! `C0MPUTE_URL` override it, for CI and other machines.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_URL: &str = "https://c0mpute.com";

#[derive(Parser, Debug)]
#[command(
    name = "c0mpute chat",
    about = "Chat with open models on the c0mpute network",
    long_about = "Chat with open models on the c0mpute network (served by Infernet provider nodes).\n\n\
Sign in once with `c0mpute chat login` (CoinPay, a DID is required), then:\n\n  \
c0mpute chat \"what is a LoRA adapter?\"\n  \
c0mpute chat                      # interactive\n  \
cat file.md | c0mpute chat --system \"summarize this\"\n  \
c0mpute chat models | whoami | logout\n  \
c0mpute chat mcp                  # stdio MCP server for Claude Code, Cursor, ...\n  \
c0mpute chat connect hosting --name <node>  # publish what you /push hosting in the chat\n  \
c0mpute chat push hosting ls | nodes | status <name> -w | rm <name> | <name> <dir>\n  \
c0mpute chat plugins ls | add <url|name> | rm | on | off <name>  # tools from remote MCP servers\n\n\
Secure p2p messaging moved to `c0mpute dm`."
)]
struct Args {
    /// Model id (see `c0mpute chat models`); default: any live model.
    #[arg(long, short)]
    model: Option<String>,
    /// System prompt.
    #[arg(long, short)]
    system: Option<String>,
    /// Cap on generated tokens.
    #[arg(long)]
    max_tokens: Option<u32>,
    /// Sampling temperature (0-2).
    #[arg(long)]
    temperature: Option<f32>,
    /// Print the full JSON response instead of streaming text.
    #[arg(long)]
    json: bool,
    /// Ask a worker directly over libp2p (`infernet.inference`): no sign-in,
    /// no c0mpute.com. The default when this machine is not signed in.
    #[arg(long)]
    p2p: bool,
    /// With --p2p: the most you pay a worker for one answer, in USD.
    #[arg(long, default_value_t = 0.01)]
    max_price: f64,
    /// With --p2p: seconds to wait for an answer.
    #[arg(long, default_value_t = 300)]
    timeout_s: u64,
    /// c0mpute site URL (default https://c0mpute.com, or $C0MPUTE_URL).
    #[arg(long)]
    url: Option<String>,
    /// The prompt. Omit for an interactive session, or pipe it on stdin.
    prompt: Vec<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    url: Option<String>,
    key: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".c0mpute").join("chat.json"))
}

fn load() -> Saved {
    config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store(saved: &Saved) -> Result<PathBuf> {
    let path = config_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let body = serde_json::to_string_pretty(saved)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, body)?;
    Ok(path)
}

pub(crate) fn base_url(flag: Option<&str>) -> String {
    flag.map(str::to_string)
        .or_else(|| std::env::var("C0MPUTE_URL").ok().filter(|s| !s.is_empty()))
        .or_else(|| load().url)
        .unwrap_or_else(|| DEFAULT_URL.into())
        .trim_end_matches('/')
        .to_string()
}

fn api_key() -> Option<String> {
    std::env::var("C0MPUTE_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| load().key)
}

pub(crate) fn require_key() -> Result<String> {
    api_key().ok_or_else(|| anyhow!("not signed in: run `c0mpute chat login` (or set C0MPUTE_API_KEY)"))
}

pub(crate) fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("c0mpute-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

/// Pull `error.message` (or `error`) out of an API error body.
pub(crate) fn api_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    anyhow!("HTTP {}: {msg}", status.as_u16())
}

pub async fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("login") => return login(args.get(1..).unwrap_or_default()).await,
        Some("logout") => return logout(),
        Some("whoami") => return whoami(&base_url(None)).await,
        Some("models") => return models(&base_url(None)).await,
        Some("mcp") => return mcp_stdio(&base_url(None)).await,
        Some("connect") => return crate::chat_connect::run(args.get(1..).unwrap_or_default()).await,
        Some("push") => return crate::chat_push::run(args.get(1..).unwrap_or_default()).await,
        Some("plugins") | Some("plugin") => return crate::chat_plugins::run(args.get(1..).unwrap_or_default()).await,
        _ => {}
    }
    let a = Args::try_parse_from(std::iter::once("c0mpute chat".to_string()).chain(args.iter().cloned()))
        .unwrap_or_else(|e| e.exit());
    let url = base_url(a.url.as_deref());
    let p2p = a.p2p || api_key().is_none();
    if p2p && !a.p2p {
        eprintln!("(not signed in: asking a c0mpute worker peer to peer; `c0mpute chat login` uses c0mpute.com instead)");
    }

    let mut prompt = a.prompt.join(" ");
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut piped = String::new();
        stdin.lock().read_to_string(&mut piped)?;
        if !piped.trim().is_empty() {
            prompt = if prompt.is_empty() { piped } else { format!("{prompt}\n\n{piped}") };
        }
    }

    let mut messages = Vec::new();
    if let Some(s) = &a.system {
        messages.push(json!({"role": "system", "content": s}));
    }
    if prompt.trim().is_empty() {
        if !std::io::stdin().is_terminal() {
            bail!("no prompt: pass it as an argument or pipe it on stdin");
        }
        if p2p {
            return P2pChat::boot(&a).await?.repl(&a, messages).await;
        }
        return repl(&url, &require_key()?, &a, messages).await;
    }
    messages.push(json!({"role": "user", "content": prompt}));
    if p2p {
        let chat = P2pChat::boot(&a).await?;
        let (text, raw) = chat.ask(&a, a.model.clone(), &messages).await?;
        if a.json {
            println!("{}", serde_json::to_string_pretty(&raw)?);
        } else {
            println!("{text}");
        }
        return Ok(());
    }
    let key = require_key()?;
    let mut turn = complete(&url, &key, &a, &messages, a.json, &[]).await?;
    if a.json {
        println!("{}", turn.answer);
        return Ok(());
    }
    // A chat plugin wants to run a destructive tool: ask on the terminal.
    while !turn.approvals.is_empty() && std::io::stdin().is_terminal() {
        let ok = ask_approvals(&turn.approvals)?;
        if ok.is_empty() {
            break;
        }
        turn = complete(&url, &key, &a, &messages, false, &ok).await?;
    }
    Ok(())
}

/// One answer from c0mpute.com, plus any plugin calls waiting for approval.
pub(crate) struct Turn {
    answer: String,
    /// (what it wants to run, signed approval token)
    approvals: Vec<(String, String)>,
}

/// "plugin · tool {args}" for a plugin event.
fn describe_call(e: &Value) -> String {
    let args = e.get("arguments").map(|a| a.to_string()).unwrap_or_default();
    let args = if args.len() > 200 { format!("{}…", &args[..args.char_indices().nth(200).map(|(i, _)| i).unwrap_or(args.len())]) } else { args };
    format!("{} · {} {}", e["plugin"].as_str().unwrap_or("?"), e["tool"].as_str().unwrap_or("?"), if args == "{}" { String::new() } else { args })
}

/// Print a chat plugin step to stderr (the answer itself goes to stdout).
fn show_plugin_event(e: &Value, approvals: &mut Vec<(String, String)>) {
    match e["type"].as_str() {
        Some("tool_call") => eprintln!("\x1b[2m[plugin] {}\x1b[0m", describe_call(e)),
        Some("tool_result") => {
            let ok = e["ok"].as_bool().unwrap_or(false);
            let text: String = e["text"].as_str().unwrap_or("").chars().take(160).collect::<String>().replace('\n', " ");
            eprintln!("\x1b[2m[plugin] {} {} {}\x1b[0m", if ok { "ok" } else { "failed" }, e["tool"].as_str().unwrap_or(""), text);
        }
        Some("approval_needed") => {
            let what = describe_call(e);
            eprintln!("\x1b[33m[plugin] wants to run {what}\x1b[0m");
            if let Some(t) = e["approval"].as_str() {
                approvals.push((what, t.to_string()));
            }
        }
        Some("plugin_error") => eprintln!("\x1b[33m[plugin] {}\x1b[0m", e["message"].as_str().unwrap_or("")),
        _ => {}
    }
}

/// y/N on the terminal for each waiting call; returns the approved tokens.
fn ask_approvals(waiting: &[(String, String)]) -> Result<Vec<String>> {
    let mut ok = Vec::new();
    for (what, token) in waiting {
        eprint!("approve {what}? [y/N] ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if matches!(line.trim(), "y" | "Y" | "yes") {
            ok.push(token.clone());
        }
    }
    Ok(ok)
}

/// One completion. Streams text to stdout (returns it too), or returns raw JSON with `json`.
async fn complete(url: &str, key: &str, a: &Args, messages: &[Value], json_out: bool, approvals: &[String]) -> Result<Turn> {
    let mut body = json!({"messages": messages, "stream": !json_out, "plugin_events": true});
    if !approvals.is_empty() {
        body["approvals"] = json!(approvals);
    }
    if let Some(m) = &a.model {
        body["model"] = json!(m);
    }
    if let Some(t) = a.max_tokens {
        body["max_tokens"] = json!(t);
    }
    if let Some(t) = a.temperature {
        body["temperature"] = json!(t);
    }
    let mut res = client()?
        .post(format!("{url}/api/v1/chat/completions"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    let status = res.status();
    if !status.is_success() {
        let text = res.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            bail!("{}\nrun `c0mpute chat login`", api_error(status, &text));
        }
        return Err(api_error(status, &text));
    }
    if json_out {
        return Ok(Turn { answer: res.text().await?, approvals: Vec::new() });
    }

    let mut out = std::io::stdout();
    let mut answer = String::new();
    let mut waiting = Vec::new();
    let mut buf = String::new();
    'read: while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| anyhow!("the answer stream was cut off (the serving node or the network restarted); try again"))?
    {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim().to_string();
            buf.drain(..=nl);
            let Some(data) = line.strip_prefix("data:").map(str::trim) else { continue };
            if data == "[DONE]" {
                break 'read;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(msg) = v.pointer("/error/message").and_then(Value::as_str) {
                out.flush().ok();
                bail!("{msg}");
            }
            if v["object"].as_str() == Some("c0mpute.plugin_event") {
                show_plugin_event(&v["event"], &mut waiting);
                continue;
            }
            if let Some(t) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                answer.push_str(t);
                print!("{t}");
                out.flush().ok();
            }
        }
    }
    println!();
    Ok(Turn { answer, approvals: waiting })
}

/// What happened to a turn run through [`Input::during`].
enum TurnOutcome<T> {
    Done(T),
    /// Ctrl+C: the request was dropped (which closes it, so the server stops
    /// the model and any plugin tool loop).
    Interrupted,
}

/// The interactive session's keyboard: lines arrive from a reader thread, so
/// the person can keep typing while an answer streams. A line typed during a
/// turn is queued and sent when the turn ends; Ctrl+C interrupts the turn and
/// the next queued line goes at once. Ctrl+C at an empty prompt exits.
struct Input {
    rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    queue: std::collections::VecDeque<String>,
    closed: bool,
}

impl Input {
    fn spawn() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(line.trim_end_matches(['\r', '\n']).to_string()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self::from_receiver(rx)
    }

    fn from_receiver(rx: tokio::sync::mpsc::UnboundedReceiver<String>) -> Self {
        Self { rx, queue: Default::default(), closed: false }
    }

    /// The next line to act on: a queued one first, else wait for the
    /// keyboard. None on end of input or Ctrl+C.
    async fn next(&mut self) -> Option<String> {
        if let Some(q) = self.queue.pop_front() {
            eprintln!("\x1b[32m> \x1b[0m{q}  \x1b[2m(queued)\x1b[0m");
            return Some(q);
        }
        if self.closed {
            return None;
        }
        eprint!("\x1b[32m> \x1b[0m");
        std::io::stderr().flush().ok();
        tokio::select! {
            l = self.rx.recv() => {
                if l.is_none() {
                    self.closed = true;
                    eprintln!();
                }
                l
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!();
                None
            }
        }
    }

    /// One raw line (approval prompts), bypassing the queue.
    async fn line(&mut self) -> Option<String> {
        if self.closed {
            return None;
        }
        let l = self.rx.recv().await;
        if l.is_none() {
            self.closed = true;
        }
        l
    }

    /// Run a turn while still listening: typed lines queue, Ctrl+C interrupts.
    async fn during<T>(&mut self, fut: impl std::future::Future<Output = T>) -> TurnOutcome<T> {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => return TurnOutcome::Done(r),
                l = self.rx.recv(), if !self.closed => match l {
                    None => self.closed = true,
                    Some(l) if l.trim().is_empty() => {}
                    Some(l) => {
                        self.queue.push_back(l.trim().to_string());
                        eprintln!(
                            "\x1b[2m(queued {}: sends when this answer finishes · Ctrl+C stops it now)\x1b[0m",
                            self.queue.len()
                        );
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    println!();
                    eprintln!(
                        "\x1b[33m(stopped{})\x1b[0m",
                        if self.queue.is_empty() { String::new() } else { format!("; sending {} queued", self.queue.len()) }
                    );
                    return TurnOutcome::Interrupted;
                }
            }
        }
    }
}

/// y/N for each waiting call, read through the session's [`Input`].
async fn ask_approvals_in(input: &mut Input, waiting: &[(String, String)]) -> Vec<String> {
    let mut ok = Vec::new();
    for (what, token) in waiting {
        eprint!("approve {what}? [y/N] ");
        std::io::stderr().flush().ok();
        let line = input.line().await.unwrap_or_default();
        if matches!(line.trim(), "y" | "Y" | "yes") {
            ok.push(token.clone());
        }
    }
    ok
}

async fn repl(url: &str, key: &str, a: &Args, mut messages: Vec<Value>) -> Result<()> {
    let mut model = a.model.clone();
    eprintln!(
        "c0mpute chat · {} · /model <id>, /models, /reset, /exit · type while it answers to queue · Ctrl+C stops an answer",
        model.as_deref().unwrap_or("any live model")
    );
    let mut input = Input::spawn();
    let base = messages.clone();
    loop {
        let Some(line) = input.next().await else { return Ok(()) };
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            "/reset" => {
                messages = base.clone();
                eprintln!("(new conversation)");
                continue;
            }
            "/models" => {
                models(url).await?;
                continue;
            }
            _ if line.starts_with("/model") => {
                let m = line.trim_start_matches("/model").trim();
                model = (!m.is_empty() && m != "auto").then(|| m.to_string());
                eprintln!("(model: {})", model.as_deref().unwrap_or("any live model"));
                continue;
            }
            _ => {}
        }
        messages.push(json!({"role": "user", "content": line}));
        let args = Args { model: model.clone(), ..clone_args(a) };
        let mut result = input.during(complete(url, key, &args, &messages, false, &[])).await;
        while let TurnOutcome::Done(Ok(turn)) = &result {
            if turn.approvals.is_empty() {
                break;
            }
            let ok = ask_approvals_in(&mut input, &turn.approvals).await;
            if ok.is_empty() {
                break;
            }
            result = input.during(complete(url, key, &args, &messages, false, &ok)).await;
        }
        match result {
            TurnOutcome::Done(Ok(turn)) => messages.push(json!({"role": "assistant", "content": turn.answer})),
            TurnOutcome::Done(Err(e)) => {
                messages.pop();
                eprintln!("error: {e:#}");
            }
            // The cut-off answer is not kept, so the next message starts clean.
            TurnOutcome::Interrupted => {
                messages.pop();
            }
        }
    }
}

fn clone_args(a: &Args) -> Args {
    Args {
        model: a.model.clone(),
        system: a.system.clone(),
        max_tokens: a.max_tokens,
        temperature: a.temperature,
        json: false,
        p2p: a.p2p,
        max_price: a.max_price,
        timeout_s: a.timeout_s,
        url: a.url.clone(),
        prompt: vec![],
    }
}

/// The `infernet.inference` spec for one turn.
fn p2p_spec(a: &Args, model: Option<String>, messages: &[Value]) -> Result<Value> {
    let mut spec = json!({"v": 1, "messages": messages});
    if let Some(m) = model {
        spec["model"] = json!(m);
    }
    if let Some(t) = a.max_tokens {
        spec["max_tokens"] = json!(t);
    }
    if let Some(t) = a.temperature {
        spec["temperature"] = json!(t);
    }
    c0mpute_core::inference::parse_spec(&spec).map_err(|e| anyhow!(e))?;
    Ok(spec)
}

/// Chat with workers over libp2p: one buyer node for the whole session, one
/// `infernet.inference` auction per turn, answers fetched from the winner and
/// checked against its receipt.
struct P2pChat {
    bridge: c0mpute_core::jobs_gateway::JobsBridge,
}

impl P2pChat {
    async fn boot(a: &Args) -> Result<Self> {
        let sup = c0mpute_core::Supervisor::boot_buyer(c0mpute_core::Config::default()).await?;
        // Any token: the bridge's HTTP side is never mounted here.
        let token: String = std::iter::repeat_n('x', 32).collect();
        let bridge = c0mpute_core::jobs_gateway::JobsBridge::new(sup.libp2p, sup.store, token, a.max_price, Vec::new())?;
        // Let the swarm reach peers before the first auction.
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(Self { bridge })
    }

    async fn ask(&self, a: &Args, model: Option<String>, messages: &[Value]) -> Result<(String, Value)> {
        let spec = p2p_spec(a, model, messages)?;
        let r = self
            .bridge
            .submit_once(c0mpute_core::inference::WORKLOAD, spec, Duration::from_secs(a.timeout_s.clamp(10, 900)))
            .await
            .context("no worker answered (none serving infernet.inference for this model, or all over --max-price)")?;
        let v = r.get("result").cloned().unwrap_or(r.clone());
        if v["ok"] != json!(true) {
            bail!("the worker could not answer: {}", v["error"].as_str().unwrap_or("?"));
        }
        Ok((v["text"].as_str().unwrap_or_default().to_string(), r))
    }

    async fn repl(&self, a: &Args, mut messages: Vec<Value>) -> Result<()> {
        let mut model = a.model.clone();
        eprintln!(
            "c0mpute chat (peer to peer) · {} · /model <id>, /reset, /exit · type while it answers to queue · Ctrl+C stops an answer",
            model.as_deref().unwrap_or("any served model")
        );
        let mut input = Input::spawn();
        let base = messages.clone();
        loop {
            let Some(line) = input.next().await else { return Ok(()) };
            let line = line.trim();
            match line {
                "" => continue,
                "/exit" | "/quit" => return Ok(()),
                "/reset" => {
                    messages = base.clone();
                    eprintln!("(new conversation)");
                    continue;
                }
                _ if line.starts_with("/model") => {
                    let m = line.trim_start_matches("/model").trim();
                    model = (!m.is_empty() && m != "auto").then(|| m.to_string());
                    eprintln!("(model: {})", model.as_deref().unwrap_or("any served model"));
                    continue;
                }
                _ => {}
            }
            messages.push(json!({"role": "user", "content": line}));
            match input.during(self.ask(a, model.clone(), &messages)).await {
                TurnOutcome::Done(Ok((answer, _))) => {
                    println!("{answer}");
                    messages.push(json!({"role": "assistant", "content": answer}));
                }
                TurnOutcome::Done(Err(e)) => {
                    messages.pop();
                    eprintln!("error: {e:#}");
                }
                TurnOutcome::Interrupted => {
                    messages.pop();
                }
            }
        }
    }
}

async fn login(rest: &[String]) -> Result<()> {
    let mut url_flag = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => url_flag = it.next().cloned(),
            "-h" | "--help" => {
                println!("c0mpute chat login [--url https://c0mpute.com]\n\nSigns this machine in: approve the code on c0mpute.com with CoinPay (a DID is required).");
                return Ok(());
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let url = base_url(url_flag.as_deref());
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "this machine".into());
    let http = client()?;
    let res = http
        .post(format!("{url}/api/v1/cli/device"))
        .json(&json!({"label": format!("c0mpute cli on {host}")}))
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let d: Value = serde_json::from_str(&text)?;
    let device = d["device_code"].as_str().context("no device_code")?.to_string();
    let code = d["user_code"].as_str().unwrap_or_default();
    let link = d["verification_uri_complete"].as_str().unwrap_or_default();
    let interval = d["interval"].as_u64().unwrap_or(3).max(1);
    let expires = d["expires_in"].as_u64().unwrap_or(600);

    println!("Open this link and sign in with CoinPay (a DID is required):\n\n  {link}\n\nand check it shows the code  {code}\n");
    open_browser(link);
    eprint!("waiting for approval");
    let deadline = std::time::Instant::now() + Duration::from_secs(expires);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        eprint!(".");
        let res = match http
            .post(format!("{url}/api/v1/cli/token"))
            .json(&json!({"device_code": device}))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        match res.status().as_u16() {
            200 => {
                let v: Value = res.json().await?;
                let key = v["key"].as_str().context("no key in response")?.to_string();
                let saved = Saved { url: (url != DEFAULT_URL).then(|| url.clone()), key: Some(key) };
                let path = store(&saved)?;
                eprintln!();
                println!("signed in; key saved to {}", path.display());
                return whoami(&url).await;
            }
            428 | 429 => continue,
            410 => break,
            s => {
                let t = res.text().await.unwrap_or_default();
                eprintln!();
                return Err(api_error(reqwest::StatusCode::from_u16(s)?, &t));
            }
        }
    }
    eprintln!();
    bail!("the code expired before it was approved; run `c0mpute chat login` again")
}

pub(crate) fn open_browser(link: &str) {
    if link.is_empty() || std::env::var_os("SSH_CONNECTION").is_some() {
        return;
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(link)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn logout() -> Result<()> {
    let path = config_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("signed out (removed {}). Revoke the key itself at https://c0mpute.com/chat → CLI / API / MCP keys.", path.display());
    } else {
        println!("not signed in");
    }
    Ok(())
}

async fn whoami(url: &str) -> Result<()> {
    let key = require_key()?;
    let res = client()?.get(format!("{url}/api/v1/me")).bearer_auth(&key).send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    match v["name"].as_str() {
        Some(n) => println!("{n}  {}", v["did"].as_str().unwrap_or_default()),
        None => println!("{}", v["did"].as_str().unwrap_or_default()),
    }
    Ok(())
}

async fn models(url: &str) -> Result<()> {
    let res = client()?.get(format!("{url}/v1/models")).send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let list = v["data"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("(no models online right now)");
    }
    for m in list {
        println!("{}", m["id"].as_str().unwrap_or_default());
    }
    Ok(())
}

/// stdio MCP server: each JSON-RPC line on stdin is POSTed to /api/mcp and the
/// answer written back as one line. Logs go to stderr only.
async fn mcp_stdio(url: &str) -> Result<()> {
    let key = require_key()?;
    let http = client()?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
        let id_present = id.is_some();
        let res = http
            .post(format!("{url}/api/mcp"))
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .body(line)
            .send()
            .await;
        let reply = match res {
            Ok(r) if r.status().as_u16() == 202 => None,
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                if serde_json::from_str::<Value>(&text).is_ok() {
                    Some(text)
                } else {
                    Some(rpc_error(id, &format!("HTTP {}: {}", status.as_u16(), text.chars().take(200).collect::<String>())))
                }
            }
            Err(e) => Some(rpc_error(id, &format!("could not reach {url}: {e}"))),
        };
        // Notifications (no id) never get an answer, even an error.
        if let (Some(r), true) = (reply, id_present) {
            writeln!(out, "{}", r.replace('\n', " "))?;
            out.flush()?;
        }
    }
    Ok(())
}

fn rpc_error(id: Option<Value>, msg: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": {"code": -32000, "message": msg}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_typed_during_a_turn_queue_and_come_back_first_in_order() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut input = Input::from_receiver(rx);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<&str>();
        let typing = tx.clone();
        let turn = async move { done_rx.await.unwrap() };
        let feeder = tokio::spawn(async move {
            typing.send("second".into()).unwrap();
            typing.send("   ".into()).unwrap();
            typing.send("third".into()).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            done_tx.send("answer").unwrap();
        });
        match input.during(turn).await {
            TurnOutcome::Done(a) => assert_eq!(a, "answer"),
            TurnOutcome::Interrupted => panic!("not interrupted"),
        }
        feeder.await.unwrap();
        assert_eq!(input.queue, ["second", "third"]);
        tx.send("typed later".into()).unwrap();
        assert_eq!(input.next().await.as_deref(), Some("second"));
        assert_eq!(input.next().await.as_deref(), Some("third"));
        assert_eq!(input.next().await.as_deref(), Some("typed later"));
        drop(tx);
        assert_eq!(input.next().await, None);
        // End of input during a turn does not spin or end the turn early.
        let r = input.during(async { tokio::time::sleep(Duration::from_millis(5)).await; 7 }).await;
        assert!(matches!(r, TurnOutcome::Done(7)));
    }

    #[test]
    fn parses_prompt_and_flags() {
        let a = Args::try_parse_from(["c0mpute chat", "--model", "qwen2.5:7b", "-s", "terse", "what", "is", "2+2"]).unwrap();
        assert_eq!(a.model.as_deref(), Some("qwen2.5:7b"));
        assert_eq!(a.system.as_deref(), Some("terse"));
        assert_eq!(a.prompt.join(" "), "what is 2+2");
        assert!(!a.p2p);
    }

    #[test]
    fn p2p_turn_is_a_valid_inference_spec() {
        let a = Args::try_parse_from(["c0mpute chat", "--p2p", "--max-tokens", "64", "--temperature", "0.3", "hi"]).unwrap();
        assert!(a.p2p);
        let msgs = vec![json!({"role": "system", "content": "terse"}), json!({"role": "user", "content": "hi"})];
        let spec = p2p_spec(&a, Some("qwen2.5:0.5b".into()), &msgs).unwrap();
        assert_eq!(spec["v"], 1);
        assert_eq!(spec["model"], "qwen2.5:0.5b");
        assert_eq!(spec["max_tokens"], 64);
        assert_eq!(
            c0mpute_core::inference::required_capabilities(&spec),
            vec!["infernet:inference".to_string(), "infernet:inference:model:qwen2.5:0.5b".to_string()]
        );
        let any = p2p_spec(&a, None, &msgs).unwrap();
        assert!(any.get("model").is_none());
        // A turn the workers would refuse never leaves this machine.
        let bad = Args::try_parse_from(["c0mpute chat", "--p2p", "--temperature", "9", "hi"]).unwrap();
        assert!(p2p_spec(&bad, None, &msgs).is_err());
    }

    #[test]
    fn api_error_reads_openai_envelope() {
        let e = api_error(reqwest::StatusCode::UNAUTHORIZED, r#"{"error":{"message":"nope"}}"#);
        assert_eq!(e.to_string(), "HTTP 401: nope");
        let e = api_error(reqwest::StatusCode::GONE, r#"{"error":"expired_token"}"#);
        assert_eq!(e.to_string(), "HTTP 410: expired_token");
    }

    #[test]
    fn rpc_error_keeps_id() {
        let v: Value = serde_json::from_str(&rpc_error(Some(json!(7)), "x")).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["message"], "x");
    }
}
