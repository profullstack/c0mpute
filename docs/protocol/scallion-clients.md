# Scallion Network: native and OpenVPN client modes

**Status:** client compatibility foundations, not a running VPN.
The current `c0mpute` binary does **not** redirect traffic, run a
layered-encrypted Scallion circuit, provide a local TUN device, run an
OpenVPN server, or offer a downloadable production `.ovpn` file.

## Client modes

| Mode | Client | Infrastructure | Current status |
|---|---|---|---|
| `native` | c0mpute CLI, future desktop/mobile apps | Scallion TUN -> authenticated 3-hop circuits -> opt-in exit | Not implemented |
| `openvpn` | OpenVPN Connect on supported mobile and desktop OSes | Optional OpenVPN-protocol entry gateway -> same Scallion 3-hop circuit -> opt-in exit | Not implemented |

**The modes are independent.** A native client does not need OpenVPN.
An OpenVPN Connect client does need a real OpenVPN-compatible entry
gateway; a libp2p peer cannot accept a `.ovpn` connection directly.
No Scallion node becomes an OpenVPN gateway unless its operator opts
in. Neither mode is allowed to silently fall back to direct egress or
to the other mode.

## Diagnostics, not a tunnel

```sh
c0mpute scallion modes
c0mpute scallion modes --json
c0mpute scallion doctor --mode native
c0mpute scallion doctor --mode openvpn --json
```

These commands report **not ready** until prerequisites actually exist.
They do not start a VPN, expose listeners, reconfigure routes or claim
that basic libp2p NAT traversal relays provide anonymity.

A mode must not claim readiness merely because some remote provider
advertises the right capabilities. The local runtime must establish
the data-plane implementation, opt-in exits, DNS leak handling and
no-direct-fallback guarantees, successful multi-node adversarial
integration tests and independent security review.

## OpenVPN profile export primitive

`c0mpute_net::scallion_clients::create_openvpn_profile` formats a
standard OpenVPN client configuration for a **previously provisioned,
trusted** OpenVPN-protocol server. The future gateway-provisioning
component will provide its IPv4/FQDN, port, TCP/UDP setting, expected
TLS certificate name and pinned CA certificate. The builder:

- requires nonzero port, a safe public DNS/IPv4 host, a validated
  certificate name and basic single-block PEM syntax;
- prevents line injection into OpenVPN directives;
- adds server certificate EKU checks and certificate-name validation;
- uses OpenVPN's interactive `auth-user-pass` instead of embedding
  client secrets or private keys, and `setenv CLIENT_CERT 0` as required
  by OpenVPN Connect for cert-free password-authentication profiles;
- does **not** install system-wide routes, bypass DNS configuration,
  enable gateway access or connect to a destination.

It currently performs **PEM syntax validation only**, not X.509
chain validation. Only an authenticated, independently verified
provisioner may supply a trusted CA. Export should not be exposed as
a public CLI command until the OpenVPN gateway, TLS provisioning and
end-to-end tests are in place. The generator is also intentionally
conservative about hosts: it supports public IPv4 literals and ASCII
FQDNs, not IPv6 literals, pending proper validation and testing.

Generating a `.ovpn` profile is **not** equivalent to providing a
working OpenVPN endpoint. If any required component is missing,
the eventual client must refuse to connect.

## Required implementation and release gates

1. Build and independently review a real layered-encrypted three-hop
   transport; libp2p relay v2 is only NAT traversal.
2. Integrate measured route ranking, sticky sessions, validated peer
   diversity and independently consented exits.
3. Build the native Scallion TUN client with platform-specific
   routes/DNS/IPv6 and a mandatory leak-blocking fail-closed behavior.
4. Separately build the opt-in OpenVPN-protocol gateway, with authenticated
   credentials, certificate provisioning and bounded resource limits.
5. Produce signed, *working* `.ovpn` profiles only for provisioned
   gateways with a verifiable TLS identity. Import using stock OpenVPN
   Connect on desktop and phones.
6. Test normal traffic and forced node/exit failures against controlled
   destinations; verify DNS/IP leak protection, failover, session
   continuity, connection teardown and operator abuse controls.
7. Require an independent security review before advertising anonymity
   or deploying to untrusted public network clients.

Scallion must not be used to defeat export controls, sanctions or
identity checks required by regulated services. Participation in a
permissionless network is distinct from permission to access any
specific GPU or remote compute service.

See [Scallion architecture](scallion-network.md) and
[exit policy](scallion-exit-policy.md). Tracks #386 and #389.
