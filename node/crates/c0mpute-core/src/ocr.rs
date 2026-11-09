//! `ocr.extract`: pull the text out of an image or a PDF on a worker's CPU.
//!
//! The engine is the operator's own Tesseract (`tesseract` on PATH, from the
//! OS package manager); PDFs are rasterized page by page with poppler's
//! `pdftoppm` when the box has it. Nothing is installed by the node itself, so
//! a node advertises `c0mpute:role:ocr` only when `tesseract` answers, and bids
//! on a PDF or a language only when it can actually serve it.
//!
//! The input travels by hash: the buyer stores it as content-addressed
//! [`crate::pieces::INPUT_PIECE_BYTES`] pieces and announces each, the spec
//! names the whole hash (plus `input_size` and `input_pieces` when there is
//! more than one piece) and the buyer's peer id, and the worker fetches and
//! verifies every piece and the whole. A spec with only `input_hash` (a
//! one-piece input, or an older buyer) is fetched as a single chunk.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use c0mpute_net::topics::{JobOffer, JobStatus};
use c0mpute_net::Libp2pNetwork;
use c0mpute_store::ChunkStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::pieces::InputRef;
use crate::runner::{AcceptedJob, WorkerLoad, cleanup_job_dir, publish_receipt, safe_job_dir};

pub const WORKLOAD: &str = "ocr.extract";
/// Capability tag: this node has a working `tesseract`.
pub const ROLE_TAG: &str = "c0mpute:role:ocr";
/// Largest image or PDF a buyer may send.
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
/// Most pages one job may OCR (PDF pages or multi-page TIFF frames).
pub const MAX_PAGES: usize = 50;
/// Cap on the extracted text (the result carries it twice: whole and per page).
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
/// Cap on the stored result blob.
pub const MAX_RESULT_BYTES: usize = 12 * 1024 * 1024;
pub const DEFAULT_LANG: &str = "eng";
/// Input formats a job may carry.
pub const FORMATS: &[&str] = &["png", "jpg", "tiff", "webp", "pdf"];
/// Rasterization resolution for PDF pages (Tesseract's sweet spot).
const PDF_DPI: &str = "300";
const PAGE_TIMEOUT: Duration = Duration::from_secs(180);
const RENDER_TIMEOUT: Duration = Duration::from_secs(300);

/// What a buyer asks for.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OcrSpec {
    pub v: u32,
    /// Content hash of the whole input, already announced by its holder
    /// (as one chunk, or as the pieces below).
    pub input_hash: String,
    /// Input size in bytes; present with `input_pieces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_size: Option<u64>,
    /// Hashes of the input's [`crate::pieces::INPUT_PIECE_BYTES`] pieces, in
    /// order. Absent: `input_hash` is a single chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_pieces: Option<Vec<String>>,
    /// Peer that holds the input (normally the buyer). Without it the worker
    /// looks the hash up on the DHT.
    #[serde(default)]
    pub input_provider_peer_id: Option<String>,
    /// One of [`FORMATS`]; the worker re-detects it from the bytes.
    pub format: String,
    /// Tesseract language(s), e.g. `eng` or `eng+deu`.
    #[serde(default = "default_lang")]
    pub lang: String,
}

fn default_lang() -> String {
    DEFAULT_LANG.to_string()
}

impl OcrSpec {
    /// The input as a checked [`InputRef`].
    pub fn input(&self) -> std::result::Result<InputRef, String> {
        InputRef::parse(
            "input",
            &self.input_hash,
            self.input_size,
            self.input_pieces.as_deref(),
            MAX_INPUT_BYTES,
        )
    }
}

/// `[a-z_+]{3,32}` with every `+`-separated part non-empty: Tesseract's
/// language codes (`eng`, `chi_sim`, `eng+deu`). Also keeps the value safe to
/// hand to a subprocess argument.
pub fn valid_lang(lang: &str) -> bool {
    (3..=32).contains(&lang.len())
        && lang
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '+')
        && lang.split('+').all(|part| !part.is_empty())
}

pub fn parse_spec(spec: &Value) -> std::result::Result<OcrSpec, String> {
    let s: OcrSpec = serde_json::from_value(spec.clone())
        .map_err(|e| format!("invalid ocr.extract spec: {e}"))?;
    if s.v != 1 {
        return Err(format!("unsupported ocr.extract spec version {}", s.v));
    }
    s.input()?;
    if let Some(peer) = s.input_provider_peer_id.as_deref() {
        peer.parse::<c0mpute_net::PeerId>()
            .map_err(|_| "input_provider_peer_id is not a peer id".to_string())?;
    }
    if !FORMATS.contains(&s.format.as_str()) {
        return Err(format!("format must be one of {}", FORMATS.join(", ")));
    }
    if !valid_lang(&s.lang) {
        return Err("lang must match [a-z_+]{3,32}, e.g. eng or eng+deu".into());
    }
    Ok(s)
}

/// Identify an input by its magic bytes. `None` means unsupported.
pub fn detect_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("pdf")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("tiff")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// A buyer's check on a worker's result: a JSON object with `v == 1` and a
/// string `text`.
pub fn valid_result(value: &Value) -> bool {
    value.is_object()
        && value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("text").is_some_and(Value::is_string)
}

// ─── local runtime probe ─────────────────────────────────────────────────

/// What this box's OCR tooling can do. Probed once per process: the role tag,
/// the job subscription and the bid filter all read the same answer, so they
/// never disagree. Installing Tesseract takes effect at the next start.
#[derive(Debug, Clone, Default)]
pub struct Runtime {
    pub tesseract: Option<String>,
    pub langs: Vec<String>,
    pub pdf: bool,
}

impl Runtime {
    pub fn ready(&self) -> bool {
        self.tesseract.is_some() && !self.langs.is_empty()
    }

    /// Every `+`-part of `lang` is installed.
    pub fn has_lang(&self, lang: &str) -> bool {
        lang.split('+')
            .all(|part| self.langs.iter().any(|l| l == part))
    }

    /// Whether this node should bid on a job with this spec.
    pub fn can_serve(&self, spec: &OcrSpec) -> bool {
        self.ready() && self.has_lang(&spec.lang) && (spec.format != "pdf" || self.pdf)
    }
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(probe)
}

fn probe() -> Runtime {
    let tesseract = std::process::Command::new("tesseract")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            // Older releases print the version on stderr.
            let text = if o.stdout.is_empty() {
                o.stderr
            } else {
                o.stdout
            };
            String::from_utf8_lossy(&text)
                .lines()
                .next()
                .unwrap_or("tesseract")
                .trim()
                .to_string()
        });
    let langs = if tesseract.is_some() {
        std::process::Command::new("tesseract")
            .arg("--list-langs")
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|o| {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.push('\n');
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                parse_lang_list(&text)
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // poppler's tools print their version and exit; we only need them to run.
    let runs = |bin: &str| {
        std::process::Command::new(bin)
            .arg("-v")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    let pdf = runs("pdftoppm") && runs("pdfinfo");
    let rt = Runtime {
        tesseract,
        langs,
        pdf,
    };
    if rt.ready() {
        info!(version = ?rt.tesseract, langs = rt.langs.len(), pdf = rt.pdf, "ocr: tesseract found");
    }
    rt
}

/// `tesseract --list-langs` output: a header line, then one code per line.
pub fn parse_lang_list(text: &str) -> Vec<String> {
    let mut langs: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.contains(':'))
        .filter(|l| {
            l.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        })
        .map(str::to_string)
        .collect();
    langs.sort();
    langs.dedup();
    langs
}

/// Bid filter: skip offers this box cannot complete (a language it lacks, a
/// PDF without poppler, or a spec it cannot read).
pub fn offer_filter() -> crate::dispatch::OfferFilter {
    Arc::new(|offer: &JobOffer| {
        offer
            .spec_inline
            .as_ref()
            .and_then(|spec| parse_spec(spec).ok())
            .is_some_and(|spec| runtime().can_serve(&spec))
    })
}

// ─── extraction ──────────────────────────────────────────────────────────

/// OCR `input` (already verified) and build the result blob.
pub async fn extract(input: &[u8], lang: &str, job_dir: &Path) -> Result<Value> {
    let rt = runtime();
    if !rt.ready() {
        bail!("tesseract is not installed on this node");
    }
    if !valid_lang(lang) || !rt.has_lang(lang) {
        bail!("tesseract language {lang:?} is not installed on this node");
    }
    let format = detect_format(input).context("input is not a PNG, JPEG, TIFF, WebP or PDF")?;
    tokio::fs::create_dir_all(job_dir).await?;
    let input_path = job_dir.join(format!("input.{format}"));
    tokio::fs::write(&input_path, input).await?;

    let pages: Vec<String> = if format == "pdf" {
        if !rt.pdf {
            bail!("this node has no pdftoppm/pdfinfo for PDF input");
        }
        let count = pdf_page_count(&input_path).await?;
        if count == 0 {
            bail!("the PDF has no pages");
        }
        if count > MAX_PAGES {
            bail!("the PDF has {count} pages; the limit is {MAX_PAGES}");
        }
        let images = render_pdf(&input_path, job_dir).await?;
        let mut out = Vec::with_capacity(images.len());
        for image in &images {
            out.push(
                tesseract(image, lang)
                    .await?
                    .trim_end_matches('\u{c}')
                    .to_string(),
            );
        }
        out
    } else {
        // A multi-page TIFF comes back as pages separated by form feeds.
        let text = tesseract(&input_path, lang).await?;
        split_pages(&text)
    };
    if pages.len() > MAX_PAGES {
        bail!(
            "the input has {} pages; the limit is {MAX_PAGES}",
            pages.len()
        );
    }
    let pages: Vec<String> = pages.into_iter().map(|p| p.trim().to_string()).collect();
    let text = pages.join("\n\n");
    if text.len() > MAX_TEXT_BYTES {
        bail!("extracted text is over the {MAX_TEXT_BYTES}-byte limit");
    }
    Ok(json!({
        "v": 1,
        "ok": true,
        "text": text,
        "pages": pages
            .iter()
            .enumerate()
            .map(|(i, t)| json!({ "page": i + 1, "text": t }))
            .collect::<Vec<_>>(),
        "engine": "tesseract",
        "engine_version": rt.tesseract,
        "lang": lang,
        "format": format,
    }))
}

/// Split Tesseract output on its page separator (form feed), dropping the
/// empty tail after the last page.
pub fn split_pages(text: &str) -> Vec<String> {
    let mut pages: Vec<String> = text.split('\u{c}').map(str::to_string).collect();
    if pages.len() > 1 && pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    pages
}

async fn run_tool(mut command: Command, timeout: Duration, what: &str) -> Result<Vec<u8>> {
    crate::resources::apply_job_limits(&mut command);
    command.env(
        "OMP_THREAD_LIMIT",
        crate::resources::cpu_threads_per_job().to_string(),
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().with_context(|| format!("start {what}"))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("{what} timed out after {}s", timeout.as_secs()))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{what} failed ({}): {}",
            output.status,
            stderr.lines().last().unwrap_or("no output")
        );
    }
    Ok(output.stdout)
}

async fn tesseract(image: &Path, lang: &str) -> Result<String> {
    let mut command = Command::new("tesseract");
    command.arg(image).arg("stdout").arg("-l").arg(lang);
    let out = run_tool(command, PAGE_TIMEOUT, "tesseract").await?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

async fn pdf_page_count(pdf: &Path) -> Result<usize> {
    let mut command = Command::new("pdfinfo");
    command.arg(pdf);
    let out = run_tool(command, Duration::from_secs(30), "pdfinfo").await?;
    parse_pdfinfo_pages(&String::from_utf8_lossy(&out)).context("pdfinfo reported no page count")
}

pub fn parse_pdfinfo_pages(text: &str) -> Option<usize> {
    text.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Rasterize every page to grayscale PNG; returns them in page order.
async fn render_pdf(pdf: &Path, job_dir: &Path) -> Result<Vec<PathBuf>> {
    let pages_dir = job_dir.join("pages");
    tokio::fs::create_dir_all(&pages_dir).await?;
    let mut command = Command::new("pdftoppm");
    command
        .args(["-r", PDF_DPI, "-gray", "-png", "-l"])
        .arg(MAX_PAGES.to_string())
        .arg(pdf)
        .arg(pages_dir.join("page"));
    run_tool(command, RENDER_TIMEOUT, "pdftoppm").await?;
    let mut images = Vec::new();
    let mut entries = tokio::fs::read_dir(&pages_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = page_number(&name) {
            images.push((n, entry.path()));
        }
    }
    images.sort();
    if images.is_empty() {
        bail!("pdftoppm rendered no pages");
    }
    Ok(images.into_iter().map(|(_, p)| p).collect())
}

/// `page-7.png` / `page-07.png` → 7.
fn page_number(name: &str) -> Option<usize> {
    name.strip_prefix("page-")?
        .strip_suffix(".png")?
        .parse()
        .ok()
}

// ─── worker runner ───────────────────────────────────────────────────────

pub fn spawn_ocr_runner(
    net: Arc<Libp2pNetwork>,
    store: ChunkStore,
    cache_root: PathBuf,
    load: WorkerLoad,
) -> mpsc::Sender<AcceptedJob> {
    let (tx, mut rx) = mpsc::channel::<AcceptedJob>(8);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let Ok(slot) = load.compute_slots().acquire_owned().await else {
                return;
            };
            let net = net.clone();
            let store = store.clone();
            let cache_root = cache_root.clone();
            let load = load.clone();
            tokio::spawn(async move {
                let _slot = slot;
                let started = Instant::now();
                let job_id = job.accept.job_id.clone();
                let result = run_ocr_job(&net, &store, &cache_root, &job).await;
                cleanup_job_dir(&cache_root, &job_id).await;
                match result {
                    Ok(hash) => {
                        load.record_job(started.elapsed());
                        info!(%job_id, %hash, "ocr runner: job complete");
                        publish_receipt(&net, &job.accept, Some(hash), JobStatus::Completed).await;
                    }
                    Err(error) => {
                        warn!(%job_id, err = %error, "ocr runner: job failed");
                        publish_receipt(&net, &job.accept, None, JobStatus::Failed).await;
                    }
                }
            });
        }
    });
    tx
}

/// Fetch and verify a spec's input from its holder (pieces or one chunk).
pub async fn fetch_input(net: &Arc<Libp2pNetwork>, spec: &OcrSpec) -> Result<Vec<u8>> {
    let input = spec.input().map_err(anyhow::Error::msg)?;
    let provider = spec
        .input_provider_peer_id
        .as_deref()
        .map(str::parse)
        .transpose()
        .context("invalid input provider peer id")?;
    input
        .fetch(net, provider, MAX_INPUT_BYTES)
        .await
        .context("fetch the OCR input (maximum 32 MiB)")
}

async fn run_ocr_job(
    net: &Arc<Libp2pNetwork>,
    store: &ChunkStore,
    cache_root: &Path,
    job: &AcceptedJob,
) -> Result<String> {
    let spec = crate::workflow::resolve_spec(net, job).await?;
    let spec = parse_spec(&spec).map_err(anyhow::Error::msg)?;
    let input = fetch_input(net, &spec).await?;
    if detect_format(&input) != Some(spec.format.as_str()) {
        bail!("OCR input is not the {} the spec declared", spec.format);
    }
    let job_dir = safe_job_dir(cache_root, &job.accept.job_id)?;
    let result = extract(&input, &spec.lang, &job_dir).await?;
    let bytes = serde_json::to_vec(&result)?;
    if bytes.len() > MAX_RESULT_BYTES {
        bail!("OCR result is over the {MAX_RESULT_BYTES}-byte limit");
    }
    crate::pieces::publish_result_bytes(net, store, &bytes)
        .await
        .context("publish OCR result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use c0mpute_proto::Hash;

    fn hash() -> String {
        Hash::of(b"x").to_hex()
    }

    #[test]
    fn langs() {
        assert!(valid_lang("eng"));
        assert!(valid_lang("chi_sim"));
        assert!(valid_lang("eng+deu"));
        assert!(!valid_lang("en"));
        assert!(!valid_lang("ENG"));
        assert!(!valid_lang("eng+"));
        assert!(!valid_lang("+eng"));
        assert!(!valid_lang("eng;rm"));
        assert!(!valid_lang("../eng"));
        assert!(!valid_lang(&"a".repeat(33)));
    }

    #[test]
    fn spec_validation() {
        let ok = json!({"v":1,"input_hash":hash(),"format":"png"});
        let s = parse_spec(&ok).unwrap();
        assert_eq!(s.lang, "eng");
        assert!(
            parse_spec(&json!({"v":2,"input_hash":hash(),"format":"png"}))
                .unwrap_err()
                .contains("version")
        );
        assert!(parse_spec(&json!({"v":1,"input_hash":"zz","format":"png"})).is_err());
        assert!(parse_spec(&json!({"v":1,"input_hash":hash(),"format":"gif"})).is_err());
        assert!(parse_spec(&json!({"v":1,"input_hash":hash(),"format":"pdf","lang":"x"})).is_err());
        assert!(
            parse_spec(
                &json!({"v":1,"input_hash":hash(),"format":"pdf","input_provider_peer_id":"nope"})
            )
            .is_err()
        );
        // Pieces: size and list together, the count matching the size.
        let two = json!([hash(), hash()]);
        let big = crate::pieces::INPUT_PIECE_BYTES + 1;
        assert!(
            parse_spec(&json!({"v":1,"input_hash":hash(),"format":"png","input_size":big,"input_pieces":two}))
                .is_ok()
        );
        assert!(
            parse_spec(&json!({"v":1,"input_hash":hash(),"format":"png","input_size":big,"input_pieces":[hash()]}))
                .unwrap_err()
                .contains("pieces")
        );
        assert!(
            parse_spec(&json!({"v":1,"input_hash":hash(),"format":"png","input_size":MAX_INPUT_BYTES + 1,"input_pieces":two}))
                .is_err()
        );
        assert!(parse_spec(&json!({"v":1,"input_hash":hash(),"format":"png","input_size":5})).is_err());
        let peer = c0mpute_net::PeerId::random().to_base58();
        assert!(
            parse_spec(&json!({"v":1,"input_hash":hash(),"format":"pdf","lang":"eng+deu","input_provider_peer_id":peer}))
                .is_ok()
        );
    }

    #[test]
    fn formats() {
        assert_eq!(detect_format(b"%PDF-1.7\n"), Some("pdf"));
        assert_eq!(detect_format(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(detect_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(detect_format(b"II*\0...."), Some("tiff"));
        assert_eq!(detect_format(b"MM\0*...."), Some("tiff"));
        assert_eq!(detect_format(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(detect_format(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(detect_format(b"GIF89a"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn result_validation() {
        assert!(valid_result(
            &json!({"v":1,"ok":true,"text":"hi","pages":[]})
        ));
        assert!(!valid_result(&json!({"v":2,"text":"hi"})));
        assert!(!valid_result(&json!({"v":1,"text":3})));
        assert!(!valid_result(&json!({"v":1})));
        assert!(!valid_result(&json!([1])));
    }

    #[test]
    fn tool_output_parsing() {
        let list = "List of available languages in \"/usr/share/tesseract-ocr/5/tessdata/\" (3):\neng\nosd\nchi_sim\n";
        assert_eq!(parse_lang_list(list), vec!["chi_sim", "eng", "osd"]);
        assert_eq!(
            parse_pdfinfo_pages("Title: x\nPages:          12\nEncrypted: no\n"),
            Some(12)
        );
        assert_eq!(parse_pdfinfo_pages("Title: x\n"), None);
        assert_eq!(split_pages("one\n\u{c}two\n\u{c}"), vec!["one\n", "two\n"]);
        assert_eq!(split_pages("only"), vec!["only"]);
        assert_eq!(page_number("page-07.png"), Some(7));
        assert_eq!(page_number("page-12.png"), Some(12));
        assert_eq!(page_number("input.pdf"), None);
    }

    #[test]
    fn serve_gate() {
        let rt = Runtime {
            tesseract: Some("tesseract 5.3.0".into()),
            langs: vec!["eng".into(), "deu".into()],
            pdf: false,
        };
        let spec = |format: &str, lang: &str| OcrSpec {
            v: 1,
            input_hash: hash(),
            input_size: None,
            input_pieces: None,
            input_provider_peer_id: None,
            format: format.into(),
            lang: lang.into(),
        };
        assert!(rt.can_serve(&spec("png", "eng")));
        assert!(rt.can_serve(&spec("png", "eng+deu")));
        assert!(!rt.can_serve(&spec("png", "fra")));
        assert!(!rt.can_serve(&spec("pdf", "eng")));
        assert!(!Runtime::default().can_serve(&spec("png", "eng")));
    }

    /// A one-page PDF with `line` set in 36pt Helvetica.
    fn text_pdf(line: &str) -> Vec<u8> {
        let content = format!("BT /F1 36 Tf 72 700 Td ({line}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    /// Runs only where `tesseract` (and, for the PDF half, poppler) is
    /// installed; elsewhere it says so and passes.
    #[tokio::test]
    async fn tesseract_reads_a_rendered_page() {
        let rt = runtime();
        if !rt.ready() || !rt.has_lang("eng") || !rt.pdf {
            eprintln!("skipping: tesseract with eng and poppler-utils are not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = text_pdf("HELLO C0MPUTE 42");
        let result = extract(&pdf, "eng", &dir.path().join("pdf")).await.unwrap();
        assert!(valid_result(&result), "{result}");
        assert_eq!(result["format"], "pdf");
        assert_eq!(result["pages"].as_array().unwrap().len(), 1);
        let text = result["text"].as_str().unwrap().to_uppercase();
        assert!(text.contains("HELLO") && text.contains("42"), "{text}");

        // Same page as a PNG, through the image path.
        let png = std::fs::read(
            &render_pdf(&dir.path().join("pdf/input.pdf"), dir.path())
                .await
                .unwrap()[0],
        )
        .unwrap();
        let result = extract(&png, "eng", &dir.path().join("png")).await.unwrap();
        assert_eq!(result["format"], "png");
        let text = result["text"].as_str().unwrap().to_uppercase();
        assert!(text.contains("HELLO") && text.contains("42"), "{text}");

        assert!(extract(&pdf, "zzz", &dir.path().join("x")).await.is_err());
        assert!(
            extract(b"GIF89a....", "eng", &dir.path().join("y"))
                .await
                .is_err()
        );
    }
}
