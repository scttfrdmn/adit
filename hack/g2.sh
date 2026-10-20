#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G2 (#48), run from the operator machine against a fleet placed by `aditctl fleet up`: one
# multi-GPU client (p5en.48xlarge) and N proxies (c8gn.48xlarge). Steps, in order (STEPS to pick):
#
#   refresh   check out ADIT_REF and rebuild adit on every box (hack/install.sh, in parallel)
#   health    hack/health.sh per proxy against the client, ALL_RAILS=1 (both proxy EFA devices)
#   start     start adit-proxy on every proxy (PROXY_ARGS) and read its configuration back
#   plans     presign T2, T2-64 (its first 64 objects) and T1 on the client
#   confirm50 E<N> on T2-64 unpinned vs pinned, counting NIXL duplicate-fragment warnings (#50)
#   ladder    proxy ladder: ARMS (E8 E4 E2 E1) interleaved over CYCLES on T2, all GPUs
#   sweep     at E<N>: chunk 4/16 MiB and slots SWEEP_SLOTS, one T2 read each (restarts the proxies)
#   t1        T1 at E<N>, striped and spread over all GPUs, 3 runs
#   bsweep    path B tuning grid on T2-64 (threads x task size x objects in flight), all GPUs
#   ceiling   H-ceiling: B512 B1024 E<N> interleaved on T2-64, all GPUs (CEIL_ARMS to override)
#   collect   copy every results CSV and proxy /metrics off the boxes, push to the bench bucket
#
#   ADIT_SPAWN=... STEPS="refresh health start plans ladder" bash hack/g2.sh <fleet> [proxies=8]
#
# Results land in $OUT locally and under s3://$BUCKET/adit/g2/ after every step (logs streamed off
# the instances, law 7).
set +e
set -uo pipefail
echo "opts=$-"
FLEET=${1:?fleet name} N=${2-8}
SPAWN=${ADIT_SPAWN:-spawn}
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
REGION=${REGION:-us-east-1}
ADIT_REF=${ADIT_REF:-$(git rev-parse --abbrev-ref HEAD)}
STEPS=${STEPS:-refresh health start plans ladder sweep t1 ceiling collect}
PROXY_ARGS=${PROXY_ARGS:---chunk-mib 8 --slots 2048 --throughput-gbps 600}
CYCLES=${CYCLES:-3}
ARMS=${ARMS:-E8 E4 E2 E1}
SWEEP_SLOTS=${SWEEP_SLOTS:-1024 4096}
BUF_GIB=${BUF_GIB:-64}
E_CONCURRENCY=${E_CONCURRENCY:-128}
E_ARGS=${E_ARGS:---nixl-progress-delay-us 100 --agent-per-gpu --pin-gpu-proxy}
CLIENT=adit-$FLEET-client-0
OUT=${OUT:-bench/runs/g2-$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$OUT"
on() { local box=$1; shift; timeout "${ON_TIMEOUT:-900}" "$SPAWN" connect "$box" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently'; }
log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$OUT/run.log"; }
push() { AWS_PROFILE=${AWS_PROFILE-aws} aws s3 cp --recursive --quiet "$OUT" "s3://$BUCKET/adit/g2/$(basename "$OUT")/" --region "$REGION"; }
trap push EXIT
want() { [[ " $STEPS " == *" $1 "* ]]; }

# Boxes and private addresses from the fleet.
status=$(go run ./cmd/aditctl fleet status --name "$FLEET" 2>/dev/null)
echo "$status" >"$OUT/fleet.txt"
ip_of() { awk -v b="$1" '$1==b{for(i=1;i<=NF;i++) if($i ~ /^private=/){sub("private=","",$i); print $i}}' <<<"$status"; }
CIP=$(ip_of "$CLIENT")
PROXIES=() PIPS=()
for i in $(seq 0 $((N - 1))); do
    PROXIES+=("adit-$FLEET-proxy-$i")
    PIPS+=("$(ip_of "adit-$FLEET-proxy-$i")")
done
URLS=$(printf 'http://%s:8480,' "${PIPS[@]}")
URLS=${URLS%,}
log "fleet=$FLEET client=$CLIENT ($CIP) proxies=${PROXIES[*]} ips=${PIPS[*]} ref=$ADIT_REF"
[[ -n $CIP && ${#PIPS[@]} -eq $N && ! " ${PIPS[*]} " == *"  "* ]] || { log "missing addresses; see $OUT/fleet.txt"; exit 1; }
DEVS=$(on "$CLIENT" "nvidia-smi --query-gpu=index --format=csv,noheader | paste -sd, -" | tail -1)
log "client GPUs: $DEVS"
REPO=/opt/adit-src/adit

start_proxies() {
    # The proxy's S3 client opens a connection per in-flight request: at 2,048 slots and a 600 Gb/s
    # CRT target that exceeds the default 1,024 open files (AWS_ERROR_MAX_FDS_EXCEEDED -> 502, #48).
    local args=$1 tag=$2
    for k in "${!PROXIES[@]}"; do
        on "${PROXIES[$k]}" "pkill -x adit-proxy; sleep 1; ulimit -n \$(ulimit -Hn); ulimit -n; bgrun -o /tmp/proxy-$tag.out env RUST_LOG=warn adit-proxy --listen 0.0.0.0:8480 --agent-name ${PROXIES[$k]} $args; sleep 3" >>"$OUT/run.log" &
    done
    wait
    # Read the configuration back from every running proxy, not the flag.
    for k in "${!PIPS[@]}"; do
        on "$CLIENT" "curl -s http://${PIPS[$k]}:8480/metrics | grep -E '^adit_proxy_(config|info)' | head -5" | sed "s/^/${PROXIES[$k]} $tag: /" | tee -a "$OUT/proxy-config.txt"
    done
}

client_run() {
    # Long work runs under bgrun on the client (spawn connect block-buffers); poll until its process
    # exits (not until a marker line: a command that fails to parse never prints one). BUCKET and
    # REGION are exported to it: the on-box runners region-check and read them (G2 #48 ran its
    # on-box checks against the default bucket when they were not).
    local name=$1 cmd=$2
    local pid
    pid=$(on "$CLIENT" "cd $REPO && ulimit -n \$(ulimit -Hn) && bgrun -o /tmp/g2-$name.out bash -c 'export BUCKET=$BUCKET REGION=$REGION; $cmd; echo EXIT=\$?'" | grep -o 'pid=[0-9]*' | cut -d= -f2)
    [[ -n $pid ]] || { log "$name: did not start"; return 1; }
    local t0=$SECONDS
    while :; do
        sleep 30
        on "$CLIENT" "tail -1 /tmp/g2-$name.out" | tail -1 | sed "s/^/  $name: /"
        on "$CLIENT" "kill -0 $pid 2>/dev/null && echo running" | grep -q running || break
        (( SECONDS - t0 > ${STEP_TIMEOUT:-3600} )) && { log "$name: still running after ${STEP_TIMEOUT:-3600}s; leaving it"; break; }
    done
    on "$CLIENT" "cat /tmp/g2-$name.out" >"$OUT/$name.out"
    log "$name: $(grep -E '^EXIT=|syntax error|command not found' "$OUT/$name.out" | head -2 | tr '\n' ' ')"
    push
}

if want refresh; then
    log "refresh: install.sh on every box at $ADIT_REF"
    ON_TIMEOUT=2400 on "$CLIENT" "cd $REPO && git fetch -q origin && git checkout -q $ADIT_REF && git reset -q --hard origin/$ADIT_REF && sudo ADIT_REF=$ADIT_REF bash hack/install.sh client 2>&1 | tail -15" >"$OUT/refresh-client.txt" &
    for p in "${PROXIES[@]}"; do
        ON_TIMEOUT=2400 on "$p" "cd $REPO && git fetch -q origin && git checkout -q $ADIT_REF && git reset -q --hard origin/$ADIT_REF && sudo ADIT_REF=$ADIT_REF bash hack/install.sh proxy 2>&1 | tail -15" >"$OUT/refresh-$p.txt" &
    done
    wait
    for f in "$OUT"/refresh-*.txt; do
        log "$(basename "$f"): $(grep -E '^adit=|install complete|FAIL' "$f" | tr '\n' ' ')"
    done
    grep -L 'install complete' "$OUT"/refresh-*.txt | grep -q . && { log "refresh failed on some box; stopping"; exit 1; }
    on "$CLIENT" "ls /sys/class/infiniband | wc -l; cat /var/lib/adit/versions" | tee "$OUT/client-versions.txt"
    push
fi

if want health; then
    for k in "${!PROXIES[@]}"; do
        on "${PROXIES[$k]}" "pkill -x adit-proxy; ls /sys/class/infiniband | paste -sd' '" | sed "s/^/${PROXIES[$k]} efa: /" | tee -a "$OUT/run.log"
        if ! ALL_RAILS=${ALL_RAILS-1} bash "$(dirname "$0")/health.sh" "$CLIENT" "${PROXIES[$k]}" "$CIP" "${MIN_GBPS:-180}" | tee -a "$OUT/health.txt" | grep -q 'HEALTH OK'; then
            log "${PROXIES[$k]} failed the health gate; stopping"
            exit 1
        fi
    done
    grep 'HEALTH' "$OUT/health.txt" | tee -a "$OUT/run.log"
    push
fi

if want start; then
    start_proxies "$PROXY_ARGS" base
fi

if want plans; then
    on "$CLIENT" "command -v aditctl >/dev/null || (aws s3 cp --only-show-errors s3://$BUCKET/adit/bin/aditctl-linux-amd64 /tmp/aditctl && sudo install /tmp/aditctl /usr/local/bin/aditctl)
        for w in t1 t2; do aws s3 cp --only-show-errors s3://$BUCKET/adit/manifests/\$w.jsonl /tmp/\$w.jsonl && aditctl presign --region $REGION --manifest /tmp/\$w.jsonl --out /tmp/\$w-plan.jsonl --ttl 8h; done
        head -64 /tmp/t2-plan.jsonl >/tmp/t2-64-plan.jsonl; wc -l /tmp/t1-plan.jsonl /tmp/t2-plan.jsonl /tmp/t2-64-plan.jsonl" | tee -a "$OUT/run.log"
fi

common="CYCLES=$CYCLES DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY"

if want confirm50; then
    # #50 mechanism check: the same E8 read of T2-64 unpinned (8 proxies may write into one agent)
    # and pinned (one proxy per agent). The NIXL collision theory predicts "Duplicate fragment
    # received" warnings and partial objects only in the unpinned arm.
    log "confirm50: E$N on T2-64, unpinned vs pinned"
    client_run confirm50 "PLAN=/tmp/t2-64-plan.jsonl PROXIES=$URLS DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-confirm50.csv bash bench/paths/confirm50.sh T2-64"
fi

if want ladder; then
    log "ladder: $ARMS x $CYCLES cycles on T2"
    client_run ladder "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=\"$ARMS\" STRIPE=0 E_ARGS=\"$E_ARGS\" $common OUT=/tmp/g2-ladder.csv bash bench/paths/interleave.sh T2"
fi

if want sweep; then
    for c in 4 16; do
        s=$(( 2048 * 8 / c ))
        log "sweep: chunk ${c} MiB, slots $s (in-flight bytes held at 16 GiB per proxy)"
        start_proxies "--chunk-mib $c --slots $s --throughput-gbps 600" "c$c"
        client_run "sweep-c$c" "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=1 STRIPE=0 CHUNK_MIB=$c E_ARGS=\"$E_ARGS\" DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-sweep.csv bash bench/paths/interleave.sh T2"
    done
    for s in $SWEEP_SLOTS; do
        log "sweep: chunk 8 MiB, slots $s"
        start_proxies "--chunk-mib 8 --slots $s --throughput-gbps 600" "s$s"
        client_run "sweep-s$s" "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=1 STRIPE=0 E_ARGS=\"$E_ARGS\" DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-sweep.csv bash bench/paths/interleave.sh T2"
    done
    start_proxies "$PROXY_ARGS" base
fi

if want t1; then
    log "t1: E$N, spread over all GPUs, each GPU's segment from its pinned proxy"
    client_run t1 "PLAN=/tmp/t1-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=3 STRIPE=1 E_ARGS=\"$E_ARGS --spread\" DEVICES=$DEVS BUF_GIB=16 E_CONCURRENCY=$(( $(tr -cd , <<<"$DEVS" | wc -c) + 1 )) OUT=/tmp/g2-t1.csv bash bench/paths/interleave.sh T1"
fi

if want bsweep; then
    # Path B at its best on this client before H-ceiling (law 3): threads x task size x objects in
    # flight on T2-64, 8 GPUs, one read each, cold. BSWEEP_* override the grid.
    log "bsweep: KvikIO RemoteFile grid on T2-64"
    client_run bsweep "PLAN=/tmp/t2-64-plan.jsonl DEVICES=$DEVS BUF_GIB=$BUF_GIB THREADS=\"${BSWEEP_THREADS:-128 512 1024}\" TASK_MIB=\"${BSWEEP_TASK_MIB:-16 64}\" CONC=\"${BSWEEP_CONC:-8 32}\" OUT=/tmp/g2-bsweep.csv bash bench/paths/B/sweep.sh T2-64"
fi

if want ceiling; then
    log "ceiling: B512 B1024 E$N on T2-64, all GPUs"
    client_run ceiling "PLAN=/tmp/t2-64-plan.jsonl PROXIES=$URLS ARMS=\"${CEIL_ARMS:-B512 B1024 E$N}\" STRIPE=0 E_ARGS=\"$E_ARGS\" B_CONCURRENCY=${B_CONCURRENCY:-16} B_TASK_MIB=${B_TASK_MIB:-64} $common OUT=/tmp/g2-ceiling.csv bash bench/paths/interleave.sh T2-64"
fi

if want collect; then
    for f in confirm50 ladder sweep t1 ceiling bsweep; do
        on "$CLIENT" "test -f /tmp/g2-$f.csv && cat /tmp/g2-$f.csv" >"$OUT/g2-$f.csv"
        on "$CLIENT" "test -f /tmp/g2-$f.order && cat /tmp/g2-$f.order" >"$OUT/g2-$f.order"
    done
    for k in "${!PIPS[@]}"; do
        on "$CLIENT" "curl -s http://${PIPS[$k]}:8480/metrics" >"$OUT/metrics-${PROXIES[$k]}.txt"
    done
    # Per-arm snapshots (interleave.sh) go straight from the client to the bench bucket.
    on "$CLIENT" "for d in /tmp/g2-*-snap; do [ -d \$d ] && aws s3 cp --recursive --quiet \$d s3://$BUCKET/adit/g2/$(basename "$OUT")/\$(basename \$d)/ --region $REGION; done; ls -d /tmp/g2-*-snap 2>/dev/null | wc -l" | sed 's/^/snapshot dirs pushed: /' | tee -a "$OUT/run.log"
    on "$CLIENT" "cat /var/lib/adit/versions; nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv" >"$OUT/client-versions.txt"
    ls -la "$OUT" | tee -a "$OUT/run.log"
fi
log "done: $OUT"
