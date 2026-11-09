# Git hosting: repos kept alive by the network

c0mpute hosts git repositories with no server of record. A repo is signed by its owner's
key, hosting nodes keep three self-healing copies of it, and any gateway serves clones,
with the owner's machine offline. There is no account, no coordinator and no central
index: it is the [hosting](hosting.md) machinery with git on top. You use plain `git`;
the `c0mpute git` commands are shortcuts.

`c0mpute git --help` prints this guide in short form. `c0mpute git <command> -h` shows
one command.

## Quick start

```sh
c0mpute git install                        # once: puts git-remote-c0mpute on PATH
cd my-project
c0mpute git init my-project                # adds remote "origin" = c0mpute://<your-key>/my-project
git push origin main                       # signed by your key, kept by the network x3

# anyone, anywhere, no account:
git clone c0mpute://<owner-key>/my-project                                 # verified by the helper
git clone https://gateway.c0mpute.com/git/<owner-key>/my-project.git       # plain git, nothing installed
```

That is the whole setup. Everything after it is ordinary git.

## Commands

| Command | What it does |
|---|---|
| `c0mpute git install` | Put `git-remote-c0mpute` (a link to the `c0mpute` binary) next to it, so plain `git push`/`git clone` understand `c0mpute://` URLs. `--dir <dir>` to put it elsewhere on PATH. Alias of `install-helper`. |
| `c0mpute git init <repo>` | Print `c0mpute://<your-key>/<repo>`; inside a work tree, add it as remote `origin` (`--remote <name>` for another name). Publishes nothing until you push. |
| `c0mpute git url <repo>` | Print the clone URL of one of your repos. |
| `c0mpute git key` | Print this machine's owner key (the `<owner-key>` in URLs). |
| `c0mpute git clone <url> [dir]` | `git clone` with the helper on PATH, so it works before `install`. `--gateway <url>` to pick a gateway. |
| `c0mpute git push [git args]` | `git push` with the helper on PATH. `--gateway <url>` goes before git's arguments: `c0mpute git push --gateway https://gateway.c0mpute.com origin main`. |
| `c0mpute git ls` | Repos a gateway knows, with live copy counts (`--gateway <url>`; default your node, else the public gateway). |
| `c0mpute git help` / `-h` / `--help` | Help; `--help` adds the full guide. |

## URLs

| URL | Means |
|---|---|
| `c0mpute://<owner-key>/<repo>` | Anyone's repo. `<owner-key>` is 64 hex (`c0mpute git key`). |
| `c0mpute://<repo>` | A repo of this machine's key: the short form for your own remotes. |
| `c0mpute://<20-hex site id>` | The URL form of c0mpute before 0.3.11; still clones (see [Old repos](#old-repos)). |
| `https://<gateway>/git/<owner-key>/<repo>.git` | The same repo over git's plain ("dumb") HTTP: read-only, no c0mpute needed. |

Repo names are 1-59 of `a-z`, `0-9` and `-`.

## Everyday git

It is git: `git push`, `git pull`, `git fetch`, branches, tags (annotated tags too),
`git push origin --delete <branch>`. A few rules come from the network:

- **Only the owner key can push.** Everyone else can clone and fetch.
- **Fast-forward only** unless you force: `git push --force` (or a `+refspec`) rewrites
  history, as on any forge. Tags never move without force.
- **Two machines, one key:** the gateway refuses a push built on a version it no longer
  has as current ("the repo moved on since you fetched it"). `git pull`, then push again.
- **Size:** hosting's site limits apply (see [Limits](#limits)).

## Actions: push to deploy

A push can deploy. When it moves the default branch (HEAD) and that commit has a
`c0mpute.toml` with an `[app]` table, the push also deploys the commit as a
[c0mpute app](apps.md): three copies, failover, served at `/a/<app_id>/` on every
gateway and on your custom domains.

```toml
# c0mpute.toml at the repo root
[app]
name = "api"
port = 8080
health = "/healthz"
```

```sh
git push origin main
# c0mpute: deploying app api from 1a2b3c4d5e6f
# c0mpute: app api deployed: /a/<app_id>/ on any gateway
```

The full `[app]` reference is in [apps.md](apps.md). Turn deploys off for one repo with
`git config c0mpute.deploy false`. A failed deploy is reported but does not fail the
push: the repo is stored either way.

There is no CI runner yet (no build or test jobs triggered by a push). To run checks on
push today, keep a GitHub mirror with Actions, or run them before you push.

## A second machine

The owner key is the machine's c0mpute identity, `<data_dir>/buyer/identity.key`
(`~/.local/share/c0mpute/buyer/identity.key` on Linux). To push from another machine,
copy that file there. Multi-writer repos (a signed list of keys) are not built yet.

## Gateways

Pushes and clones go through a gateway over HTTP. The helper uses, in order:

1. `--gateway` on `clone`/`push`/`ls`, or `C0MPUTE_GIT_GATEWAY` (comma-separated), else
2. `git config c0mpute.gateway` (repeatable, per repo or `--global`), else
3. your own node, `http://127.0.0.1:7777`, then the public `https://gateway.c0mpute.com`.

Reads ask every listed gateway and take the newest version that verifies. Pushes go to the
first gateway that accepts. Gateways are untrusted: the helper checks the owner signature
and every file hash itself, so a gateway can refuse or lag but cannot forge or corrupt a
repo. Public gateways are on the node board (`gateway` on `/api/nodes`) and on
[d1sks](https://d1sks.pwa.pit.world/disks?hosting=1).

A new repo is served at once by the gateway you pushed to. Other gateways learn about it
by gossip, which can take a few minutes; until then they answer "no such repo (yet)".

Running a gateway? It accepts pushes unless you turn them off:

```toml
# ~/.config/c0mpute/config.toml
[hosting]
git_pushes = false      # reads and clones are always served
```

A push needs room on the gateway: the node's hosting budget (automatic, from free disk,
unless `[hosting] max_bytes` is set), the free-disk floor and the per-owner limit, like
any copy it takes. In private mode a node accepts pushes only from its own key.

## Old repos

c0mpute before 0.3.11 stored a repo as `refs.json` plus one `repo.bundle`. Nothing to do:
such repos still clone, also by their old `c0mpute://<site id>` URL, and the owner's next
push rewrites the repo in the current layout (one full pack) with its whole history.

## Troubleshooting

| Symptom | Cause, fix |
|---|---|
| `git: 'remote-c0mpute' is not a git command` | The helper is not on PATH: `c0mpute git install` (or use `c0mpute git push`/`clone`). |
| `HTTP 507 ... hosting budget` on push | That gateway has no room. Push through another: `--gateway https://gateway.c0mpute.com`. |
| `no such repo on the network (yet)` | That gateway has not heard of it. Wait a few minutes, or use the gateway you pushed to. |
| `the repo moved on since you fetched it` | Someone (your other machine) pushed first. `git pull`, then push. |
| `non-fast-forward` | Your branch is behind. Pull, or `git push --force` if you mean to rewrite. |
| `belongs to key ...; this machine's key is ...` | Pushing to another key's repo. Only its owner can push. |
| Anything slow | `C0MPUTE_GIT_TRACE=1 git push ...` prints phase timings. |

## How it works

| Piece | What it is |
|---|---|
| Repo | The hosting site named `git-<repo>` of the owner key. `site_id` = 20 hex of blake3(owner, `git-<repo>`). Record, versioning (`seq`), owner signature, replicas, healing, proxying, proof-of-storage and d1sks terms are hosting's, unchanged. Hosting nodes older than this plugin already keep copies of repos. |
| Files | A git "dumb HTTP" repository: `HEAD`, `info/refs` (with peeled tags), `objects/info/packs`, `objects/pack/pack-<sha>.pack` + `.idx`. |
| Ref map | `c0mpute-git.json`: `refs` (ref -> object id), `head`, `packs` (oldest first), `prev` (the manifest hash this version replaces), `force`. The owner-signed record pins the manifest, the manifest pins every file by blake3: the refs are signed. |
| Push | The helper fetches the current version, checks fast-forward (`+`/`--force` skips it, tags never move without it), packs only the objects the repo lacks (`git pack-objects --revs new ^old`), writes the new ref map, signs a record with `seq` > the old one, and POSTs it with only the new chunks to a gateway. Every 16 packs it writes one full pack instead. |
| Gateway checks | Owner signature; record and manifest agree; the ref map's `prev` is the version the gateway knows (no lost updates when the owner pushes from two places); a non-forced push keeps every existing pack byte for byte; budgets; every chunk present (sent, local, or fetched from the current holders). Then the gateway holds the repo and gossips the record, and the hosting fill brings it to three copies. |
| Fetch | The helper lists refs from the verified ref map and downloads packs newest first until git has every object it asked for. Packs are named by content, so an incremental fetch downloads only new packs. |
| Dumb HTTP | `GET /git/<owner>/<repo>.git/<path>` serves the repo's files exactly; git's dumb protocol needs nothing else. Packs are `immutable`-cached, refs `no-cache`. |
| Push to deploy | When a push moves HEAD's branch and that commit has a `c0mpute.toml` with an `[app]` table, the helper deploys it as an app (see [apps](apps.md)). `git config c0mpute.deploy false` turns it off. |
| Old layout | The first helper (v0.2.99 to v0.3.x) stored a repo as `refs.json` plus one `repo.bundle`, under the same site name. Those repos still clone (the helper unpacks the bundle), also by the old `c0mpute://<20-hex site id>` URL, which a gateway resolves through `GET /git/v1/sites/<id>`. The owner's next push rewrites the repo as one full pack and drops the bundle; nothing to do by hand. |

### HTTP API (every gateway)

| Route | |
|---|---|
| `GET /git/v1/repos` | repos this gateway knows: owner, repo, `c0mpute://` URL, clone path, seq, size, live copies |
| `GET /git/v1/repos/<owner>/<repo>` | `{record, manifest, holders}`: the signed record and the manifest JSON it pins |
| `POST /git/v1/repos/<owner>/<repo>` | push: `C0GIT1\n`, u32-BE header length, header JSON `{record, manifest}`, then chunks as u32-BE length + bytes. 200 `{site_id, seq, holders}`; 400 malformed; 403 bad signature or pushes off; 409 stale or rule broken; 502 a needed chunk unreachable; 507 over budget |
| `GET /git/v1/sites/<site_id>` | the same `{record, manifest, holders}` by hosting site id (old `c0mpute://<site id>` URLs); the client checks the id against the record |
| `GET /git/<owner>/<repo>.git/<path>` | repo files for git's dumb HTTP protocol |

### Limits

1 GiB per repo (`MAX_REPO_BYTES`: above that hosting would stripe the site across nodes,
and a push is held whole by its gateway), and the gateway's hosting budget plus the
per-owner limit. Repo names are 1-59 of `a-z`, `0-9` and `-`. SHA-1 repositories. Keep
large binaries out of git.
## Paying hosts

Repos are hosting sites, so free copies are kept by volunteers exactly like free sites, and
the paid path (d1sks preferred hosts, proofs, per-GiB-month settlement) applies to the
site `git-<repo>` the same way. The push path does not pick preferred hosts yet; a repo
published today is a free (v1) site.

## Not yet

- **Smart HTTP.** Clones are dumb HTTP: a clone downloads every pack (bounded by repacking
  every 16 pushes), there is no object negotiation. Fine at the 1 GiB repo cap.
- **Collaborators.** One key writes a repo. Multi-writer repos need a signed writer list in
  the ref map and gateways checking it.
- **Server-side fast-forward.** Gateways enforce `prev` and pack retention, not commit
  ancestry (that would need git on every gateway). The pusher's git and helper refuse
  non-fast-forward pushes unless forced; since only the owner can sign, a forced push is the
  owner's call, as on any forge.
- **A gateway that has not heard of a repo yet.** Hosting has no "ask the network for
  record X" query, only gossip (holders re-gossip records every 5 minutes). If every
  gateway the owner points at is too new to know an existing repo, the helper sees "no
  such repo" and the push is signed as a first version, which then wins on `seq`. The
  old packs stay on holders until garbage collection, but the refs are replaced. Point
  pushes at a gateway that has been up a while (your own node is), or list several.
- **CI / actions beyond deploy.** A push can deploy an app (see above), but there is no
  runner for build or test jobs on push.
- git-lfs, sha256 repositories, a web UI, issues and pull requests.

## Testing

`cargo test -p c0mpute-cli --test git_e2e` runs the whole thing on loopback: four hosting
nodes and two gateways in-process, real `git` and the real `git-remote-c0mpute`. It pushes,
waits for three copies, clones from a gateway that holds nothing (through the helper with
another key, and over dumb HTTP), pushes incrementally and pulls, pushes from a second
checkout, checks that a divergent push is refused and a forced one accepted, that a stranger
cannot push, and that an annotated tag survives a fresh clone. `C0MPUTE_GIT_TRACE=1` prints
push timings.
