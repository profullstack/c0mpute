#!/bin/bash
# Boot a rented-GPU c0mpute video worker: warm the weights, then join.
set -u
LOGS=/logs
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

echo "== $(date -u +%FT%TZ) prefetching: ${C0MPUTE_PREFETCH_MODELS}"
python3 /opt/c0mpute-salad/prefetch.py ${C0MPUTE_PREFETCH_MODELS} || echo "prefetch failed (the runner will pull on first job)"
echo "== $(date -u +%FT%TZ) prefetch done"
wait
c0mpute video check || true

echo "== $(date -u +%FT%TZ) starting worker (video only)"
c0mpute worker start --roles video --no-storage --no-hosting 2>&1 | tee -a "$LOGS/worker.log"
echo "== $(date -u +%FT%TZ) worker exited ${PIPESTATUS[0]}"
exit 1
