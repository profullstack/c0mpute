//! Scallion client interoperability: two independent client surfaces.
//!
//! Native Scallion and OpenVPN Connect are *alternative* entry transports.
//! Neither is operational merely because a client can select its mode.
//! This file defines capability gating and a hardened OpenVPN profile
//! generator. It does not run an OpenVPN server or forward traffic.
//!
//! The live client must fail closed, without fallback to direct Internet
//! access or from one client mode to another, until audited multipath
//! encryption, DNS leak handling and exit enforcement are integrated.
//!
//! See docs/protocol/scallion-clients.md.

use std::net::Ipv4Addr;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMode {
    /// c0mpute's eventual native Scallion TUN client.
    Native,
    /// Standard OpenVPN clients using an optional opt-in gateway.
    OpenVpn,
}

impl ClientMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::OpenVpn => "openvpn",
        }
    }
}

/// Evidence verified by the *local* runtime, NOT a remote peer's claims.
/// Advertising these boolean flags over libp2p does not prove audit status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifiedComponents {
    pub encrypted_multihop_transport: bool,
    pub opt_in_exit_policy: bool,
    pub protected_dns_and_no_direct_fallback: bool,
    pub adversarial_e2e_tests: bool,
    pub independent_security_review: bool,
    pub native_tun_adapter: bool,
    pub openvpn_gateway_adapter: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModeReadiness {
    pub mode: &'static str,
    pub ready: bool,
    /// Unsatisfied release/connection prerequisites, no silent fallback.
    pub blockers: Vec<&'static str>,
}

pub fn readiness(mode: ClientMode, components: VerifiedComponents) -> ModeReadiness {
    let mut blockers = Vec::new();
    if !components.encrypted_multihop_transport {
        blockers.push("audited layered-encrypted multi-hop transport is not implemented");
    }
    if !components.opt_in_exit_policy {
        blockers.push("consenting exit enforcement is not connected to the data plane");
    }
    if !components.protected_dns_and_no_direct_fallback {
        blockers.push("DNS, IPv4/IPv6 and fail-closed leak handling are not verified");
    }
    if !components.adversarial_e2e_tests {
        blockers.push("multi-node adversarial end-to-end tests have not passed");
    }
    if !components.independent_security_review {
        blockers.push("independent security review is outstanding");
    }
    match mode {
        ClientMode::Native if !components.native_tun_adapter => {
            blockers.push("native TUN client transport is not implemented");
        }
        ClientMode::OpenVpn if !components.openvpn_gateway_adapter => {
            blockers.push("opt-in OpenVPN protocol entry gateway is not implemented");
        }
        _ => {}
    }
    ModeReadiness {
        mode: mode.as_str(),
        ready: blockers.is_empty(),
        blockers,
    }
}

/// On today's codebase neither client mode is network-ready. This function
/// intentionally does not read environment toggles or unauthenticated
/// network capabilities to claim readiness.
pub fn current_readiness(mode: ClientMode) -> ModeReadiness {
    readiness(mode, VerifiedComponents::default())
}

/// Builder input for a standard OpenVPN client profile. A future
/// authenticated gateway provisioning service must provide this information.
/// A profile cannot establish the Scallion network unless that gateway exists.
#[derive(Clone, Debug)]
pub struct OpenVpnGatewayProfile {
    pub host: String,
    pub port: u16,
    pub transport: OpenVpnTransport,
    /// Name in a certificate issued by the pinned CA.
    pub expected_server_name: String,
    /// Trusted PEM root certificate provided out of band by the operator.
    pub ca_certificate_pem: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenVpnTransport {
    Udp,
    Tcp,
}

/// Generate a basic OpenVPN Connect-compatible client profile for a
/// *provisioned, authenticated* gateway. No private key, access token or
/// password is embedded. The server must support auth-user-pass itself.
///
/// Deliberately do not add 'redirect-gateway': a profile alone cannot
/// establish a device-wide fail-closed kill switch or DNS-leak protection.
/// Full-tunnel operation must be enabled only after OS-specific E2E checks.
pub fn create_openvpn_profile(gateway: &OpenVpnGatewayProfile) -> Result<String> {
    let host = validate_host(&gateway.host)?;
    ensure!(gateway.port != 0, "OpenVPN port must be non-zero");
    let expected = validate_cert_name(&gateway.expected_server_name)?;
    let ca = validate_single_pem_certificate(&gateway.ca_certificate_pem)?;
    let proto = match gateway.transport {
        OpenVpnTransport::Udp => "udp",
        OpenVpnTransport::Tcp => "tcp-client",
    };
    Ok(format!(
        "client\ndev tun\nproto {proto}\nremote {host} {port}\nresolv-retry infinite\nnobind\npersist-key\npersist-tun\nremote-cert-tls server\nverify-x509-name {expected} name\ntls-version-min 1.2\nauth-user-pass\nauth-nocache\nverb 3\n<ca>\n{ca}\n</ca>\n",
        port = gateway.port,
    ))
}

fn validate_host(input: &str) -> Result<&str> {
    ensure!(!input.is_empty() && input.len() <= 253, "invalid OpenVPN gateway hostname length");
    ensure!(input.is_ascii(), "gateway name must be DNS ASCII (IDNA-encoded)");
    ensure!(!input.ends_with('.') && !input.starts_with('-'), "invalid gateway name");
    if let Ok(ip) = input.parse::<Ipv4Addr>() {
        ensure!(crate::scallion_exit::public_ip(ip.into()), "gateway must not use reserved IPv4 address");
        return Ok(input);
    }
    let labels: Vec<_> = input.split('.').collect();
    ensure!(labels.len() >= 2, "gateway must be a fully qualified domain");
    ensure!(labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && label.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
            && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }), "invalid gateway domain");
    let tld = labels[labels.len() - 1];
    ensure!(tld.len() >= 2 && tld.bytes().any(|byte| byte.is_ascii_alphabetic()), "invalid domain suffix");
    ensure!(!["local", "localhost", "internal", "lan", "home", "test", "invalid", "onion"].contains(&tld), "local or nonpublic gateway address");
    Ok(input)
}

fn validate_cert_name(input: &str) -> Result<&str> {
    ensure!(!input.is_empty() && input.len() <= 253, "invalid certificate name");
    ensure!(
        input.is_ascii()
            && input.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')),
        "certificate name has unsafe characters"
    );
    ensure!(!input.starts_with('.') && !input.ends_with('.'), "invalid certificate name");
    Ok(input)
}

fn validate_single_pem_certificate(pem: &str) -> Result<String> {
    let pem = pem.replace("\r\n", "\n");
    let lines: Vec<_> = pem.trim().lines().collect();
    ensure!(lines.len() >= 3, "CA certificate must contain a PEM block");
    ensure!(lines.first() == Some(&"-----BEGIN CERTIFICATE-----"), "CA PEM header missing");
    ensure!(lines.last() == Some(&"-----END CERTIFICATE-----"), "CA PEM footer missing");
    let body = &lines[1..lines.len() - 1];
    ensure!(body.iter().all(|line| {
        !line.is_empty()
            && line.len() <= 128
            && line.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    }), "CA PEM contains invalid or injected content");
    let encoded_len: usize = body.iter().map(|line| line.len()).sum();
    ensure!((64..=64_000).contains(&encoded_len), "CA PEM payload outside accepted limits");
    // A production certificate provisioner must also parse X.509 and
    // validate the root's identity independently of this syntax check.
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PEM: &str = "-----BEGIN CERTIFICATE-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n-----END CERTIFICATE-----";

    fn demo_profile() -> OpenVpnGatewayProfile {
        OpenVpnGatewayProfile {
            host: "entry.example.org".into(),
            port: 1194,
            transport: OpenVpnTransport::Udp,
            expected_server_name: "entry.example.org".into(),
            ca_certificate_pem: SAMPLE_PEM.into(),
        }
    }

    #[test]
    fn default_readiness_is_closed_for_both_modes() {
        for mode in [ClientMode::Native, ClientMode::OpenVpn] {
            let status = current_readiness(mode);
            assert!(!status.ready);
            assert!(!status.blockers.is_empty());
        }
    }

    #[test]
    fn each_mode_has_independent_prerequisites_no_automatic_fallback() {
        let shared = VerifiedComponents {
            encrypted_multihop_transport: true,
            opt_in_exit_policy: true,
            protected_dns_and_no_direct_fallback: true,
            adversarial_e2e_tests: true,
            independent_security_review: true,
            native_tun_adapter: true,
            openvpn_gateway_adapter: false,
        };
        assert!(readiness(ClientMode::Native, shared).ready);
        assert!(!readiness(ClientMode::OpenVpn, shared).ready);
        let reverse = VerifiedComponents {
            native_tun_adapter: false,
            openvpn_gateway_adapter: true,
            ..shared
        };
        assert!(readiness(ClientMode::OpenVpn, reverse).ready);
        assert!(!readiness(ClientMode::Native, reverse).ready);
    }

    #[test]
    fn profile_uses_tls_verification_and_prompts_for_credentials() {
        let profile = create_openvpn_profile(&demo_profile()).unwrap();
        assert!(profile.contains("remote entry.example.org 1194"));
        assert!(profile.contains("proto udp"));
        assert!(profile.contains("remote-cert-tls server"));
        assert!(profile.contains("verify-x509-name entry.example.org name"));
        assert!(profile.contains("auth-user-pass\n"));
        assert!(profile.contains("<ca>\n-----BEGIN CERTIFICATE-----\n"));
        assert!(!profile.contains("redirect-gateway"));
        assert!(!profile.contains("script-security"));
    }

    #[test]
    fn rejects_untrusted_gateway_hosts_and_certificate_injection() {
        for host in [
            "localhost", "10.0.0.1", "127.0.0.1", "169.254.169.254",
            "entry.example.org\nscript-security 3",
            "entry.example.org;evil", "entry.example.org/../../", "entry.example.org ",
            "a..example.org", "entry.local", "0x7f000001", "2130706433",
        ] {
            let mut cfg = demo_profile();
            cfg.host = host.into();
            assert!(create_openvpn_profile(&cfg).is_err(), "accepted host {host}");
        }
        let mut cfg = demo_profile();
        cfg.ca_certificate_pem = SAMPLE_PEM.replace(
            "-----END CERTIFICATE-----",
            "</ca>\nscript-security 3\n-----END CERTIFICATE-----",
        );
        assert!(create_openvpn_profile(&cfg).is_err());
        let mut cfg = demo_profile();
        cfg.expected_server_name = "entry.example.org\nsetenv DEBUG YES".into();
        assert!(create_openvpn_profile(&cfg).is_err());
    }

    #[test]
    fn cannot_export_profile_with_missing_server_identity() {
        let mut cfg = demo_profile();
        cfg.expected_server_name.clear();
        assert!(create_openvpn_profile(&cfg).is_err());
        cfg = demo_profile();
        cfg.port = 0;
        assert!(create_openvpn_profile(&cfg).is_err());
        cfg = demo_profile();
        cfg.ca_certificate_pem = "NO CERTIFICATE".into();
        assert!(create_openvpn_profile(&cfg).is_err());
    }

    #[test]
    fn tcp_adapter_profile_is_explicit() {
        let mut cfg = demo_profile();
        cfg.transport = OpenVpnTransport::Tcp;
        let profile = create_openvpn_profile(&cfg).unwrap();
        assert!(profile.contains("proto tcp-client"));
    }
}
