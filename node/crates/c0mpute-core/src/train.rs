//! The `train` plugin: `c0mpute train lora`, a front door to the LoRA
//! training c0mpute already runs as `infernet.text.train` ([`crate::text`]).
//!
//! It does not train anything itself. It checks the buyer's dataset, turns
//! the common JSONL shapes into the corpus the worker's peft runner reads,
//! writes the job config from a handful of flags, and hands both to
//! [`crate::text::train`], which auctions the job peer to peer, relays
//! progress, and keeps the adapter only if its sha256 matches.
//!
//! Accepted record shapes (one JSON object per line, mixing allowed):
//!
//! - `{"text": "...", "title"?: "..."}`: kept as `{title, text}`; the worker
//!   trains it as one `user: "Brief me on: <title>"` / `assistant: <text>`
//!   exchange, loss on the whole exchange (as before).
//! - `{"prompt": "...", "completion": "..."}`: a one-exchange conversation,
//!   the prompt verbatim as the user turn.
//! - `{"messages": [{"role", "content"}, ...]}`: kept whole, every turn in
//!   order. The worker renders it with the base model's chat template and
//!   computes the loss on the assistant turns only. A system turn the same in
//!   every record also becomes the job's `system_prompt` (the adapter's
//!   default for chat); records without one get that prompt at train time.
//!
//! Conversations need a runner that reports the `messages` dataset format
//! (Infernet with peft `formats: ["text", "messages"]`), so a corpus holding
//! any requires [`crate::text::MESSAGES_FORMAT_TAG`] of the worker.

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

/// Same ceiling the worker enforces on the corpus it downloads.
pub const MAX_DATASET_BYTES: usize = 512 << 20;
pub const MAX_RECORDS: usize = 1_000_000;
/// One record (title + text) after normalization.
pub const MAX_RECORD_BYTES: usize = 1 << 20;
/// The peft runner holds out ~5% for eval and refuses fewer than two documents.
pub const MIN_RECORDS: usize = 2;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct DatasetReport {
    pub records: usize,
    pub text: usize,
    pub prompt_completion: usize,
    pub messages: usize,
    /// Blank lines skipped.
    pub blank: usize,
    pub bytes_in: usize,
    pub bytes_out: usize,
    /// The one system prompt the `messages` records share, if any.
    pub system_prompt: Option<String>,
}

fn str_field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// Check a JSONL dataset and convert it to the worker's `{title, text}` corpus.
pub fn normalize_dataset(bytes: &[u8]) -> Result<(Vec<u8>, DatasetReport)> {
    if bytes.len() > MAX_DATASET_BYTES {
        bail!("dataset is {} MiB; the limit is {} MiB", bytes.len() >> 20, MAX_DATASET_BYTES >> 20);
    }
    let text = std::str::from_utf8(bytes).map_err(|e| anyhow::anyhow!("dataset is not UTF-8: {e}"))?;
    let mut report = DatasetReport { bytes_in: bytes.len(), ..Default::default() };
    let mut out = Vec::with_capacity(bytes.len());
    let mut system: Option<String> = None;
    let mut system_conflict = false;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() {
            report.blank += 1;
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| anyhow::anyhow!("line {n}: not JSON: {e}"))?;
        if !v.is_object() {
            bail!("line {n}: each line must be a JSON object");
        }
        let (record, bytes) = if let Some(t) = str_field(&v, "text") {
            report.text += 1;
            let title = str_field(&v, "title").unwrap_or("");
            (json!({"title": title, "text": t}), title.len() + t.len())
        } else if let (Some(p), Some(c)) = (str_field(&v, "prompt"), str_field(&v, "completion")) {
            // The prompt verbatim as the user turn (not a "Brief me on:" title).
            report.prompt_completion += 1;
            (json!({"messages": [{"role": "user", "content": p}, {"role": "assistant", "content": c}]}), p.len() + c.len())
        } else if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
            let mut turns = Vec::with_capacity(msgs.len());
            let mut bytes = 0;
            let mut learns = false;
            for m in msgs {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                let content = m.get("content").and_then(Value::as_str);
                let Some(content) = content else {
                    bail!("line {n}: every message needs a string content");
                };
                match role {
                    "system" => match &system {
                        None if !system_conflict => system = Some(content.to_string()),
                        Some(s) if s != content => system_conflict = true,
                        _ => {}
                    },
                    "user" => {}
                    "assistant" => learns |= !content.trim().is_empty(),
                    other => bail!("line {n}: message role must be system, user or assistant, got {other:?}"),
                }
                bytes += content.len();
                turns.push(json!({"role": role, "content": content}));
            }
            if !learns {
                bail!("line {n}: messages need an assistant turn to learn from");
            }
            report.messages += 1;
            // Every turn, in order: the worker trains the whole conversation
            // (loss on the assistant turns).
            (json!({"messages": turns}), bytes)
        } else {
            bail!("line {n}: expected {{\"text\"}}, {{\"prompt\",\"completion\"}} or {{\"messages\"}}");
        };
        if bytes > MAX_RECORD_BYTES {
            bail!("line {n}: record is over {} KiB", MAX_RECORD_BYTES >> 10);
        }
        report.records += 1;
        if report.records > MAX_RECORDS {
            bail!("dataset has more than {MAX_RECORDS} records");
        }
        serde_json::to_writer(&mut out, &record)?;
        out.push(b'\n');
    }
    if report.records < MIN_RECORDS {
        bail!("dataset needs at least {MIN_RECORDS} records with text (found {})", report.records);
    }
    if out.len() > MAX_DATASET_BYTES {
        bail!("normalized dataset is over {} MiB", MAX_DATASET_BYTES >> 20);
    }
    report.bytes_out = out.len();
    report.system_prompt = if system_conflict { None } else { system };
    Ok((out, report))
}

/// `org/name` with the characters Hugging Face allows: never a path or URL.
pub fn valid_base_model(s: &str) -> bool {
    let mut parts = s.split('/');
    let (Some(org), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 96
            && !p.starts_with('.')
            && !p.contains("..")
            && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    ok(org) && ok(name)
}

/// The flags `c0mpute train lora` turns into a job config.
#[derive(Clone, Debug, PartialEq)]
pub struct LoraOptions {
    pub name: String,
    pub base_model: String,
    pub epochs: u32,
    pub rank: u32,
    pub lr: f64,
    pub seq_len: u32,
    pub batch_size: u32,
    pub max_steps: Option<u32>,
    pub system_prompt: Option<String>,
}

impl Default for LoraOptions {
    fn default() -> Self {
        Self {
            name: "adapter".into(),
            base_model: String::new(),
            epochs: 1,
            rank: 8,
            lr: 2e-4,
            seq_len: 256,
            batch_size: 4,
            max_steps: None,
            system_prompt: None,
        }
    }
}

impl LoraOptions {
    pub fn validate(&self) -> Result<()> {
        if !valid_base_model(&self.base_model) {
            bail!("--base must be a Hugging Face model id like Qwen/Qwen2.5-0.5B-Instruct");
        }
        if self.name.is_empty() || self.name.len() > 64 || !self.name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            bail!("--name must be 1-64 characters of letters, digits, '-', '_' or '.'");
        }
        if !(1..=20).contains(&self.epochs) {
            bail!("--epochs must be 1..=20");
        }
        if !(1..=128).contains(&self.rank) {
            bail!("--rank must be 1..=128");
        }
        if !(self.lr.is_finite() && self.lr > 0.0 && self.lr <= 1e-2) {
            bail!("--lr must be in (0, 0.01]");
        }
        if !(32..=4096).contains(&self.seq_len) {
            bail!("--seq-len must be 32..=4096");
        }
        if !(1..=64).contains(&self.batch_size) {
            bail!("--batch-size must be 1..=64");
        }
        if self.max_steps.is_some_and(|s| !(1..=1_000_000).contains(&s)) {
            bail!("--max-steps must be 1..=1000000");
        }
        if self.system_prompt.as_ref().is_some_and(|s| s.len() > 8 * 1024) {
            bail!("--system-prompt is over 8 KiB");
        }
        Ok(())
    }

    /// The `@infernetprotocol/training` job config `infernet.text.train` runs.
    /// An explicit `--system-prompt` wins over one found in the dataset.
    pub fn job(&self, report: &DatasetReport) -> Value {
        let mut hp = json!({
            "lr": self.lr,
            "batch_size": self.batch_size,
            "epochs": self.epochs,
            "seq_len": self.seq_len,
            "lora_r": self.rank,
        });
        if let Some(s) = self.max_steps {
            hp["max_steps"] = json!(s);
        }
        let mut job = json!({
            "name": self.name,
            "kind": "lora",
            "base_model": self.base_model,
            "dataset": {"url": "corpus.jsonl", "format": "jsonl"},
            "hyperparams": hp,
            "output": {"destination_url": "adapter", "format": "peft-lora"},
        });
        if let Some(s) = self.system_prompt.clone().or_else(|| report.system_prompt.clone()) {
            job["system_prompt"] = json!(s);
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_all_three_shapes() {
        let src = concat!(
            "{\"text\":\"a body\",\"title\":\"A\"}\n",
            "\n",
            "{\"prompt\":\"Q?\",\"completion\":\"Ans.\"}\n",
            "{\"messages\":[{\"role\":\"system\",\"content\":\"be brief\"},{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":\"hello\"}]}\n",
        );
        let (out, r) = normalize_dataset(src.as_bytes()).unwrap();
        assert_eq!((r.records, r.text, r.prompt_completion, r.messages, r.blank), (3, 1, 1, 1, 1));
        assert_eq!(r.system_prompt.as_deref(), Some("be brief"));
        let lines: Vec<Value> = std::str::from_utf8(&out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0], json!({"title": "A", "text": "a body"}));
        assert_eq!(lines[1], json!({"messages": [{"role": "user", "content": "Q?"}, {"role": "assistant", "content": "Ans."}]}));
        assert_eq!(
            lines[2],
            json!({"messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]})
        );
        assert!(crate::text::corpus_has_conversations(&out));
    }

    #[test]
    fn multi_turn_chats_keep_every_turn_in_order() {
        let chat = json!({"messages": [
            {"role": "system", "content": "You are a pricing coach."},
            {"role": "user", "content": "I sell a SaaS."},
            {"role": "assistant", "content": "What does it replace?"},
            {"role": "user", "content": "Spreadsheets."},
            {"role": "assistant", "content": "Price against the hours it saves."},
        ]});
        let src = format!("{chat}\n{chat}\n");
        let (out, r) = normalize_dataset(src.as_bytes()).unwrap();
        assert_eq!((r.records, r.messages), (2, 2));
        let first: Value = serde_json::from_str(std::str::from_utf8(&out).unwrap().lines().next().unwrap()).unwrap();
        // Nothing dropped, nothing reordered: both exchanges are there.
        assert_eq!(first, chat);
        assert_eq!(r.system_prompt.as_deref(), Some("You are a pricing coach."));
        // A record whose only assistant turn is empty teaches nothing.
        let empty = b"{\"messages\":[{\"role\":\"user\",\"content\":\"q\"},{\"role\":\"assistant\",\"content\":\" \"}]}\n{\"text\":\"x\"}\n";
        assert!(normalize_dataset(empty).unwrap_err().to_string().contains("assistant"));
        // Extra fields on a message are not carried to the worker.
        let extra = b"{\"messages\":[{\"role\":\"user\",\"content\":\"q\",\"name\":\"n\"},{\"role\":\"assistant\",\"content\":\"a\"}]}\n{\"text\":\"x\"}\n";
        let (out, _) = normalize_dataset(extra).unwrap();
        assert!(!std::str::from_utf8(&out).unwrap().contains("\"name\""));
        // Text-only corpora stay {title, text} and need no format tag.
        let (out, _) = normalize_dataset(b"{\"text\":\"a\"}\n{\"text\":\"b\",\"title\":\"t\"}\n").unwrap();
        assert!(!crate::text::corpus_has_conversations(&out));
    }

    #[test]
    fn a_long_conversation_counts_toward_the_record_cap() {
        let turn = "a".repeat(MAX_RECORD_BYTES / 4);
        let msgs: Vec<Value> = (0..5).map(|i| json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": turn})).collect();
        let src = format!("{}\n{{\"text\":\"x\"}}\n", json!({"messages": msgs}));
        assert!(normalize_dataset(src.as_bytes()).unwrap_err().to_string().contains("KiB"));
    }

    #[test]
    fn refuses_bad_datasets() {
        assert!(normalize_dataset(b"{\"text\":\"only one\"}\n").unwrap_err().to_string().contains("at least 2"));
        assert!(normalize_dataset(b"not json\n{\"text\":\"x\"}\n").unwrap_err().to_string().contains("line 1"));
        assert!(normalize_dataset(b"[1]\n[2]\n").unwrap_err().to_string().contains("object"));
        assert!(normalize_dataset(b"{\"foo\":1}\n{\"text\":\"x\"}\n").unwrap_err().to_string().contains("expected"));
        let no_assistant = b"{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}\n{\"text\":\"x\"}\n";
        assert!(normalize_dataset(no_assistant).unwrap_err().to_string().contains("assistant"));
        let bad_role = b"{\"messages\":[{\"role\":\"tool\",\"content\":\"x\"},{\"role\":\"assistant\",\"content\":\"y\"}]}\n{\"text\":\"x\"}\n";
        assert!(normalize_dataset(bad_role).is_err());
        let huge = format!("{{\"text\":\"{}\"}}\n{{\"text\":\"x\"}}\n", "a".repeat(MAX_RECORD_BYTES + 1));
        assert!(normalize_dataset(huge.as_bytes()).unwrap_err().to_string().contains("KiB"));
        assert!(normalize_dataset(&[0xff, 0xfe, b'\n']).unwrap_err().to_string().contains("UTF-8"));
    }

    #[test]
    fn differing_system_prompts_are_not_guessed() {
        let src = concat!(
            "{\"messages\":[{\"role\":\"system\",\"content\":\"a\"},{\"role\":\"assistant\",\"content\":\"x\"}]}\n",
            "{\"messages\":[{\"role\":\"system\",\"content\":\"b\"},{\"role\":\"assistant\",\"content\":\"y\"}]}\n",
        );
        assert_eq!(normalize_dataset(src.as_bytes()).unwrap().1.system_prompt, None);
    }

    #[test]
    fn base_model_ids() {
        for ok in ["Qwen/Qwen2.5-0.5B-Instruct", "HuggingFaceTB/SmolLM2-135M-Instruct", "a/b"] {
            assert!(valid_base_model(ok), "{ok}");
        }
        for bad in ["", "qwen", "/etc/passwd", "../x/y", "a/b/c", "https://x/y", "a/..", "a/b c"] {
            assert!(!valid_base_model(bad), "{bad}");
        }
    }

    #[test]
    fn job_matches_what_text_train_accepts() {
        let o = LoraOptions { base_model: "Qwen/Qwen2.5-0.5B-Instruct".into(), rank: 16, epochs: 2, max_steps: Some(50), ..Default::default() };
        o.validate().unwrap();
        let r = DatasetReport { system_prompt: Some("from data".into()), ..Default::default() };
        let job = o.job(&r);
        assert_eq!(job["kind"], "lora");
        assert_eq!(job["hyperparams"]["lora_r"], 16);
        assert_eq!(job["hyperparams"]["epochs"], 2);
        assert_eq!(job["hyperparams"]["max_steps"], 50);
        assert_eq!(job["system_prompt"], "from data");
        // The same spec validation the worker runs.
        let spec = crate::text::TextTrainSpec {
            corpus_key: None,
            job: job.clone(),
            dataset_url: String::new(),
            dataset_hash: format!("sha256:{}", "a".repeat(64)),
            artifact_upload_url: String::new(),
            events_url: None,
            corpus_hash: Some(c0mpute_proto::Hash::of(b"corpus").to_hex()),
            corpus_size: None,
            corpus_pieces: None,
            adapter_pieces: true,
            run_id: Some("r1".into()),
        };
        spec.validate().unwrap();
        let o2 = LoraOptions { system_prompt: Some("flag".into()), ..o.clone() };
        assert_eq!(o2.job(&r)["system_prompt"], "flag");
        for bad in [
            LoraOptions { rank: 0, ..o.clone() },
            LoraOptions { epochs: 21, ..o.clone() },
            LoraOptions { lr: 0.5, ..o.clone() },
            LoraOptions { lr: f64::NAN, ..o.clone() },
            LoraOptions { name: "a b".into(), ..o.clone() },
            LoraOptions { base_model: "x".into(), ..o.clone() },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
