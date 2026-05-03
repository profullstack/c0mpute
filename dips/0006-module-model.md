---
dip: 0006
title: "Module model: TOML manifest + subprocess dispatch with marketplace-style DX"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion: docs/c0mpute-v1.md
implementation:
supersedes:
superseded-by:
---

## Summary

c0mpute supports installable **modules** — workload providers that the
host CLI (`c0mpute`) can dispatch to. v1 ships with three modules
pre-installed: **coinpay**, **infernet**, **transcode**. The mechanism
also accommodates third-party modules later.

A module is described by a TOML manifest, distributed as a binary or
container image, and invoked by `c0mpute` either as a subprocess
(coinpay, infernet) or as an in-process workload handler (transcode,
which lives in the same Cargo workspace).

The DX/UX intentionally mirrors the b1dz module store and threatcrush
plugin marketplace — marketplace UI, capability tags, install tracking,
per-module reviews — while the underlying mechanism is suited to a Rust
multi-binary stack rather than embedded TS plugins.

## Motivation

We need the right plugin model. Three options bracketed it:

1. **Static, in-tree only.** Every workload type is a hardcoded match in
   `c0mpute`'s code. Cheap; doesn't scale to third-party modules.
2. **Dynamic library plugins (dlopen).** A module is a `.so`/`.dylib`
   loaded at runtime. Powerful, but Rust ABI is unstable, security model
   is hard, and it's overkill for v1.
3. **Manifest + subprocess.** Each module is a separate binary or
   container; `c0mpute` discovers it via a manifest and dispatches via
   subprocess. Cheap to start, scales to third parties, and matches what
   marketplace-style projects (kubectl plugins, Dagger modules,
   threatcrush) do successfully.

We pick #3, with one optimization: tightly-coupled in-tree workloads
(transcode, today) skip the subprocess hop and run as in-process
handlers. The dispatcher is the same either way.

## Detailed design

### Module manifest (`module.toml`)

Borrowed from threatcrush, extended with workload-type fields:

```toml
[module]
id = "transcode"                     # stable, slug-style, unique
name = "FFmpeg Transcoding"
version = "0.1.0"                    # semver
kind = "workload"                    # workload | service | sdk
description = "..."
author = "Profullstack"
license = "MIT"
homepage = "https://c0mpute.com/modules/transcode"

[module.requirements]
c0mpute = ">=0.1.0"                  # host CLI version range
os = ["linux", "darwin"]
arch = ["x86_64", "aarch64"]
capabilities = [
  "ffmpeg",                          # external binary needed on PATH
  "gpu:nvidia?",                     # optional capability (not required)
]

[module.workloads]
# Each workload type this module handles. Used by c0mpute's dispatcher.
"ffmpeg.transcode" = { command = "transcode", validation = "ffprobe" }

[module.dispatch]
# How c0mpute should invoke this module
mode = "in-process"                  # in-process | subprocess | container
# subprocess example:
#   mode = "subprocess"
#   binary = "c0mpute-transcode"     # discovered on PATH
# container example:
#   mode = "container"
#   image = "ghcr.io/c0mpute/ffmpeg-runner@sha256:..."

[module.config.defaults]
preset = "video-1080p"
```

### Built-in modules at v1

| ID         | Kind        | Dispatch       | Notes                                 |
|------------|-------------|----------------|---------------------------------------|
| transcode  | workload    | in-process     | FFmpeg, ships in the c0mpute binary   |
| coinpay    | service     | subprocess     | separate `coinpay` binary on PATH     |
| infernet   | workload    | subprocess     | separate `infernet` binary on PATH    |

The transcode module is in-process for v1 because it shares so much code
with c0mpute-core (job manifest, content addressing) that a subprocess
boundary would mean serializing those types over stdio for no benefit.
A future signed-third-party transcode runner would use `mode = "container"`.

### Module distribution — explicitly NOT npm

Modules are distributed as one of:

1. **Static binary tarballs** signed with minisign, served from
   `https://c0mpute.com/modules/<id>/<version>/<artifact>.tar.gz`.
   Same shape as the c0mpute / coinpay / infernet release artifacts.
2. **OCI container images** with a pinned digest, for `mode = "container"`
   modules.
3. **In-tree** for first-party modules compiled into the c0mpute binary
   itself (transcode today).

Modules are **not** published to npm. The Bun-based dashboard /
coordinator code in this repo uses `bun install` against private
workspace packages (`@c0mpute/*`, all `private: true`); we don't ship
public npm packages, and we don't want a 300-package marketplace shadow
on npm to reason about. The marketplace lives at `c0mpute.com/modules`,
served from our own infrastructure.

If a future module *does* want to ship a JavaScript SDK, the SDK lives
inside the module's tarball (and gets `bun install`-ed from a local
path or git URL) — not as a public npm package.

This decision flows from the broader project distribution model:
`curl https://c0mpute.com/install.sh | sh` is the canonical install
path, and the binaries self-update against the same release feed.
There is no Node toolchain on a worker box; there is no point in
pretending modules are npm packages just because some of them might
contain JS.

### Module discovery

Order of search:

1. Built-in registry (statically compiled into `c0mpute`): transcode +
   any future first-party in-tree modules.
2. `~/.config/c0mpute/modules/<id>/module.toml` — user-installed.
3. `/etc/c0mpute/modules/<id>/module.toml` — system-installed.

`c0mpute modules list` shows everything found, with status per source.

### Module invocation

For `mode = "in-process"`: the workload handler is registered in c0mpute
at compile time and called directly.

For `mode = "subprocess"`:

```
<binary> handle-job
  --manifest /path/to/job.json   # JSON-encoded job manifest
  --input-dir /path/to/inputs
  --output-dir /path/to/outputs
  --receipt-fd 3                 # write receipt JSON here
```

The module reads the job manifest from the provided path, runs its
workload, writes outputs into `output-dir`, and writes a signed receipt
JSON to fd 3. Exit codes:

- `0` — job complete; receipt written.
- `64-79` — job failed in a normal way (validation error, timeout, etc.).
  The receipt fd should still receive an error receipt.
- Anything else — module crashed; coordinator retries on a fresh worker.

For `mode = "container"`: same protocol but inside an OCI container with
the manifest bind-mounted at `/job/manifest.json`, etc.

### Marketplace UX (web dashboard)

Mirroring threatcrush's `/store` page:

- `c0mpute.com/modules` — paginated list, search by name/keywords/tags,
  category filter, sort by popular/newest/top-rated.
- `c0mpute.com/modules/<slug>` — detail page with description (Markdown),
  versions, reviews, install command, capability tags, pricing.
- "Install" button copies `c0mpute modules install <slug>` to clipboard.

Mirroring b1dz's per-user `installed_plugins`:

- Postgres `module_installs` table — anonymous install analytics
  (per-version, per-platform), like threatcrush.
- Postgres `user_modules` table — per-user enabled modules + config +
  optional `paid_until` for paid modules later.
- Per-module config form on the dashboard (typed fields: string, number,
  bool, secret) reads `[module.config.schema]` from `module.toml`.

### Module submission UX

`c0mpute modules submit <manifest.toml>` — uploads a manifest to
`c0mpute.com` for review. The web has a form too; both call the same
`POST /api/modules` endpoint. From threatcrush, copy:

- `POST /api/modules/fetch-meta` to auto-populate from a GitHub URL
  (README, logo, version).
- One-review-per-email upsert pattern (`POST /api/modules/<slug>/reviews`).
- Anonymous install tracking via POST (not GET) to avoid prefetch noise.

### Trust boundary

Modules **do not** receive long-lived credentials. The host (c0mpute)
holds escrow tokens through CoinPay and signs receipts with the
operator's DID. A module receives only what's in its job manifest:
inputs (or signed-URL pointers), runtime config, expected output schema.
Modules sign their part of the receipt with a per-execution ephemeral
key derived from CoinPay; that signature is what gets aggregated upstream.

This means a buggy or malicious module can fail a job, but cannot drain
escrow or impersonate the operator across jobs.

## Alternatives considered

**Static in-tree only.** See option 1 above. Acceptable for v1 if we
don't believe in third-party modules. We do, so we want the manifest
contract from day one even if 90% of v1 modules are in-tree.

**dlopen plugins.** Rust ABI instability + security cost + tooling
complexity. Not worth it; subprocess is plenty fast for these workload
sizes.

**WASM modules.** Genuinely interesting (sandbox-by-design, language-
agnostic) but the ecosystem isn't ready for FFmpeg-grade workloads, and
GPU access from WASM is a non-starter today. Revisit in 1–2 years.

**Marketplace as a centralized service forever.** v1 yes. Long term we
want module manifests to be content-addressed and discoverable without
c0mpute.com being a single point of failure. Out of scope here.

## Migration & rollout

Greenfield. Land in this order:

1. `module.toml` schema + parser in `c0mpute-core`.
2. In-process workload handler trait + transcode handler (existing
   `c0mpute-transcode` code adapted to it).
3. Subprocess dispatch path + the contract spec above.
4. `c0mpute modules list` / `install` / `enable` / `disable`.
5. Web `/modules` catalog page (Phase 2).
6. Web submission flow (Phase 3).

## Open questions

- **Module signing.** threatcrush's roadmap mentions module signing —
  we should require it for paid modules from day one and make it
  optional but recommended for free ones. Defer concrete signing scheme
  to a follow-up DIP.
- **Dependencies between modules.** A module declaring `requires =
  ["coinpay >= 0.1"]` is plausible. Probably not v1.
- **Hot reload.** Should `c0mpute modules install <slug>` take effect
  without restarting a running worker? Probably yes for `kind = "workload"`,
  no for `kind = "service"`.

## Out of scope

- Module *runtime* sandboxing for in-process modules. The same security
  model as c0mpute itself applies; if you're running a third-party
  in-process module you're trusting their code.
- Module-to-module RPC. v1 modules talk through c0mpute's job manifest
  contract, not directly to each other.
