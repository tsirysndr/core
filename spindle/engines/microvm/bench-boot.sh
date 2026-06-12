#!/usr/bin/env bash
# quick boot-time benchmark for the spindle nixos microvm.
# boots N times running a trivial command, reports wall-clock + systemd-analyze.
# needs: sudo modprobe vhost_vsock
set -euo pipefail

N="${1:-5}"
cd "$(git rev-parse --show-toplevel)"

strip_ansi() { sed -E "s/$(printf '\033')\[[0-9;]*[a-zA-Z]//g; s/$(printf '\033')\([a-zA-Z]//g"; }

echo ">>> building runner + image"
nix develop --command go build -o spindle/spindle-microvm-run ./cmd/spindle-microvm-run
TARBALL=$(nix build .#spindle-nixos-image-tarball --no-link --print-out-paths)

WORK=$(mktemp -d -t spindle-bench-XXXXXX)
trap 'chmod -R +w "$WORK" 2>/dev/null || true; rm -rf "$WORK"' EXIT
mkdir -p "$WORK/image"
tar -C "$WORK/image" -xzf "$TARBALL"
SPEC="$WORK/image/spec.json"

echo ">>> systemd-analyze breakdown"
spindle/spindle-microvm-run --image-spec "$SPEC" --work-dir "$WORK/analyze" --exec-timeout 60s -- \
  /run/current-system/sw/bin/systemd-analyze time 2>/dev/null | strip_ansi | grep -i startup || true

echo ">>> $N timed boot+exec(true) runs"
total=0
for i in $(seq 1 "$N"); do
  start=$EPOCHREALTIME
  spindle/spindle-microvm-run --image-spec "$SPEC" --work-dir "$WORK/run$i" --exec-timeout 60s -- \
    /run/current-system/sw/bin/true >/dev/null 2>&1
  end=$EPOCHREALTIME
  ms=$(( (${end%.*} - ${start%.*}) * 1000 + (10#${end#*.} - 10#${start#*.}) / 1000 ))
  echo "  run $i: ${ms}ms"
  total=$((total + ms))
  rm -rf "$WORK/run$i"
done
echo ">>> mean wall-clock: $((total / N))ms over $N runs"
