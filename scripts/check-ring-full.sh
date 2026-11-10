#!/usr/bin/env bash
# Fill a real filesystem underneath the ring buffer.
#
# Everything else about the ring is tested against its byte budget, which is
# a number this code chose. A filesystem saying ENOSPC is not a number this
# code chose, and the first time it was tried the recording simply ended:
# the disk stayed full and no video was kept.
#
# No root needed. A user namespace can mount a tmpfs of its own, which is a
# real filesystem that really runs out.
set -euo pipefail

cd "$(dirname "$0")/.."

SIZE="${RING_FS_SIZE:-8m}"

if ! unshare --user --map-root-user --mount true 2>/dev/null; then
    echo "this needs unprivileged user namespaces, which this kernel or policy does not allow." >&2
    echo "the same test runs with any small filesystem:" >&2
    echo "  RING_FULL_DIR=/mnt/something-small cargo test -p vms-gateway -- --ignored a_full_filesystem" >&2
    exit 1
fi

echo "== a $SIZE filesystem, filled by the ring it is too small for"
cargo build -q --tests -p vms-gateway
unshare --user --map-root-user --mount sh -c "
    set -e
    mkdir -p /tmp/ring-full-check
    mount -t tmpfs -o size=$SIZE tmpfs /tmp/ring-full-check
    RING_FULL_DIR=/tmp/ring-full-check cargo test -p vms-gateway -- --ignored a_full_filesystem --nocapture
    echo
    echo \"   held afterwards: \$(ls /tmp/ring-full-check/camera-1/*.m4s | wc -l) segments\"
    df -h /tmp/ring-full-check | tail -1
"
