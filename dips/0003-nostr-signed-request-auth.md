---
dip: 0003
title: "Nostr-keyed signed-request auth (superseded)"
status: Superseded
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation:
supersedes:
superseded-by: 0007
---

> **Superseded by [DIP-0007](0007-coinpay-did-identity.md).** This DIP
> proposed Schnorr / NIP-07 signed-request envelopes as the auth layer.
> The c0mpute rebrand made CoinPay DID the canonical identity layer; the
> wire format (canonical request string + replay-protection mechanics)
> carries forward, but identities are now `did:coinpay:...` rather than
> raw nostr pubkeys. Kept on disk as historical record.
