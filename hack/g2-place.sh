#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G2 placement attempt (#48): try to land the GPU client plus proxy-0 of fleet `g2`, in order
# p5en then p5, us-east-1 then us-east-2, spot before on-demand. Stops at the first that lands;
# prints PLACED <region> <type> or NONE. Attempts that fail cost nothing. Proxies 1..7 are added
# after the smoke test with `aditctl fleet up --clients 0 --proxy-start 1`.
#
#   AWS_PROFILE=aws ADIT_SPAWN=~/.xtmp/spawn-661-bin bash hack/g2-place.sh
set +e
set -uo pipefail
echo "opts=$-"
declare -A CAND=(
    [us-east-1]="us-east-1a:spot,us-east-1c:spot,us-east-1b:spot,us-east-1d:spot,us-east-1a,us-east-1c,us-east-1b,us-east-1d,us-east-1f"
    [us-east-2]="us-east-2a:spot,us-east-2b:spot,us-east-2c:spot,us-east-2a,us-east-2b,us-east-2c"
)
declare -A CAMI=([us-east-1]=ami-074a42dcdc3e24113 [us-east-2]=ami-0e7becd51d15e4e52)
declare -A PAMI=([us-east-1]=ami-0fa5a88e250674e12 [us-east-2]=ami-0def793130cdf7428)
declare -A BKT=([us-east-1]=scttfrdmn-lith-bench [us-east-2]=scttfrdmn-adit-bench-use2)
# Cost limits sized for 2 h on-demand (spored's hard kill); TTL 2 h, extended as the session needs.
declare -A CAP=([p5en.48xlarge]=130 [p5.48xlarge]=115)
LOG=bench/runs/g2-place-$(date -u +%Y%m%dT%H%M%SZ).log
mkdir -p bench/runs
for t in p5en.48xlarge p5.48xlarge; do
    for r in us-east-1 us-east-2; do
        # p5en is not offered in us-east-1b/1d/1f; fleet up skips AZs without a default subnet only,
        # so drop them here rather than spend three API retries each.
        c=${CAND[$r]}
        [[ $t == p5en.48xlarge && $r == us-east-1 ]] && c="us-east-1a:spot,us-east-1c:spot,us-east-1a,us-east-1c"
        echo "[$(date -u +%TZ)] try $t in $r" | tee -a "$LOG"
        out=$(timeout 2400 go run ./cmd/aditctl fleet up --name g2 --candidates "$c" --client-type "$t" \
            --proxy-type c8gn.48xlarge --proxies 1 --proxy-spot --ttl 2h --client-cost-limit "${CAP[$t]}" \
            --proxy-cost-limit 25 --client-ami "${CAMI[$r]}" --proxy-ami "${PAMI[$r]}" --bucket "${BKT[$r]}" \
            --profile "${AWS_PROFILE:-aws}" 2>&1)
        rc=$?
        echo "$out" >>"$LOG"
        grep -E '^adit-g2-' <<<"$out" | tee -a "$LOG"
        if grep -q '^adit-g2-client-0' <<<"$out"; then
            [[ $rc -ne 0 ]] && echo "fleet up exit $rc after the client landed: check proxy-0 ($LOG)"
            echo "PLACED $r $t"
            exit 0
        fi
    done
done
echo "NONE (log $LOG)"
