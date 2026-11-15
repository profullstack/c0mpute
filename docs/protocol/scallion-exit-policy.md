# Scallion Network: safe exit-admission primitive

**Status:** pre-transport security foundation. This is **not a working
Internet proxy, a VPN, an onion network, or an anonymity guarantee**.

`c0mpute_net::scallion_exit` defines a conservative, deny-by-default
destination policy for a future opt-in Scallion exit role. Ordinary
c0mpute compute workers do not become exits by virtue of running a node.

## Contract

- Exit traffic is **disabled by default**; no destination is allowed
  without explicit operator consent and an allowlist of TCP ports.
- Resolve the requested host *once* using a trusted, bounded DNS
  resolver, then pass **all** DNS A/AAAA addresses to
  `ExitPolicy::approve(host, port, resolved_ips)`.
- Refuse the entire DNS result if any answer is private, loopback,
  link-local, shared, reserved, documentation, multicast or a known
  transition range. Never select a good-looking address from a result
  that contains a bad one.
- Refuse local and ambiguous hostnames, Unicode that was not
  IDNA-normalized to ASCII, numeric IP shorthand, userinfo, slashes
  and zone identifiers.
- Use `ApprovedDestination::socket_addrs()` to dial the IP address
  **without re-resolving its hostname**. After connecting, call
  `check_connected(peer_addr)` and fail if the socket peer differs
  from the preapproved IP/port.
- Preserve the original requested hostname for HTTPS SNI and
  certificate verification.

Example only (not wired into a proxy):

```rust
use c0mpute_net::scallion_exit::ExitPolicy;
use std::net::IpAddr;

// A future exit operator must consciously enable this.
let policy = ExitPolicy::opt_in([443])?;
// Collect all A/AAAA results with a bounded, trustworthy resolver.
let records: Vec<IpAddr> = vec!["1.1.1.1".parse()?];
let approved = policy.approve("example.com", 443, &records)?;
for socket in approved.socket_addrs() {
    // A future runtime would connect to this *numeric* SocketAddr.
    // It must never perform a second name-based resolution.
    println!("permitted endpoint: {socket}");
}
```

## What this does not protect

- The code is not yet used by a live exit service. No SOCKS5/TUN
  listener, routing proxy, onion encryption, key exchange or traffic
  relay is implemented here.
- DNS answers themselves may be malicious or stale; the caller must
  use an independently verified resolver, enforce query timeouts and
  filter all addresses. A target may change routing after connect.
- Allowlists of TCP ports and public IP ranges are only the first layer
  of abuse prevention. A production exit needs per-client quotas,
  connection/byte/time limits, target/ASN policies, verified
  operator consent, logging appropriate to the risk, and legal review.
- Anonymity requires a separate audited encrypted multihop protocol;
  the libp2p NAT traversal relay by itself is not a privacy circuit.
- Applicable sanctions, export controls or service-specific identity
  checks are **not** bypassed by the presence of this network layer.

## Tests

```sh
cargo test -p c0mpute-net scallion_exit
```

Unit tests check default-deny behavior, explicit ports, private and
special-use IPv4/IPv6 (including mapped and transition addresses),
local and malformed names, mixed public/private DNS results, DNS
answer limits, HTTPS-target hostnames, approved socket pinning and
rejected rebinding. Live integrated exit tests will be required once
an actual exit service exists.

See [Scallion Network](scallion-network.md) for the overall roadmap.
