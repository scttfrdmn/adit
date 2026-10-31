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
#   G<g>P<ids>[-b<GiB>][-d<us>]
#               pair probe (#53): the first g of DEVICES served by the proxies at indices <ids>
#               (digits into PROXIES), one NIXL agent per GPU. With no more proxies than GPUs, GPU
#               g is pinned to proxy g mod n; with more, objects go round-robin over the proxies.
#               -b sets the per-GPU buffer (default BUF_GIB), -d the NIXL progress delay (default
#               100). E.g. G1P0 (1 GPU, 1 proxy), G1P01 (1 GPU, 2 proxies), G2P0 (2 GPUs, 1 proxy).
#               E_ARGS is not used.
# Each arm = one read of the workload, one results row, the same oracle and counter window as the
# per-path runners.
#
# Knobs: CYCLE_BASE (added to the cycle number in snapshot names, for callers that run one cycle
# per call; #71), DEVICES (GPUs, default 0), DEVICES_PER_ARM (1: arm E<k> uses the first k GPUs), BUF_GIB (per GPU, default 65), CHUNK_MIB (E, default 8), B_CONCURRENCY (objects in flight
# for B, default 1), E_CONCURRENCY (default 1), STRIPE, E_ARGS (extra adit-get flags, e.g.
# "--spread --chunk-mib 16 --nixl-progress-delay-us 100"), E_CONC_PER_PROXY (E arms: concurrency k x
# this for E<k>, so per-proxy load is the same on every rung; #60 held the total fixed instead),
# CORES (1: keep adit-get core dumps, ulimit -c, for #98; the core_pattern is the caller's).
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
[[ ${CORES-0} == 1 ]] && ulimit -c "${CORE_LIMIT_KB:-20000000}"
# Per-arm snapshots (law 6, #51): every proxy's /metrics (pipeline histograms, its EFA counters) and
# the client's EFA hw_counters per device, before and after each E arm, under ${OUT%.csv}-snap/.
SNAP=${OUT%.csv}-snap
snap() {
    # Every loop variable is local: bash scoping is dynamic, and arm()'s cycle counter is `c`
    # (a global `c` here once redirected each arm's stderr into a sysfs path, skipping the arm).
    # A proxy argument is a URL (saved as proxy-<position>) or <label>=<URL> (saved as <label>).
    local tag=$1 p d f i=0
    shift
    mkdir -p "$SNAP/$tag"
    for p in "$@"; do
        if [[ $p == *=http* ]]; then
            curl -s -m 10 "${p#*=}/metrics" >"$SNAP/$tag/${p%%=*}.txt"
        else
            curl -s -m 10 "$p/metrics" >"$SNAP/$tag/proxy-$i.txt"
        fi
        i=$((i + 1))
    done
    for d in /sys/class/infiniband/*; do
        for f in "$d"/ports/1/hw_counters/*; do printf '%s %s %s\n' "$(basename "$d")" "$(basename "$f")" "$(cat "$f")"; done
    done >"$SNAP/$tag/client-efa.txt"
}
# Wait for an aborted adit-get to leave the GPUs (it can sit in teardown holding every GPU's
# buffer: the next arm then fails with CUDA out-of-memory, G2 rerun); up to 180 s.
gpu_wait() {
    local a=$1 c=$2 w=0
    while { pgrep -x adit-get >/dev/null || nvidia-smi --query-compute-apps=pid --format=csv,noheader | grep -q .; } && (( w < 180 )); do
        sleep 2; w=$((w + 2))
    done
    (( w > 0 )) && log "arm $a cycle $c: waited ${w}s for the previous adit-get to release the GPUs"
    return 0
}
# Slot occupancy sampler (#86): every OCC_EVERY s (default 0.5) while adit-get runs (E and G arms), each proxy's
# adit_proxy_slots_in_use gauge, as `t_ms proxy-<i> slots` lines in the arm's -occupancy.txt.
occupancy() {
    local out=$1 gpid=$2 p i t0
    shift 2
    t0=$(date +%s%3N)
    while kill -0 "$gpid" 2>/dev/null; do
        i=0
        for p in "$@"; do
            echo "$(( $(date +%s%3N) - t0 )) proxy-$i $(curl -s -m 2 "${p#*=}/metrics" | awk '$1=="adit_proxy_slots_in_use"{print $2}')"
            i=$((i + 1))
        done
        sleep "${OCC_EVERY:-0.5}"
    done >"$out"
}
arm() {
    local a=$1 c=$(( $2 + ${CYCLE_BASE:-0} ))
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
            local px devs=$DEVICES
            px=$(IFS=,; echo "${PX[*]:0:k}")
            # DEVICES_PER_ARM=1: arm E<k> uses only the first k GPUs, one per proxy, so every rung
            # is balanced under --pin-gpu-proxy (G2 run 5; #60 for the full 8-proxy ladder).
            if [[ ${DEVICES_PER_ARM-0} == 1 ]]; then
                devs=$(tr ',' '\n' <<<"$DEVICES" | head -n "$k" | paste -sd, -)
            fi
            [[ $STRIPE == 1 && $k -gt 1 ]] && st=(--stripe)
            local conc=$E_CONCURRENCY
            [[ -n ${E_CONC_PER_PROXY-} ]] && conc=$(( k * E_CONC_PER_PROXY ))
            snap "c$c-$a-pre" "${PX[@]:0:k}"
            # NIXL warnings (e.g. "Duplicate fragment received": #50) land in the arm's stderr file.
            export NIXL_LOG_LEVEL=${NIXL_LOG_LEVEL:-WARN}
            # shellcheck disable=SC2086  # E_ARGS is a flag list
            adit-get --proxies "$px" "${st[@]}" --plan "$PLAN" --agent-name "il-$a-$c-$$" --devices "$devs" \
                --buffer-gib "$BUF_GIB" --chunk-mib "$CHUNK_MIB" --concurrency "$conc" --runs 1 $E_ARGS \
                --meta path=E --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>"$SNAP/c$c-$a.stderr" | grep -E '^E,' | tail -1 &
            local ep=$!
            occupancy "$SNAP/c$c-$a-occupancy.txt" "$ep" "${PX[@]:0:k}" &
            wait "$ep"
            wait
            snap "c$c-$a-post" "${PX[@]:0:k}"
            gpu_wait "$a" "$c" ;;
        G[0-9]*P[0-9]*)
            local spec=${a%%-*} opts="" g ids buf=$BUF_GIB delay=100 o
            [[ $a == *-* ]] && opts=${a#*-}
            g=${spec#G}; g=${g%%P*}; ids=${spec#*P}
            for o in ${opts//-/ }; do
                case $o in b[0-9]*) buf=${o#b} ;; d[0-9]*) delay=${o#d} ;; *) log "arm $a: unknown option $o"; return 1 ;; esac
            done
            local devs px=() lab=() i pin=()
            devs=$(tr ',' '\n' <<<"$DEVICES" | head -n "$g" | paste -sd, -)
            (( $(tr ',' '\n' <<<"$devs" | grep -c .) == g )) || { log "arm $a: only $DEVICES"; return 1; }
            for (( i = 0; i < ${#ids}; i++ )); do
                local id=${ids:i:1}
                (( id < ${#PX[@]} )) || { log "arm $a: no proxy $id"; return 1; }
                px+=("${PX[$id]}"); lab+=("proxy-$id=${PX[$id]}")
            done
            (( ${#px[@]} <= g )) && pin=(--pin-gpu-proxy)
            snap "c$c-$a-pre" "${lab[@]}"
            export NIXL_LOG_LEVEL=${NIXL_LOG_LEVEL:-WARN}
            adit-get --proxies "$(IFS=,; echo "${px[*]}")" --plan "$PLAN" --agent-name "il-$a-$c-$$" --devices "$devs" \
                --agent-per-gpu "${pin[@]}" --nixl-progress-delay-us "$delay" \
                --buffer-gib "$buf" --chunk-mib "$CHUNK_MIB" --concurrency "$E_CONCURRENCY" --runs 1 \
                --meta path=E --meta "workload=$WORKLOAD" --meta "cold=$COLD" \
                --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT" --out "$OUT" 2>"$SNAP/c$c-$a.stderr" | grep -E '^E,' | tail -1 &
            local gp=$!
            occupancy "$SNAP/c$c-$a-occupancy.txt" "$gp" "${lab[@]}" &
            wait "$gp"
            wait
            snap "c$c-$a-post" "${lab[@]}"
            gpu_wait "$a" "$c" ;;
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
