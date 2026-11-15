#!/bin/sh
# DEMO_CAMERAS test patterns, each with its name and a running clock burned in
# so a viewer can tell live video from a loop, pushed to the gateway over RTMP
# as demo-1, demo-2, ...
#
# Baseline profile and no B-frames, because that is what every browser's
# WebRTC H.264 decoder takes. The gateway refuses a stream key it has not been
# told about, so until up.sh has registered the sources each publisher fails
# and tries again a few seconds later.
set -u

count="${DEMO_CAMERAS:-3}"
url="${RTMP_URL:?RTMP_URL is the gateway ingest, e.g. rtmp://127.0.0.1:1935/live}"
font=/usr/share/fonts/dejavu/DejaVuSans.ttf

pattern() {
    case $(( ($1 - 1) % 4 )) in
        0) echo testsrc2 ;;
        1) echo smptehdbars ;;
        2) echo testsrc ;;
        *) echo rgbtestsrc ;;
    esac
}

publish() {
    i="$1"
    while :; do
        ffmpeg -hide_banner -loglevel warning -nostdin -re \
            -f lavfi -i "$(pattern "$i")=size=640x360:rate=15" \
            -vf "drawtext=fontfile=$font:text='Demo camera $i   %{localtime} UTC':x=16:y=16:fontsize=20:fontcolor=white:box=1:boxcolor=black@0.6:boxborderw=6" \
            -c:v libx264 -preset ultrafast -tune zerolatency -profile:v baseline -pix_fmt yuv420p \
            -g 30 -bf 0 -b:v 600k -maxrate 600k -bufsize 600k \
            -f flv "$url/demo-$i"
        sleep 5
    done
}

if [ "$count" -le 0 ]; then
    echo "DEMO_CAMERAS=$count: no demo cameras"
    exec sleep infinity
fi

i=1
while [ "$i" -le "$count" ]; do
    publish "$i" &
    i=$(( i + 1 ))
done
wait
