---
dip: 0002
title: "Single `depin` binary with product-line subcommands"
status: Superseded
authors:
  - anthony@profullstack.com
created: 2026-05-03
updated: 2026-05-03
discussion:
implementation: node/crates/c0mpute-cli/src/main.rs, scripts/install.sh
supersedes:
superseded-by: 0005
---

> **Superseded by DIP-0005** (c0mpute.com rebrand). The CLI is now three
> binaries — `c0mpute`, `coinpay`, `infernet` — installed together by the
> canonical installer. `c0mpute` delegates to the others via subprocess
> for high-level commands. The "one binary, namespaced subcommands"
> argument doesn't survive the brand split: CoinPay is its own product
> identity reused across multiple Profullstack marketplaces, and Infernet
> Protocol is an externally-known protocol — neither belongs as a
> subcommand under `depin`.

## Summary

Ship a single CLI binary named `depin`. Today's commands live under
`depin video <command>` — `depin video start`, `depin video doctor`,
`depin video config set ...`. Future product lines plug in as siblings:
`depin storage`, `depin compute`. Top-level commands like
`depin --help` and `depin version` work without a line argument.

Installs to `~/.depin/bin/depin`. Config under `~/.config/depin/config.toml`
on Linux (XDG) and equivalent on macOS.

## Motivation

The CLI is the most public surface a provider/operator touches. Two options
with future product lines:

1. **One binary per line.** `quest`, `depin-storage`, `depin-compute`. Three
   PATH entries, three `which`-checks, three doctors, three update flows.
   Operators running multiple lines on the same machine have to wrangle
   versions independently.
2. **One binary, namespaced subcommands.** Operator installs `depin` once;
   adding a new line is `depin <line> ...`. One self-upgrade, one config
   tree, one doctor that knows all lines.

Option 2 mirrors `git`, `kubectl`, `aws` — well-trodden UX. Same logic that
drove DIP-0001 (URL namespace) drives this: pick the structure now, before
operators and integrators wire things up against the wrong shape.

## Detailed design

### Binary

- Cargo bin name: `depin` (was `quest` in the very first scaffold).
- Internal Rust crates keep `quest-*` names — those are the *brand* of the
  video product, not user-visible.
- Top-level help lists product lines:

  ```
  depin <COMMAND>

  Commands:
    video    Quest — decentralized video transcoding & hosting
    version  Print binary version
    help     Print this message or the help of the given subcommand
  ```

### Subcommand surface (today)

```
depin video start [--roles ...] [--storage 500GB] [--gpu]
depin video stop | status | restart
depin video doctor [--fix] [--report]
depin video config get|set|list <key> [<value>]
depin video upgrade [--check]
depin video logs [--follow] [--since 1h]
depin video peers | jobs | earnings
depin video withdraw <amount> <currency>
depin video upload <file> [--rendition 1080p,720p,480p]
depin video videos | show <id>
```

### Install layout

```
~/.depin/
├── bin/
│   └── depin
└── (state lives under ~/.config/depin and ~/.local/share/depin via XDG)
```

- Env vars: `DEPIN_HOME` (install root), `DEPIN_VERSION` (installer pin),
  `DEPIN_RELEASE_BASE` (override for testing), `DEPIN_CONFIG` (config path).
- PATH: installer appends `$HOME/.depin/bin` to `~/.bashrc`, `~/.zshrc`,
  `~/.profile`.

### Future expansion

When `depin storage` ships:

1. Add a new clap subcommand `Storage { cmd: StorageCmd }` to `TopCmd`.
2. New crate (e.g. `storage-core`) with the role-supervisor for storage.
3. `depin doctor` becomes line-aware — `depin video doctor` runs video
   checks; `depin storage doctor` runs storage checks; `depin doctor`
   without a line runs both.

The binary stays one artifact; product lines are crates that compile in.

## Alternatives considered

**Multiple binaries (`quest`, `depin-storage`, ...).** See motivation —
worse UX, worse upgrade story, encourages product lines to drift apart.

**Plugin model where lines are separate binaries discovered at runtime.**
Like `git`'s `git-foo` convention or `kubectl` plugins. Plausible at scale
but premature: today everything ships from this monorepo and one Cargo
build, and we don't have any third-party lines to support. Revisit if we
ever do.

**Keep the `quest` name top-level.** Locks the parent brand to one product.
Same problem as the URL namespace decision in DIP-0001.

## Migration & rollout

This DIP applied during initial scaffolding — no users to migrate. Going
forward:

- Installer is idempotent; re-running upgrades the binary in place.
- Old `~/.quest/` install directory is *not* recognized; if we ever need to
  migrate users from the briefly-used `quest` binary, that's a follow-up
  DIP with explicit migration steps. (No production users at acceptance, so
  no migration required.)

## Open questions

- **`depin doctor` (no line argument):** should it run all installed lines'
  checks, or print a list of available lines? Punted to whenever line #2
  exists.
- **Shell completions:** do we ship `depin completions <shell>` or rely on
  per-line completions? Probably the former; will be a small follow-up.

## Out of scope

- The user-facing brand naming for individual lines ("Quest" for video).
  That's marketing copy, not CLI structure.
- API client SDKs in other languages — they target the REST API, not the
  CLI shape.
