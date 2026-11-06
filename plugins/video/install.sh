#!/usr/bin/env sh
# video plugin installer.
#
# Unlike the in-process plugins there is nothing to compile: the workload
# runs in a container. So this script checks the three things a node needs to
# serve video jobs and pulls the image. It deliberately does NOT install a
# GPU driver or a container runtime — both are host decisions with reboots
# attached, and guessing wrong on someone's box is worse than telling them.
#
# Served at https://c0mpute.com/plugins/video/install.sh.
# Source: https://github.com/profullstack/c0mpute/tree/master/plugins/video
set -eu

IMAGE="${C0MPUTE_VIDEO_IMAGE:-ghcr.io/profullstack/c0mpute-video-runner:candidate}"
SOURCE_REPO="${C0MPUTE_VIDEO_SOURCE_REPO:-https://github.com/profullstack/c0mpute.git}"

say()  { printf '\033[1;36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

missing=0

if command -v docker >/dev/null 2>&1; then
  ok "container runtime present ($(docker --version 2>/dev/null | head -1))"
else
  warn "no docker on PATH — install a container runtime, then re-run this script"
  missing=1
fi

if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi --query-gpu=name,memory.total \
     --format=csv,noheader >/dev/null 2>&1; then
  ok "NVIDIA GPU: $(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader | head -1)"
  # 24 GiB is the floor for the default model (wan2.2-ti2v-5b). A smaller
  # card can hold the plugin but will lose or fail jobs, so say so now.
  vram_mib=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits | head -1 | tr -d ' ')
  case "$vram_mib" in
    ''|*[!0-9]*) : ;;
    *) [ "$vram_mib" -lt 23000 ] && warn "this card has ${vram_mib} MiB; the default model wants ~24 GiB" ;;
  esac
else
  warn "no NVIDIA GPU detected — video jobs need one (the runner asks docker for --gpus all)"
  missing=1
fi

if [ "$missing" -eq 0 ]; then
  say "pulling $IMAGE (several GB, once)"
  if ! docker pull "$IMAGE"; then
    # The registry is a convenience, not a dependency: private, rate-limited
    # or offline is no reason a box with a card cannot serve video. This repo
    # is public, so build the same image from source.
    warn "could not pull $IMAGE (private, rate-limited or offline)"
    if command -v git >/dev/null 2>&1; then
      say "building it from source instead (several minutes)"
      src=$(mktemp -d)
      # shellcheck disable=SC2064
      trap "rm -rf '$src'" EXIT INT TERM
      if git clone --depth 1 "$SOURCE_REPO" "$src" \
         && docker build -t "$IMAGE" -f "$src/plugins/video/runner/Dockerfile" \
              "$src/plugins/video"; then
        ok "runner image built from source"
      else
        warn "could not build the runner image"
        missing=1
      fi
    else
      warn "install git to build the runner from source, then re-run this script"
      missing=1
    fi
  else
    ok "runner image pulled"
  fi
fi

if [ "$missing" -eq 0 ]; then
  say "selftesting the image (no GPU needed)"
  if docker run --rm "$IMAGE" selftest; then
    ok "runner works on this box"
  else
    warn "the runner image failed its own selftest; do not enable the role yet"
    missing=1
  fi
fi

cat <<EOF

video runs Wan 2.2 in $IMAGE.
Models, and the VRAM each needs: https://c0mpute.com/plugins

Check this node:        c0mpute video check --selftest
List the model options: c0mpute video models
Serve video jobs:       c0mpute worker start --roles video

The first job of each model pulls tens of GB of weights into the node's
weights cache. That pull is billed like inference on a rented card, so warm
the cache before you advertise:
  c0mpute video check --selftest
EOF

exit "$missing"
