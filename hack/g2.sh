#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G2 (#48), run from the operator machine against a fleet placed by `aditctl fleet up`: one
# multi-GPU client (p5en.48xlarge) and N proxies (c8gn.48xlarge). Steps, in order (STEPS to pick):
#
#   refresh   check out ADIT_REF and rebuild adit on every box (hack/install.sh, in parallel)
#   health    hack/health.sh per proxy against the client, ALL_RAILS=1: STOPS adit-proxy and probes the
#             pair with separate probe processes (not the measured proxies); run start after it
#   start     start adit-proxy on every proxy (PROXY_ARGS) and read its configuration back
#   plans     presign T2, T2-64 / T2-256 (its first 64 / 256 objects) and T1 on the client
#   confirm50 E<N> on T2-64 unpinned vs pinned, counting NIXL duplicate-fragment warnings (#50)
#   repro51   #51 client-abort reproduction: modes x runs on T2-64 (bench/paths/repro51.sh)
#   csweep    #53: E<N> on T2 at client concurrency CSWEEP_CONC (128 256 512), CSWEEP_CYCLES each
#   slotcmp   E<N> on T2 at SLOTCMP_SLOTS (2048 4096), alternated per cycle, proxies restarted per level
#   ladder    proxy ladder: ARMS (E8 E4 E2 E1) interleaved over CYCLES on T2, all GPUs
#   pairs     per-pair cap probe (#53): PAIR_ARMS (G<g>P<ids> arms, bench/paths/interleave.sh)
#             interleaved over PAIR_CYCLES on T2-256
#   pairslots PAIRSLOT_ARMS at SLOTCMP_SLOTS, alternated per cycle, proxies restarted per level
#   pairbind  NUMA binding with RDMA (#86): PAIRBIND_ARMS on T2-256 with adit-proxy unbound, bound to
#             node 0 (numactl), and bound with NIXL allowed both rails; modes alternated per cycle
#   sweep     at E<N>: chunk 4/16 MiB and slots SWEEP_SLOTS, one T2 read each (restarts the proxies)
#   t1        T1 at E<N>, striped and spread over all GPUs, 3 runs
#   bsweep    path B tuning grid on T2-64 (threads x task size x objects in flight), all GPUs
#   ceiling   H-ceiling: B512 B1024 E<N> interleaved on T2-64, all GPUs (CEIL_ARMS to override;
#             CEIL_E_CONCURRENCY for the E arm, B_TASK_MIB / B_CONCURRENCY for B's best point)
#   r98       #98 reproduction: E<N> on T2 R98_RUNS times (default 10) with adit-get core dumps kept
#             and libfabric warnings (FI_LOG_LEVEL), then the client's EFA kernel messages and the
#             proxies' failed-transfer warnings
#   pairsplit #53 localization on proxy 0 / GPU 0, rotated per cycle (SPLIT_CYCLES, default 3):
#             fetch-only (adit-fetch-bench, adit-proxy stopped) vs G1P0 with proxy 0's per-thread
#             CPU sampled; then one G1P0 under perf record. Write-only is the health probe.
#   collect   copy every results CSV and proxy /metrics off the boxes, push to the bench bucket
#
#   ADIT_SPAWN=... STEPS="refresh health start plans ladder" bash hack/g2.sh <fleet> [proxies=8]
#
# OFFLINE=1 for boxes with no internet route (launched --no-public-ip): refresh builds from source
# staged in the bucket (hack/stage-src.sh) instead of GitHub.
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
# spawn connect's own lines are dropped, including the SSM banner of private-only boxes (#60: it
# reached the results CSVs).
on() { local box=$1; shift; timeout "${ON_TIMEOUT:-900}" "$SPAWN" connect "$box" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently|no public IP \(private only\)|via SSM RunCommand'; }
log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$OUT/run.log"; }
RAW=s3://$BUCKET/adit/g2/$(basename "$OUT")
push() { AWS_PROFILE=${AWS_PROFILE-aws} aws s3 cp --recursive --quiet "$OUT" "$RAW/" --region "$REGION"; }
trap push EXIT

# Boxes and private addresses from the fleet.
status=$(go run ./cmd/aditctl fleet status --name "$FLEET" 2>/dev/null)
echo "$status" >"$OUT/fleet.txt"
ip_of() { awk -v b="$1" '$1==b{for(i=1;i<=NF;i++) if($i ~ /^private=/){sub("private=","",$i); print $i}}' <<<"$status"; }
CIP=$(ip_of "$CLIENT")
PROXIES=() PIPS=()
# The proxies the fleet actually has, in index order: a proxy that never placed (capacity) leaves a
# gap in the numbering (G2 run 4: proxy-5), so N is what is there, not what was asked for.
while read -r p; do
    PROXIES+=("$p")
    PIPS+=("$(ip_of "$p")")
done < <(awk -v f="adit-$FLEET-proxy-" 'index($1,f)==1{print $1}' <<<"$status" | sort -t- -k4 -n)
# --pin-gpu-proxy maps GPU g to proxy g mod N: unless N divides the GPU count, some proxies carry
# twice the bytes and bound the arm (G2 run 4: E7 on 8 GPUs). Warn; E arms with such N are unbalanced.
if (( ${#PROXIES[@]} != N )); then
    echo "fleet has ${#PROXIES[@]} proxies, not $N: using ${#PROXIES[@]}" | tee -a "$OUT/run.log"
    N=${#PROXIES[@]}
fi
URLS=$(printf 'http://%s:8480,' "${PIPS[@]}")
URLS=${URLS%,}
log "fleet=$FLEET client=$CLIENT ($CIP) proxies=${PROXIES[*]} ips=${PIPS[*]} ref=$ADIT_REF"
[[ -n $CIP && ${#PIPS[@]} -eq $N && ! " ${PIPS[*]} " == *"  "* ]] || { log "missing addresses; see $OUT/fleet.txt"; exit 1; }
DEVS=$(on "$CLIENT" "nvidia-smi --query-gpu=index --format=csv,noheader | paste -sd, -" | tail -1)
NGPU=$(tr ',' '\n' <<<"$DEVS" | grep -c .)
for a in $ARMS; do
    k=${a#E}
    [[ ${DEVICES_PER_ARM-0} != 1 && $k =~ ^[0-9]+$ ]] && (( NGPU % k != 0 )) && log "WARNING: $a with $NGPU GPUs pinned is unbalanced (GPU g -> proxy g mod $k): $((NGPU % k)) proxies carry an extra GPU"
done
log "client GPUs: $DEVS"
REPO=/opt/adit-src/adit

start_proxies() {
    # The proxy's S3 client opens a connection per in-flight request: at 2,048 slots and a 600 Gb/s
    # CRT target that exceeds the default 1,024 open files (AWS_ERROR_MAX_FDS_EXCEEDED -> 502, #48).
    # wrap (optional): words between `env RUST_LOG=warn` and adit-proxy, e.g. VAR=x or numactl ....
    local args=$1 tag=$2 wrap=${3-}
    for k in "${!PROXIES[@]}"; do
        on "${PROXIES[$k]}" "pkill -x adit-proxy; sleep 1; ulimit -n \$(ulimit -Hn); ulimit -n; bgrun -o /tmp/proxy-$tag.out env RUST_LOG=warn $wrap adit-proxy --listen 0.0.0.0:8480 --agent-name ${PROXIES[$k]} $args; sleep 3" >>"$OUT/run.log" &
    done
    wait
    # Read the configuration back from every running proxy, not the flag.
    for k in "${!PIPS[@]}"; do
        on "$CLIENT" "curl -s http://${PIPS[$k]}:8480/metrics | grep -E '^adit_proxy_(config|info)' | head -5" | sed "s/^/${PROXIES[$k]} $tag: /" | tee -a "$OUT/proxy-config.txt"
    done
}

client_run() { box_run "$CLIENT" "$@"; }

box_run() {
    # Long work runs under bgrun on the client (spawn connect block-buffers); poll until its process
    # exits (not until a marker line: a command that fails to parse never prints one). BUCKET and
    # REGION are exported to it: the on-box runners region-check and read them (G2 #48 ran its
    # on-box checks against the default bucket when they were not).
    local box=$1 name=$2 cmd=$3
    local pid
    pid=$(on "$box" "cd $REPO && ulimit -n \$(ulimit -Hn) && bgrun -o /tmp/g2-$name.out bash -c 'export BUCKET=$BUCKET REGION=$REGION; $cmd; echo EXIT=\$?'" | grep -o 'pid=[0-9]*' | cut -d= -f2)
    [[ -n $pid ]] || { log "$name: did not start"; return 1; }
    local t0=$SECONDS
    while :; do
        sleep 30
        on "$box" "tail -1 /tmp/g2-$name.out" | tail -1 | sed "s/^/  $name: /"
        on "$box" "kill -0 $pid 2>/dev/null && echo running" | grep -q running || break
        (( SECONDS - t0 > ${STEP_TIMEOUT:-3600} )) && { log "$name: still running after ${STEP_TIMEOUT:-3600}s; leaving it"; break; }
    done
    on "$box" "cat /tmp/g2-$name.out" >"$OUT/$name.out"
    log "$name: $(grep -E '^EXIT=|syntax error|command not found' "$OUT/$name.out" | head -2 | tr '\n' ' ')"
    push
}

# The on-box refresh command for a role. OFFLINE=1 (boxes with no internet route, --no-public-ip):
# ADIT_REF is staged in the bucket (hack/stage-src.sh) and install.sh, from the staged tree, builds
# from it; otherwise the box's checkout pulls ADIT_REF from GitHub.
refresh_cmd() {
    if [[ -n ${SRC_S3-} ]]; then
        echo "rm -rf /tmp/adit-stage && mkdir -p /tmp/adit-stage && aws s3 cp --only-show-errors $SRC_S3/adit.tar.gz /tmp/adit-stage/ --region $REGION && tar xzf /tmp/adit-stage/adit.tar.gz -C /tmp/adit-stage adit/hack/install.sh && sudo ADIT_SRC_S3=$SRC_S3 ADIT_SRC_REGION=$REGION bash /tmp/adit-stage/adit/hack/install.sh $1 2>&1 | tail -15"
    else
        echo "cd $REPO && git fetch -q origin && git checkout -q $ADIT_REF && git reset -q --hard origin/$ADIT_REF && sudo ADIT_REF=$ADIT_REF bash hack/install.sh $1 2>&1 | tail -15"
    fi
}

step_refresh() {
    log "refresh: install.sh on every box at $ADIT_REF$([[ ${OFFLINE-} == 1 ]] && echo " (offline)")"
    if [[ ${OFFLINE-} == 1 ]]; then
        SRC_S3=$(BUCKET=$BUCKET REGION=$REGION bash hack/stage-src.sh "$ADIT_REF") || { log "stage-src failed; stopping"; exit 1; }
        log "refresh: staged $SRC_S3"
    fi
    ON_TIMEOUT=2400 on "$CLIENT" "$(refresh_cmd client)" >"$OUT/refresh-client.txt" &
    for p in "${PROXIES[@]}"; do
        ON_TIMEOUT=2400 on "$p" "$(refresh_cmd proxy)" >"$OUT/refresh-$p.txt" &
    done
    wait
    for f in "$OUT"/refresh-*.txt; do
        log "$(basename "$f"): $(grep -E '^adit=|install complete|FAIL' "$f" | tr '\n' ' ')"
    done
    grep -L 'install complete' "$OUT"/refresh-*.txt | grep -q . && { log "refresh failed on some box; stopping"; exit 1; }
    on "$CLIENT" "ls /sys/class/infiniband | wc -l; cat /var/lib/adit/versions" | tee "$OUT/client-versions.txt"
    push
}

step_health() {
    for k in "${!PROXIES[@]}"; do
        on "${PROXIES[$k]}" "pkill -x adit-proxy; ls /sys/class/infiniband | paste -sd' '" | sed "s/^/${PROXIES[$k]} efa: /" | tee -a "$OUT/run.log"
        if ! ALL_RAILS=${ALL_RAILS-1} bash "$(dirname "$0")/health.sh" "$CLIENT" "${PROXIES[$k]}" "$CIP" "${MIN_GBPS:-180}" | tee -a "$OUT/health.txt" | grep -q 'HEALTH OK'; then
            log "${PROXIES[$k]} failed the health gate; stopping"
            exit 1
        fi
    done
    grep 'HEALTH' "$OUT/health.txt" | tee -a "$OUT/run.log"
    push
}

step_start() {
    start_proxies "$PROXY_ARGS" base
}

step_plans() {
    on "$CLIENT" "command -v aditctl >/dev/null || (aws s3 cp --only-show-errors s3://$BUCKET/adit/bin/aditctl-linux-amd64 /tmp/aditctl && sudo install /tmp/aditctl /usr/local/bin/aditctl)
        for w in t1 t2; do aws s3 cp --only-show-errors s3://$BUCKET/adit/manifests/\$w.jsonl /tmp/\$w.jsonl && aditctl presign --region $REGION --manifest /tmp/\$w.jsonl --out /tmp/\$w-plan.jsonl --ttl 8h; done
        head -64 /tmp/t2-plan.jsonl >/tmp/t2-64-plan.jsonl; head -256 /tmp/t2-plan.jsonl >/tmp/t2-256-plan.jsonl
        wc -l /tmp/t1-plan.jsonl /tmp/t2-plan.jsonl /tmp/t2-64-plan.jsonl /tmp/t2-256-plan.jsonl" | tee -a "$OUT/run.log"
}

common="CYCLES=$CYCLES DEVICES=$DEVS DEVICES_PER_ARM=${DEVICES_PER_ARM-0} BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY${E_CONC_PER_PROXY:+ E_CONC_PER_PROXY=$E_CONC_PER_PROXY}"

step_confirm50() {
    # #50 mechanism check: the same E8 read of T2-64 unpinned (8 proxies may write into one agent)
    # and pinned (one proxy per agent). The NIXL collision theory predicts "Duplicate fragment
    # received" warnings and partial objects only in the unpinned arm.
    log "confirm50: E$N on T2-64, unpinned vs pinned"
    client_run confirm50 "PLAN=/tmp/t2-64-plan.jsonl PROXIES=$URLS DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-confirm50.csv bash bench/paths/confirm50.sh T2-64"
}

step_repro51() {
    # #51 reproduction: repeated T2-64 reads per mode (single agent / agent per GPU / per GPU with
    # the CUDA address workaround off), counting client aborts. R51_RUNS, R51_MODES override.
    log "repro51: ${R51_MODES:-single pergpu pergpu-nowa} x ${R51_RUNS:-10} on T2-64"
    client_run repro51 "PLAN=/tmp/t2-64-plan.jsonl PROXIES=$URLS DEVICES=$DEVS BUF_GIB=${R51_BUF_GIB:-20} E_CONCURRENCY=$E_CONCURRENCY RUNS=${R51_RUNS:-10} MODES=\"${R51_MODES:-single pergpu pergpu-nowa}\" OUT=/tmp/g2-repro51.csv bash bench/paths/repro51.sh T2-64"
    on "$CLIENT" "cat /tmp/g2-repro51-r51/summary.txt" >"$OUT/repro51-summary.txt"
    on "$CLIENT" "cd /tmp && tar czf /tmp/repro51.tgz g2-repro51.csv g2-repro51-r51 && aws s3 cp --quiet /tmp/repro51.tgz s3://$BUCKET/adit/g2/$(basename "$OUT")/repro51.tgz --region $REGION && echo pushed"
}

step_csweep() {
    # #53: E<N> on T2 at several client concurrencies (objects in flight), CSWEEP_CYCLES cycles,
    # one output (and per-arm snapshot directory) per level so levels do not overwrite each other,
    # rotated, same proxies (no restart). CSWEEP_CONC overrides the levels.
    for cc in ${CSWEEP_CONC:-128 256 512}; do
        log "csweep: E$N on T2 at concurrency $cc"
        client_run "csweep-c$cc" "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=${CSWEEP_CYCLES:-2} STRIPE=0 E_ARGS=\"$E_ARGS\" DEVICES=$DEVS DEVICES_PER_ARM=${DEVICES_PER_ARM-0} BUF_GIB=$BUF_GIB E_CONCURRENCY=$cc OUT=/tmp/g2-csweep-c$cc.csv bash bench/paths/interleave.sh T2"
    done
}

step_slotcmp() {
    # Same-session slot attribution (#53): E<N> on T2 at each SLOTCMP_SLOTS level, alternating the
    # level order per cycle (SLOTCMP_CYCLES), restarting the proxies with that pool each time. One
    # output and snapshot directory per level. Leaves the proxies at the last level run.
    local levels=(${SLOTCMP_SLOTS:-2048 4096}) c k s
    for c in $(seq 1 "${SLOTCMP_CYCLES:-3}"); do
        for k in "${!levels[@]}"; do
            s=${levels[$(( (k + c - 1) % ${#levels[@]} ))]}
            log "slotcmp: cycle $c, slots $s"
            start_proxies "--chunk-mib 8 --slots $s --throughput-gbps 600" "s$s"
            client_run "slotcmp-c$c-s$s" "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=1 CYCLE_BASE=$((c - 1)) STRIPE=0 E_ARGS=\"$E_ARGS\" DEVICES=$DEVS DEVICES_PER_ARM=${DEVICES_PER_ARM-0} BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-slotcmp-s$s.csv bash bench/paths/interleave.sh T2"
        done
    done
}

step_pairs() {
    # Per-pair cap (#53, G2 run 5: one GPU from one proxy capped near 175 Gb/s). Defaults, with
    # proxies 0 and 1 on two rails and proxy 2 on one:
    #   G1P0   1 GPU, 1 proxy (the run 5 E1 pair)       G1P01  1 GPU, 2 proxies: a per-GPU cap stays
    #   G2P0   2 GPUs, 1 proxy: a per-proxy cap stays    G1P2   1 GPU, 1 one-rail proxy
    #   G2P01  2 GPUs, 2 proxies pinned (balanced E2)    G1P0-b16 / -b96   16 / 3 waves (64 GiB: 4)
    #   G1P0-d0  NIXL progress delay 0 (100 elsewhere)
    local arms=${PAIR_ARMS:-G1P0 G1P01 G2P0 G1P2 G2P01 G1P0-b16 G1P0-b96 G1P0-d0}
    log "pairs: $arms x ${PAIR_CYCLES:-3} cycles on T2-256"
    client_run pairs "PLAN=/tmp/t2-256-plan.jsonl PROXIES=$URLS ARMS=\"$arms\" CYCLES=${PAIR_CYCLES:-3} DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-pairs.csv bash bench/paths/interleave.sh T2-256"
}

step_pairslots() {
    # The slot lever where it binds (G2 run 5 E1: 0.75-0.82 s slot wait at 4096): PAIRSLOT_ARMS at
    # each SLOTCMP_SLOTS level, alternating the order per cycle, proxies restarted per level; then
    # the proxies go back to PROXY_ARGS.
    local levels=(${SLOTCMP_SLOTS:-2048 4096}) c k s
    for c in $(seq 1 "${PAIR_CYCLES:-3}"); do
        for k in "${!levels[@]}"; do
            s=${levels[$(( (k + c - 1) % ${#levels[@]} ))]}
            log "pairslots: cycle $c, slots $s"
            start_proxies "--chunk-mib 8 --slots $s --throughput-gbps 600" "s$s"
            client_run "pairslots-c$c-s$s" "PLAN=/tmp/t2-256-plan.jsonl PROXIES=$URLS ARMS=\"${PAIRSLOT_ARMS:-G1P0 G1P2}\" CYCLES=1 CYCLE_BASE=$((c - 1)) DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-pairslots-s$s.csv bash bench/paths/interleave.sh T2-256"
        done
    done
    start_proxies "$PROXY_ARGS" base
}

step_pairbind() {
    # Does the fetch-only NUMA binding gain (fetch-fpplace: node 0 CPUs and memory, 27-29% less CPU
    # per GB) hold with RDMA writes? Modes, alternated per cycle, proxies restarted per mode:
    #   unbound  adit-proxy as in every G2 run
    #   bound    numactl --cpunodebind=0 --membind=0 (node of ens68/S3 and of rail rdmap83s0); NIXL
    #            picks DRAM_SEG rails from the pool's NUMA node, so expect rail 0 only
    #   bound2r  bound, with NIXL_LIBFABRIC_MAX_BW_PER_DRAM_SEG high enough to use both rails
    # After each start: the pool's placement (numa_maps of adit-proxy) and NUMA balancing state.
    local modes=(${PAIRBIND_MODES:-unbound bound bound2r}) c k m w p
    for p in "${PROXIES[@]}"; do
        on "$p" "command -v numactl >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq numactl >/dev/null 2>&1; numactl --hardware | head -3" | sed "s/^/$p: /" | tee -a "$OUT/run.log"
    done
    for c in $(seq 1 "${PAIR_CYCLES:-3}"); do
        for k in "${!modes[@]}"; do
            m=${modes[$(( (k + c - 1) % ${#modes[@]} ))]}
            case $m in
                unbound) w="" ;;
                bound) w="numactl --cpunodebind=0 --membind=0" ;;
                bound2r) w="NIXL_LIBFABRIC_MAX_BW_PER_DRAM_SEG=100000 numactl --cpunodebind=0 --membind=0" ;;
                *) log "pairbind: unknown mode $m"; return 1 ;;
            esac
            log "pairbind: cycle $c, mode $m"
            start_proxies "$PROXY_ARGS" "pb-$m" "$w"
            for p in "${PROXIES[@]}"; do
                on "$p" "sleep 15; pid=\$(pgrep -x adit-proxy); echo numa_balancing=\$(cat /proc/sys/kernel/numa_balancing) pid=\$pid; sudo cat /proc/\$pid/numa_maps | grep -oE 'N[0-9]+=[0-9]+' | awk -F'[N=]' '{n[\$2]+=\$3} END{for (k in n) printf \"numa_maps_node%s_pages=%d \", k, n[k]; print \"\"}'; grep Cpus_allowed_list /proc/\$pid/status; sudo tr '\\0' '\\n' </proc/\$pid/environ | grep -E '^NIXL_' || echo 'no NIXL_ env'" | sed "s/^/c$c $m $p: /" | tee -a "$OUT/pairbind-placement.txt"
            done
            client_run "pairbind-c$c-$m" "PLAN=/tmp/t2-256-plan.jsonl PROXIES=$URLS ARMS=\"${PAIRBIND_ARMS:-G1P0 G2P0}\" CYCLES=1 CYCLE_BASE=$((c - 1)) DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$E_CONCURRENCY OUT=/tmp/g2-pairbind-$m.csv bash bench/paths/interleave.sh T2-256"
        done
    done
    start_proxies "$PROXY_ARGS" base
}

step_ladder() {
    log "ladder: $ARMS x $CYCLES cycles on T2"
    client_run ladder "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=\"$ARMS\" STRIPE=0 E_ARGS=\"$E_ARGS\" $common OUT=/tmp/g2-ladder.csv bash bench/paths/interleave.sh T2"
}

step_sweep() {
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
}

step_t1() {
    log "t1: E$N, spread over all GPUs, each GPU's segment from its pinned proxy"
    client_run t1 "PLAN=/tmp/t1-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=3 STRIPE=1 E_ARGS=\"$E_ARGS --spread\" DEVICES=$DEVS BUF_GIB=16 E_CONCURRENCY=$(( $(tr -cd , <<<"$DEVS" | wc -c) + 1 )) OUT=/tmp/g2-t1.csv bash bench/paths/interleave.sh T1"
}

step_bsweep() {
    # Path B at its best on this client before H-ceiling (law 3): threads x task size x objects in
    # flight on T2-64, 8 GPUs, one read each, cold. BSWEEP_* override the grid.
    log "bsweep: KvikIO RemoteFile grid on T2-64"
    client_run bsweep "PLAN=/tmp/t2-64-plan.jsonl DEVICES=$DEVS BUF_GIB=$BUF_GIB THREADS=\"${BSWEEP_THREADS:-512 2048}\" TASK_MIB=\"${BSWEEP_TASK_MIB:-4 8 16}\" CONC=\"${BSWEEP_CONC:-32 64 128}\" OUT=/tmp/g2-bsweep.csv bash bench/paths/B/sweep.sh T2-64"
}

step_ceiling() {
    log "ceiling: ${CEIL_ARMS:-B512 B1024 E$N} on T2-64, all GPUs"
    client_run ceiling "PLAN=/tmp/t2-64-plan.jsonl PROXIES=$URLS ARMS=\"${CEIL_ARMS:-B512 B1024 E$N}\" STRIPE=0 E_ARGS=\"$E_ARGS\" B_CONCURRENCY=${B_CONCURRENCY:-16} B_TASK_MIB=${B_TASK_MIB:-64} $common E_CONCURRENCY=${CEIL_E_CONCURRENCY:-$E_CONCURRENCY} OUT=/tmp/g2-ceiling.csv bash bench/paths/interleave.sh T2-64"
}

step_r98() {
    # #98: the E<N> arm that failed RDMA writes to GPUs 4-7 and aborted in the EFA provider, repeated
    # on the same proxies. Cores go to /var/tmp (the DLAMI's apport pipe kept none in #60).
    on "$CLIENT" "sudo sysctl -qw kernel.core_pattern=/var/tmp/core.%e.%p.%t; cat /proc/sys/kernel/core_pattern; sudo dmesg | wc -l >/tmp/r98-dmesg0" | tee -a "$OUT/run.log"
    for p in "${PROXIES[@]}"; do on "$p" "wc -l </tmp/proxy-base.out >/tmp/r98-log0 2>/dev/null"; done
    log "r98: E$N x ${R98_RUNS:-10} on T2, cores kept, FI_LOG_LEVEL=${R98_FI_LOG:-warn}"
    client_run r98 "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=E$N CYCLES=${R98_RUNS:-10} STRIPE=0 CORES=1 FI_LOG_LEVEL=${R98_FI_LOG:-warn} E_ARGS=\"$E_ARGS\" DEVICES=$DEVS DEVICES_PER_ARM=${DEVICES_PER_ARM-0} BUF_GIB=$BUF_GIB E_CONCURRENCY=${R98_CONCURRENCY:-$E_CONCURRENCY} OUT=/tmp/g2-r98.csv bash bench/paths/interleave.sh T2"
    on "$CLIENT" "sudo dmesg -T | tail -n +\$((\$(cat /tmp/r98-dmesg0) + 1)) | grep -iE 'efa|rdmap|nvidia|xid|segfault' | tail -300; echo '# cores'; ls -la /var/tmp/core.* 2>/dev/null" >"$OUT/r98-client-kernel.txt"
    for p in "${PROXIES[@]}"; do
        on "$p" "tail -n +\$((\$(cat /tmp/r98-log0) + 1)) /tmp/proxy-base.out | sed 's/\x1b\[[0-9;]*m//g' | grep -E 'WARN|ERROR' | head -200" | sed "s/^/$p: /"
    done >"$OUT/r98-proxy-warn.txt"
    log "r98: $(grep -c . "$OUT/r98-proxy-warn.txt") proxy warning lines; $(grep -c 'core\.' "$OUT/r98-client-kernel.txt") cores"
    push
}

step_pairsplit() {
    # #53: where proxy 0 / GPU 0's ~169 Gb/s sits. Per cycle, in rotated order:
    #   fetch  adit-fetch-bench on proxy 0 (the production fetch engine and slot pool, write a no-op)
    #          at the proxy's slots and SPLIT_OBJS objects in flight, adit-proxy stopped;
    #   e1     G1P0 at concurrency SPLIT_OBJS, proxy 0's threads sampled (bench/paths/threadcpu.sh).
    # Then one G1P0 with proxy 0 under perf record. Both read all of T2.
    local p0=${PROXIES[0]} objs=${SPLIT_OBJS:-128} slots modes=(fetch e1) c k m
    slots=$(grep -oE -- '--slots [0-9]+' <<<"$PROXY_ARGS" | awk '{print $2}')
    on "$p0" "aws s3 cp --only-show-errors s3://$BUCKET/adit/bin/aditctl-linux-arm64 /tmp/aditctl --region $REGION && sudo install /tmp/aditctl /usr/local/bin/aditctl
        aws s3 cp --only-show-errors s3://$BUCKET/adit/manifests/t2.jsonl /tmp/t2.jsonl --region $REGION && aditctl presign --region $REGION --manifest /tmp/t2.jsonl --out /tmp/t2-plan.jsonl --ttl 8h; wc -l /tmp/t2-plan.jsonl; rm -rf /tmp/ps-fetch*" | tee -a "$OUT/run.log"
    for c in $(seq 1 "${SPLIT_CYCLES:-3}"); do
        for k in "${!modes[@]}"; do
            m=${modes[$(( (k + c - 1) % ${#modes[@]} ))]}
            log "pairsplit: cycle $c, $m"
            if [[ $m == fetch ]]; then
                on "$p0" "pkill -x adit-proxy; sleep 2" >/dev/null
                box_run "$p0" "pairsplit-c$c-fetch" "PLAN=/tmp/t2-plan.jsonl CYCLES=1 ARMS=s${slots:-4096}o$objs OUT=/tmp/ps-fetch-c$c.csv bash bench/paths/fetchprobe.sh T2"
                on "$p0" "cat /tmp/ps-fetch-c$c.csv" >"$OUT/pairsplit-fetch-c$c.csv"
                on "$p0" "aws s3 cp --recursive --quiet /tmp/ps-fetch-c$c-snap $RAW/pairsplit-fetch-c$c-snap/ --region $REGION" >/dev/null
                start_proxies "$PROXY_ARGS" "ps-c$c"
            else
                on "$p0" "bgrun -o /tmp/ps-tc-c$c.out bash $REPO/bench/paths/threadcpu.sh \$(pgrep -x adit-proxy) /tmp/ps-threads-c$c.txt 1" >/dev/null
                client_run "pairsplit-c$c-e1" "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=G1P0 CYCLES=1 CYCLE_BASE=$((c - 1)) DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$objs OUT=/tmp/g2-pairsplit.csv bash bench/paths/interleave.sh T2"
                # Thread samples are megabytes: off the box through the bucket, not spawn connect
                # (SSM output is bounded).
                on "$p0" "pkill -f threadcpu.sh; gzip -f /tmp/ps-threads-c$c.txt && aws s3 cp --only-show-errors /tmp/ps-threads-c$c.txt.gz $RAW/pairsplit-threads-c$c.txt.gz --region $REGION" >/dev/null
                AWS_PROFILE=${AWS_PROFILE-aws} aws s3 cp --only-show-errors "$RAW/pairsplit-threads-c$c.txt.gz" "$OUT/" --region "$REGION"
            fi
        done
    done
    log "pairsplit: G1P0 under perf record on $p0"
    # perf covers the whole arm: started before it, stopped (SIGINT) after.
    on "$p0" "sudo sysctl -qw kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0; bgrun -o /tmp/ps-perf.out sudo perf record -F 99 -g -p \$(pgrep -x adit-proxy) -o /tmp/ps.perf.data" >/dev/null
    client_run pairsplit-perf "PLAN=/tmp/t2-plan.jsonl PROXIES=$URLS ARMS=G1P0 CYCLES=1 CYCLE_BASE=9 DEVICES=$DEVS BUF_GIB=$BUF_GIB E_CONCURRENCY=$objs OUT=/tmp/g2-pairsplit-perf.csv bash bench/paths/interleave.sh T2"
    ON_TIMEOUT=600 on "$p0" "sudo pkill -INT -x perf; while pgrep -x perf >/dev/null; do sleep 2; done; sudo perf report -i /tmp/ps.perf.data --stdio --no-children -g none --sort comm,dso,symbol --percent-limit 0.3 2>/dev/null | grep -v '^\$' | head -200" >"$OUT/pairsplit-perf-top.txt"
    on "$p0" "sudo perf report -i /tmp/ps.perf.data --stdio --no-children -g none --sort comm --percent-limit 0.5 2>/dev/null | grep -v '^\$' | head -60" >"$OUT/pairsplit-perf-comm.txt"
    push
}

step_collect() {
    for f in confirm50 ladder csweep csweep-c128 csweep-c256 csweep-c512 slotcmp-s2048 slotcmp-s4096 sweep t1 ceiling bsweep pairs pairslots-s2048 pairslots-s4096 pairbind-unbound pairbind-bound pairbind-bound2r r98 pairsplit pairsplit-perf; do
        on "$CLIENT" "test -f /tmp/g2-$f.csv && cat /tmp/g2-$f.csv" >"$OUT/g2-$f.csv"
        on "$CLIENT" "test -f /tmp/g2-$f.order && cat /tmp/g2-$f.order" >"$OUT/g2-$f.order"
        # One drop_caches status line per cold rung (#70).
        on "$CLIENT" "test -f /tmp/g2-$f.cold && cat /tmp/g2-$f.cold" >"$OUT/g2-$f.cold"
    done
    for k in "${!PIPS[@]}"; do
        on "$CLIENT" "curl -s http://${PIPS[$k]}:8480/metrics" >"$OUT/metrics-${PROXIES[$k]}.txt"
    done
    # Per-arm snapshots (interleave.sh) go straight from the client to the bench bucket.
    on "$CLIENT" "for d in /tmp/g2-*-snap; do [ -d \$d ] && aws s3 cp --recursive --quiet \$d s3://$BUCKET/adit/g2/$(basename "$OUT")/\$(basename \$d)/ --region $REGION; done; ls -d /tmp/g2-*-snap 2>/dev/null | wc -l" | sed 's/^/snapshot dirs pushed: /' | tee -a "$OUT/run.log"
    on "$CLIENT" "cat /var/lib/adit/versions; nvidia-smi --query-gpu=name,memory.total,driver_version --format=csv" >"$OUT/client-versions.txt"
    ls -la "$OUT" | tee -a "$OUT/run.log"
}
# Steps run in the order STEPS lists them (G2 rerun: running them in file order put the closing
# health step, which stops every adit-proxy, before t1 and ceiling, leaving those arms no proxies).
for s in $STEPS; do
    declare -F "step_$s" >/dev/null || { log "unknown step $s"; exit 2; }
    "step_$s"
done
log "done: $OUT"
