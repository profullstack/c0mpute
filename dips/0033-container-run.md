---
dip: 0033
title: "container.run: builds and container jobs, results back, no git"
status: Review
authors:
  - riotcoder@profullstack.com
created: 2026-11-11
updated: 2026-11-11
discussion: <PR link>
implementation: <PR link, set when status=Final>
---

# DIP 0033: `container.run`

## What

Offload a `docker build`, or one command in a container, to another machine
on the network and get the result back. The input is a plain tarball of any
directory: no git remote, no registry push, no CI service.

It implements the "OCI image digest + command + input blob + output path"
shape that `docs/c0mpute-v1.md` (Security and Sandboxing) asks public jobs to
take, and adds the build step that shape needs to be useful for deploys.

Motivation: dev2 builds every fleet site's image on itself, and build scratch
(Rust `target/`, Next caches, BuildKit cache) kept pushing its disk under
1 GiB free on 2026-11-11. The same machine has idle big-disk peers (vienna,
dev3) on the network. Apps (DIP 0031) already build images, but on the
owner's machine; git hosting Actions only deploy. Nothing ran a build
elsewhere and returned it.

## Workload

Gossip topic `c0mpute/jobs/container.run`, role `containers` (opt-in, never a
default), capability tag `c0mpute:role:containers`, advertised only once
`docker info` and `docker buildx version` answer.

### Spec v1 (sealed to the winner, DIP-0032)

```json
{
  "v": 1,
  "mode": "build" | "run",
  "build": { "dockerfile": "Dockerfile", "build_args": {"K": "V"}, "target": "runner" },
  "image": "name@sha256:<64 hex>",
  "cmd": ["sh", "-c", "make test"],
  "env": {"K": "V"},
  "workdir": "/work",
  "outputs": ["dist", "/tmp/report.xml"],
  "return_image": false,
  "cpus": 2, "memory_mb": 4096, "disk_mb": 20480, "timeout_secs": 1800,
  "network": true,
  "context_hash": "...", "context_size": 0, "context_pieces": ["..."], "context_key": "...", "context_provider_peer_id": "...",
  "input_hash": "...",   "input_size": 0,   "input_pieces": ["..."],   "input_key": "...",   "input_provider_peer_id": "..."
}
```

- `mode: "build"`: `build` + `context_*`; no `image`, `cmd` or `outputs`.
  Returns the image.
- `mode: "run"`: `cmd` plus exactly one of `image` (digest-pinned, never a
  floating tag) or `build` (+ `context_*`: build first, run in the result;
  `return_image` also returns it). `input_*` is copied into `workdir` before
  the command; `outputs` are copied out after it, also when it failed (test
  reports).
- Blobs use the `crate::pieces` field names (1 MiB pieces, whole-file hash,
  sealed under `<prefix>_key`); the provider is the buyer. Context and input:
  up to 2 GiB each.
- Limits: cpus 0.1-64 (default 2), memory 64 MiB-256 GiB (4 GiB), disk
  64 MiB-1 TiB (20 GiB), timeout 10 s-6 h (30 min). Workers cap them lower
  (`[containers]`).
- Strings are bounded and shaped so none can become a docker flag: build arg
  and env keys are `[A-Za-z_][A-Za-z0-9_]*`, the Dockerfile path is relative
  without `..`, the target is `[A-Za-z0-9_.-]`, every value goes in a
  `--flag=value` argument, the command follows the image.

Public view (what bidders see): `v`, `mode`, `cpus`, `memory_mb`, `disk_mb`,
`timeout_secs`, `network`. The Dockerfile path, args, env, command, image
and every blob stay sealed.

### Result

```json
{
  "v": 1, "ok": true, "mode": "build", "duration_ms": 61234,
  "error": "build failed (exit 1)",
  "build_log_tail": "...", "log_tail": "...", "exit_code": 0, "oom_killed": false,
  "image":   { "id": "sha256:...", "size": 1, "hash": "<blake3>", "stored": "<root>", "format": "docker-save+gzip" },
  "outputs": [ { "path": "dist", "size": 1, "hash": "<blake3>", "stored": "<root>", "format": "tar+gzip" },
               { "path": "missing", "missing": true, "error": "..." } ],
  "egress_guard": { "build": false, "run": true },
  "worker": { "docker": "29.1.3" }
}
```

A failed build or a non-zero exit is a Completed receipt with `ok: false`, the
reason and the log tails (16 KiB per stage), so the requester sees why. A
Failed receipt means the worker itself broke. Each artifact is stored sealed
with `publish_result_file` (pieces + manifest); the buyer fetches `stored`,
opens it with the job's result key and checks `size` and `hash` against the
plaintext. `image.id` is the image config digest: the value N-of-M checks
compare. Result JSON at most 1 MiB; image at most 4 GiB; each output at most
2 GiB; at most 16 outputs.

### Live log

`c0mpute.container.log.v1` (request-response): `{job_id, from}` ->
`{known, stage, text, next, done}`. Only the job's buyer (authenticated peer
id) gets an answer; the worker keeps the last 512 KiB of each job's
build/run output for an hour. `c0mpute build` and `c0mpute run` print it as
it arrives.

## Worker

- `docker buildx build --builder default --progress=plain --load` with the
  context on stdin, so the daemon's own BuildKit builds. No docker-container
  builder: buildx starts that one `--privileged`, and the rootless image
  needs unprivileged user namespaces, which Ubuntu 24.04+ hosts refuse by
  default. Never `--allow` an entitlement (`security.insecure`,
  `network.host`), never `--secret` or `--ssh`.
- The image goes out with `docker save | gzip` (streamed, capped) and the
  job's tag is removed. The BuildKit cache is pruned to `cache_gb` after
  every build (`docker builder prune --max-used-space`, falling back to
  `--keep-storage`).
- A run step is `docker create` with `--cpus`, `--memory` = `--memory-swap`,
  `--pids-limit 4096`, `--cap-drop ALL` plus the eight capabilities package
  managers need, `--security-opt no-new-privileges`, no volumes or mounts,
  then `docker cp -` for the input, `docker start --attach`, `docker cp
  <path> -` for each output. The container is removed after the job, and the
  image too when the job pulled it.
- Network: run steps use the `c0mpute-jobs` bridge (`c0jobs0`, inter-container
  traffic off), or `none`. With `egress_guard` (default) and root, iptables
  drops traffic from that bridge to 0/8, 10/8, 100.64/10, 127/8, 169.254/16
  (cloud metadata), 172.16/12, 192.0.0/24, 192.168/16, 198.18/15, 224/4 and
  240/4 in `DOCKER-USER`, and all of it in `INPUT` (the host's own
  services). Rules are re-checked before every run. Builds go through the
  daemon's default bridge, which the guard covers only when the operator opts
  in with `guard_build_bridge = "docker0"` (it is shared with every
  default-network container on the host). Each result says which guards were
  up.
- Disk: per-job scratch under `work_dir` (put it on the big disk). Every
  5 s a watchdog stops the job when its scratch passes `disk_mb` or the
  scratch or docker disk goes under `min_free_gb` (default min(20 GiB, 10%));
  bids need `disk_mb` of headroom above that floor. Scratch is removed after
  every job; result pieces are dropped after 6 hours.
- Timeout: one deadline for the whole job; the process is killed (killing the
  buildx client cancels the build) and the container removed.
- Jobs run on the role's own slots (`max_jobs`, default 1).

## Trust

Both directions are closed by default.

- **Builder protects itself:** a `containers` node bids only for buyers it
  trusts: the identities on its own machine plus `[containers] buyers`, by
  signed gossip publisher (the same check as private mode). `open = true`
  serves anyone. A build runs the buyer's Dockerfile with network access;
  BuildKit's default sandbox (no privileges, default seccomp/AppArmor) is
  what stands between it and the host.
- **Buyer protects itself:** `c0mpute build` / `run` and the jobs bridge
  send jobs only to named builders (`--peer`, `[containers] builders`,
  `C0MPUTE_BUILDERS`, `allow_peers` / `C0MPUTE_JOBS_ALLOW_PEERS`) unless
  told `--any-worker`. The receipt hash proves which builder sent the bytes,
  not that it built them honestly: an untrusted builder can return any image
  it likes, with a backdoor in it, and the buyer would deploy it. That is a
  supply-chain risk on the order of trusting a CI provider, so deploys use
  the operator's own nodes.
- **Hook for untrusted builders:** `container_run::verify_reproducible(ids,
  n)` accepts an image id only when `n` builders agree. Run the same build on
  M builders and require N to match; this needs reproducible Dockerfiles
  (digest-pinned bases, `SOURCE_DATE_EPOCH`, locked dependencies) before ids
  agree across machines. Not wired into the CLI yet.
- Confidentiality: specs and blobs are sealed to the winner (DIP-0032), so
  relays and losing bidders see only the public view. The winner sees the
  source and every build arg: keep secrets out of build args and env (the
  deploy integration passes only public `NEXT_PUBLIC_*`-style values).

## Surfaces

- CLI: `c0mpute build <dir|tarball> [-f Dockerfile] [--build-arg K=V ...]
  [--target T] [-o image.tar.gz] [--load] [-t tag ...]`, and `c0mpute run
  [--image ref | --build <dir>] [--in <dir>] [--out <path> ...] [--dest dir]
  [-e K=V] -- <cmd...>` (exit code = the remote command's). Both take
  `--peer`, `--any-worker`, limits, `--max-price`, `--json`. `c0mpute
  containers show | id | allow | deny | open | close | add-builder |
  rm-builder` manages the config. The buyer identity is `<data dir>/build`
  (`C0MPUTE_BUILD_IDENTITY`), locked while in use so two builds from one
  machine queue instead of sharing a peer id.
- Jobs bridge: `POST /v1/blobs` (raw tarball, up to 2 GiB) -> `{blob}`;
  `POST /v1/jobs` with `workload: "container.run"` and the spec above using
  `context_blob` / `input_blob`; the response adds `image_url` and
  `outputs[].url` (`GET /v1/blobs/{id}`, kept one hour). No 300 s cap for
  this workload: the job's own timeout applies.
- MCP: `container_build` and `container_run` on `c0mpute apps mcp`.
- c0mpute.com REST: not added. The site's job routes run on its shared cloud
  node for its own users; a build there would need upload storage and a
  builder allowlist per account. Callers run `c0mpute jobs serve` instead.

## Not in v1

- Registry push from the builder (results come back to the buyer instead).
- Live log over the bridge (it logs to its own output).
- Payment beyond the normal auction price; own builders bid 0.
- Rootless or VM-isolated builders.
