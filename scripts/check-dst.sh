#!/usr/bin/env bash
# Run a schedule across a daylight-saving change, on this machine's own time
# zone database, through the same chrono::Local the gateway uses.
#
# The rest of the schedule tests use the copy of the IANA database compiled
# into chrono-tz, which is deterministic and is not what a gateway at a site
# will be reading. This reads /usr/share/zoneinfo like the gateway does.
set -euo pipefail

cd "$(dirname "$0")/.."

ZONE="${TZ:-Europe/Madrid}"
echo "== nine to five across the clocks going back, in $ZONE"
TZ="$ZONE" cargo test -p vms-gateway -- --ignored --nocapture --test-threads=1 \
    this_box_keeps_nine_to_five_across_its_own_daylight_saving_change
