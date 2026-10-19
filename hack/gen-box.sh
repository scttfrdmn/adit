#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Generator role: write a workload's objects to the bench bucket from a box in the bucket's region
# (`aditctl gen` verifies S3's stored composite for every object) and push the manifest and the log.
# Deterministic by seed, so a re-run rewrites identical bytes.
#
#   spawn launch ... --command "aws s3 cp s3://BUCKET/adit/bin/gen-box.sh /tmp/ && bash /tmp/gen-box.sh T2"
#   bash hack/gen-box.sh <T1|T2|T3> [objects=0 (all)]
set +e
set -uo pipefail
echo "opts=$-"
W=${1:?workload} N=${2-0}
BUCKET=${BUCKET:-scttfrdmn-lith-bench} REGION=${REGION:-us-east-1}
w=$(tr '[:upper:]' '[:lower:]' <<<"$W")
LOG=/tmp/gen-$w.log MAN=/tmp/$w.jsonl
dest=s3://$BUCKET/adit
push() {
    aws s3 cp "$LOG" "$dest/results/gen-$w-$(hostname).log" --region "$REGION" --quiet
    [[ -s $MAN ]] && aws s3 cp "$MAN" "$dest/manifests/$w.jsonl" --region "$REGION" --quiet
}
trap push EXIT
exec > >(tee -a "$LOG") 2>&1
aws s3 cp "$dest/bin/aditctl-linux-$(dpkg --print-architecture)" /tmp/aditctl --region "$REGION" --quiet || { echo "no aditctl"; exit 1; }
chmod +x /tmp/aditctl
/tmp/aditctl region-check --bucket "$BUCKET" --region "$REGION" || { echo "REGION CHECK FAILED"; exit 1; }
/tmp/aditctl gen -workload "$W" -objects "$N" -objects-parallel 16 -region "$REGION" -bucket "$BUCKET" -out "$MAN"
rc=$?
echo "gen exit $rc"
exit "$rc"
