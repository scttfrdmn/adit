#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# #51 reproduction on the GPU client: repeated E reads of PLAN in each MODE, counting client aborts
# (the libfabric EFA provider's "Local memory registration is invalid ... IOVA" inside NIXL's
# progress thread), partial objects and oracle results per run. Rows go to OUT; each run's stderr
# (FI_LOG_LEVEL=warn, NIXL_LOG_LEVEL=WARN) to OUT's -r51/ directory.
#
#   PLAN=... PROXIES=http://a:8480,... DEVICES=0,1,2,3 RUNS=10 bash bench/paths/repro51.sh T2-64
#
# Modes (MODES to override):
#   single      one NIXL agent registering every GPU's buffer
#   pergpu      --agent-per-gpu --pin-gpu-proxy (the G2 rerun configuration)
#   pergpu-nowa the same with NIXL_DISABLE_CUDA_ADDR_WA=1 (CUDA address workaround off)
P=E
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
PROXIES=${PROXIES:?proxy URLs}
DEVICES=${DEVICES:-0}
BUF_GIB=${BUF_GIB:-20}
E_CONCURRENCY=${E_CONCURRENCY:-128}
MODES=(${MODES:-single pergpu pergpu-nowa})
DIR=${OUT%.csv}-r51
mkdir -p "$DIR"
export FI_LOG_LEVEL=${FI_LOG_LEVEL:-warn} NIXL_LOG_LEVEL=${NIXL_LOG_LEVEL:-WARN}
summary=$DIR/summary.txt
for r in $(seq 1 "$RUNS"); do
    # Modes rotate per run so time-of-session effects spread across them.
    for k in "${!MODES[@]}"; do
        m=${MODES[$(( (k + r - 1) % ${#MODES[@]} ))]}
        args=()
        envs=()
        case $m in
            single) ;;
            pergpu) args=(--agent-per-gpu --pin-gpu-proxy) ;;
            pergpu-nowa) args=(--agent-per-gpu --pin-gpu-proxy); envs=(NIXL_DISABLE_CUDA_ADDR_WA=1) ;;
            *) log "unknown mode $m"; continue ;;
        esac
        cold
        err=$DIR/$m-$r.err
        t0=$SECONDS
        row=$(env "${envs[@]}" adit-get --proxies "$PROXIES" --plan "$PLAN" --agent-name "r51$m$r$$" "${args[@]}" \
            --devices "$DEVICES" --buffer-gib "$BUF_GIB" --concurrency "$E_CONCURRENCY" --runs 1 \
            --nixl-progress-delay-us 100 "${META[@]}" --out "$OUT" 2>"$err" | grep -E '^E,' | tail -1)
        rc=${PIPESTATUS[0]}
        # An aborting adit-get can linger in teardown holding the GPUs: wait for it before the next run.
        w=0
        while { pgrep -x adit-get >/dev/null || nvidia-smi --query-compute-apps=pid --format=csv,noheader | grep -q .; } && (( w < 180 )); do
            sleep 2; w=$((w + 2))
        done
        aborts=$(grep -c 'abort()' "$err")
        iova=$(grep -c 'does not match IOVA' "$err")
        dup=$(grep -c 'Duplicate fragment' "$err")
        line="run=$r mode=$m rc=$rc secs=$((SECONDS - t0)) aborts=$aborts iova=$iova dup_frag=$dup row=$(cut -d, -f13,14,23 <<<"$row") waited=${w}s"
        echo "$line" | tee -a "$summary"
    done
done
log "done: $summary"
awk '{for(i=1;i<=NF;i++){split($i,a,"="); v[a[1]]=a[2]}; n[v["mode"]]++; ab[v["mode"]]+=(v["aborts"]>0); bad[v["mode"]]+=(v["rc"]!=0)} END{for(m in n) printf "%s: runs=%d aborted=%d nonzero_exit=%d\n", m, n[m], ab[m], bad[m]}' "$summary" | tee -a "$summary"
