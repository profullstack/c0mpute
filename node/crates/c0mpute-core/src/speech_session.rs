//! Warm, pinned speech sessions: the difference between a voicemail and a
//! conversation.
//!
//! A one-shot `tts.synthesize` job pays for everything every time — a 4 second
//! bid window in the auction, a fresh Python, and a Kokoro model load — before
//! it says a word. Measured on 2026-11-05 through the speech gateway, a
//! four-word line cost 5.8 s wall; the model load alone is 4.6 s on a 16-core
//! box and 13.7 s on a 6-core one.
//!
//! A session pays those costs once. `open` starts a worker that holds Kokoro
//! in memory; every utterance after that is a line of JSON down a pipe to a
//! process that is already warm. Measured end to end through the gateway on
//! 2026-11-05:
//!
//! | node | open | "Yes." (0.9 s) | 1.9 s of audio | 4.0 s of audio |
//! |---|---|---|---|---|
//! | Ryzen 9 7950X3D, idle | 1.27 s | **433 ms** | 682 ms | 1,187 ms |
//! | Ryzen 7 5825U, load 9 | 4.7 s | 1.5 s | 2.2 s | 4.3 s |
//! | 6-core, load 3 | 13.7 s | 11.3 s | 14.2 s | 40.9 s |
//!
//! So on a capable node this is a conversation — about a third of realtime,
//! sub-second to the first word — and on a weak one it is not, which is why a
//! session belongs on a node that can hold it and why `say` reports its own
//! synthesis time. Hand it one sentence at a time: at 0.3x the caller can play
//! sentence one while two renders and stay ahead, and even at 1x it beats
//! waiting on a paragraph.
//!
//! Sessions are deliberately cheap to lose: anything idle past `IDLE_TTL` is
//! reaped, a worker that dies is simply gone and the next `say` reopens, and
//! `MAX_SESSIONS` caps how many warm models one node will hold. Nothing here
//! is durable — a session is a phone call's worth of state, not a record.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

/// Drop a session that has not spoken for this long. A phone call that ends
/// without telling us should not hold a model open.
pub const IDLE_TTL: Duration = Duration::from_secs(5 * 60);

/// How many warm models one node will hold at once. Each is a Python process
/// with an ONNX session in it, so this is a memory bound, not a CPU one.
pub const MAX_SESSIONS: usize = 16;

/// How long to wait for the worker to report its model loaded.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for one utterance before giving up on the worker.
const SAY_TIMEOUT: Duration = Duration::from_secs(120);

/// What the worker prints once its model is in memory.
#[derive(Debug, Deserialize)]
struct ReadyLine {
    #[serde(default)]
    ready: bool,
    #[serde(default)]
    load_ms: u64,
    #[serde(default)]
    error: Option<String>,
}

/// What the worker prints for each utterance.
#[derive(Debug, Deserialize)]
struct SaidLine {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    seconds: f32,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    error: Option<String>,
}

/// One utterance's result: where the audio is and what it cost.
#[derive(Debug, Clone, Serialize)]
pub struct Spoken {
    pub wav: PathBuf,
    /// Seconds of audio produced.
    pub seconds: f32,
    /// Milliseconds spent synthesizing, as the worker measured it.
    pub synth_ms: u64,
}

/// A Python worker holding a loaded Kokoro, fed one line of JSON per utterance.
struct WarmWorker {
    /// Owned so the process lives as long as the session and is killed with
    /// it (`kill_on_drop`); its pid is logged so an operator can find it.
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    /// How long the model took to load, kept for the session's own reporting.
    load_ms: u64,
    next_id: u64,
}

impl WarmWorker {
    /// Start a worker and wait for it to say its model is loaded.
    async fn start(
        python: &std::path::Path,
        root: &std::path::Path,
        engine: &str,
    ) -> Result<Self> {
        let mut command = Command::new(python);
        command
            .arg("-c")
            .arg(super::speech::WORKER_SOURCE)
            .arg("serve")
            .arg(root)
            .arg(engine)
            .env("HF_HOME", root.join("cache/huggingface"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // espeak-ng is extracted to a temp dir and dlopen'd, and Docker's
        // default /tmp is noexec, which fails at the first word rather than at
        // load. See `speech::speech_tmpdir`.
        if let Some(tmp) = crate::speech::speech_tmpdir().await {
            command.env("TMPDIR", tmp);
        }
        crate::resources::apply_job_limits(&mut command);
        let mut child = command.spawn().context(
            "start the warm speech worker; run `c0mpute tts setup --engine all` first",
        )?;
        let stdin = child.stdin.take().context("warm worker has no stdin")?;
        let stdout = child.stdout.take().context("warm worker has no stdout")?;
        let mut stdout = BufReader::new(stdout).lines();

        let first = tokio::time::timeout(READY_TIMEOUT, stdout.next_line())
            .await
            .context("the warm speech worker did not load its model in time")?
            .context("reading from the warm speech worker")?
            .context("the warm speech worker exited before loading its model")?;
        let ready: ReadyLine = serde_json::from_str(&first)
            .with_context(|| format!("warm worker said {first:.200} instead of ready"))?;
        if !ready.ready {
            bail!(
                "the warm speech worker could not load {engine}: {}",
                ready.error.unwrap_or_else(|| "no reason given".into())
            );
        }
        info!(
            load_ms = ready.load_ms,
            pid = child.id(),
            "warm speech worker ready"
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            load_ms: ready.load_ms,
            next_id: 0,
        })
    }

    /// Speak one line. The worker writes the WAV itself and tells us how long
    /// it took, so a caller can see the realtime factor it is getting.
    async fn say(
        &mut self,
        text: &str,
        voice: &str,
        locale: &str,
        speed: f32,
        out: &std::path::Path,
    ) -> Result<Spoken> {
        self.next_id += 1;
        let request = serde_json::json!({
            "id": self.next_id.to_string(),
            "text": text,
            "voice": voice,
            "locale": locale,
            "speed": speed,
            "out": out,
        });
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await.context(
            "the warm speech worker closed its input; the session is gone and must be reopened",
        )?;
        self.stdin.flush().await?;

        let answer = tokio::time::timeout(SAY_TIMEOUT, self.stdout.next_line())
            .await
            .context("the warm speech worker did not answer in time")?
            .context("reading from the warm speech worker")?
            .context("the warm speech worker exited mid-utterance")?;
        let said: SaidLine = serde_json::from_str(&answer)
            .with_context(|| format!("warm worker said {answer:.200}"))?;
        if !said.ok {
            bail!(
                "the warm speech worker could not say it: {}",
                said.error.unwrap_or_else(|| "no reason given".into())
            );
        }
        Ok(Spoken {
            wav: out.to_path_buf(),
            seconds: said.seconds,
            synth_ms: said.ms,
        })
    }
}

impl Drop for WarmWorker {
    fn drop(&mut self) {
        // kill_on_drop reaps the process; this only says which one went.
        debug!(pid = self.child.id(), "dropping a warm speech worker");
    }
}

/// One pinned session: a warm worker plus when it last spoke.
struct Session {
    engine: &'static str,
    voice: String,
    /// Shared by every session on the same engine: one loaded model serves
    /// all voices (Pocket caches each voice's state), so a five-voice show
    /// costs one model in RAM, not five.
    worker: Arc<Mutex<WarmWorker>>,
    last_used: Mutex<Instant>,
    opened_at: Instant,
    load_ms: u64,
    dir: PathBuf,
    utterances: Mutex<u64>,
}

/// What `open` tells the caller.
#[derive(Debug, Clone, Serialize)]
pub struct OpenedSession {
    pub session: String,
    pub engine: &'static str,
    pub voice: String,
    /// What the one-time model load cost, so the caller can see what it saved.
    pub load_ms: u64,
}

/// The warm sessions this node is holding.
#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Arc<Session>>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a pinned session: start a warm Kokoro and keep it.
    ///
    /// The voice must be one this node actually serves, because the whole
    /// point is that the model is already loaded when the first word is
    /// wanted — discovering a missing voice later would defeat it.
    pub async fn open(&self, voice: &str) -> Result<OpenedSession> {
        // Voice names are unique across engines, so the voice picks the
        // engine: a Pocket voice (the agents' default) gets a warm Pocket.
        let engine = crate::speech::engine_for_voice(voice);
        if engine == "piper" {
            bail!("warm sessions serve Pocket and Kokoro voices, not Piper ({voice})");
        }
        if !crate::speech::tts_voice_available(engine, voice) {
            bail!(
                "this node does not serve the {engine} voice {voice}; \
                 run `c0mpute tts setup --engine {engine}`"
            );
        }
        self.reap().await;
        {
            let sessions = self.inner.read().await;
            if sessions.len() >= MAX_SESSIONS {
                bail!(
                    "this node is already holding {MAX_SESSIONS} warm speech sessions; \
                     close one or wait for an idle one to be reaped"
                );
            }
        }

        let python = if engine == "pocket" {
            crate::speech::python_path()
        } else {
            crate::speech::kokoro_python_installed().unwrap_or_else(crate::speech::kokoro_python)
        };
        let root = crate::speech::speech_root();
        let shared = {
            let sessions = self.inner.read().await;
            sessions
                .values()
                .find(|s| s.engine == engine)
                .map(|s| (s.worker.clone(), s.load_ms))
        };
        let (worker, load_ms) = match shared {
            Some(found) => found,
            None => {
                let worker = WarmWorker::start(&python, &root, engine).await?;
                let load_ms = worker.load_ms;
                (Arc::new(Mutex::new(worker)), load_ms)
            }
        };

        let session = uuid::Uuid::new_v4().to_string();
        let dir = root.join("sessions").join(&session);
        tokio::fs::create_dir_all(&dir).await?;
        let now = Instant::now();
        let entry = Arc::new(Session {
            engine,
            voice: voice.to_string(),
            worker,
            last_used: Mutex::new(now),
            opened_at: now,
            load_ms,
            dir,
            utterances: Mutex::new(0),
        });
        self.inner.write().await.insert(session.clone(), entry);
        info!(%session, voice, load_ms, "opened a warm speech session");
        Ok(OpenedSession {
            session,
            engine,
            voice: voice.to_string(),
            load_ms,
        })
    }

    /// Speak one line on an open session.
    ///
    /// Hand this one sentence at a time: synthesis runs at roughly one times
    /// realtime on a CPU node, so a caller that plays the first sentence while
    /// the second renders stays ahead, and a caller that waits for a paragraph
    /// does not.
    pub async fn say(
        &self,
        session_id: &str,
        text: &str,
        locale: &str,
        speed: f32,
    ) -> Result<Spoken> {
        let session = {
            let sessions = self.inner.read().await;
            sessions.get(session_id).cloned()
        };
        let Some(session) = session else {
            bail!("no warm speech session {session_id}: it was closed, reaped, or never opened");
        };

        let index = {
            let mut utterances = session.utterances.lock().await;
            *utterances += 1;
            *utterances
        };
        let out = session.dir.join(format!("{index:06}.wav"));

        // One utterance at a time per session: the worker is a single pipe,
        // and a phone call speaks in sequence anyway.
        let spoken = {
            let mut worker = session.worker.lock().await;
            worker.say(text, &session.voice, locale, speed, &out).await
        };
        match spoken {
            Ok(spoken) => {
                *session.last_used.lock().await = Instant::now();
                debug!(
                    session = %session_id,
                    synth_ms = spoken.synth_ms,
                    audio_s = spoken.seconds,
                    "warm session spoke"
                );
                Ok(spoken)
            }
            Err(error) => {
                // A broken worker is not worth keeping: drop the session so
                // the next call opens a fresh one instead of failing forever.
                warn!(session = %session_id, err = %error, "dropping a broken warm speech session");
                self.close(session_id).await;
                Err(error)
            }
        }
    }

    /// Close a session and let its model go.
    pub async fn close(&self, session_id: &str) -> bool {
        let removed = self.inner.write().await.remove(session_id);
        if let Some(session) = removed {
            let dir = session.dir.clone();
            drop(session);
            let _ = tokio::fs::remove_dir_all(dir).await;
            info!(session = %session_id, "closed a warm speech session");
            return true;
        }
        false
    }

    /// Drop every session idle past the TTL. Called on open, and worth calling
    /// from a timer if a node holds sessions for long stretches.
    pub async fn reap(&self) {
        let now = Instant::now();
        let mut stale = Vec::new();
        {
            let sessions = self.inner.read().await;
            for (id, session) in sessions.iter() {
                if now.duration_since(*session.last_used.lock().await) > IDLE_TTL {
                    stale.push(id.clone());
                }
            }
        }
        for id in stale {
            warn!(session = %id, "reaping an idle warm speech session");
            self.close(&id).await;
        }
    }

    /// What this node is holding, for the gateway's status and for operators.
    pub async fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.inner.read().await;
        let mut rows = Vec::with_capacity(sessions.len());
        for (id, session) in sessions.iter() {
            rows.push(SessionSummary {
                session: id.clone(),
                engine: session.engine,
                voice: session.voice.clone(),
                load_ms: session.load_ms,
                age_s: session.opened_at.elapsed().as_secs(),
                idle_s: session.last_used.lock().await.elapsed().as_secs(),
                utterances: *session.utterances.lock().await,
            });
        }
        rows.sort_by(|a, b| a.session.cmp(&b.session));
        rows
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session: String,
    pub engine: &'static str,
    pub voice: String,
    pub load_ms: u64,
    pub age_s: u64,
    pub idle_s: u64,
    pub utterances: u64,
}

/// Split a line into the chunks a caller should pipeline.
///
/// Synthesis runs near one times realtime, so the way to a short
/// time-to-first-word is to render a sentence, start playing it, and render
/// the next while it plays. Sentences are kept whole because Kokoro's prosody
/// depends on them, and a very long one is split on commas rather than left to
/// block the pipeline.
pub fn chunk_for_streaming(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(40);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for sentence in split_sentences(text) {
        if sentence.is_empty() {
            continue;
        }
        if current.is_empty() && sentence.len() <= max_chars {
            current = sentence;
            continue;
        }
        if current.len() + 1 + sentence.len() <= max_chars {
            current.push(' ');
            current.push_str(&sentence);
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if sentence.len() <= max_chars {
            current = sentence;
        } else {
            // One sentence longer than a chunk: break it on commas, then on
            // whitespace, so nothing blocks the pipeline for seconds.
            for piece in split_long(&sentence, max_chars) {
                chunks.push(piece);
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        if matches!(c, '.' | '!' | '?')
            && chars
                .peek()
                .map(|next| next.is_whitespace())
                .unwrap_or(true)
        {
            out.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn split_long(sentence: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for piece in sentence.split_inclusive(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        if current.is_empty() {
            current = piece.to_string();
        } else if current.len() + 1 + piece.len() <= max_chars {
            current.push(' ');
            current.push_str(piece);
        } else {
            out.push(std::mem::take(&mut current));
            current = piece.to_string();
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    // Still too long (no commas): fall back to whitespace.
    out.into_iter()
        .flat_map(|piece| {
            if piece.len() <= max_chars {
                return vec![piece];
            }
            let mut parts = Vec::new();
            let mut building = String::new();
            for word in piece.split_whitespace() {
                if building.is_empty() {
                    building = word.to_string();
                } else if building.len() + 1 + word.len() <= max_chars {
                    building.push(' ');
                    building.push_str(word);
                } else {
                    parts.push(std::mem::take(&mut building));
                    building = word.to_string();
                }
            }
            if !building.is_empty() {
                parts.push(building);
            }
            parts
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_line_is_one_chunk() {
        assert_eq!(
            chunk_for_streaming("Yes, I can hear you.", 200),
            vec!["Yes, I can hear you."]
        );
    }

    #[test]
    fn sentences_are_kept_whole_and_packed_up_to_the_limit() {
        let chunks =
            chunk_for_streaming("One sentence here. Another one here. A third one here.", 40);
        assert_eq!(
            chunks,
            vec!["One sentence here. Another one here.", "A third one here."]
        );
    }

    #[test]
    fn an_absurdly_small_limit_is_clamped_rather_than_obeyed() {
        // Kokoro's prosody comes from whole clauses, so chunking down to a few
        // characters would sound worse than the latency it saved.
        assert_eq!(
            chunk_for_streaming("One. Two. Three.", 1),
            vec!["One. Two. Three."]
        );
    }

    #[test]
    fn the_first_chunk_is_short_so_the_first_word_comes_quickly() {
        // The point of chunking: a caller can play chunk one while two renders.
        let text = "Anthony, riotcoder. The deploy finished and every check passed. \
                    Do you want it on the rest of the fleet?";
        let chunks = chunk_for_streaming(text, 60);
        assert!(chunks.len() >= 2, "{chunks:?}");
        assert!(chunks[0].len() <= 60, "{:?}", chunks[0]);
        assert_eq!(
            chunks.concat().replace(' ', "").len(),
            text.replace(' ', "").replace('\n', "").len()
        );
    }

    #[test]
    fn a_sentence_longer_than_a_chunk_is_split_on_commas() {
        let text = "We shipped the thing, then we checked it, then we called him, and it worked.";
        let chunks = chunk_for_streaming(text, 30);
        assert!(chunks.iter().all(|c| c.len() <= 40), "{chunks:?}");
        assert!(chunks.len() >= 3, "{chunks:?}");
    }

    #[test]
    fn a_sentence_with_no_punctuation_still_gets_broken_up() {
        let text = "word ".repeat(60);
        let chunks = chunk_for_streaming(&text, 50);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 50), "{chunks:?}");
    }

    #[test]
    fn empty_text_asks_for_nothing() {
        assert!(chunk_for_streaming("", 100).is_empty());
        assert!(chunk_for_streaming("   \n  ", 100).is_empty());
    }

    #[tokio::test]
    async fn saying_into_a_session_that_does_not_exist_is_an_error_not_a_panic() {
        let store = SessionStore::new();
        let error = store.say("nope", "hello", "en-us", 1.0).await.unwrap_err();
        assert!(
            error.to_string().contains("no warm speech session"),
            "{error}"
        );
        assert!(!store.close("nope").await);
        assert!(store.list().await.is_empty());
    }
}
