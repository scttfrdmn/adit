#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# #50 mechanism check, on the GPU client: the same E read of a plan unpinned (several proxies may
# write into one NIXL agent) and pinned (--pin-gpu-proxy: one proxy per agent), counting NIXL's
# "Duplicate fragment received" warnings per arm. The id-collision theory predicts warnings and
# partial objects only unpinned. Rows go to OUT; NIXL stderr to OUT's -c50-<arm>.err files.
#
#   PLAN=... PROXIES=http://a:8480,... DEVICES=0,...,7 bash bench/paths/confirm50.sh T2-64
P=E
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan}
PROXIES=${PROXIES:?proxy URLs}
DEVICES=${DEVICES:-0}
BUF_GIB=${BUF_GIB:-64}
E_CONCURRENCY=${E_CONCURRENCY:-128}
export NIXL_LOG_LEVEL=${NIXL_LOG_LEVEL:-WARN}
for m in unpinned pinned; do
    pin=()
    [[ $m == pinned ]] && pin=(--pin-gpu-proxy)
    err=${OUT%.csv}-c50-$m.err
    cold
    adit-get --proxies "$PROXIES" --plan "$PLAN" --agent-name "c50$m$$" --agent-per-gpu "${pin[@]}" --devices "$DEVICES" \
        --buffer-gib "$BUF_GIB" --concurrency "$E_CONCURRENCY" --runs 1 --nixl-progress-delay-us 100 \
        "${META[@]}" --meta "client_instance=$CLIENT_INSTANCE" --out "$OUT" 2>"$err" | grep -E '^E,' | cut -d, -f11-15,23
    echo "$m: exit=${PIPESTATUS[0]} duplicate_fragment_warnings=$(grep -c 'Duplicate fragment' "$err")"
done
