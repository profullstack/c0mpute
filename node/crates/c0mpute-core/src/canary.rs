//! Auction canary: every few minutes, put one tiny offer per workload on the
//! network and count who bids.
//!
//! A probe is a real `JobOffer` (the same spec a gateway would send for a
//! small job), published by the status aggregator, whose bid window closes
//! without an accept: nothing runs, a bidder only reserves capacity for the
//! window plus a few seconds. Each probe also notes how many nodes advertised
//! the workload's tags at the time, so "no bids because nobody can serve it"
//! and "no bids although three nodes could" are told apart. The numbers land
//! in [`crate::auction_stats`] and from there in the status JSON
//! (`network.auction_canary`) that c0mpute.com/status renders.

use std::sync::Arc;
use std::time::Duration;

use c0mpute_net::Libp2pNetwork;
use tracing::{info, warn};

use crate::capabilities::Registry;

/// Default spacing between two probes of the same workload.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long a probe collects bids (the gateway gives images 6 s).
pub const PROBE_WINDOW: Duration = Duration::from_secs(6);

/// One workload's probe: the offer it sends and the tags a node needs.
pub struct Probe {
    pub workload: &'static str,
    pub spec: serde_json::Value,
    pub required: Vec<String>,
}

/// The workloads probed, each with the smallest spec its workers accept.
pub fn probes() -> Vec<Probe> {
    vec![
        Probe {
            workload: "tts.synthesize",
            spec: serde_json::json!({
                "text": "c0mpute canary.",
                "engine": "kokoro",
                "voice": "af_sarah",
                "locale": "en-US",
                "speed": 1.0,
                "persona": "",
            }),
            required: vec!["c0mpute:role:tts".into(), "c0mpute:tts:engine:kokoro".into()],
        },
        Probe {
            workload: crate::diffusion::WORKLOAD_TXT2IMG,
            spec: serde_json::json!({ "prompt": "c0mpute canary", "width": 256, "height": 256 }),
            required: vec![
                crate::diffusion::ROLE_TAG.into(),
                crate::diffusion::MODEL_TAG.into(),
            ],
        },
        Probe {
            workload: crate::music::WORKLOAD_TXT2MUSIC,
            spec: serde_json::json!({ "prompt": "c0mpute canary", "duration": 10 }),
            required: vec![crate::music::ROLE_TAG.into(), crate::music::MODEL_TAG.into()],
        },
        Probe {
            workload: crate::video::WORKLOAD_TXT2VID,
            spec: serde_json::json!({
                "model": crate::video_cpu::MODEL_ID,
                "prompt": "c0mpute canary",
                "frames": 17,
                "steps": 8,
                "width": 480,
                "height": 272,
            }),
            required: vec![
                crate::video::ROLE_TAG.into(),
                crate::video::model_tag(crate::video_cpu::MODEL_ID),
            ],
        },
    ]
}

/// `C0MPUTE_CANARY_SECS`: unset = [`DEFAULT_INTERVAL`], `0` = off.
pub fn interval_from_env() -> Option<Duration> {
    match std::env::var("C0MPUTE_CANARY_SECS") {
        Err(_) => Some(DEFAULT_INTERVAL),
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs.max(60))),
            Err(_) => Some(DEFAULT_INTERVAL),
        },
    }
}

/// How many advertised nodes carry every one of `required`.
pub async fn capable_nodes(registry: &Registry, required: &[String]) -> usize {
    registry
        .list()
        .await
        .iter()
        .filter(|(_, ad)| required.iter().all(|t| ad.tags.contains(t)))
        .count()
}

/// Probe every workload once per `interval`, spread evenly across it, until
/// the process exits. Waits a minute first so the registry has heard ads.
pub async fn run(net: Arc<Libp2pNetwork>, registry: Registry, interval: Duration) {
    let probes = probes();
    let gap = interval / probes.len().max(1) as u32;
    info!(interval_secs = interval.as_secs(), "canary: probing auctions");
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        for probe in &probes {
            let capable = capable_nodes(&registry, &probe.required).await;
            match crate::buyer::probe_auction(
                net.clone(),
                probe.workload,
                probe.spec.clone(),
                probe.required.clone(),
                PROBE_WINDOW,
            )
            .await
            {
                Ok(bids) => {
                    crate::auction_stats::record_probe(probe.workload, bids, capable);
                    if bids == 0 && capable > 0 {
                        warn!(workload = probe.workload, capable, "canary: no bids although nodes advertise the workload");
                    } else {
                        info!(workload = probe.workload, bids, capable, "canary: probe closed");
                    }
                }
                Err(e) => {
                    crate::auction_stats::record_probe(probe.workload, 0, capable);
                    warn!(workload = probe.workload, err = %e, "canary: probe failed");
                }
            }
            tokio::time::sleep(gap.saturating_sub(PROBE_WINDOW)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_specs_pass_the_workers_own_validation() {
        for p in probes() {
            match p.workload {
                w if w == crate::diffusion::WORKLOAD_TXT2IMG => {
                    let s: crate::diffusion::Txt2ImgSpec = serde_json::from_value(p.spec).unwrap();
                    s.validate().unwrap();
                }
                w if w == crate::music::WORKLOAD_TXT2MUSIC => {
                    let s: crate::music::Txt2MusicSpec = serde_json::from_value(p.spec).unwrap();
                    s.validate().unwrap();
                }
                w if w == crate::video::WORKLOAD_TXT2VID => {
                    let j: crate::video::VideoJobInline = serde_json::from_value(p.spec).unwrap();
                    crate::video_cpu::fits(&j, w).unwrap();
                }
                _ => {}
            }
        }
    }
}
