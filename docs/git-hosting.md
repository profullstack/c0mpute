# Git hosting: repos kept alive by the network

The `git` plugin hosts git repositories on c0mpute with no server of record. A repo is
signed by its owner's key, hosting nodes keep three self-healing copies of it, and any
gateway serves clones. There is no coordinator and no central index: it is the
[hosting](hosting.md) machinery with git on top.

```sh
c0mpute git install-helper                 # once: puts git-remote-c0mpute on PATH
cd my-project
c0mpute git init my-project                # prints c0mpute://<your-key>/my-project, adds remote "origin"
git push origin main                       # signed, sent to a gateway, replicated x3

# anyone, anywhere:
git clone c0mpute://<owner-key>/my-project                       # via the remote helper, verified
git clone https://<any-gateway>/git/<owner-key>/my-project.git   # plain git, no helper
c0mpute git ls --gateway https://<gateway>                       # repos a gateway knows
```

`c0mpute git clone` and `c0mpute git push` are the same as `git clone`/`git push` with
the helper put on PATH for that one command, so they work before `install-helper`.

## Who can push

The owner key is this machine's c0mpute identity (`<data_dir>/buyer/identity.key`, the
same key that owns hosting sites; `c0mpute git key` prints it). Only that key can publish
a new version of `c0mpute://<that-key>/<repo>`. To push from a second machine, copy the
key file there (multi-writer repos are not built yet, see below).

`c0mpute://<repo>` with no owner means "my key", handy for your own remotes.

## Gateways

The helper reads from and pushes to, in order:

1. `C0MPUTE_GIT_GATEWAY` (comma-separated), else
2. `git config c0mpute.gateway` (repeatable, per repo or global), else
3. `http://127.0.0.1:7777`, the local node.

Reads ask every listed gateway and take the newest version that verifies. Pushes go to
the first gateway that accepts. Any gateway works: the helper checks the owner signature
and every chunk hash itself, so a gateway can refuse or lag but cannot forge or corrupt a
repo. Public gateways are on the node board (`gateway` on `/api/nodes`) and on
[d1sks](https://d1sks.pwa.pit.world/disks?hosting=1).

A gateway accepts pushes unless its operator turns them off:

```toml
# ~/.config/c0mpute/config.toml
[hosting]
git_pushes = false      # reads and clones are always served
```

Pushes count against the gateway's `hosting.max_bytes` and the 1 GiB per-owner budget,
like any site it holds. In private mode a node accepts pushes only from its own key.

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

### HTTP API (every gateway)

| Route | |
|---|---|
| `GET /git/v1/repos` | repos this gateway knows: owner, repo, `c0mpute://` URL, clone path, seq, size, live copies |
| `GET /git/v1/repos/<owner>/<repo>` | `{record, manifest, holders}`: the signed record and the manifest JSON it pins |
| `POST /git/v1/repos/<owner>/<repo>` | push: `C0GIT1\n`, u32-BE header length, header JSON `{record, manifest}`, then chunks as u32-BE length + bytes. 200 `{site_id, seq, holders}`; 400 malformed; 403 bad signature or pushes off; 409 stale or rule broken; 502 a needed chunk unreachable; 507 over budget |
| `GET /git/<owner>/<repo>.git/<path>` | repo files for git's dumb HTTP protocol |

### Limits

256 MiB and 10,000 files per repo (hosting's site limits). Repo names are 1-59 of
`a-z`, `0-9` and `-`. SHA-1 repositories.

## Paying hosts

Repos are hosting sites, so free copies are kept by volunteers exactly like free sites, and
the paid path (d1sks preferred hosts, proofs, per-GiB-month settlement) applies to the
site `git-<repo>` the same way. The push path does not pick preferred hosts yet; a repo
published today is a free (v1) site.

## Not yet

- **Smart HTTP.** Clones are dumb HTTP: a clone downloads every pack (bounded by repacking
  every 16 pushes), there is no object negotiation. Fine at hosting's 256 MiB cap.
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
- git-lfs, sha256 repositories, a web UI, issues and pull requests.

## Testing

`cargo test -p c0mpute-cli --test git_e2e` runs the whole thing on loopback: four hosting
nodes and two gateways in-process, real `git` and the real `git-remote-c0mpute`. It pushes,
waits for three copies, clones from a gateway that holds nothing (through the helper with
another key, and over dumb HTTP), pushes incrementally and pulls, pushes from a second
checkout, checks that a divergent push is refused and a forced one accepted, that a stranger
cannot push, and that an annotated tag survives a fresh clone. `C0MPUTE_GIT_TRACE=1` prints
push timings.
