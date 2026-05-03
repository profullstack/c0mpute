---
dip: 0004
title: "Pin contributor toolchain via mise; keep operator install minimal"
status: Accepted
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: .mise.toml, scripts/dev-setup.sh
supersedes:
superseded-by:
---

## Summary

Two install paths, two audiences:

1. **Operators** running `depin video start` on a homelab/datacenter box.
   They get a single static binary via `curl | sh` from
   `https://c0mpute.com/install.sh`. No runtime, no toolchain
   manager, no Node, no Cargo. The static Rust binary is the whole story.

2. **Contributors** working in this repo. They run `scripts/dev-setup.sh`
   (or `mise install` directly) and get every pinned tool the workspace
   needs: Rust, Bun, Node, plus `ffmpeg` and friends. Versions live in
   `.mise.toml` at the repo root.

This DIP locks in the split so we don't accidentally leak heavyweight
contributor deps into the operator install — a real risk while we're
prototyping.

## Motivation

`infernet-protocol` showed us that mise-bootstrapping in the user installer
is excellent for a Node-based CLI (their CLI *is* JavaScript and needs
Node). the c0mpute CLI is a static Rust binary — operators don't need a
toolchain at all.

But contributors absolutely do. Today the repo wants a specific Rust, a
specific Bun, a specific Node for Next.js, plus FFmpeg. Without a pin, two
laptops compile the same commit differently and we waste a day on it.

mise solves the contributor side cleanly while infernet's installer pattern
informs the *next* generation of installers if we ever ship a non-static
component to operators.

## Detailed design

### Repo-root `.mise.toml`

```toml
[tools]
rust = "1.92"
node = "20"
bun = "1.3"

[env]
# Coordinator + dashboard local defaults
BASE_PATH = "/video"

[tasks.test]
description = "Run the Rust unit tests + TS typecheck"
run = [
  "cargo test --manifest-path node/Cargo.toml",
  "bun run lint"
]

[tasks.dev]
description = "Run coordinator + dashboard concurrently"
run = "bun run dev:coordinator & bun run dev:web; wait"
```

### `scripts/dev-setup.sh`

A short bash script that:

1. Detects mise (`command -v mise`); installs via `curl https://mise.run | sh`
   if missing, exporting `~/.local/bin` to PATH for the rest of the script.
2. Runs `mise install` — picks up everything in `.mise.toml`.
3. Runs `bun install` to populate the workspace.
4. Prints next-step hints (`mise run dev`, `mise run test`, etc.).

Idempotent: safe to re-run. If mise is already installed and the toolchain
is hot, the script is a few hundred milliseconds.

### Operator install (`scripts/install.sh`) — explicit non-goal

The operator install **does not** install mise, Node, Rust, or anything
else. It installs the static `depin` binary, runs `depin video doctor`,
and exits. Anything else we add to that script we should question hard:
the `curl | sh` audience does not want surprises.

If a future product line requires a runtime (e.g. Node-based agent),
that's a follow-up DIP. Don't sneak it in.

### Doc updates

- `README.md` quickstart points at `scripts/dev-setup.sh`.
- The "Run the Rust node locally" section explicitly notes the contributor
  vs operator distinction.

## Alternatives considered

**Bundle mise into the operator installer.** This is what infernet does,
and it's the right call for them — their CLI is JS and *needs* a Node
runtime. the c0mpute CLI doesn't, so adding mise to operator installs is dead
weight that ages badly (mise version drift, surprise file writes under
`~/.local`).

**asdf instead of mise.** Same shape, smaller community, slower. mise wins
on UX (project-local config, faster startup, `mise run` task runner) and
that's what infernet picked too.

**rustup + nvm + direct bun install.** Three tools, three configs, three
shell-rc edits per contributor. mise replaces all three with one config
file in the repo.

**Pin via `rust-toolchain.toml` + `package.json#engines`.** Half-solution —
covers Rust + Node version pinning but doesn't manage installation. We'd
still tell contributors "go install rustup, go install nvm, ...". mise
covers both pinning *and* installation.

## Migration & rollout

Greenfield. Land `.mise.toml` and `scripts/dev-setup.sh` together.

If a contributor doesn't want mise: their environment has to satisfy the
versions in `.mise.toml` some other way. We won't actively block alternate
flows but we won't troubleshoot them either — `mise install` is the
supported path.

## Open questions

- Do we want a `Justfile` *in addition to* `mise run` tasks, or is mise's
  task runner enough? Lean toward "mise tasks only" — fewer moving parts.
- Should we pin `ffmpeg` via mise (it has an ffmpeg plugin) or expect
  system-installed FFmpeg? Lean toward system FFmpeg because the bundled
  one would balloon clone size and the version skew on test runners
  doesn't actually bite us yet. Revisit if it does.

## Out of scope

- Operator install bootstrapping a runtime — see "explicit non-goal" above.
- CI toolchain — GitHub Actions / similar will install mise itself in the
  job, separate concern from local dev.
- Editor / IDE setup (rust-analyzer, eslint configs).
