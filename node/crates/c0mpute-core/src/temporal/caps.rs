//! What the local Infernet can execute, as capability tags.
//!
//! Tags are only advertised when `infernet model capabilities --json`
//! answers, i.e. the operator installed the infernet-liquid runtime
//! themselves (`infernet model runtime setup`). Nothing is installed or
//! downloaded from here.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{INFER_WORKLOAD, TRAIN_WORKLOAD};

pub const RAM_BUCKETS_GB: [u64; 7] = [2, 4, 8, 16, 32, 64, 128];

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RuntimeCaps {
    pub workloads: Vec<String>,
    pub families: Vec<String>,
    pub devices: Devices,
    pub ram_mb: u64,
    #[serde(default)]
    pub threads: u64,
    pub runtime: RuntimeRef,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Devices {
    #[serde(default)]
    pub cpu: bool,
    #[serde(default)]
    pub cuda: bool,
    #[serde(default)]
    pub mps: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RuntimeRef {
    pub name: String,
    pub version: String,
    pub lock_hash: String,
}

/// Ask the local `infernet` what temporal workloads it can run.
pub async fn probe(infernet: &Path) -> Result<RuntimeCaps> {
    let out = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new(infernet)
            .args(["model", "capabilities", "--json"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .context("infernet model capabilities timed out")?
    .with_context(|| format!("run {}", infernet.display()))?;
    if !out.status.success() {
        bail!(
            "infernet model capabilities failed (runtime not installed? `infernet model runtime setup --yes`): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parse infernet model capabilities --json")
}

pub fn model_tag(model_hash: &str) -> String {
    format!("infernet:temporal:model:{}", model_hash.trim_start_matches("sha256:"))
}

fn ram_tag(gb: u64) -> String {
    format!("c0mpute:ram:{gb}gb+")
}

/// Tags for a node whose Infernet reported `caps`, serving `models`.
/// `train` / `infer` say which workloads this node opted into.
pub fn tags(caps: &RuntimeCaps, models: &[String], train: bool, infer: bool) -> Vec<String> {
    let mut t = vec![
        "infernet:temporal:runtime".to_string(),
        format!("infernet:temporal:runtime:{}", caps.runtime.version),
    ];
    if train && caps.workloads.iter().any(|w| w == TRAIN_WORKLOAD) {
        t.push(TRAIN_WORKLOAD.replace('.', ":"));
    }
    if infer && caps.workloads.iter().any(|w| w == INFER_WORKLOAD) {
        t.push(INFER_WORKLOAD.replace('.', ":"));
    }
    t.push("infernet:temporal:device:cpu".into());
    if caps.devices.cuda {
        t.push("infernet:temporal:device:cuda".into());
    }
    if caps.devices.mps {
        t.push("infernet:temporal:device:mps".into());
    }
    for gb in RAM_BUCKETS_GB {
        if caps.ram_mb >= gb * 1024 {
            t.push(ram_tag(gb));
        }
    }
    if infer {
        for m in models {
            t.push(model_tag(m));
        }
    }
    t
}

/// What a buyer needs from a worker.
#[derive(Clone, Debug, Default)]
pub struct Requirements {
    pub gpu: bool,
    pub min_ram_gb: Option<u64>,
    pub runtime_version: Option<String>,
    pub model_hash: Option<String>,
}

/// Requirements → the `required_capabilities` of a `JobOffer`. The existing
/// dispatcher drops offers whose tags the worker does not advertise.
pub fn required_tags(workload: &str, r: &Requirements) -> Vec<String> {
    let mut t = vec![workload.replace('.', ":")];
    if r.gpu {
        t.push("infernet:temporal:device:cuda".into());
    }
    if let Some(min) = r.min_ram_gb {
        let bucket = RAM_BUCKETS_GB.iter().copied().find(|b| *b >= min).unwrap_or(*RAM_BUCKETS_GB.last().unwrap());
        t.push(ram_tag(bucket));
    }
    if let Some(v) = &r.runtime_version {
        t.push(format!("infernet:temporal:runtime:{v}"));
    }
    if let Some(m) = &r.model_hash {
        t.push(model_tag(m));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(cuda: bool, ram_mb: u64) -> RuntimeCaps {
        RuntimeCaps {
            workloads: vec![TRAIN_WORKLOAD.into(), INFER_WORKLOAD.into()],
            families: vec!["cfc".into(), "ltc".into(), "gru".into()],
            devices: Devices { cpu: true, cuda, mps: false },
            ram_mb,
            threads: 8,
            runtime: RuntimeRef { name: "infernet-liquid".into(), version: "0.1.0".into(), lock_hash: format!("sha256:{}", "0".repeat(64)) },
        }
    }

    fn eligible(adv: &[String], req: &[String]) -> bool {
        req.iter().all(|r| adv.contains(r))
    }

    #[test]
    fn filtering_by_runtime_device_ram_and_model() {
        let m = format!("sha256:{}", "d".repeat(64));
        let cpu16 = tags(&caps(false, 16 * 1024), &[m.clone()], true, true);
        let gpu64 = tags(&caps(true, 64 * 1024), &[], true, false);
        let train_cpu = required_tags(TRAIN_WORKLOAD, &Requirements { min_ram_gb: Some(12), ..Default::default() });
        assert!(eligible(&cpu16, &train_cpu));
        assert!(eligible(&gpu64, &train_cpu));
        let train_gpu = required_tags(TRAIN_WORKLOAD, &Requirements { gpu: true, ..Default::default() });
        assert!(!eligible(&cpu16, &train_gpu));
        assert!(eligible(&gpu64, &train_gpu));
        let big = required_tags(TRAIN_WORKLOAD, &Requirements { min_ram_gb: Some(40), ..Default::default() });
        assert!(!eligible(&cpu16, &big));
        assert!(eligible(&gpu64, &big));
        let infer = required_tags(INFER_WORKLOAD, &Requirements { model_hash: Some(m.clone()), ..Default::default() });
        assert!(eligible(&cpu16, &infer));
        assert!(!eligible(&gpu64, &infer), "gpu64 did not opt into infer nor load the model");
        let ver = required_tags(TRAIN_WORKLOAD, &Requirements { runtime_version: Some("9.9.9".into()), ..Default::default() });
        assert!(!eligible(&cpu16, &ver));
    }

    #[test]
    fn nothing_advertised_for_workloads_the_runtime_lacks() {
        let mut c = caps(false, 8192);
        c.workloads = vec![];
        let t = tags(&c, &[], true, true);
        assert!(!t.iter().any(|x| x == "infernet:temporal:train" || x == "infernet:temporal:infer"));
    }
}
