#!/bin/sh
# Run the image like the scorer: drun.sh DATA_DIR OUTPUT_DIR
# Env: IMAGE (default sessions:amd), PLATFORM (linux/amd64), CPUS (8), MEM (12g), SCRATCH_ROOT.
set -eu
IMAGE="${IMAGE:-sessions:amd}"
PLATFORM="${PLATFORM:-linux/amd64}"
CPUS="${CPUS:-8}"
MEM="${MEM:-12g}"
DATA=$(cd "$1" && pwd)
mkdir -p "$2"; OUT=$(cd "$2" && pwd)
SCRATCH=$(mktemp -d "${SCRATCH_ROOT:-${TMPDIR:-/tmp}}/scratch.XXXXXX")
chmod 777 "$OUT" "$SCRATCH"
trap 'rm -rf "$SCRATCH" 2>/dev/null || true' EXIT
docker run --rm --platform "$PLATFORM" \
  --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 \
  --cap-drop ALL --security-opt no-new-privileges \
  --pids-limit 4096 --cpus "$CPUS" --memory "$MEM" --memory-swap "$MEM" \
  --user 1000:1000 \
  -v "$DATA":/data:ro -v "$OUT":/output -v "$SCRATCH":/scratch \
  -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch \
  -e NCPU="${CPUS%.*}" -e MEM_GB="${MEM%g}" ${EXTRA_ENV:-} \
  "$IMAGE"
