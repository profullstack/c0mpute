# Apps: backends on c0mpute

Hosting keeps static sites alive. **Apps** do the same for things that run:
an API, an MCP server, a bot, a worker with a SQLite database. Any container
image that serves HTTP on one port. The network keeps three copies of every
app and runs one of them. Nothing coordinates this.

```sh
c0mpute apps deploy ./api --name api --port 8080 --state /data   # build, publish, run
c0mpute apps secrets set api DATABASE_KEY=...                     # sealed to holders
c0mpute apps ls                                                  # apps a gateway knows
c0mpute apps status api
c0mpute apps logs api                                            # owner only
c0mpute apps destroy api --yes
```

The app is then served by every gateway at `http://<gateway>:7777/a/<app_id>/`,
and on custom domains (below).

## git push to deploy

```sh
c0mpute git install                       # adds git-remote-c0mpute next to c0mpute
git remote add c0 c0mpute://api
git push c0 main                          # stores the repo; deploys if c0mpute.toml has [app]
git clone c0mpute://<owner-key>/api       # anyone; works with the owner offline
```

A repository is a hosting site (`git-<name>`) signed by your key (see
[git hosting](git-hosting.md)), so the network keeps three copies and only you
can move a ref. Pushes are fast-forward only unless forced. Turn deploys off
with `git config c0mpute.deploy false`.

`c0mpute.toml`:

```toml
[app]
name = "api"
port = 8080
health = "/healthz"
state = "/data"          # snapshotted to standbys, restored on failover
snapshot_secs = 30
memory_mb = 512
cpus = 0.5
replicas = 3
domains = ["api.example.org"]
# image = "ghcr.io/me/api@sha256:..."   # instead of building the repo

[app.env]
NODE_ENV = "production"
```

## How it works

| Piece | What happens |
|---|---|
| Record | The owner signs an app record: image, port, public env, state path, replicas, domains. The app id comes from the owner key + name; the highest `seq` wins. |
| Image | `docker build`, then `docker save` and gzip, published as a hosting site signed by the owner (three copies, no registry). Or a digest-pinned registry image (`--pull`). Max 1 GiB gzipped. |
| Placement | Every `apps` node gossips what it holds every 10 s on `c0mpute/apps/v1`. Every node computes the same holders by rendezvous hashing over the live nodes with room. Holders keep their copy, the shortfall is filled, and the surplus lowest-ranked standby lets go. |
| Primary | One holder runs the container. A live holder claiming primary keeps it (sticky). When two claim it, the higher `(epoch, rank)` wins and the other stops. With no claimant for 35 s, the ready holder with the newest snapshot promotes itself at `epoch + 1`. |
| State | Every `snapshot_secs` the primary pauses the container, copies the state directory out (crash-consistent), unpauses, and stores it as AES-256-GCM pieces of 1 MiB under the app key. Pieces are convergent, so unchanged pieces are the same chunks and standbys pull only what changed. On failover the new primary restores the newest snapshot. **Recovery point = one snapshot interval.** |
| Secrets | Secret env plus the app key travel sealed to each holder's X25519 key (`Seal` gossip): from the owner's CLI at deploy, then from any holder to newcomers, so the owner can go offline. The record carries only their blake3. |
| Serving | Gateways forward to the primary over the `apps.http` libp2p RPC, so NAT'd primaries work. Requests up to 700 KiB, responses up to 5 MiB, 15 s upstream timeout, no websockets. The app sees `X-Forwarded-For/Host/Proto` and `X-Forwarded-Prefix: /a/<id>`. |
| Sandbox | `--cap-drop ALL` (plus CHOWN, SETUID, SETGID, DAC_OVERRIDE, FOWNER, NET_BIND_SERVICE), `no-new-privileges`, memory/CPU/pid limits, 64 MiB tmpfs `/tmp`, published on loopback only, no restart policy (the supervisor decides what runs). |
| Network | On a root worker the container joins the `c0mpute-apps` network behind an egress guard: it reaches the public internet but not private, loopback, link-local (cloud metadata) or CGNAT addresses, and not the host. On a non-root worker there is no guard (see below). |
| Disk | State up to 1 GiB, and by default 2 GiB for state plus everything written inside the container. An app over the limit is stopped and restarts from its last snapshot. |

## Custom domains

Point the domain (A/AAAA) at one or more gateways and add:

```
_c0mpute-app.api.example.org.  TXT  "app=<app_id>"
```

A gateway serves the host only when the owner-signed record lists the domain
**and** DNS agrees. Apps are tried before static sites.

## Running apps for the network

`apps` is **opt-in** (it runs other people's containers and needs docker):

```sh
c0mpute worker start --roles ...,apps
```

```toml
[apps]
max_apps = 4          # held at once (primary or standby)
max_memory_mb = 2048  # reserved by held apps together
owners = []           # owner keys whose apps this node runs; empty = anyone's
egress_guard = true           # keep apps off your LAN, the host and metadata addresses
require_egress_guard = false  # true: run no apps unless that guard is in place
max_disk_mb = 2048            # per app: state volume plus writes inside the container
```

Every gateway routes to apps whether or not it runs any.

**Whose apps.** With `owners` empty the node runs containers for anyone on
the network, and says so in its log at start.

**Network.** The egress guard is a set of iptables rules on the apps bridge
(`c0apps0`), the same ones `container.run` uses plus one that lets the app
answer the node's own proxy. Only a root worker can add them. A non-root
worker keeps apps on docker's default bridge, where a container **can reach
your LAN, the host and cloud metadata addresses**; the node logs that once at
start. Set `require_egress_guard = true` to run no apps in that case. An app
that must reach a private address (a database on your LAN) needs
`egress_guard = false`. The guard is IPv4 only.

**Disk.** Every minute the node asks docker how much a running app has written
inside its container and adds the size of its last state snapshot. Over
`max_disk_mb` (0 = no per-app limit), or with state over 1 GiB, the app is
stopped, the reason is recorded (`error` in `GET /apps/v1/apps/<id>` on that
node, and its log), and another holder takes over from the last snapshot. The
stopped app's state volume is left on the node: what it holds past the last
snapshot exists nowhere else, so removing it is the owner's decision. Apps are also stopped, and
none started, while the docker disk has less free than the smaller of 20 GiB
and 10% (the floor `container.run` keeps). Where docker can enforce a quota
itself (overlay2 on xfs with project quotas) the container is also created
with `--storage-opt size=`.

## Limits, honestly

- **One writer.** A stateful app has a single primary. A healed partition can
  briefly run two; the lower epoch stops, and writes it took since the last
  snapshot are lost. This is a replicated single-node database, not a
  distributed one (no Raft yet; rqlite/dqlite as an app needs node-to-node
  links, which come next).
- **Pin your hosts for anything private.** By default any `apps` node may
  hold an app, and every holder receives its secrets and app key. `--host
  <peer-id>` (repeatable; `hosts = [...]` in `c0mpute.toml`) signs an allowlist
  into the record: holders seal secrets only to, and take snapshots only from,
  those peers, and nodes off the list never take a copy. Operators can
  likewise limit whose apps they run with `[apps] owners`.
- **Operators can read a running app.** The node running the container sees
  its memory and its state volume, as with any VPS host. Snapshots at rest on
  standbys are encrypted.
- **HTTP only, buffered.** No websockets, SSE, raw TCP, or uploads over 700 KiB yet.
- **Not paid yet.** Placement is free and best effort; per-hour pricing will
  reuse the hosting settlement rails.
