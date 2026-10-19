#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path E: adit. The proxies must already be serving (hack/g0c.sh starts one per rung; G2's stripe
# ladder starts N). This runner reads the plan into the GPU through them and checks every object.
#
# Path knobs:
#   PROXIES        comma-separated proxy base URLs (required)
#   CHUNK_MIB      chunk requested of proxies (default 8)
#   CONCURRENCY    objects in flight (default 64)
#   STRIPE         1 to stripe each object across all proxies (default 0)
#   BUF_GIB        GPU destination buffer (default: the plan's total, capped at 80)
#   PROGRESS_US    NIXL progress-thread delay (default 0 = NIXL default; see #17)
P=E
source "$(dirname "$0")/../lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
PROXIES=${PROXIES:?proxy URLs}
CHUNK_MIB=${CHUNK_MIB:-8}
CONCURRENCY=${CONCURRENCY:-64}
STRIPE=${STRIPE:-0}
PROGRESS_US=${PROGRESS_US:-0}
total_gib=$(( ($(jq -s 'map(.size)|add' "$PLAN") >> 30) + 1 ))
BUF_GIB=${BUF_GIB:-$(( total_gib < 80 ? total_gib : 80 ))}
stripe=(); [[ $STRIPE == 1 ]] && stripe=(--stripe)
cold
adit-get --proxies "$PROXIES" --plan "$PLAN" --agent-name "adit-$WORKLOAD-$$" --buffer-gib "$BUF_GIB" \
    --chunk-mib "$CHUNK_MIB" --concurrency "$CONCURRENCY" "${stripe[@]}" --runs "$RUNS" \
    --nixl-progress-delay-us "$PROGRESS_US" "${META[@]}" --out "$OUT" 2>&1 | grep -E '^E,|rror'
log "done: $OUT"
