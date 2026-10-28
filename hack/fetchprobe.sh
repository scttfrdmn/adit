#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Fetch-only probe (#53), run from the operator machine against one proxy box placed by
# `aditctl fleet up --clients 0 --proxies 1`: rebuild adit at ADIT_REF, presign T2-256 on the box,
# run bench/paths/fetchprobe.sh there (ARMS, CYCLES), then pull the results and push the per-arm
# snapshots to the bench bucket. No GPU client is involved.
#
#   AWS_PROFILE=aws BUCKET=... REGION=... ADIT_REF=main bash hack/fetchprobe.sh <fleet> [out-dir]
set +e
set -uo pipefail
echo "opts=$-"
FLEET=${1:?fleet name}
SPAWN=${ADIT_SPAWN:-spawn}
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
REGION=${REGION:-us-east-1}
ADIT_REF=${ADIT_REF:-$(git rev-parse --abbrev-ref HEAD)}
BOX=adit-$FLEET-proxy-0
OUT=${2:-bench/runs/fp-$(date -u +%Y%m%dT%H%M%SZ)}
REPO=/opt/adit-src/adit
mkdir -p "$OUT"
on() { timeout "${ON_TIMEOUT:-900}" "$SPAWN" connect "$BOX" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently'; }
log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$OUT/run.log"; }
push() { AWS_PROFILE=${AWS_PROFILE-aws} aws s3 cp --recursive --quiet "$OUT" "s3://$BUCKET/adit/fetchprobe/$(basename "$OUT")/" --region "$REGION"; }
trap push EXIT

log "fetchprobe: box=$BOX ref=$ADIT_REF bucket=$BUCKET region=$REGION"
ON_TIMEOUT=2400 on "cd $REPO && git fetch -q origin && git checkout -q $ADIT_REF && git reset -q --hard origin/$ADIT_REF && sudo ADIT_REF=$ADIT_REF bash hack/install.sh proxy 2>&1 | tail -15" >"$OUT/refresh.txt"
log "refresh: $(grep -E '^adit=|install complete|FAIL' "$OUT/refresh.txt" | tr '\n' ' ')"
grep -q 'install complete' "$OUT/refresh.txt" || { log "refresh failed; stopping"; exit 1; }
on "aws s3 cp --only-show-errors s3://$BUCKET/adit/bin/aditctl-linux-arm64 /tmp/aditctl --region $REGION && sudo install /tmp/aditctl /usr/local/bin/aditctl
    aws s3 cp --only-show-errors s3://$BUCKET/adit/manifests/t2.jsonl /tmp/t2.jsonl --region $REGION && aditctl presign --region $REGION --manifest /tmp/t2.jsonl --out /tmp/t2-plan.jsonl --ttl 8h
    head -256 /tmp/t2-plan.jsonl >/tmp/t2-256-plan.jsonl; wc -l /tmp/t2-256-plan.jsonl; cat /var/lib/adit/versions" | tee -a "$OUT/run.log"
# Clear the previous run's outputs on the box: fetchprobe.sh appends (the first fetch probe's rerun
# inherited its predecessor's rows).
on "rm -rf /tmp/fp.csv /tmp/fp.order /tmp/fp.cold /tmp/fp-snap /tmp/fp.out" >/dev/null
pid=$(on "cd $REPO && bgrun -o /tmp/fp.out bash -c 'export BUCKET=$BUCKET REGION=$REGION; PLAN=/tmp/t2-256-plan.jsonl CYCLES=${CYCLES:-3} ${ARMS:+ARMS=\"$ARMS\"} ${NET_IFACES:+NET_IFACES=$NET_IFACES} OUT=/tmp/fp.csv bash bench/paths/fetchprobe.sh T2-256; echo EXIT=\$?'" | grep -o 'pid=[0-9]*' | cut -d= -f2)
[[ -n $pid ]] || { log "fetchprobe did not start"; exit 1; }
t0=$SECONDS
while :; do
    sleep 30
    on "tail -1 /tmp/fp.out" | tail -1 | sed 's/^/  fp: /'
    on "kill -0 $pid 2>/dev/null && echo running" | grep -q running || break
    (( SECONDS - t0 > ${STEP_TIMEOUT:-3600} )) && { log "still running after ${STEP_TIMEOUT:-3600}s; leaving it"; break; }
done
on "cat /tmp/fp.out" >"$OUT/fp.out"
on "cat /tmp/fp.csv" >"$OUT/fp.csv"
on "cat /tmp/fp.order" >"$OUT/fp.order"
on "cat /tmp/fp.cold" >"$OUT/fp.cold"
on "cat /tmp/fp-snap/topology.txt" >"$OUT/topology.txt"
on "aws s3 cp --recursive --quiet /tmp/fp-snap s3://$BUCKET/adit/fetchprobe/$(basename "$OUT")/fp-snap/ --region $REGION && echo snapshots pushed" | tee -a "$OUT/run.log"
log "fetchprobe: $(grep -E '^EXIT=' "$OUT/fp.out" | tr '\n' ' ') rows=$(grep -vc '^label' "$OUT/fp.csv")"
