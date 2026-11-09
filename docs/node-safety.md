# Running a node: what it can touch

A c0mpute node runs work for strangers on your machine. That deserves a plain
answer to "what am I trusting?", so here it is. If anything below is wrong,
open an issue; this page is meant to be checkable against the code.

## What the installer does

`curl -fsSL https://c0mpute.com/install.sh | sh` (source: [`scripts/install.sh`](../scripts/install.sh)):

- Downloads the `c0mpute`, `infernet` and `coinpay` release binaries and
  **verifies each one's minisign signature** against the public key in
  [`minisign.pub`](../minisign.pub) before installing it. A bad signature aborts the install.
- Installs everything under your home directory: binaries in `~/.c0mpute/bin`,
  data in `~/.local/share/c0mpute` (or a bigger volume it symlinks, unless
  `NO_STORAGE_RELOCATE=1`).
- Starts the worker as **your user, not root**: a systemd *user* service
  (`~/.config/systemd/user/c0mpute-worker.service`), with systemd lingering so
  it starts at boot. Without systemd it runs as a background daemon.
- Uses `sudo` only if it is already configured, and only for conveniences:
  a `/usr/local/bin` symlink and a `/etc/profile.d` PATH hook, plus the
  system service if you ask for one with `--system-service`. It never runs the
  worker as root. Skip all of it by running the installer without sudo.

## What the worker runs

- Jobs are the workload types your node advertises (`c0mpute worker status`
  lists them): inference through Infernet, FFmpeg transcodes, storage shards,
  LoRA training, speech, music and image generation. They are run by the
  installed, signed binaries; a buyer sends data and parameters, not code to execute.
  Container workloads (video, game servers) start only pinned, well-known images, and only
  when Docker or Podman already works on the box.
- Jobs run as child processes of the worker with CPU-thread and GPU limits.
  **This is not a sandbox.** A job runs with your user's permissions, so a
  bug in a workload runtime could read files your user can read. If that is
  not acceptable for your main machine, run the node in a VM, a container or
  on a spare box. It earns the same there.
- The node keeps itself up to date (`C0MPUTE_NO_AUTO_UPDATE=1` turns that
  off). Updates are verified against the same minisign key before the binary
  is swapped.

## Models it serves

- **House models** (on by default): the models Infernet lists at
  [infernetprotocol.com/house-models.json](https://infernetprotocol.com/house-models.json)
  that fit your RAM and leave 10% of the disk free. Turn them off with
  `infernet house-models off`, or `--no-house-models` at install.
- **Uncensored house models are off unless you opt in** (`--uncensored-models`
  at install, or `infernet house-models uncensored on`).

## Network

Outbound connections only; no port forwarding needed. Some workloads make
outbound HTTP requests on a buyer's behalf from your IP (`crawl.fetch`,
`workflow.step` http calls). Those are limited to public addresses, capped in size and
time, and are listed in `c0mpute worker status` like every other workload. The node joins the
libp2p mesh and advertises its capabilities, benchmark score and (if you set
them) a nickname and referral handle, which appear on the public
[node board](https://c0mpute.com/nodes).

## Money: what pays today

- **Paid inference** through Infernet pays out via CoinPay to the address you
  set with `infernet payout set <coin> <address>`.
- **Hosting and ads** settlement through CoinPay is built (PRs #162, #163,
  #166) but **off by default** while it is tested.
- **Storage payouts** ([CIP-006](prds/006-challenges-metering-payouts.md)) are
  not paying yet.

The network is small and paid demand is early. Expect small, irregular
earnings for now, and budget for the electricity and wear your hardware takes.

## Removing it

```sh
systemctl --user disable --now c0mpute-worker   # or: c0mpute worker stop
rm -rf ~/.c0mpute ~/.local/share/c0mpute ~/.config/systemd/user/c0mpute-worker.service
loginctl disable-linger "$USER"                 # if nothing else of yours needs it
```

With a system service, also run
`sudo systemctl disable --now c0mpute-worker && sudo rm /etc/systemd/system/c0mpute-worker.service`.
Remove Infernet with `infernet uninstall`.
