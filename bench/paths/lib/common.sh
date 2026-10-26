# SPDX-License-Identifier: Apache-2.0
# Sourced by every bench/paths/<P>/run.sh: the shared contract (README.md).
set +e
set -uo pipefail
echo "opts=$-"
P=${P:?path letter}
WORKLOAD=${1-}
[[ -n $WORKLOAD ]] || { echo "usage: $0 <workload>" >&2; exit 2; }
RUNS=${RUNS:-3}
COLD=${COLD:-1}
REGION=${REGION:-us-east-1}
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
mkdir -p bench/runs
OUT=${OUT:-bench/runs/$P-$WORKLOAD-$(date -u +%Y%m%dT%H%M%SZ).csv}
if [[ -z ${CLIENT_INSTANCE-} ]]; then
    tok=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
    CLIENT_INSTANCE=$(curl -s -H "X-aws-ec2-metadata-token: $tok" http://169.254.169.254/latest/meta-data/instance-type)
fi
COMMIT=${COMMIT:-$(awk -F= '/^adit=/{print $2}' /var/lib/adit/versions 2>/dev/null)}
log() { echo "[$(date -u +%FT%TZ)] $P $WORKLOAD: $*"; }
aditctl region-check --bucket "$BUCKET" --region "$REGION" || { log "region check failed; refusing to run"; exit 1; }
# Each drop's status line goes to ${OUT%.csv}.cold, one per cold rung, so every cold=1 row has its
# evidence (#70); a failed drop is also logged to stderr.
cold() {
    [[ $COLD == 1 ]] || return 0
    local r st
    r=$(sudo bash hack/drop-caches.sh 2>&1)
    st=$?
    echo "$r" >>"${OUT%.csv}.cold"
    (( st == 0 )) || log "WARNING: $r" >&2
    return 0
}
META=(--meta "path=$P" --meta "workload=$WORKLOAD" --meta "cold=$COLD" --meta "client_instance=$CLIENT_INSTANCE" --meta "commit=$COMMIT")
log "runs=$RUNS cold=$COLD region=$REGION bucket=$BUCKET out=$OUT commit=$COMMIT"
