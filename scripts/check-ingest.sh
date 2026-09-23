#!/usr/bin/env bash
# Push video into the gateway the way an encoder does, with ffmpeg.
#
# Everything else about RTMP and SRT ingest is tested against doubles this
# repository wrote: rml_rtmp's own client half, an srt-tokio caller, a
# transport stream ffmpeg produced earlier. Doubles agree with the code that
# expects them. This does not.
set -euo pipefail

if ! command -v ffmpeg >/dev/null; then
    echo "ffmpeg is not on PATH; this check is what it is for." >&2
    exit 1
fi

for protocol in rtmp srt; do
    if ! ffmpeg -hide_banner -protocols 2>/dev/null | tr ' ' '\n' | grep -qx "$protocol"; then
        echo "this ffmpeg cannot speak $protocol" >&2
        exit 1
    fi
done

echo "== ffmpeg $(ffmpeg -version | head -1 | cut -d' ' -f3) publishing to the gateway"
cargo test -p vms-gateway --features srt -- --ignored --nocapture ffmpeg_can_
