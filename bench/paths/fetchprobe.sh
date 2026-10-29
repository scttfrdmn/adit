#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Fetch-only probe (#53, per-pair cap follow-up): adit-fetch-bench (the production fetch engine,
# slot pool and chunking with the RDMA write replaced by a no-op) on a plan, arms rotated over
# CYCLES. Runs on a proxy box. Per arm it snapshots every ENA interface's `ethtool -S` (bandwidth and
# PPS allowance counters), every EFA device's hw_counters and /proc/net/dev, and keeps the per-chunk
# timing log. Before the arms it records which network card each EFA device and ENA interface is on
# and which interface the route to S3 takes.
#
#   PLAN=/tmp/t2-256-plan.jsonl CYCLES=3 ARMS="s2048o64 s4096o64" OUT=/tmp/fp.csv bash bench/paths/fetchprobe.sh T2-256
#
# Arm s<slots>o<objects in flight>[c<max connections>][t<CRT target Gb/s>][e<event-loop threads>][h]
# [i<n>]; h fetches over plain HTTP (--scheme http: TLS A/B, #76); target default 600
# (adit-proxy's G2 setting), max connections default unset (CRT chooses); i<n> binds the CRT's S3
# connections to the first n interfaces of NET_IFACES (comma-separated; second-interface probe,
# #53), unset uses the default route.
# Once a second for the whole arm, a sampler records the bench's socket fds (the CRT's connection
# count, #79), its threads by name (CRT event loops vs tokio) and per-thread CPU (pidstat -t).
# PROFILE_ARMS run once more after the cycles, the whole bench under perf record -g as the login
# user (rows labelled <arm>-perf, perturbed by the profiler, not comparison rows; under sudo the
# bench lost root's open-files limit and failed chunks).
# Mechanism tokens (#76): n0 turns automatic NUMA balancing off for the arm (kernel.numa_balancing,
# restored after); b<k> runs the bench under numactl --cpunodebind=k --membind=k.
P=F
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
CYCLES=${CYCLES:-3}
ARMS=(${ARMS:-s2048o64 s4096o64 s4096o16 s4096o128 s8192o128 s4096o64c256 s4096o64c1024 s4096o64t1200})
n=${#ARMS[@]}
# The CRT opens a connection per in-flight request: at the default 1,024 open files it runs out of
# descriptors and chunks fail and retry (the first fetch-probe run, 2026-10-28: every arm above 256
# connections). Same fix as adit-proxy in hack/g2.sh.
ulimit -n "$(ulimit -Hn)"
log "open files: $(ulimit -n)"
SNAP=${OUT%.csv}-snap
mkdir -p "$SNAP"
imds() {
    local tok
    tok=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
    curl -s -H "X-aws-ec2-metadata-token: $tok" "http://169.254.169.254/latest/meta-data/$1"
}
topology() {
    local d n mac
    echo "# EFA devices: name pci"
    for d in /sys/class/infiniband/*; do echo "efa $(basename "$d") $(basename "$(readlink -f "$d/device")")"; done
    echo "# network interfaces: name pci mac network-card device-number"
    for n in /sys/class/net/*; do
        [[ -e $n/device ]] || continue
        mac=$(cat "$n/address")
        echo "net $(basename "$n") $(basename "$(readlink -f "$n/device")") $mac card=$(imds "network/interfaces/macs/$mac/network-card") devnum=$(imds "network/interfaces/macs/$mac/device-number")"
    done
    echo "# route to S3"
    local host ip
    host=$(head -1 "$PLAN" | sed -E 's|.*"url":"https?://([^/]+)/.*|\1|')
    ip=$(getent ahostsv4 "$host" | awk 'NR==1{print $1}')
    echo "s3 host=$host ip=$ip route=$(ip route get "$ip" | head -1)"
}
counters() {
    local n d f
    for n in /sys/class/net/*; do
        [[ -e $n/device ]] || continue
        ethtool -S "$(basename "$n")" 2>/dev/null | sed "s/^/$(basename "$n") /"
    done
    for d in /sys/class/infiniband/*; do
        for f in "$d"/ports/1/hw_counters/*; do echo "$(basename "$d") $(basename "$f") $(cat "$f")"; done
    done
    cat /proc/net/dev
}
# NET_IFACES defaults to the box's ENA interfaces, the default-route interface first.
if [[ -z ${NET_IFACES-} ]]; then
    NET_IFACES=$( { ip route show default | awk '{print $5; exit}'
        for nd in /sys/class/net/*; do
            [[ -e $nd/device/driver && $(basename "$(readlink -f "$nd/device/driver")") == ena ]] && basename "$nd"
        done; } | awk 'NF && !s[$0]++' | paste -sd, -)
fi
log "NET_IFACES=$NET_IFACES"
topology | tee "$SNAP/topology.txt"
arm() {
    local a=$1 c=$2 perf=${3-} slots objs conn="" tgt=600 extra=()
    slots=$(sed -E 's/^s([0-9]+).*/\1/' <<<"$a")
    objs=$(sed -E 's/^s[0-9]+o([0-9]+).*/\1/' <<<"$a")
    [[ $a =~ c([0-9]+) ]] && conn=${BASH_REMATCH[1]}
    [[ $a =~ t([0-9]+) ]] && tgt=${BASH_REMATCH[1]}
    [[ $a =~ e([0-9]+) ]] && extra+=(--event-loop-threads "${BASH_REMATCH[1]}")
    [[ ${a%-perf} =~ h$|h(i[0-9]) ]] && extra+=(--scheme http)
    local nb_was="" bind=()
    if [[ $a =~ n0 ]]; then
        nb_was=$(cat /proc/sys/kernel/numa_balancing)
        sudo sysctl -qw kernel.numa_balancing=0
    fi
    [[ $a =~ b([0-9]+) ]] && bind=(numactl --cpunodebind="${BASH_REMATCH[1]}" --membind="${BASH_REMATCH[1]}")
    [[ -n $conn ]] && extra+=(--max-connections "$conn")
    if [[ $a =~ i([0-9]+) ]]; then
        local nif ifs
        nif=${BASH_REMATCH[1]}
        ifs=$(tr ',' '\n' <<<"${NET_IFACES:?arm $a needs NET_IFACES}" | head -n "$nif" | paste -sd, -)
        extra+=(--network-interfaces "$ifs")
    fi
    cold
    counters >"$SNAP/c$c-$a-pre.txt"
    grep -E '^numa_(hint_faults|pages_migrated|hint_faults_local)' /proc/vmstat >"$SNAP/c$c-$a-vmstat-pre.txt"
    local pre=() st
    # Profiled arms run the whole bench under perf (call graphs). The first seconds are the slot
    # pool's allocation (kernel clear_page); perf-top separates it by symbol.
    [[ -n $perf ]] && pre=(perf record -F 99 -g -o "$SNAP/$a.perf.data" --)
    echo "# numa_balancing=$(cat /proc/sys/kernel/numa_balancing) bind=${bind[*]:-none} open_files=$(ulimit -n)" >"$SNAP/c$c-$a-env.txt"
    "${pre[@]}" "${bind[@]}" adit-fetch-bench --plan "$PLAN" --slots "$slots" --objects-inflight "$objs" --chunk-mib 8 \
        --throughput-gbps "$tgt" "${extra[@]}" --region "$REGION" --runs 1 --label "$a${perf:+-perf}" \
        --chunk-log "$SNAP/c$c-$a-chunks.csv" 2>"$SNAP/c$c-$a.stderr" >"$SNAP/c$c-$a.out" &
    local wpid=$! bpid="" i
    # Sampler for the whole arm, once a second (#79): the bench's socket fds, threads by name, and
    # per-thread CPU (pidstat). No ss: with thousands of sockets it is slower than a whole arm.
    for i in $(seq 1 50); do bpid=$(pgrep -n -x adit-fetch-benc) && break; sleep 0.1; done
    if [[ -n $bpid ]]; then
        pidstat -t -u -p "$bpid" 1 >"$SNAP/c$c-$a-pidstat.txt" 2>/dev/null &
        local spid=$!
        while kill -0 "$bpid" 2>/dev/null; do
            echo "t=$(date -u +%T.%N | cut -c1-12) sockets=$(sudo ls -l /proc/$bpid/fd 2>/dev/null | grep -c 'socket:') $(ps -L -o comm= -p "$bpid" 2>/dev/null | sed -E 's/[0-9]+$//' | sort | uniq -c | awk '{printf "%s=%s ", $2, $1}')"
            sleep 1
        done >"$SNAP/c$c-$a-readback.txt"
        kill "$spid" 2>/dev/null
    fi
    wait "$wpid"
    st=$?
    [[ -n $nb_was ]] && sudo sysctl -qw kernel.numa_balancing="$nb_was"
    grep -E '^numa_(hint_faults|pages_migrated|hint_faults_local)' /proc/vmstat >>"$SNAP/c$c-$a-env.txt"
    counters >"$SNAP/c$c-$a-post.txt"
    [[ -s $OUT ]] || grep '^label,' "$SNAP/c$c-$a.out" | head -1 >"$OUT"
    grep -v '^label,' "$SNAP/c$c-$a.out" | grep "^$a${perf:+-perf}," | tail -1 | tee -a "$OUT"
    (( st == 0 )) || log "arm $a cycle $c: adit-fetch-bench exit $st"
    local failed
    failed=$(grep "^$a${perf:+-perf}," "$SNAP/c$c-$a.out" | tail -1 | awk -F, '{print $16}')
    [[ ${failed:-0} == 0 ]] || log "WARNING: arm $a cycle $c: $failed chunks failed" >&2
    gzip -f "$SNAP/c$c-$a-chunks.csv"
}
for c in $(seq 1 "$CYCLES"); do
    for k in $(seq 0 $((n - 1))); do
        a=${ARMS[$(( (k + c - 1) % n ))]}
        printf '%s cycle=%s arm=%s ' "$(date -u +%T)" "$c" "$a" | tee -a "${OUT%.csv}.order"
        arm "$a" "$c" | cut -d, -f11,12,13,14,15,16 | tee -a "${OUT%.csv}.order"
    done
done
# perf as the login user needs these; recorded and restored.
if [[ -n ${PROFILE_ARMS-} ]]; then
    pp_was=$(cat /proc/sys/kernel/perf_event_paranoid) kp_was=$(cat /proc/sys/kernel/kptr_restrict)
    sudo sysctl -qw kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0
    log "perf: paranoid $pp_was -> -1, kptr_restrict $kp_was -> 0 (restored after)"
fi
for a in ${PROFILE_ARMS-}; do
    printf '%s cycle=P arm=%s-perf ' "$(date -u +%T)" "$a" | tee -a "${OUT%.csv}.order"
    arm "$a" P perf | cut -d, -f11,12,13,14,15,16 | tee -a "${OUT%.csv}.order"
    # Flat symbol list (no call graphs, so it is not truncated by them), callers of the top symbols,
    # and by thread name.
    perf report -i "$SNAP/$a.perf.data" --stdio --no-children -g none --sort dso,symbol --percent-limit 0.2 2>/dev/null | grep -v '^$' | head -150 >"$SNAP/$a.perf-top.txt"
    perf report -i "$SNAP/$a.perf.data" --stdio --no-children -g caller,0.5,callee --sort symbol --percent-limit 3 2>/dev/null | head -600 >"$SNAP/$a.perf-callers.txt"
    perf report -i "$SNAP/$a.perf.data" --stdio --no-children -g none --sort comm --percent-limit 0.5 2>/dev/null | grep -v '^$' | head -40 >"$SNAP/$a.perf-comm.txt"
    rm -f "$SNAP/$a.perf.data"
done
[[ -n ${PROFILE_ARMS-} ]] && sudo sysctl -qw kernel.perf_event_paranoid="$pp_was" kernel.kptr_restrict="$kp_was"
log "done: $OUT (order: ${OUT%.csv}.order, snapshots: $SNAP)"
