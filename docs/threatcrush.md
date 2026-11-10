# ThreatCrush on a c0mpute node

A c0mpute gateway listens on the public internet (`:7777`). Scanners, credential
stuffers and exploit kits find it like any other web server.
[ThreatCrush](https://threatcrush.com) is an optional host security daemon. It
tails web and auth logs, scores requests against attack rules, and bans repeat
offenders in nftables, with a doubling ladder that never bans permanently.

The node side is built into c0mpute and **off by default**. The daemon is a
separate install.

## Install

```sh
c0mpute plugin install threatcrush
```

Run it as the user the worker runs as (root for a root systemd unit). It:

1. installs the ThreatCrush CLI/daemon if it is missing
   (`https://threatcrush.com/install.sh`);
2. adds `[threatcrush] enabled = true` to your c0mpute config, so the gateway
   writes an access log;
3. registers that log with the daemon in
   `/etc/threatcrush/threatcrushd.conf.d/c0mpute.conf` (or
   `~/.threatcrush/threatcrushd.conf.d/` for a per-user daemon):

   ```toml
   [log-watcher]
   paths = ["/home/you/.local/share/c0mpute/gateway/access.log"]
   ```

Then restart the worker and the daemon. Bans need a root daemon
(`sudo threatcrush install-service`). A user-mode daemon only reports.
Reading extra log paths from `conf.d` needs threatcrush 0.13.77 or later.

## The access log

```toml
[threatcrush]
enabled = true
# access_log  = "/var/log/c0mpute/access.log"   # default <data_dir>/gateway/access.log
# max_log_mib = 256                             # rotate here, one .1 kept
# trusted_proxies = ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"]
```

`C0MPUTE_THREATCRUSH=1|0` overrides `enabled`.

One line per request, in nginx's `vhost` format, which ThreatCrush already
parses:

```
gw.example:7777 45.33.22.11 - - [10/Nov/2026:13:55:36 +0000] "GET /s/53021d6e8d5a0e1aac48/ HTTP/1.1" 402 614 "-" "GPTBot/1.2"
```

The client address is the TCP peer. When the peer is a **trusted proxy** (by
default loopback and private ranges, such as the Moshpit edge on the same box),
it is `X-Real-IP` or the last `X-Forwarded-For` hop instead. Without this
ThreatCrush would see every request as coming from your own proxy and could
ban it. A peer that is not trusted is logged as itself, whatever headers it
sends.

The writer runs on its own thread behind a bounded queue. Under a flood it
drops lines rather than slow requests down.

## With x402

The two plugins work together. Paying crawlers show up as 402 then 200. A
client that keeps probing after its 402s, or that sends attack payloads, is
ThreatCrush's to ban. ThreatCrush rates 402s `low`, and its default
`min_severity = "high"` keeps them off the ban ladder, so a crawler that only
declines to pay is never banned for it.
