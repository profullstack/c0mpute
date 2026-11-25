# Seeding

A c0mpute node can seed the torrents c0mpute publishes: our own releases and
other files we are allowed to redistribute. It is a node feature like
hosting or Scallion relaying, and it is **off until the operator opts in**,
because it uses disk (up to a cap you set) and upstream bandwidth.

Source: [`c0mpute-seeding`](../node/crates/c0mpute-seeding) (manifest, plan,
engine) and [`seeding_node.rs`](../node/crates/c0mpute-core/src/seeding_node.rs)
(config, the worker loop, status).

## What a node seeds, and what it never does

A node seeds exactly two things:

1. **The published list.** Torrents named in a manifest that c0mpute signs
   with its release key. The node fetches it, checks the minisign signature
   against the release key pinned in the binary (the same key that signs the
   binaries, [`minisign.pub`](../minisign.pub)), and refuses the whole list
   on any doubt (see [Manifest checks](#manifest-checks)).
2. **Your own torrents.** `.torrent` files you list in
   `[seeding] local_torrents`. Only list content you may distribute.

Nothing else can add a torrent:

- no job, peer, capability ad or gateway request can add one; the worker's
  gateway exposes a read-only status page and nothing else;
- no DHT, no local peer discovery, no UPnP, no uTP: peers come only from the
  trackers named in the signed list or in your own `.torrent` file;
- no crawling, no index, no search, no add API and no `c0mpute seeding add`.

The list compiled into the binary is empty. Every torrent a node seeds from
us arrives in a signed manifest.

## Turn it on

Pick one:

```sh
c0mpute seeding on                        # 20 GiB cap, 2048 KiB/s upload cap, TCP 4241
c0mpute seeding on --max-bytes 100GiB --max-upload-kib 4096 --port 4241
```

or in the worker's `config.toml` (`c0mpute seeding status` prints its path):

```toml
[seeding]
enabled = true
max_bytes = 21474836480          # 20 GiB
max_upload_kib_per_sec = 2048
port = 4241
```

or, for a fleet, in the worker's environment, without editing files:

```sh
C0MPUTE_SEEDING=1
C0MPUTE_SEEDING_MAX_BYTES=107374182400   # 100 GiB
```

Then restart the worker (`systemctl --user restart c0mpute-worker`, or stop
and start `c0mpute worker start`). A running worker keeps the settings it
started with.

Turn it off with `c0mpute seeding off`, `enabled = false` or
`C0MPUTE_SEEDING=0`. Data already downloaded stays in the seeding directory
until you delete it.

A private node (`c0mpute private on`, `C0MPUTE_PRIVATE=1`) never seeds,
whatever the seeding settings say.

## See what it does

```sh
c0mpute seeding status           # on or off, why, the limits, what the running worker reports
c0mpute seeding status --json
c0mpute seeding list             # each torrent: state, bytes held, uploaded, origin; skipped ones and why
c0mpute seeding list --json
c0mpute worker status            # one "seeding:" line, and a "seeding" object in --json
```

`list` and the worker part of `status` ask the local worker's gateway
(`GET /seeding/v1/status` on `http://127.0.0.1:7777` by default, `--gateway`
to change it). A worker that seeds also puts a small summary in its
capability ad (`hardware.seeding`: `{ "v": 1, "torrents", "held_bytes",
"max_bytes" }`). It carries no list of torrents, and no node reads another
node's summary to decide anything.

## Settings

| `[seeding]` key | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | `C0MPUTE_SEEDING` (`1/true/yes/on`, `0/false/no/off`) | `false` | Seed at all. The environment wins over the file. |
| `max_bytes` | `C0MPUTE_SEEDING_MAX_BYTES` (bytes) | 20 GiB | Most bytes of torrents this node holds. Never exceeded. |
| `max_upload_kib_per_sec` | `C0MPUTE_SEEDING_MAX_UPLOAD_KIB` | `2048` | Upload cap for all torrents together, KiB/s. `0`: no cap. |
| `port` | `C0MPUTE_SEEDING_PORT` | `4241` | TCP port peers connect to. `0`: any free port. Give each worker its own when several run on one machine. |
| `manifest_url` | `C0MPUTE_SEEDING_MANIFEST_URL` | `https://c0mpute.com/seeding/manifest.json` | Where the signed list is fetched; its signature is `<url>.minisig`. The key it must be signed with is pinned and cannot be configured. |
| `dir` | `C0MPUTE_SEEDING_DIR` | `<storage root>/seeding` | Where seeding keeps the verified manifest (`manifest/`) and torrent data (`data/<info hash>/`). |
| `only` | | `[]` | When not empty, take only these info hashes from the published list. It can narrow the list, never add to it. |
| `skip` | | `[]` | Info hashes from the published list this node does not seed. |
| `local_torrents` | | `[]` | Absolute paths of your own `.torrent` files to seed as well. They count against `max_bytes`. |

Every doubtful setting leaves seeding off rather than guessing: an
unrecognized `C0MPUTE_SEEDING` value, or a limit in the environment that is
not a number in range (`C0MPUTE_SEEDING_MAX_BYTES=20GB` is refused; give
bytes). `c0mpute seeding status` names the setting and the reason.
Setting only limits (`max_bytes`, `C0MPUTE_SEEDING_MAX_BYTES`, ...) never
turns seeding on by itself.

## Disk and bandwidth

- **The cap.** Torrents are taken in list order (published list first, then
  your local ones). A torrent is taken only if it still fits whole under the
  cap; one that does not fit is skipped (`c0mpute seeding list` shows it and
  why) and a smaller one after it may still fit. The planned total never
  exceeds `max_bytes`.
- **Declared size is enforced.** The plan counts the size the signed list
  declares. If a torrent's real metadata says it is larger, the node refuses
  it and downloads nothing.
- **The disk floor.** Like hosting, seeding keeps 5% of the disk free
  (between 10 and 50 GiB). When the disk is short of that, the cap in effect
  (`cap_bytes_now` in the status) drops below `max_bytes` so seeding takes
  nothing new; what it already holds stays counted as its own.
- **Shared with hosting.** Hosting's automatic budget is computed from the
  disk's free space above that same floor, so whatever seeding holds is
  already taken out of the room hosting sees. With a fixed `[hosting]
  max_bytes`, size the two so they fit on the disk together.
- **Withdrawn torrents are deleted.** When a torrent leaves the plan (removed
  from the published list, listed in `skip`, or no longer fitting), the node
  stops seeding it and deletes its files. Any `data/<info hash>/` directory
  the plan does not name is removed too, so what is on disk is what the plan
  allows.
- **Upload cap.** One cap for all torrents together.

## Network

Peers connect to the TCP `port` (4241 by default) on every address. Open or
forward it to seed to peers that cannot reach you otherwise; without it the
node still uploads to the peers it connects to itself. There is no UPnP.
Trackers are reached over UDP, HTTP or HTTPS as the list names them.

## How it runs

When seeding is on, the worker:

1. starts an in-process BitTorrent engine (TCP only, no DHT),
2. fetches the manifest and its `.minisig`, verifies them, and keeps the last
   verified copy in `<dir>/manifest/` (re-verified every time it is read; if
   the URL is unreachable the node keeps seeding from that copy until it
   expires),
3. plans what fits the cap and hands exactly that plan to the engine,
4. repeats every hour (every 5 minutes while something is not in place yet,
   such as a torrent whose metadata no peer has sent yet).

Nothing about it is kept in the engine between restarts: every start begins
from a freshly verified plan, and data already on disk is checked, not
downloaded again.

## Manifest checks

A node refuses the whole manifest when:

- the signature does not verify against the pinned release key, or there is
  none, or it is malformed;
- the signature's trusted comment does not name `file:seeding-manifest.json`
  (so a valid signature over a binary cannot pass for a manifest);
- `kind` is not `c0mpute.seeding.manifest` or `v` is not `1`;
- it has expired (`expires_at` is set and in the past);
- its `seq` is lower than the highest this node has accepted (a replayed old
  manifest cannot bring back a withdrawn torrent);
- any entry is malformed: info hash not 40 hex characters, listed twice,
  `bytes` 0, no license, a `source` that is not https, a tracker that is not
  udp/http/https, more than 16 trackers;
- it is over 1 MiB or lists more than 1024 torrents.

## Publishing the list (c0mpute maintainers only)

The rule: **only torrents we have positive evidence we may redistribute**:
our own releases, official Linux ISOs and other free software under its
license, public domain works with no new copyrightable layer, Creative
Commons. Unknown means no. Never a third-party index, never a DHT find.

1. Make or get the `.torrent` file for each release and seed it from a
   machine you control first.
2. Print its entry (info hash, name and size come from the file, so nobody
   types a hash):

   ```sh
   c0mpute seeding entry w3bs-os-1.2.iso.torrent \
     --license GPL-3.0-or-later \
     --source https://w3bs.org/download \
     --tracker udp://tracker.example.org:1337/announce
   ```

3. Put the entries in a manifest. Start from
   [`seeding-manifest.example.json`](seeding-manifest.example.json); raise
   `seq` above the last published one every time, set `issued_at` to now,
   and set `expires_at` if nodes should stop on their own when you stop
   publishing (for example 90 days ahead; leave `0` for no expiry).
4. Sign it with the release key:

   ```sh
   MINISIGN_SECRET_KEY_FILE=/path/to/c0mpute-release.key \
     scripts/sign-seeding-manifest.sh manifest.json
   ```

   (or `MINISIGN_SECRET_KEY` with the key text; `MINISIGN_PASSWORD` or a
   prompt for its password). It writes `manifest.json.minisig`, checks it
   against `minisign.pub`, and runs `c0mpute seeding verify` when a c0mpute
   with seeding is on PATH. You can also run
   `c0mpute seeding verify manifest.json` yourself.
5. Publish both files so that `https://c0mpute.com/seeding/manifest.json`
   and `https://c0mpute.com/seeding/manifest.json.minisig` serve them (for
   the web app: `apps/web/public/seeding/`). Nodes pick it up within the
   hour.

To withdraw a torrent, publish a new manifest without it and with a higher
`seq`; every node deletes it on its next refresh.

Never commit the secret key, and never put a torrent in the list because a
node or a user asked for it over the network.
