#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Interleaved comparison on one workload (pre-G2, #45): arms run in a rotated order every cycle so
# time-of-session effects (S3 warm-up on a key read repeatedly) spread evenly across arms instead
# of confounding them. Runs on the GPU client; proxies must already be serving.
#
#   PLAN=... PROXY1=http://ip:8480 PROXY2=http://ip:8480 CYCLES=6 bash bench/paths/interleave.sh T1
#
# Arms (ARMS to override): B512 B1024 (KvikIO RemoteFile, 512/1024 threads, 64 MiB tasks),
# E1 (adit, PROXY1 only), E2 (adit, PROXY1+PROXY2, stripe). Each arm = one read of the workload,
# one results row, the same oracle and counter window as the per-path runners.
P=X
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
PROXY1=${PROXY1:?}
PROXY2=${PROXY2:?}
CYCLES=${CYCLES:-6}
ARMS=(${ARMS:-B512 B1024 E1 E2})
PY=${PY:-/opt/adit-venv/bin/python}
n=${#ARMS[@]}
arm() {
    local a=$1 c=$2
    cold
    case $a in
        B512|B1024)
            local th=${a#B}
            $PY bench/paths/lib/gpuload.py --mode remotefile --plan "$PLAN" --threads "$th" --task-size-mib 64 \
                --concurrency 1 --region "$REGION" --runs 1 --meta path=B --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>&1 | tail -1 ;;
        E1|E2)
            local px=$PROXY1 st=()
            [[ $a == E2 ]] && { px=$PROXY1,$PROXY2; st=(--stripe); }
            adit-get --proxies "$px" "${st[@]}" --plan "$PLAN" --agent-name "il-$a-$c-$$" --buffer-gib 65 --chunk-mib 8 \
                --concurrency 1 --runs 1 --meta path=E --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>&1 | grep -E '^E,' | tail -1 ;;
        *) log "unknown arm $a"; return 1 ;;
    esac
}
for c in $(seq 1 "$CYCLES"); do
    for k in $(seq 0 $((n - 1))); do
        a=${ARMS[$(( (k + c - 1) % n ))]}
        printf '%s cycle=%s arm=%s ' "$(date -u +%T)" "$c" "$a" | tee -a "${OUT%.csv}.order"
        arm "$a" "$c" | cut -d, -f13,14,15,23 | tee -a "${OUT%.csv}.order"
    done
done
log "done: $OUT (order and timestamps: ${OUT%.csv}.order)"
