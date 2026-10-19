#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# #18 time series: the write-only NIXL probe (proxy DRAM -> client VRAM, no S3) repeated on fresh
# processes every INTERVAL seconds, with the proxy's EFA error/retransmit counters per run, summed
# over every EFA device, and `rails`: devices whose write bytes moved / devices (multi-card proxies
# such as c8gn.48xlarge have two; a run on one of them is a configuration defect, G2).
#
#   ADIT_SPAWN=... bash hack/wseries.sh <client-box> <proxy-box> <client-ip> <runs> <interval-s> <mib> <inflight> <label>
set +e
set -uo pipefail
echo "opts=$-"
CLIENT=$1 PROXY=$2 CIP=$3 RUNS=$4 INTERVAL=$5 MIB=$6 INFL=$7 LABEL=$8
SPAWN=${ADIT_SPAWN:-spawn}
OUT=bench/runs/wseries-$LABEL-$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT"
on() { local box=$1; shift; timeout 600 "$SPAWN" connect "$box" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently'; }
# One line: summed write bytes, wr_err, retrans_pkts, retrans_timeouts, then per-device write bytes.
ctr='b=0 e=0 r=0 t=0 per=; for d in /sys/class/infiniband/*; do c=$d/ports/1/hw_counters; w=$(cat $c/rdma_write_bytes); b=$((b+w)); e=$((e+$(cat $c/rdma_write_wr_err))); r=$((r+$(cat $c/retrans_pkts))); t=$((t+$(cat $c/retrans_timeout_events))); per=$per\ $w; done; echo $b $e $r $t$per'
# Devices whose counter moved between two `ctr` lines, as active/total.
rails() { awk -v a="$1" -v b="$2" 'BEGIN{n=split(a,x," ");split(b,y," ");m=0;for(i=5;i<=n;i++)if(y[i]>x[i])m++;printf "%d/%d",m,n-4}'; }
echo "label,run,utc,mib,inflight,secs,gbps,failed,wr_err_delta,retrans_pkts_delta,retrans_timeouts_delta,verdict,rails" >"$OUT/series.csv"
for i in $(seq 1 "$RUNS"); do
    on "$CLIENT" "pkill -f 'adit-nixl-probe-targe[t]'" >/dev/null
    sleep 1
    on "$CLIENT" "bgrun -o /tmp/pt.out adit-nixl-probe-target --mib $MIB --timeout-s 300; sleep 6" >/dev/null
    c0=$(on "$PROXY" "$ctr" | tail -1)
    read -r b0 e0 r0 t0 _ <<<"$c0"
    line=$(on "$PROXY" "adit-nixl-probe-init --target $CIP:8481 --mib $MIB --chunk-mib 8 --concurrency $INFL 2>&1" | tee -a "$OUT/raw.txt" | grep -E '^(INIT|TARGET)' | tr '\n' ' ')
    c1=$(on "$PROXY" "$ctr" | tail -1)
    read -r b1 e1 r1 t1 _ <<<"$c1"
    secs=$(grep -o 'secs=[0-9.]*' <<<"$line" | head -1 | cut -d= -f2)
    gbps=$(grep -o 'gbps=[0-9.]*' <<<"$line" | head -1 | cut -d= -f2)
    failed=$(grep -o 'failed=[0-9]*' <<<"$line" | head -1 | cut -d= -f2)
    verdict=$(grep -o -E 'TARGET (PASS|FAIL)' <<<"$line" | awk '{print $2}')
    row="$LABEL,$i,$(date -u +%FT%TZ),$MIB,$INFL,${secs-},${gbps-},${failed-},$((${e1:-0}-${e0:-0})),$((${r1:-0}-${r0:-0})),$((${t1:-0}-${t0:-0})),${verdict:-none},$(rails "$c0" "$c1")"
    echo "$row" | tee -a "$OUT/series.csv"
    [[ $i -lt $RUNS ]] && sleep "$INTERVAL"
done
echo "done: $OUT"
