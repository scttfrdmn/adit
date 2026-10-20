#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Interleaved comparison on one workload (pre-G2 #45, G2 #48): arms run in a rotated order every
# cycle so time-of-session effects (S3 warm-up on a key read repeatedly) spread evenly across arms
# instead of confounding them. Runs on the GPU client; proxies must already be serving.
#
#   PLAN=... PROXIES=http://a:8480,http://b:8480,... CYCLES=3 ARMS="E8 E4 E2 E1" bash bench/paths/interleave.sh T2
#   PLAN=... PROXY1=http://a:8480 PROXY2=http://b:8480 CYCLES=6 bash bench/paths/interleave.sh T1
#
# Arms (ARMS to override; default B512 B1024 E1 E2):
#   B<threads>  KvikIO RemoteFile, <threads> threads, 64 MiB tasks, objects round-robin over DEVICES
#   E<n>        adit through the first n of PROXIES (PROXY1,PROXY2 when PROXIES is unset); with
#               STRIPE=1 (default) and n > 1 each object is striped across the n proxies
# Each arm = one read of the workload, one results row, the same oracle and counter window as the
# per-path runners.
#
# Knobs: DEVICES (GPUs, default 0), BUF_GIB (per GPU, default 65), CHUNK_MIB (E, default 8), B_CONCURRENCY (objects in flight
# for B, default 1), E_CONCURRENCY (default 1), STRIPE, E_ARGS (extra adit-get flags, e.g.
# "--spread --chunk-mib 16 --nixl-progress-delay-us 100").
P=X
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
PROXIES=${PROXIES:-${PROXY1:?PROXIES or PROXY1}${PROXY2:+,$PROXY2}}
IFS=, read -r -a PX <<<"$PROXIES"
CYCLES=${CYCLES:-6}
ARMS=(${ARMS:-B512 B1024 E1 E2})
DEVICES=${DEVICES:-0}
BUF_GIB=${BUF_GIB:-65}
B_CONCURRENCY=${B_CONCURRENCY:-1}
E_CONCURRENCY=${E_CONCURRENCY:-1}
STRIPE=${STRIPE:-1}
E_ARGS=${E_ARGS:-}
CHUNK_MIB=${CHUNK_MIB:-8}
PY=${PY:-/opt/adit-venv/bin/python}
n=${#ARMS[@]}
# Per-arm snapshots (law 6, #51): every proxy's /metrics (pipeline histograms, its EFA counters) and
# the client's EFA hw_counters per device, before and after each E arm, under ${OUT%.csv}-snap/.
SNAP=${OUT%.csv}-snap
snap() {
    # Every loop variable is local: bash scoping is dynamic, and arm()'s cycle counter is `c`
    # (a global `c` here once redirected each arm's stderr into a sysfs path, skipping the arm).
    local tag=$1 p d f i=0
    shift
    mkdir -p "$SNAP/$tag"
    for p in "$@"; do
        curl -s -m 10 "$p/metrics" >"$SNAP/$tag/proxy-$i.txt"
        i=$((i + 1))
    done
    for d in /sys/class/infiniband/*; do
        for f in "$d"/ports/1/hw_counters/*; do printf '%s %s %s\n' "$(basename "$d")" "$(basename "$f")" "$(cat "$f")"; done
    done >"$SNAP/$tag/client-efa.txt"
}
arm() {
    local a=$1 c=$2
    cold
    case $a in
        B[0-9]*)
            local th=${a#B}
            $PY bench/paths/lib/gpuload.py --mode remotefile --plan "$PLAN" --threads "$th" --task-size-mib "${B_TASK_MIB:-64}" \
                --devices "$DEVICES" --buffer-gib "$BUF_GIB" --concurrency "$B_CONCURRENCY" --region "$REGION" --runs 1 \
                --meta path=B --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>&1 | tail -1 ;;
        E[0-9]*)
            local k=${a#E} st=()
            (( k >= 1 && k <= ${#PX[@]} )) || { log "arm $a: only ${#PX[@]} proxies"; return 1; }
            local px
            px=$(IFS=,; echo "${PX[*]:0:k}")
            [[ $STRIPE == 1 && $k -gt 1 ]] && st=(--stripe)
            snap "c$c-$a-pre" "${PX[@]:0:k}"
            # NIXL warnings (e.g. "Duplicate fragment received": #50) land in the arm's stderr file.
            export NIXL_LOG_LEVEL=${NIXL_LOG_LEVEL:-WARN}
            # shellcheck disable=SC2086  # E_ARGS is a flag list
            adit-get --proxies "$px" "${st[@]}" --plan "$PLAN" --agent-name "il-$a-$c-$$" --devices "$DEVICES" \
                --buffer-gib "$BUF_GIB" --chunk-mib "$CHUNK_MIB" --concurrency "$E_CONCURRENCY" --runs 1 $E_ARGS \
                --meta path=E --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>"$SNAP/c$c-$a.stderr" | grep -E '^E,' | tail -1
            snap "c$c-$a-post" "${PX[@]:0:k}" ;;
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
