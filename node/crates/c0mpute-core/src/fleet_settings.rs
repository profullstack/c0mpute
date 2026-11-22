//! Node settings the operator sets from their fleet (c0mpute.com/dashboard/nodes,
//! `c0mpute fleet models|house-models|uncensored|hosting|storage`, the TUI and
//! the fleet MCP tools), as signed fleet commands the node checks first (see
//! the CLI's `fleet_agent`).
//!
//! They live in `<config dir>/fleet-settings.json`, apart from config.toml,
//! so a fleet change never rewrites the operator's own file: an unset field
//! (null) means "what this node would do anyway". The previous file is kept
//! as `fleet-settings.json.bak`.
//!
//! How each one takes effect:
//! - `models` (an allowlist), `house_models` and `uncensored` filter the chat
//!   models this node advertises and bids on, at once (no restart): see
//!   [`filter_models`], applied in `inference::ready_models`. `house_models =
//!   false` also stops the house runtime from installing or starting again.
//! - `hosting` and `storage` decide whether the worker holds sites and
//!   shards; the worker reads them at start, so the node drains running jobs
//!   and restarts into the new setting. `--no-storage` / `--no-hosting` on the
//!   command line, and `C0MPUTE_STORAGE` / `C0MPUTE_HOSTING`, still win.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const FILE: &str = "fleet-settings.json";
/// Most models one allowlist may name.
pub const MAX_MODELS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSettings {
    /// Serve only these chat models (exact ids). None or empty: every model.
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Serve the network's house models (the house runtime and its GGUFs).
    #[serde(default)]
    pub house_models: Option<bool>,
    /// Serve uncensored / abliterated models.
    #[serde(default)]
    pub uncensored: Option<bool>,
    /// Hold sites for the network.
    #[serde(default)]
    pub hosting: Option<bool>,
    /// Store shards for the network.
    #[serde(default)]
    pub storage: Option<bool>,
}

/// The fleet actions this module handles.
pub const ACTIONS: [&str; 5] = ["models", "house-models", "uncensored", "hosting", "storage"];

pub fn handles(action: &str) -> bool {
    ACTIONS.contains(&action)
}

/// Whether changing this action needs the worker to restart to take effect.
pub fn needs_restart(action: &str) -> bool {
    matches!(action, "hosting" | "storage")
}

/// Chat model ids: the same rule the inference spec applies.
pub fn valid_model_id(m: &str) -> bool {
    !m.is_empty() && m.len() <= 128 && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@'))
}

/// The settings after one fleet action with its (signed) args, or why the
/// args are refused. `args` is the JSON object the site signed.
pub fn applied(cur: &FleetSettings, action: &str, args: &Value) -> Result<FleetSettings, String> {
    let mut next = cur.clone();
    if action == "models" {
        let list = match &args["models"] {
            Value::Null => Vec::new(),
            Value::Array(a) => {
                let mut out = Vec::new();
                for m in a {
                    let Some(s) = m.as_str().map(str::trim) else {
                        return Err("models: a list of model ids".into());
                    };
                    if !valid_model_id(s) {
                        return Err(format!("not a model id: {s:?}"));
                    }
                    if !out.iter().any(|x| x == s) {
                        out.push(s.to_string());
                    }
                }
                out
            }
            _ => return Err("models: a list of model ids (empty = every model)".into()),
        };
        if list.len() > MAX_MODELS {
            return Err(format!("at most {MAX_MODELS} models"));
        }
        next.models = if list.is_empty() { None } else { Some(list) };
        return Ok(next);
    }
    let on = match &args["on"] {
        Value::Bool(b) => Some(*b),
        // `{"on": null}` hands the setting back to the node's own config.
        Value::Null if args.get("on").is_some() => None,
        _ => return Err(format!("{action}: args {{\"on\": true|false}}")),
    };
    match action {
        "house-models" => next.house_models = on,
        "uncensored" => next.uncensored = on,
        "hosting" => next.hosting = on,
        "storage" => next.storage = on,
        other => return Err(format!("not a node setting: {other}")),
    }
    Ok(next)
}

/// Ids of the network's house chat models (CPU runtime and GPU Ollama).
pub fn is_house_model(id: &str) -> bool {
    crate::inference_auto::HOUSE_MODELS.iter().any(|m| m.id == id)
        || id == crate::inference::HOUSE_DEFAULT_MODEL
        || id == crate::inference::HOUSE_GPU_MODEL
        || [24 * 1024, 6 * 1024, 0].iter().any(|v| crate::inference_gpu::models_for_vram(*v).contains(&id))
}

/// Uncensored / abliterated fine-tunes, by their id.
pub fn is_uncensored(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    ["uncensored", "abliterated", "abliterate", "nsfw"].iter().any(|w| l.contains(w))
}

/// The chat models to advertise, given what the runtime lists.
pub fn filter_models(models: Vec<String>, s: &FleetSettings) -> Vec<String> {
    models
        .into_iter()
        .filter(|m| s.house_models != Some(false) || !is_house_model(m))
        .filter(|m| s.uncensored != Some(false) || !is_uncensored(m))
        .filter(|m| match &s.models {
            Some(allow) if !allow.is_empty() => allow.iter().any(|a| a == m),
            _ => true,
        })
        .collect()
}

// ── the file ──────────────────────────────────────────────────────────────────

static CURRENT: RwLock<Option<FleetSettings>> = RwLock::new(None);

fn path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join(FILE))
}

pub fn load_from(p: &Path) -> FleetSettings {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// The settings now (read once per process, then kept in memory).
pub fn current() -> FleetSettings {
    if let Ok(g) = CURRENT.read() {
        if let Some(s) = g.as_ref() {
            return s.clone();
        }
    }
    let s = path().map(|p| load_from(&p)).unwrap_or_default();
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(s.clone());
    }
    s
}

/// Write the file whole (tmp + rename, the old one kept as `.bak`), then use it.
pub fn save_to(p: &Path, s: &FleetSettings) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    if p.exists() {
        let _ = std::fs::copy(p, p.with_extension("json.bak"));
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default())?;
    std::fs::rename(&tmp, p)
}

pub fn save(s: &FleetSettings) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::other("no config dir"))?;
    save_to(&p, s)?;
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(s.clone());
    }
    Ok(())
}

// ── what the worker runs with ─────────────────────────────────────────────────

/// Storage, hosting and the storage root as this worker process started
/// with them (set by `worker start`), for the state a node reports.
#[derive(Clone, Debug, Default)]
pub struct Effective {
    pub storage: bool,
    pub hosting: bool,
    pub storage_root: Option<PathBuf>,
}

static EFFECTIVE: RwLock<Option<Effective>> = RwLock::new(None);

pub fn set_effective(e: Effective) {
    if let Ok(mut g) = EFFECTIVE.write() {
        *g = Some(e);
    }
}

pub fn effective() -> Option<Effective> {
    EFFECTIVE.read().ok().and_then(|g| g.clone())
}

/// Free and total bytes of the disk holding `root` (the data dir when None).
pub fn disk(root: Option<&Path>) -> Option<(u64, u64)> {
    let r = root.map(Path::to_path_buf).or_else(crate::config::data_dir)?;
    let (free, total) = crate::opendisk::disk_for(&r);
    (total > 0).then_some((free, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s() -> FleetSettings {
        FleetSettings::default()
    }

    #[test]
    fn models_allowlist_set_and_cleared() {
        let a = applied(&s(), "models", &json!({"models": ["qwen3:4b", " llama3:8b ", "qwen3:4b"]})).unwrap();
        assert_eq!(a.models, Some(vec!["qwen3:4b".to_string(), "llama3:8b".to_string()]));
        assert_eq!(applied(&a, "models", &json!({"models": []})).unwrap().models, None);
        assert_eq!(applied(&a, "models", &json!({"models": null})).unwrap().models, None);
        assert!(applied(&s(), "models", &json!({"models": ["bad id; rm -rf"]})).is_err());
        assert!(applied(&s(), "models", &json!({"models": "qwen3:4b"})).is_err());
        let many: Vec<String> = (0..=MAX_MODELS).map(|i| format!("m{i}")).collect();
        assert!(applied(&s(), "models", &json!({ "models": many })).is_err());
    }

    #[test]
    fn toggles_need_a_boolean() {
        let a = applied(&s(), "hosting", &json!({"on": false})).unwrap();
        assert_eq!(a.hosting, Some(false));
        let b = applied(&a, "storage", &json!({"on": true})).unwrap();
        assert_eq!((b.hosting, b.storage), (Some(false), Some(true)));
        assert_eq!(applied(&b, "hosting", &json!({"on": null})).unwrap().hosting, None);
        assert!(applied(&s(), "uncensored", &json!({})).is_err());
        assert!(applied(&s(), "uncensored", &json!({"on": "no"})).is_err());
        assert!(applied(&s(), "reboot", &json!({"on": true})).is_err());
    }

    #[test]
    fn filters_models() {
        let all = || {
            vec![
                crate::inference::HOUSE_DEFAULT_MODEL.to_string(),
                crate::inference::HOUSE_GPU_MODEL.to_string(),
                "llama3:8b".to_string(),
                "dolphin-uncensored:7b".to_string(),
            ]
        };
        assert_eq!(filter_models(all(), &s()), all());
        let no_house = FleetSettings { house_models: Some(false), ..s() };
        assert_eq!(filter_models(all(), &no_house), vec!["llama3:8b", "dolphin-uncensored:7b"]);
        let clean = FleetSettings { uncensored: Some(false), ..s() };
        assert_eq!(filter_models(all(), &clean), vec!["llama3:8b"]);
        let only = FleetSettings { models: Some(vec!["llama3:8b".into(), "not-installed:1b".into()]), ..s() };
        assert_eq!(filter_models(all(), &only), vec!["llama3:8b"]);
    }

    #[test]
    fn file_round_trip_keeps_a_backup() {
        let dir = std::env::temp_dir().join(format!("c0-fleet-settings-{}", std::process::id()));
        let p = dir.join(FILE);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_from(&p), s());
        let a = FleetSettings { hosting: Some(false), ..s() };
        save_to(&p, &a).unwrap();
        assert_eq!(load_from(&p), a);
        let b = FleetSettings { models: Some(vec!["x:1b".into()]), ..a.clone() };
        save_to(&p, &b).unwrap();
        assert_eq!(load_from(&p), b);
        assert_eq!(load_from(&p.with_extension("json.bak")), a);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_from(&p), s(), "a broken file means no overrides, not a crash");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
