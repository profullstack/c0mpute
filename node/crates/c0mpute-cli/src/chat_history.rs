//! Saved `c0mpute chat` conversations: one JSON file per conversation under
//! `~/.c0mpute/chats/<id>.json` (0600, dir 0700), and, when signed in, the
//! same conversation synced to the account (`/api/v1/chat/conversations`,
//! shared with the web chat and the TUI).
//!
//! The file shape is the account's: `{id, title, model, messages, updated}`
//! (`updated` is unix ms), plus `pending_run`: the run id of an answer that
//! was still streaming, so `--resume` can reattach to it after a crash or a
//! dropped connection.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Convo {
    pub id: String,
    pub title: String,
    /// The model id, or "auto" for any live model (the web chat's spelling).
    pub model: String,
    pub messages: Vec<Value>,
    pub updated: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_run: Option<String>,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 12 random base36 characters (std's per-process random SipHash keys).
pub(crate) fn new_id() -> String {
    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::new();
    while out.len() < 12 {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let mut n = h.finish();
        for _ in 0..6 {
            out.push(ALPHA[(n % 36) as usize] as char);
            n /= 36;
        }
    }
    out
}

/// The account's id rule: `^[A-Za-z0-9_-]{4,64}$` (also keeps paths safe).
pub(crate) fn valid_id(id: &str) -> bool {
    (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Convo {
    pub fn new(model: Option<&str>, messages: Vec<Value>) -> Self {
        Self {
            id: new_id(),
            title: "new chat".into(),
            model: model.unwrap_or("auto").to_string(),
            messages,
            updated: now_ms(),
            pending_run: None,
        }
    }

    /// The model to ask, None for any live model.
    pub fn model_opt(&self) -> Option<String> {
        (!self.model.is_empty() && self.model != "auto").then(|| self.model.clone())
    }

    /// Title = the first user line, 60 characters.
    pub fn retitle(&mut self) {
        if let Some(t) = title_from(&self.messages) {
            self.title = t;
        }
    }

    /// Messages the model should see: user/assistant/system with text, no
    /// local notes.
    pub fn model_messages(&self) -> Vec<Value> {
        self.messages
            .iter()
            .filter(|m| matches!(m["role"].as_str(), Some("user" | "assistant" | "system")) && m["content"].is_string())
            .map(|m| json!({"role": m["role"], "content": m["content"]}))
            .collect()
    }

    /// The body the account takes on PUT.
    pub fn put_body(&self) -> Value {
        json!({"title": self.title, "model": self.model, "messages": self.messages, "updated": self.updated})
    }
}

pub(crate) fn title_from(messages: &[Value]) -> Option<String> {
    let first = messages.iter().find(|m| m["role"] == "user")?["content"].as_str()?;
    let line = first.lines().find(|l| !l.trim().is_empty())?.trim();
    Some(line.chars().take(60).collect())
}

pub(crate) fn chats_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".c0mpute").join("chats"))
}

pub(crate) fn save_in(dir: &Path, c: &Convo) -> Result<PathBuf> {
    if !valid_id(&c.id) {
        return Err(anyhow!("bad conversation id {:?}", c.id));
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let path = dir.join(format!("{}.json", c.id));
    let tmp = dir.join(format!(".{}.json.tmp", c.id));
    let body = serde_json::to_vec_pretty(c)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&body)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub(crate) fn load_in(dir: &Path, id: &str) -> Option<Convo> {
    if !valid_id(id) {
        return None;
    }
    let s = std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&s).ok()
}

/// Every saved conversation, newest first. Unreadable files are skipped.
pub(crate) fn list_in(dir: &Path) -> Vec<Convo> {
    let mut out: Vec<Convo> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json")?;
            load_in(dir, id)
        })
        .collect();
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    out
}

pub(crate) fn latest_in(dir: &Path) -> Option<Convo> {
    list_in(dir).into_iter().next()
}

pub(crate) fn save(c: &Convo) -> Result<PathBuf> {
    save_in(&chats_dir()?, c)
}

// ── account sync ───────────────────────────────────────────────────────────

fn conversations_url(url: &str) -> String {
    format!("{url}/api/v1/chat/conversations")
}

/// PUT the conversation; on 409 (newer elsewhere) keep ours and re-PUT with
/// `updated` past the server's. Returns the `updated` the account holds.
pub(crate) async fn put(url: &str, key: &str, c: &mut Convo) -> Result<()> {
    let client = crate::ai_chat::client()?;
    for _ in 0..2 {
        let res = client
            .put(format!("{}/{}", conversations_url(url), c.id))
            .bearer_auth(key)
            .timeout(Duration::from_secs(15))
            .json(&c.put_body())
            .send()
            .await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let text = res.text().await.unwrap_or_default();
        if status.as_u16() == 409 {
            let theirs = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.pointer("/conversation/updated").and_then(Value::as_i64))
                .unwrap_or(0);
            c.updated = c.updated.max(theirs) + 1;
            continue;
        }
        return Err(crate::ai_chat::api_error(status, &text));
    }
    Err(anyhow!("the account kept a newer copy"))
}

pub(crate) async fn get(url: &str, key: &str, id: &str) -> Result<Option<Convo>> {
    if !valid_id(id) {
        return Ok(None);
    }
    let res = crate::ai_chat::client()?
        .get(format!("{}/{id}", conversations_url(url)))
        .bearer_auth(key)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let status = res.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::ai_chat::api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(Some(convo_from_api(&v)?))
}

/// The account's conversation JSON as a Convo (tolerates a missing model).
pub(crate) fn convo_from_api(v: &Value) -> Result<Convo> {
    Ok(Convo {
        id: v["id"].as_str().ok_or_else(|| anyhow!("conversation without an id"))?.to_string(),
        title: v["title"].as_str().unwrap_or("new chat").to_string(),
        model: v["model"].as_str().unwrap_or("auto").to_string(),
        messages: v["messages"].as_array().cloned().unwrap_or_default(),
        updated: v["updated"].as_i64().unwrap_or(0),
        pending_run: None,
    })
}

/// One row of `c0mpute chat history`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub id: String,
    pub title: String,
    pub updated: i64,
    pub local: bool,
    pub account: bool,
}

pub(crate) async fn list_remote(url: &str, key: &str) -> Result<Vec<(String, String, i64, bool)>> {
    let res = crate::ai_chat::client()?
        .get(conversations_url(url))
        .bearer_auth(key)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::ai_chat::api_error(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_string(),
                r["title"].as_str().unwrap_or("").to_string(),
                r["updated"].as_i64().unwrap_or(0),
                r["deleted"].as_bool().unwrap_or(false),
            ))
        })
        .collect())
}

/// Local and account conversations as one list, newest first; an account
/// tombstone hides the local copy.
pub(crate) fn merge_rows(local: &[Convo], remote: &[(String, String, i64, bool)]) -> Vec<Row> {
    let mut rows: Vec<Row> = local
        .iter()
        .map(|c| Row { id: c.id.clone(), title: c.title.clone(), updated: c.updated, local: true, account: false })
        .collect();
    for (id, title, updated, deleted) in remote {
        if let Some(i) = rows.iter().position(|r| &r.id == id) {
            if *deleted && *updated >= rows[i].updated {
                rows.remove(i);
                continue;
            }
            rows[i].account = !deleted;
            if *updated > rows[i].updated {
                rows[i].updated = *updated;
                rows[i].title = title.clone();
            }
        } else if !deleted {
            rows.push(Row { id: id.clone(), title: title.clone(), updated: *updated, local: false, account: true });
        }
    }
    rows.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// "2026-11-11 23:05" (UTC) for a unix-ms time.
pub(crate) fn when(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", tod / 3600, (tod % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_valid_and_differ() {
        let a = new_id();
        let b = new_id();
        assert!(valid_id(&a), "{a}");
        assert_ne!(a, b);
        assert!(!valid_id("../x"));
        assert!(!valid_id("abc"));
        assert!(valid_id("ab_C-9"));
    }

    #[test]
    fn file_round_trip_latest_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("chats");
        let mut a = Convo::new(None, vec![json!({"role":"system","content":"terse"}), json!({"role":"user","content":"\n  explain LoRA adapters in detail please, with examples and all the math too\nmore"})]);
        a.retitle();
        assert_eq!(a.title.chars().count(), 60);
        assert!(a.title.starts_with("explain LoRA"));
        a.updated = 1_000;
        a.pending_run = Some("run123".into());
        let mut b = Convo::new(Some("qwen2.5:7b"), vec![]);
        b.updated = 2_000;
        save_in(&d, &a).unwrap();
        save_in(&d, &b).unwrap();
        assert_eq!(load_in(&d, &a.id), Some(a.clone()));
        assert_eq!(latest_in(&d).unwrap().id, b.id);
        assert_eq!(list_in(&d).iter().map(|c| c.id.clone()).collect::<Vec<_>>(), [b.id.clone(), a.id.clone()]);
        assert_eq!(b.model_opt().as_deref(), Some("qwen2.5:7b"));
        assert_eq!(a.model_opt(), None);
        // pending_run stays local: never in the account body.
        assert!(a.put_body().get("pending_run").is_none());
        std::fs::write(d.join("junk.json"), "{not json").unwrap();
        assert_eq!(list_in(&d).len(), 2);
        assert!(load_in(&d, "../../etc/passwd").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join(format!("{}.json", a.id))).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn model_messages_drop_notes_and_extra_fields() {
        let c = Convo::new(None, vec![
            json!({"role":"user","content":"hi","at":1}),
            json!({"role":"note","content":"local only"}),
            json!({"role":"assistant","content":"hello","jobId":"x"}),
            json!({"role":"assistant"}),
        ]);
        assert_eq!(c.model_messages(), vec![json!({"role":"user","content":"hi"}), json!({"role":"assistant","content":"hello"})]);
    }

    #[test]
    fn history_merges_account_rows_and_tombstones() {
        let mut a = Convo::new(None, vec![]);
        a.id = "aaaa".into();
        a.title = "local a".into();
        a.updated = 10;
        let mut b = Convo::new(None, vec![]);
        b.id = "bbbb".into();
        b.title = "local b".into();
        b.updated = 20;
        let remote = vec![
            ("aaaa".to_string(), "renamed a".to_string(), 30, false),
            ("bbbb".to_string(), "".to_string(), 25, true),
            ("cccc".to_string(), "from the web".to_string(), 5, false),
            ("dddd".to_string(), "gone".to_string(), 6, true),
        ];
        let rows = merge_rows(&[a, b], &remote);
        assert_eq!(
            rows,
            vec![
                Row { id: "aaaa".into(), title: "renamed a".into(), updated: 30, local: true, account: true },
                Row { id: "cccc".into(), title: "from the web".into(), updated: 5, local: false, account: true },
            ]
        );
    }

    #[test]
    fn api_json_becomes_a_convo() {
        let c = convo_from_api(&json!({"id":"abcd1","title":"t","messages":[{"role":"user","content":"x"}],"updated":7})).unwrap();
        assert_eq!(c.model, "auto");
        assert_eq!(c.messages.len(), 1);
        assert!(convo_from_api(&json!({"title":"no id"})).is_err());
    }

    #[test]
    fn formats_utc_times() {
        assert_eq!(when(0), "1970-01-01 00:00");
        assert_eq!(when(1_794_438_300_000), "2026-11-11 23:05");
    }
}
