---
dip: 0002
title: "Single binary with product-line subcommands (superseded)"
status: Superseded
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation:
supersedes:
superseded-by: 0005
---

> **Superseded by [DIP-0005](0005-c0mpute-rebrand.md).** This DIP proposed
> a single CLI binary with product-line subcommands. The c0mpute rebrand
> moved to **three peer binaries** instead — `c0mpute`, `coinpay`,
> `infernet` — installed together by `c0mpute.com/install.sh`. The
> `c0mpute` umbrella delegates to peer CLIs via subprocess. Kept on disk
> as historical record.
