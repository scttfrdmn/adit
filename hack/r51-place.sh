#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# #51 reproduction placement: try to land fleet `r51` (one multi-GPU EFA client + 2 c8gn.16xlarge
# on-demand proxies) from a ranked candidate list; stop at the first that lands with its proxies.
# Prints PLACED <region> <type> bucket=<b> or NONE. A client that lands without both proxies is
# retired at once. Failed attempts cost nothing.
#
#   AWS_PROFILE=aws ADIT_SPAWN=~/.xtmp/spawn-661-bin bash hack/r51-place.sh
set +e
set -uo pipefail
echo "opts=$-"
# region: client AMI | proxy AMI | bucket
declare -A REG=(
    [us-east-1]="ami-074a42dcdc3e24113|ami-0fa5a88e250674e12|scttfrdmn-lith-bench"
    [us-east-2]="ami-0e7becd51d15e4e52|ami-0def793130cdf7428|scttfrdmn-adit-bench-use2"
    [us-west-2]="ami-0344cf181124f6e56|ami-0b72af9d934597be7|scttfrdmn-adit-bench-usw2"
)
# type | region | candidates | client cost-limit (2 h at on-demand + headroom), ranked by
# diagnostic value (p5: the hardware that aborted; g7e.24xl/48xl: several rails), then cost.
TRIES=(
    "p5.48xlarge|us-east-2|us-east-2a:spot,us-east-2b:spot,us-east-2c:spot|115"
    "p5.48xlarge|us-east-1|us-east-1a:spot,us-east-1b:spot,us-east-1c:spot,us-east-1d:spot,us-east-1f:spot|115"
    "g7e.24xlarge|us-east-1|us-east-1a,us-east-1c|38"
    "g7e.48xlarge|us-east-1|us-east-1a,us-east-1c|72"
    "p5.48xlarge|us-west-2|us-west-2a:spot,us-west-2b:spot,us-west-2c:spot,us-west-2d:spot|115"
    "g7e.12xlarge|us-east-1|us-east-1a,us-east-1c,us-east-1a:spot,us-east-1c:spot|22"
    "g7e.12xlarge|us-east-2|us-east-2c,us-east-2c:spot|22"
    "p5en.48xlarge|us-west-2|us-west-2b:spot,us-west-2c:spot,us-west-2d:spot|131"
)
LOG=bench/runs/r51-place-$(date -u +%Y%m%dT%H%M%SZ).log
mkdir -p bench/runs
for tr in "${TRIES[@]}"; do
    IFS='|' read -r t r c ccap <<<"$tr"
    IFS='|' read -r cami pami bkt <<<"${REG[$r]}"
    echo "[$(date -u +%TZ)] try $t in $r" | tee -a "$LOG"
    out=$(timeout 2400 go run ./cmd/aditctl fleet up --name r51 --candidates "$c" --client-type "$t" \
        --proxy-type c8gn.16xlarge --proxies 2 --ttl 2h --client-cost-limit "$ccap" --proxy-cost-limit 11 \
        --client-ami "$cami" --proxy-ami "$pami" --bucket "$bkt" --profile "${AWS_PROFILE:-aws}" 2>&1)
    echo "$out" >>"$LOG"
    grep -E '^adit-r51-' <<<"$out" | tee -a "$LOG"
    if grep -q '^adit-r51-client-0' <<<"$out"; then
        if [[ $(grep -c '^adit-r51-proxy-' <<<"$out") -eq 2 ]]; then
            echo "PLACED $r $t bucket=$bkt"
            exit 0
        fi
        echo "[$(date -u +%TZ)] $t landed in $r without both proxies: retiring" | tee -a "$LOG"
        timeout 1500 go run ./cmd/aditctl fleet down --name r51 --candidates "${c%%,*}" --profile "${AWS_PROFILE:-aws}" 2>&1 | tail -1 | tee -a "$LOG"
    fi
done
echo "NONE (log $LOG)"
