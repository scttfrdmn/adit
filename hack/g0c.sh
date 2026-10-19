#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G0c: single proxy -> single client throughput ladder on T1, run from the operator machine.
# For each (chunk, slots) rung: restart adit-proxy with that pool, then read T1 into the GPU with
# adit-get (oracle on every run). Proxy CPU-seconds come from its /metrics before and after.
#
#   ADIT_SPAWN=spawn bash hack/g0c.sh <client-box> <proxy-box> <proxy-private-ip> <runs> <chunks> <slots>
#   e.g. bash hack/g0c.sh adit-g0-client-0 adit-g0-proxy-0 172.31.39.31 2 "4 8 16" "1024 512 256"
#
# Slots run in the order given; give them descending so the cheapest (fastest) rung runs first
# (law 7). With CLIENT_IP set, hack/health.sh gates the pair before the ladder and every 5 rungs.
# PROXY_ARGS and GET_ARGS pass extra flags through (e.g. --meta-type get-object, --pump-idle-us).
set +e
set -uo pipefail
echo "opts=$-"
CLIENT=$1 PROXY=$2 PIP=$3 RUNS=${4-2} CHUNKS=${5-"4 8 16"} SLOTS=${6-"16 32 64 128 256"}
SPAWN=${ADIT_SPAWN:-spawn}
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
REGION=${REGION:-us-east-1}
COMMIT=$(git rev-parse --short HEAD)
OUT=bench/runs/g0c-$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT"
on() { local box=$1; shift; timeout 900 "$SPAWN" connect "$box" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently'; }
log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$OUT/run.log"; }

on "$CLIENT" "command -v aditctl >/dev/null || (aws s3 cp --only-show-errors s3://$BUCKET/adit/bin/aditctl-linux-amd64 /tmp/aditctl && sudo install /tmp/aditctl /usr/local/bin/aditctl); test -f /tmp/t1-plan.jsonl || (aws s3 cp --only-show-errors s3://$BUCKET/adit/manifests/t1.jsonl /tmp/t1.jsonl && cd /opt/adit-src/adit && aditctl presign --region $REGION --manifest /tmp/t1.jsonl --out /tmp/t1-plan.jsonl --ttl 6h) && echo plan-ok" | tee -a "$OUT/run.log"

CLIENT_IP=${CLIENT_IP-}
PROXY_ARGS=${PROXY_ARGS-}
GET_ARGS=${GET_ARGS-}
gate() {
    [[ -z $CLIENT_IP ]] && return 0
    on "$PROXY" "pkill -x adit-proxy" >/dev/null
    if ! bash "$(dirname "$0")/health.sh" "$CLIENT" "$PROXY" "$CLIENT_IP" | tee -a "$OUT/run.log" | grep -q 'HEALTH OK'; then
        log "pair failed the health gate; stopping"
        exit 1
    fi
}
gate
n=0
for c in $CHUNKS; do
    for s in $SLOTS; do
        n=$((n + 1))
        [[ $n -gt 1 && $(((n - 1) % 5)) -eq 0 ]] && gate
        log "rung chunk=${c}MiB slots=$s"
        on "$PROXY" "pkill -x adit-proxy; sleep 1; bgrun -o /tmp/proxy-c$c-s$s.out env RUST_LOG=warn adit-proxy --listen 0.0.0.0:8480 --agent-name $PROXY --chunk-mib $c --slots $s --throughput-gbps 200 $PROXY_ARGS; sleep 3" >>"$OUT/run.log"
        cpu0=$(on "$CLIENT" "curl -s http://$PIP:8480/metrics | awk '/^adit_proxy_cpu_seconds_total/{print \$2}'" | tail -1)
        on "$CLIENT" "sudo bash /opt/adit-src/adit/hack/drop-caches.sh >/dev/null; cd /opt/adit-src/adit && RUST_LOG=warn adit-get --proxies http://$PIP:8480 --plan /tmp/t1-plan.jsonl --agent-name $CLIENT-c$c-s$s --buffer-gib 65 --chunk-mib $c --concurrency 1 --runs $RUNS --meta path=E --meta workload=T1 --meta cold=1 --meta client_instance=g7e.12xlarge --meta proxy_instance=c8gn.16xlarge --meta commit=${COMMIT_ON_BOX:-$COMMIT} $GET_ARGS --out /tmp/g0c-raw.csv 2>&1 | grep -E '^E,|rror'" | tee -a "$OUT/raw.txt"
        cpu1=$(on "$CLIENT" "curl -s http://$PIP:8480/metrics | awk '/^adit_proxy_cpu_seconds_total/{print \$2}'" | tail -1)
        log "proxy_cpu_s chunk=$c slots=$s before=$cpu0 after=$cpu1 runs=$RUNS"
        echo "$c,$s,$cpu0,$cpu1,$RUNS" >>"$OUT/proxy-cpu.csv"
    done
done
on "$CLIENT" "cat /tmp/g0c-raw.csv" >"$OUT/g0c-raw.csv"
log "done: $OUT"
