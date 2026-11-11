#!/bin/bash
# Boot a rented-GPU c0mpute video worker: warm the weights, then join.
set -u
LOGS=/logs
# Auto-update: the image's binary lives in a root-owned dir, so run a copy
# from $HOME/bin, which the worker can swap in place when a release lands
# (it re-execs the new binary). C0MPUTE_NO_AUTO_UPDATE=1 pins the image's.
mkdir -p "$HOME/bin"
cp -n /usr/local/bin/c0mpute "$HOME/bin/c0mpute"
export PATH="$HOME/bin:$PATH"
mkdir -p "$LOGS" /weights/hf
# Read-only status page (Salad's gateway puts its own auth in front).
python3 -m http.server "${STATUS_PORT:-8080}" --bind :: -d "$LOGS" >/dev/null 2>&1 &
exec > >(tee -a "$LOGS/boot.log") 2>&1

echo "== $(date -u +%FT%TZ) c0mpute video worker boot ($(c0mpute --version))"
nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv,noheader || echo "nvidia-smi failed"
python3 -c "import torch; print('torch', torch.__version__, 'cuda', torch.cuda.is_available(), torch.cuda.get_device_name(0) if torch.cuda.is_available() else '-')"
df -h / /weights | tail -n +1
free -g | head -2

# The bench runs once per version before a worker advertises; do it while
# the weights download instead of after.
(c0mpute bench > "$LOGS/bench.log" 2>&1; echo "bench exit $?" >> "$LOGS/bench.log") &
bench_pid=$!

echo "== $(date -u +%FT%TZ) prefetching: ${C0MPUTE_PREFETCH_MODELS}"
python3 /opt/c0mpute-salad/prefetch.py ${C0MPUTE_PREFETCH_MODELS} || echo "prefetch failed (the runner will pull on first job)"
echo "== $(date -u +%FT%TZ) prefetch done"
# Only the bench: a bare `wait` would also wait on the status server forever.
wait "$bench_pid"
c0mpute video check || true

# C0MPUTE_ROLES adds work the card is idle for between clips: "video,music"
# renders ACE-Step songs on CUDA in well under a minute (a CPU node takes
# 11-30 min); "tts" serves the expressive Chatterbox voices (provisioned by
# `worker start`). Default stays video only.
ROLES="${C0MPUTE_ROLES:-video}"
echo "== $(date -u +%FT%TZ) starting worker (roles: $ROLES)"
c0mpute worker start --roles "$ROLES" --no-storage --no-hosting 2>&1 | tee -a "$LOGS/worker.log"
echo "== $(date -u +%FT%TZ) worker exited ${PIPESTATUS[0]}"
exit 1
