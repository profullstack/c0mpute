# Running a node: what it can touch

A c0mpute node runs work for strangers on your machine. That deserves a plain
answer to "what am I trusting?", so here it is. If anything below is wrong,
open an issue; this page is meant to be checkable against the code.

## What the installer does

`curl -fsSL https://c0mpute.com/install.sh | sh` (source: [`scripts/install.sh`](../scripts/install.sh)):

- Downloads the `c0mpute`, `infernet` and `coinpay` release binaries and
  **verifies each one's minisign signature** against the public key in
  [`minisign.pub`](../minisign.pub) before installing it. A missing or bad signature aborts the
  install. Without minisign installed, it downloads a pinned minisign build (checked by sha256)
  to do the check; on Intel Macs, which upstream does not build for, it asks you to
  `brew install minisign` first.
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
  **This is not a sandbox** (Blender renders are the exception, below). A job
  runs with your user's permissions, so a bug in a workload runtime could read
  files your user can read. If that is not acceptable for your main machine,
  run the node in a VM, a container or on a spare box. It earns the same there.
- The node keeps itself up to date (`C0MPUTE_NO_AUTO_UPDATE=1` turns that
  off). From v0.2.91 on, updates are verified against the same minisign key before
  the binary is swapped; a node on an older version checks only the sha256 until it
  is on v0.2.91 or later.
- `c0mpute plugin install` still runs the plugin's own install script without a
  signature check. Only install plugins you trust.

## Blender renders

`render` is a default role on any machine with `blender` installed. A buyer
sends a `.blend` scene, and a scene is more than pixels: it names files by
path (textures, movie clips, fonts, linked libraries, caches) and can ask
Blender to write files. The node does two things about that
(source: [`render.rs`](../node/crates/c0mpute-core/src/render.rs)).

**Scene sanitisation, always, on every OS.** Blender starts with
`--disable-autoexec`, so no Python in the scene runs. After the file loads and
before the render, the node runs its own fixed script (`SANITIZE_EXPR`; nothing
from the job is written into it) that:

- empties every file path that is absolute or leaves the job directory: images,
  movie clips, sounds, fonts, volumes, Alembic/USD cache files, sequencer
  strips and their proxies, point caches, shader nodes that read a file;
- deletes everything that came from a linked library outside the job directory;
- removes compositor File Output nodes, turns Open Shading Language off (a
  compiled shader can open any image by name), and points fluid caches and
  every scene's output at the job directory;
- turns off the render stamps that would write this machine's hostname or the
  scene's path on your disk into the image.

Data **packed** into the `.blend` is untouched, and an honest scene renders the
same pixels as before. A scene that needed a file it did not pack renders with
Blender's pink "missing" texture (or the built-in font, or without that strip):
buyers must pack their assets. If the script cannot finish, the frame is not
rendered at all.

**A sandbox, on Linux, where bubblewrap works.** At start the node tries to run
Blender inside [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`)
and, if that works, renders that way: no network, its own process and hostname
namespaces, a read-only view of the system directories and of Blender's own
install directory, an empty private `/tmp` and home directory, and the job
directory as the only thing it can write. GPU device nodes (`/dev/dri`,
`/dev/nvidia*`) and driver libraries stay visible so GPU renders keep working;
a GPU job the sandbox cannot serve (the probe checks) runs outside it.
`c0mpute` logs which one it is at start. Install it with
`sudo apt install bubblewrap`.

| | sanitisation only | with bubblewrap |
|---|---|---|
| Read a file the scene names by path into a frame | blocked by the script, for every kind of path listed above | blocked twice (the file is not there) |
| Write outside the job directory when rendering | blocked by the script, for the ways listed above (output path, File Output nodes, fluid caches) | not possible |
| What Blender does while **opening** the file, before the script runs: open a linked library by path (its data is deleted before rendering), create an empty cache directory where the scene says | **not confined** | confined |
| A bug in Blender's file readers turned into code execution | **not confined**: runs as your user | no network, no home directory, read-only system; it can still read world-readable system files and use the GPU |
| CPU, RAM and disk the render uses | thread limit and timeout only | the same |
| macOS, Windows | this column | not available |

The sandbox is not used when `bwrap` is missing, when unprivileged user
namespaces are switched off (Ubuntu 24.04 does this with AppArmor unless
bubblewrap's profile is installed), or when Blender is a snap or sits behind a
wrapper script the sandbox cannot follow. Choose with:

```toml
[render]
sandbox = "auto"     # default: bubblewrap when it works, else sanitisation only
# sandbox = "require"  # serve no render (and advertise none) without the sandbox
# sandbox = "off"      # never use bubblewrap
```

`C0MPUTE_RENDER_SANDBOX` overrides the file.

## Models it serves

- **House models** (on by default): the models Infernet lists at
  [infernetprotocol.com/house-models.json](https://infernetprotocol.com/house-models.json)
  that fit your RAM and leave 10% of the disk free. Turn them off with
  `infernet house-models off`, or `--no-house-models` at install.
- **Uncensored house models are off unless you opt in** (`--uncensored-models`
  at install, or `infernet house-models uncensored on`).
- **Text fine-tunes** (`c0mpute infernet text serve`, opt-in) download the
  buyer's base model from Hugging Face and load it. A worker does that only
  for the models you list; a job naming another gets no bid:

  ```toml
  [text]
  # The default: the two small models c0mpute's own commands and docs use.
  base_models = ["HuggingFaceTB/SmolLM2-135M-Instruct", "Qwen/Qwen2.5-0.5B-Instruct"]
  # "org/*" allows every model of one organisation, "*" any model.
  min_free_gb = 0      # free disk kept; 0 = the smaller of 20 GiB and 10% of the disk
  ```

  A job config may set only the keys c0mpute's clients send (`name`, `kind`,
  `base_model`, `system_prompt`, `limits.timeout_s` and the hyperparameters
  `lr`, `batch_size`, `epochs`, `seq_len`, `lora_r`, `max_steps`, `seed`, each
  within a fixed range); anything else is refused before the trainer sees it.
  The downloaded models live in Infernet's Hugging Face cache, which c0mpute
  does not size or clean.

## Content policy (images and video)

Every image and video job passes a child-safety guard on the gateway that
auctions it and again on your worker before it runs: sexual or nudity terms
combined with any minor indicator are refused, always, with no setting to turn
it off. Without adult mode your worker also refuses explicit prompts, adds NSFW
terms to the negative prompt, and checks every output with a small CPU NSFW
classifier (AdamCodd/vit-base-nsfw-detector, Apache-2.0, int8 ONNX pinned by
SHA-256, installed under `~/.local/share/c0mpute/nsfw`); an explicit output is
discarded and the job fails with a policy reason.

**Every node serves adult jobs; there is no per-node switch.** Since v0.3.38
every worker takes adult (18+) image and video jobs and advertises
`c0mpute:adult`. Jobs are end-to-end encrypted between buyer and worker. The
child-safety guard runs on every job and cannot be turned off.

A gateway accepts adult requests only with `[adult] buyer = true` (or
`C0MPUTE_ADULT_BUYER=1`) and awards them only to nodes whose own capability ad
carries that tag. Adult jobs are text only (no image inputs) and requests that
point at a real, identifiable person are refused; that check is a word-list
heuristic, documented in `node/crates/c0mpute-core/src/content_policy.rs`.
Refusals are counted by reason (`GET /v1/policy` on a gateway) and logged with a
hashed requester id, never the prompt.

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
