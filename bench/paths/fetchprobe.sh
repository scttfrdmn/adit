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
# Arm s<slots>o<objects in flight>[c<max connections>][t<CRT target Gb/s>]; target default 600
# (adit-proxy's G2 setting), max connections default unset (CRT chooses).
P=F
source "$(dirname "$0")/lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
CYCLES=${CYCLES:-3}
ARMS=(${ARMS:-s2048o64 s4096o64 s4096o16 s4096o128 s8192o128 s4096o64c256 s4096o64c1024 s4096o64t1200})
n=${#ARMS[@]}
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
topology | tee "$SNAP/topology.txt"
arm() {
    local a=$1 c=$2 slots objs conn="" tgt=600 extra=()
    slots=$(sed -E 's/^s([0-9]+).*/\1/' <<<"$a")
    objs=$(sed -E 's/^s[0-9]+o([0-9]+).*/\1/' <<<"$a")
    [[ $a =~ c([0-9]+) ]] && conn=${BASH_REMATCH[1]}
    [[ $a =~ t([0-9]+) ]] && tgt=${BASH_REMATCH[1]}
    [[ -n $conn ]] && extra=(--max-connections "$conn")
    cold
    counters >"$SNAP/c$c-$a-pre.txt"
    adit-fetch-bench --plan "$PLAN" --slots "$slots" --objects-inflight "$objs" --chunk-mib 8 \
        --throughput-gbps "$tgt" "${extra[@]}" --region "$REGION" --runs 1 --label "$a" \
        --chunk-log "$SNAP/c$c-$a-chunks.csv" 2>"$SNAP/c$c-$a.stderr" >"$SNAP/c$c-$a.out"
    local st=$?
    counters >"$SNAP/c$c-$a-post.txt"
    [[ -s $OUT ]] || grep '^label,' "$SNAP/c$c-$a.out" | head -1 >"$OUT"
    grep -v '^label,' "$SNAP/c$c-$a.out" | grep "^$a," | tail -1 | tee -a "$OUT"
    (( st == 0 )) || log "arm $a cycle $c: adit-fetch-bench exit $st"
    gzip -f "$SNAP/c$c-$a-chunks.csv"
}
for c in $(seq 1 "$CYCLES"); do
    for k in $(seq 0 $((n - 1))); do
        a=${ARMS[$(( (k + c - 1) % n ))]}
        printf '%s cycle=%s arm=%s ' "$(date -u +%T)" "$c" "$a" | tee -a "${OUT%.csv}.order"
        arm "$a" "$c" | cut -d, -f11,12,13,14,15,16 | tee -a "${OUT%.csv}.order"
    done
done
log "done: $OUT (order: ${OUT%.csv}.order, snapshots: $SNAP)"
