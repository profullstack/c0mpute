# Autoscale rented nodes

`c0mpute autoscale` stops a GPU node you rent by the hour when the network has
no work for it, and starts it again as soon as jobs for its models are waiting.
It is opt-in, per node, and meant for **rented** instances (Vast.ai,
SaladCloud, ...): a box you own saves nothing by stopping, so leave owned
machines out.

## Where it runs, and where your keys live

Run it on any always-on machine you own: a home server, a VPS, one of your
fixed nodes. Not on the rented box itself, which has to be started by
something that is awake.

- Your **cloud provider API key** (Vast, Salad, ...) stays on that machine, in
  a file (`api_key_file`, mode 0600) or an environment variable. It is never
  sent to c0mpute.com, and c0mpute.com never stores it.
- Your **c0mpute API key** (`C0MPUTE_API_KEY`) is used only to report each
  node's state to your dashboard (`report = true`). Without it the CLI still
  works; the dashboard just does not show the autoscale table.

## Commands

```
c0mpute autoscale init        # writes ~/.config/c0mpute/autoscale.toml (0600)
c0mpute autoscale once        # one decision per node, then exit
c0mpute autoscale run         # keep deciding (every minute)
c0mpute autoscale status      # each node: state, idle time, demand, saved
```

## Config

`~/.config/c0mpute/autoscale.toml`:

```toml
report = true            # send state to your c0mpute.com dashboard (needs C0MPUTE_API_KEY)

[[node]]
name = "vast-3060"
provider = "vast"         # vast | salad | thunder (thunder: status only, no native stop)
instance = "61205210"
node = "sdNT3kCN"         # board id or peer id
models = ["eriktonon/qwen3.8-uncensored:9b"]
api_key_file = "~/.config/c0mpute/vast.key"
idle_minutes = 20
min_run_minutes = 15
boot_timeout_minutes = 8
# busy_cmd = "~/bin/gpu-busy.sh"   # optional: exit 1 = idle; exit 0 or any failure = busy (never stop)
# pre_stop_cmd = "~/bin/stop-worker.sh"   # optional: run right before a stop
# dry_run = true          # watch only: log what it would do, never start/stop/rent

# [node.replace]          # vast only: rent a replacement if a start never comes up
# max_usd_per_hour = 0.12
# gpus = ["RTX 3060", "RTX 3070", "RTX 4060 Ti"]
# min_gpu_ram_gb = 12
# min_reliability = 0.98
# disk_gb = 60
# image = "nvidia/cuda:12.4.1-runtime-ubuntu22.04"
# onstart_file = "~/.config/c0mpute/vast-onstart.sh"
# identity_file = "~/.config/c0mpute/node-identity.key"   # keep the node id: sent as $C0MPUTE_IDENTITY_KEY_B64, your onstart writes it
```

## Policy

**Stop** a running node only when all of these hold:

- it has had no job (accepted or completed) for `idle_minutes`;
- nothing is in flight on it;
- the network has no waiting or recent demand for any of its `models`;
- `busy_cmd`, if set, says idle (exit 1; exit 0 or a failed check keeps it running). Hooks get `$C0MPUTE_AUTOSCALE_NAME`, `$C0MPUTE_AUTOSCALE_INSTANCE` and `$C0MPUTE_AUTOSCALE_NODE`, so one script follows the node across a replacement;
- it has run at least `min_run_minutes` since it was last started.

**Start** a stopped node at once when the network has offers for its models
(waiting for a node, or in the last 2 minutes), or demand nobody served
recently.

**Boot timeout**: a start that has not reached running, with the worker back
online, within `boot_timeout_minutes` is reported as `error`. With
`[node.replace]` (Vast only) the CLI rents the cheapest matching offer at or
below `max_usd_per_hour`, boots it with your `onstart_file` (and
`identity_file`, so it keeps the node id), and destroys the old instance only
after the new one serves. Nothing above your price cap is ever rented.

## The demand signal

The CLI reads `GET https://c0mpute.com/api/v1/autoscale/signal?node=<id>&models=a,b&window_s=1200`,
which merges every status aggregator's view (`GET /autoscale` on each, DIP-0014).
The aggregators watch every job offer, accept and receipt on the network, so
the signal covers jobs from any buyer, not only c0mpute.com:

```json
{
  "now_ms": 0, "window_s": 1200,
  "node": { "id": "sdNT3kCN", "peer_id": "12D3...", "online": true, "last_seen_s": 4,
            "in_flight": 0, "last_accept_ms": 0, "last_receipt_ms": 0 },
  "models": { "eriktonon/qwen3.8-uncensored:9b":
              { "offers_window": 0, "offers_2m": 0, "waiting": 0, "unserved_window": 0, "last_offer_ms": null } }
}
```

It is public: the same facts `/nodes` and `/status` already show.

## Providers

| provider | stop / start | notes |
| --- | --- | --- |
| `vast` | yes | A stopped instance still bills its disk. Its GPU can be rented by someone else while it is stopped, so a start can fail: that is what `[node.replace]` is for. |
| `salad` | yes | Container group stop / start. |
| `thunder` | status only | Thunder Compute has no native stop (only snapshot and delete). |

## Dashboard

With `report = true` each transition is sent to
`POST /api/v1/operator/autoscale` (your `C0MPUTE_API_KEY`), and
[/dashboard/nodes](https://c0mpute.com/dashboard/nodes) shows each autoscaled
node: state (running, stopped, starting, stopping, replacing, error), its last
change and why, the recent changes, and what stopping it has saved. A node
that reports is added to your account if it was not on it yet.

## Run it as a service

```ini
# ~/.config/systemd/user/c0mpute-autoscale.service
[Unit]
Description=c0mpute autoscale: stop idle rented nodes, start them on demand
After=network-online.target

[Service]
EnvironmentFile=-%h/.config/c0mpute/autoscale.env   # C0MPUTE_API_KEY=... (0600)
ExecStart=%h/.c0mpute/bin/c0mpute autoscale run
Restart=always
RestartSec=30

[Install]
WantedBy=default.target
```

```
systemctl --user enable --now c0mpute-autoscale
loginctl enable-linger $USER     # keep it running after you log out
journalctl --user -u c0mpute-autoscale -f
```
