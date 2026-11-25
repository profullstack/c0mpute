//! Scallion relay participation for a running worker: config, the signed
//! descriptor it advertises, and the directory of descriptors it hears.
//!
//! This is advertising only. A worker with participation on puts a signed
//! relay descriptor in its capability ad (`c0mpute/cap/v1`, under
//! `hardware.scallion`), re-signed well before it expires; every node that
//! keeps a capability registry verifies the descriptors it hears and lists
//! them. No circuit is built, no stream is opened and nothing is relayed:
//! the link layer, the stream layer and the exit are not implemented. This
//! is not a VPN and makes no anonymity claim.
//!
//! Defaults: relay (guard + middle) on for every worker, exit off. Exit is
//! strictly opt-in (`[scallion] exit = true`, `c0mpute scallion exit on`, or
//! `C0MPUTE_SCALLION_EXIT=1`) and anything unclear about the opt-in leaves
//! it off. Private mode never participates.
//!
//! Spec: docs/protocol/scallion-participation.md.

use std::sync::{Mutex, RwLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use c0mpute_net::scallion_handshake::RelayStaticKey;
use c0mpute_net::scallion_participation::{
    DEFAULT_EXIT_PORTS, DESCRIPTOR_REFRESH_BEFORE_MS, Participation, RelayDirectory, RelayRecord,
    role_names, sanitize_operator,
};
use c0mpute_net::{Libp2pNetwork, PeerId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

/// `0`/`false`/`no`/`off` turns relay participation off; any other value on.
pub const RELAY_ENV: &str = "C0MPUTE_SCALLION";
/// `1`/`true`/`yes`/`on` opts this node in as an exit; `0`/`false`/`no`/`off`
/// forces exit off. Any other value leaves exit off.
pub const EXIT_ENV: &str = "C0MPUTE_SCALLION_EXIT";
/// Comma-separated exit ports, overriding `[scallion] exit_ports`.
pub const EXIT_PORTS_ENV: &str = "C0MPUTE_SCALLION_EXIT_PORTS";
/// The operator label in the descriptor, overriding `[scallion] operator`.
pub const OPERATOR_ENV: &str = "C0MPUTE_SCALLION_OPERATOR";

/// The key of the descriptor inside a capability ad's `hardware` blob.
pub const AD_FIELD: &str = "scallion";
/// The local gateway path that lists what a running worker knows.
pub const RELAYS_PATH: &str = "/scallion/v1/relays";

/// Said wherever Scallion state is shown, so it is never read as a VPN.
pub const NOT_CARRYING: &str = "Scallion carries no traffic yet: nodes only advertise relay roles and verify each other's descriptors. Nothing is relayed and this is not a VPN.";

/// Longest base64 descriptor read out of an ad.
const MAX_DESCRIPTOR_B64: usize = 4096;

/// `[scallion]`: which Scallion relay roles this node advertises.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScallionConfig {
    /// Advertise this node as a guard (entry) and middle relay. On by
    /// default on every worker; `false` (or `C0MPUTE_SCALLION=0`, or
    /// `c0mpute scallion relay off`) turns participation off entirely.
    #[serde(default = "default_true")]
    pub relay: bool,
    /// Also advertise this node as an exit. Off by default and never
    /// implied by `relay`: the operator opts in (`c0mpute scallion exit on`
    /// or `C0MPUTE_SCALLION_EXIT=1`).
    #[serde(default)]
    pub exit: bool,
    /// The TCP ports an exit advertises. Only read when `exit` is on.
    #[serde(default = "default_exit_ports")]
    pub exit_ports: Vec<u16>,
    /// A self-declared operator label in the descriptor (printable ASCII, at
    /// most 64 bytes), so nodes run by one operator can say so. A claim, not
    /// proof of anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_exit_ports() -> Vec<u16> {
    DEFAULT_EXIT_PORTS.to_vec()
}

impl Default for ScallionConfig {
    fn default() -> Self {
        Self {
            relay: true,
            exit: false,
            exit_ports: default_exit_ports(),
            operator: None,
        }
    }
}

/// The environment values that override `[scallion]`.
#[derive(Clone, Debug, Default)]
pub struct ScallionEnv {
    pub relay: Option<String>,
    pub exit: Option<String>,
    pub exit_ports: Option<String>,
    pub operator: Option<String>,
}

impl ScallionEnv {
    /// This process's environment (empty values count as unset).
    pub fn from_process() -> Self {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            relay: get(RELAY_ENV),
            exit: get(EXIT_ENV),
            exit_ports: get(EXIT_PORTS_ENV),
            operator: get(OPERATOR_ENV),
        }
    }
}

/// What a node will advertise, and where each decision came from.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub participation: Participation,
    /// The operator label for the descriptor ("" when none).
    pub operator: String,
    pub relay_source: String,
    pub exit_source: String,
    /// Settings that were ignored, each worded for the operator.
    pub warnings: Vec<String>,
}

fn is_off(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn is_on(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// A comma-separated port list. Any entry that is not a port fails the lot.
pub fn parse_ports(raw: &str) -> Result<Vec<u16>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u16>().map_err(|_| format!("'{s}' is not a port")))
        .collect()
}

impl ScallionConfig {
    /// Decide what this node advertises from the config, the environment
    /// and private mode. Exit is on only when it was explicitly asked for
    /// and its port list is valid; every doubtful case leaves it off.
    pub fn resolve(&self, env: &ScallionEnv, private: bool) -> Resolved {
        let mut warnings = Vec::new();
        let operator = match env.operator.as_deref().or(self.operator.as_deref()) {
            None => String::new(),
            Some(raw) => sanitize_operator(raw).unwrap_or_else(|| {
                if !raw.trim().is_empty() {
                    warnings.push(
                        "scallion operator label ignored: it must be printable ASCII, at most 64 bytes".to_string(),
                    );
                }
                String::new()
            }),
        };
        let off = |relay_source: &str, operator: String, warnings: Vec<String>| Resolved {
            participation: Participation::off(),
            operator,
            relay_source: relay_source.to_string(),
            exit_source: "relay participation is off".to_string(),
            warnings,
        };
        if private {
            return off(
                "private mode (a private node advertises nothing)",
                operator,
                warnings,
            );
        }
        let (relay, relay_source) = match env.relay.as_deref() {
            Some(v) => (!is_off(v), RELAY_ENV),
            None => (self.relay, "[scallion] relay"),
        };
        if !relay {
            return off(relay_source, operator, warnings);
        }
        let (exit, exit_source) = match env.exit.as_deref() {
            Some(v) if is_on(v) => (true, EXIT_ENV),
            Some(v) if is_off(v) => (false, EXIT_ENV),
            Some(v) => {
                warnings.push(format!(
                    "{EXIT_ENV}={v} is not 1/true/yes/on or 0/false/no/off: exit stays off"
                ));
                (false, EXIT_ENV)
            }
            None => (self.exit, "[scallion] exit"),
        };
        let relay_only = |exit_source: &str, operator: String, warnings: Vec<String>| Resolved {
            participation: Participation::relay_only(),
            operator,
            relay_source: relay_source.to_string(),
            exit_source: exit_source.to_string(),
            warnings,
        };
        if !exit {
            return relay_only(exit_source, operator, warnings);
        }
        let ports = match env.exit_ports.as_deref() {
            Some(raw) => parse_ports(raw).map_err(|e| format!("{EXIT_PORTS_ENV}: {e}")),
            None => Ok(self.exit_ports.clone()),
        };
        match ports.and_then(|p| Participation::with_exit(p).map_err(|e| e.to_string())) {
            Ok(participation) => Resolved {
                participation,
                operator,
                relay_source: relay_source.to_string(),
                exit_source: exit_source.to_string(),
                warnings,
            },
            Err(e) => {
                warnings.push(format!("scallion exit stays off: {e}"));
                relay_only("invalid exit ports", operator, warnings)
            }
        }
    }
}

/// `resolved` as JSON for status output. Always says nothing is relayed.
pub fn status_json(resolved: &Resolved) -> Value {
    let p = &resolved.participation;
    json!({
        "relay": p.relays(),
        "exit": p.exits(),
        "roles": p.roles().map(role_names).unwrap_or_default(),
        "exit_ports": p.exit_ports(),
        "operator": resolved.operator,
        "relay_source": resolved.relay_source,
        "exit_source": resolved.exit_source,
        "warnings": resolved.warnings,
        "carries_traffic": false,
        "note": NOT_CARRYING,
    })
}

/// One line for text status output.
pub fn status_line(resolved: &Resolved) -> String {
    let p = &resolved.participation;
    match p.roles() {
        None => format!(
            "scallion: not participating ({}); advertises no relay role",
            resolved.relay_source
        ),
        Some(roles) if p.exits() => format!(
            "scallion: advertises {} (exit ports {}); nothing is relayed yet",
            role_names(roles).join(" + "),
            p.exit_ports()
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Some(roles) => format!(
            "scallion: advertises {} (exit off, opt in with `c0mpute scallion exit on`); nothing is relayed yet",
            role_names(roles).join(" + ")
        ),
    }
}

// ---------------------------------------------------------------------------
// This node's descriptor
// ---------------------------------------------------------------------------

struct Current {
    b64: String,
    expires_at_ms: u64,
}

struct Local {
    peer_id: String,
    key: RelayStaticKey,
    participation: Participation,
    operator: String,
    current: Option<Current>,
}

/// What this process advertises, set by [`start`]. `None`: nothing.
static LOCAL: Mutex<Option<Local>> = Mutex::new(None);

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Turn participation on for this worker process: load (or create) the relay
/// static key and remember what to advertise. The capability ad loop then
/// carries the descriptor. Does nothing when `resolved` says not to
/// participate. A key that cannot be read leaves participation off rather
/// than stopping the worker.
pub fn start(net: &Libp2pNetwork, resolved: &Resolved) {
    for w in &resolved.warnings {
        warn!("{w}");
    }
    let mut local = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
    *local = None;
    let Some(roles) = resolved.participation.roles() else {
        info!(reason = %resolved.relay_source, "scallion: not participating; advertising no relay role");
        return;
    };
    let key = match net.scallion_relay_key() {
        Ok(k) => k,
        Err(e) => {
            warn!(err = %format!("{e:#}"), "scallion: relay key unavailable; advertising no relay role");
            return;
        }
    };
    info!(
        roles = ?role_names(roles),
        exit_ports = ?resolved.participation.exit_ports(),
        "scallion: advertising relay roles in the capability ad (advertise only: nothing is relayed yet)"
    );
    *local = Some(Local {
        peer_id: net.peer_id().to_string(),
        key,
        participation: resolved.participation.clone(),
        operator: resolved.operator.clone(),
        current: None,
    });
}

/// This node's signed descriptor for an ad published at `now_ms`, base64.
/// The same one is reused until it nears expiry, then a fresh one is signed.
fn current_descriptor(net: &Libp2pNetwork, now_ms: u64) -> Option<String> {
    let mut guard = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
    let local = guard.as_mut()?;
    // Another network in this process (tests run several) is not this node.
    if local.peer_id != net.peer_id().to_string() {
        return None;
    }
    if let Some(c) = &local.current
        && c.expires_at_ms.saturating_sub(now_ms) > DESCRIPTOR_REFRESH_BEFORE_MS
    {
        return Some(c.b64.clone());
    }
    let descriptor = local.participation.descriptor(
        &local.peer_id,
        local.key.public(),
        &local.operator,
        now_ms,
    )?;
    match net.sign_relay_descriptor(&descriptor) {
        Ok(signed) => {
            let b64 = B64.encode(signed.encode());
            local.current = Some(Current {
                b64: b64.clone(),
                expires_at_ms: descriptor.expires_at_ms,
            });
            debug!(
                expires_at_ms = descriptor.expires_at_ms,
                "scallion: signed a fresh relay descriptor"
            );
            Some(b64)
        }
        Err(e) => {
            warn!(err = %format!("{e:#}"), "scallion: could not sign the relay descriptor");
            None
        }
    }
}

/// The ad field for a signed, encoded descriptor.
pub fn ad_value(encoded: &[u8]) -> Value {
    json!({ "v": 1, "descriptor": B64.encode(encoded) })
}

/// Put this node's descriptor into an outgoing ad's hardware blob, or make
/// sure there is none when the node does not participate.
pub fn attach(hardware: &mut Value, net: &Libp2pNetwork) {
    let Some(obj) = hardware.as_object_mut() else {
        return;
    };
    match current_descriptor(net, unix_now_ms()) {
        Some(b64) => {
            obj.insert(AD_FIELD.to_string(), json!({ "v": 1, "descriptor": b64 }));
        }
        None => {
            obj.remove(AD_FIELD);
        }
    }
}

/// The signed descriptor bytes in an ad's hardware blob, if it carries one.
pub fn descriptor_from_hardware(hardware: &Value) -> Option<Vec<u8>> {
    let raw = hardware.get(AD_FIELD)?.get("descriptor")?.as_str()?;
    (raw.len() <= MAX_DESCRIPTOR_B64)
        .then(|| B64.decode(raw).ok())
        .flatten()
}

/// Feed one received capability ad to the relay directory. A descriptor is
/// kept only if it verifies and names `source`, the peer libp2p says signed
/// the ad. An ad without one withdraws the peer.
pub fn observe(directory: &RwLock<RelayDirectory>, source: &PeerId, hardware: &Value, now_ms: u64) {
    let peer = source.to_string();
    let mut dir = directory.write().unwrap_or_else(|e| e.into_inner());
    if hardware.get(AD_FIELD).is_none() {
        dir.remove(&peer);
        return;
    }
    let Some(encoded) = descriptor_from_hardware(hardware) else {
        debug!(%peer, "scallion: ad carries an unreadable relay descriptor");
        return;
    };
    if let Err(e) = dir.accept(&peer, &encoded, now_ms) {
        debug!(%peer, err = %e, "scallion: relay descriptor refused");
    }
}

/// What this process advertises about itself, for the local listing.
pub fn local_json() -> Option<Value> {
    let guard = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
    let local = guard.as_ref()?;
    Some(json!({
        "peer_id": local.peer_id,
        "roles": local.participation.roles().map(role_names).unwrap_or_default(),
        "exit_ports": local.participation.exit_ports(),
        "operator": local.operator,
    }))
}

/// One directory entry as JSON.
pub fn relay_json(record: &RelayRecord) -> Value {
    let r = &record.relay;
    json!({
        "peer_id": r.peer_id(),
        "roles": role_names(r.roles()),
        "exit_ports": r.exit_ports(),
        "operator": r.operator(),
        "issued_at_ms": r.issued_at_ms(),
        "expires_at_ms": r.expires_at_ms(),
        "last_seen_ms": record.last_seen_ms,
    })
}

/// Gateway route [`RELAYS_PATH`]: the relays this worker has heard advertise
/// (verified descriptors only) and what it advertises itself. Read-only, and
/// everything in it is already public in the capability ads.
pub fn router(registry: crate::capabilities::Registry) -> axum::Router {
    use axum::extract::State;
    use axum::routing::get;
    use axum::{Json, Router};

    async fn relays(State(registry): State<crate::capabilities::Registry>) -> Json<Value> {
        let listed: Vec<Value> = registry.scallion_relays().iter().map(relay_json).collect();
        Json(json!({
            "carries_traffic": false,
            "note": NOT_CARRYING,
            "self": local_json(),
            "relays": listed,
        }))
    }
    Router::new()
        .route(RELAYS_PATH, get(relays))
        .with_state(registry)
}

#[cfg(test)]
mod tests {
    use c0mpute_net::libp2p::identity::Keypair;

    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn env(relay: Option<&str>, exit: Option<&str>, ports: Option<&str>) -> ScallionEnv {
        ScallionEnv {
            relay: relay.map(str::to_string),
            exit: exit.map(str::to_string),
            exit_ports: ports.map(str::to_string),
            operator: None,
        }
    }

    fn signed(id: &Keypair, p: &Participation) -> Vec<u8> {
        let peer = id.public().to_peer_id().to_string();
        p.descriptor(&peer, RelayStaticKey::generate().public(), "", NOW)
            .unwrap()
            .sign(id)
            .unwrap()
            .encode()
    }

    #[test]
    fn defaults_are_relay_on_exit_off() {
        let cfg = ScallionConfig::default();
        assert!(cfg.relay);
        assert!(!cfg.exit);
        assert_eq!(cfg.exit_ports, [80, 443]);
        // A config file that never mentions scallion means the same.
        let parsed: crate::Config = toml::from_str("").unwrap();
        assert_eq!(parsed.scallion, cfg);
        let partial: crate::Config = toml::from_str("[scallion]\n").unwrap();
        assert_eq!(partial.scallion, cfg);

        let r = cfg.resolve(&ScallionEnv::default(), false);
        assert!(r.participation.relays());
        assert!(!r.participation.exits());
        assert!(r.warnings.is_empty());
        let status = status_json(&r);
        assert_eq!(status["roles"], json!(["guard", "middle"]));
        assert_eq!(status["exit"], json!(false));
        assert_eq!(status["carries_traffic"], json!(false));
    }

    #[test]
    fn exit_is_only_ever_an_explicit_opt_in() {
        let cfg = ScallionConfig::default();
        // Config opt-in.
        let on = ScallionConfig {
            exit: true,
            ..cfg.clone()
        };
        let r = on.resolve(&ScallionEnv::default(), false);
        assert!(r.participation.exits());
        assert_eq!(r.participation.exit_ports(), [80, 443]);
        // Env opt-in, for a fleet that does not edit files.
        let r = cfg.resolve(&env(None, Some("1"), None), false);
        assert!(r.participation.exits());
        assert_eq!(r.exit_source, EXIT_ENV);
        let r = cfg.resolve(&env(None, Some("true"), Some("443, 8443")), false);
        assert_eq!(r.participation.exit_ports(), [443, 8443]);
        // Env can force it off over the config.
        assert!(
            !on.resolve(&env(None, Some("0"), None), false)
                .participation
                .exits()
        );
        // A port list alone opts nobody in.
        assert!(
            !cfg.resolve(&env(None, None, Some("80,443")), false)
                .participation
                .exits()
        );
    }

    #[test]
    fn a_doubtful_exit_setting_leaves_exit_off() {
        let on = ScallionConfig {
            exit: true,
            ..ScallionConfig::default()
        };
        // An unrecognized value never turns exit on, even over a config that had it on.
        let r = on.resolve(&env(None, Some("maybe"), None), false);
        assert!(r.participation.relays() && !r.participation.exits());
        assert_eq!(r.warnings.len(), 1);
        // Bad ports: still a relay, not an exit.
        for ports in ["", "0", "80,http", "70000"] {
            let cfg = ScallionConfig {
                exit: true,
                ..ScallionConfig::default()
            };
            let r = cfg.resolve(&env(None, None, Some(ports)), false);
            assert!(r.participation.relays(), "{ports}");
            assert!(!r.participation.exits(), "{ports}");
            assert!(!r.warnings.is_empty(), "{ports}");
        }
        let empty = ScallionConfig {
            exit: true,
            exit_ports: vec![],
            ..ScallionConfig::default()
        };
        assert!(
            !empty
                .resolve(&ScallionEnv::default(), false)
                .participation
                .exits()
        );
    }

    #[test]
    fn participation_off_advertises_nothing() {
        let off = ScallionConfig {
            relay: false,
            exit: true,
            ..ScallionConfig::default()
        };
        let r = off.resolve(&ScallionEnv::default(), false);
        assert!(!r.participation.relays());
        // Exit cannot survive relay being off, however it was asked for.
        assert!(!r.participation.exits());
        assert!(
            !off.resolve(&env(None, Some("1"), None), false)
                .participation
                .exits()
        );
        assert!(
            r.participation
                .descriptor("peer", [1; 32], "", NOW)
                .is_none()
        );
        assert_eq!(status_json(&r)["roles"], json!([]));

        let cfg = ScallionConfig::default();
        assert!(
            !cfg.resolve(&env(Some("0"), Some("1"), None), false)
                .participation
                .relays()
        );
        assert!(
            cfg.resolve(&env(Some("1"), None, None), false)
                .participation
                .relays()
        );
        // Env on beats a config that says off.
        assert!(
            off.resolve(&env(Some("1"), None, None), false)
                .participation
                .relays()
        );
        // Private mode advertises nothing at all.
        let private =
            ScallionConfig { exit: true, ..cfg }.resolve(&env(Some("1"), Some("1"), None), true);
        assert!(!private.participation.relays() && !private.participation.exits());
    }

    #[test]
    fn the_operator_label_is_checked() {
        let cfg = ScallionConfig {
            operator: Some("example operator".into()),
            ..ScallionConfig::default()
        };
        assert_eq!(
            cfg.resolve(&ScallionEnv::default(), false).operator,
            "example operator"
        );
        let from_env = ScallionEnv {
            operator: Some("fleet".into()),
            ..ScallionEnv::default()
        };
        assert_eq!(cfg.resolve(&from_env, false).operator, "fleet");
        let bad = ScallionConfig {
            operator: Some("x".repeat(65)),
            ..ScallionConfig::default()
        };
        let r = bad.resolve(&ScallionEnv::default(), false);
        assert_eq!(r.operator, "");
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn the_config_round_trips_through_toml() {
        let mut cfg = crate::Config::default();
        cfg.scallion.exit = true;
        cfg.scallion.exit_ports = vec![443];
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("[scallion]"), "{text}");
        let back: crate::Config = toml::from_str(&text).unwrap();
        assert_eq!(back.scallion, cfg.scallion);
    }

    #[test]
    fn ads_feed_the_directory_only_with_verified_descriptors() {
        let a = Keypair::generate_ed25519();
        let b = Keypair::generate_ed25519();
        let a_peer = a.public().to_peer_id();
        let b_peer = b.public().to_peer_id();
        let directory = RwLock::new(RelayDirectory::new());

        let a_ad = json!({ "version": "x", AD_FIELD: ad_value(&signed(&a, &Participation::relay_only())) });
        observe(&directory, &a_peer, &a_ad, NOW);
        // B replays A's descriptor in its own ad: refused.
        observe(&directory, &b_peer, &a_ad, NOW);
        // Garbage is refused too.
        observe(
            &directory,
            &b_peer,
            &json!({ AD_FIELD: { "v": 1, "descriptor": "!!!" } }),
            NOW,
        );
        let listed = directory.read().unwrap().list(NOW);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].relay.peer_id(), a_peer.to_string());
        let row = relay_json(&listed[0]);
        assert_eq!(row["roles"], json!(["guard", "middle"]));
        assert_eq!(row["exit_ports"], json!([]));

        let b_ad =
            json!({ AD_FIELD: ad_value(&signed(&b, &Participation::with_exit([443]).unwrap())) });
        observe(&directory, &b_peer, &b_ad, NOW);
        let listed = directory.read().unwrap().list(NOW);
        let exit = listed
            .iter()
            .find(|r| r.relay.peer_id() == b_peer.to_string())
            .unwrap();
        assert_eq!(
            relay_json(exit)["roles"],
            json!(["guard", "middle", "exit"])
        );
        assert_eq!(relay_json(exit)["exit_ports"], json!([443]));

        // An ad with no descriptor withdraws the peer (it turned participation off).
        observe(
            &directory,
            &a_peer,
            &json!({ "version": "x" }),
            NOW + 60_000,
        );
        assert_eq!(directory.read().unwrap().list(NOW + 60_000).len(), 1);
    }

    #[test]
    fn status_text_never_claims_traffic() {
        let cfg = ScallionConfig::default();
        let relay = status_line(&cfg.resolve(&ScallionEnv::default(), false));
        assert!(
            relay.contains("guard + middle") && relay.contains("nothing is relayed yet"),
            "{relay}"
        );
        let exit = status_line(&cfg.resolve(&env(None, Some("1"), None), false));
        assert!(
            exit.contains("exit ports 80,443") && exit.contains("nothing is relayed yet"),
            "{exit}"
        );
        let off = status_line(&cfg.resolve(&env(Some("0"), None, None), false));
        assert!(off.contains("not participating"), "{off}");
        assert!(NOT_CARRYING.contains("not a VPN"));
    }
}
