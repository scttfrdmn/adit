#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Pair health gate (#18): one 4 GiB write-only NIXL probe, proxy DRAM -> client VRAM. Fails unless
# it reaches MIN_GBPS and the proxy's EFA write-error counter did not move. A degraded pair once
# produced a full ladder of plausible-looking rows at 13.6 Gb/s; this stops that before it starts.
#
#   ADIT_SPAWN=... bash hack/health.sh <client-box> <proxy-box> <client-ip> [min_gbps=180]
set +e
set -uo pipefail
echo "opts=$-"
CLIENT=$1 PROXY=$2 CIP=$3 MIN=${4-180}
line=$(bash "$(dirname "$0")/wseries.sh" "$CLIENT" "$PROXY" "$CIP" 1 0 4096 32 health | grep '^health,' | tail -1)
echo "health: $line"
gbps=$(cut -d, -f7 <<<"$line")
werr=$(cut -d, -f9 <<<"$line")
verdict=$(cut -d, -f12 <<<"$line")
if [[ -z $gbps || $verdict != PASS || ${werr:-1} != 0 ]] || ! awk -v g="$gbps" -v m="$MIN" 'BEGIN{exit !(g>=m)}'; then
    echo "HEALTH FAIL: gbps=${gbps:-none} (min $MIN) wr_err_delta=${werr:-?} verdict=${verdict:-none}"
    exit 1
fi
echo "HEALTH OK: gbps=$gbps"
