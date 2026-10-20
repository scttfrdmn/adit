#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G2 placement attempt (#48): try to land the GPU client plus proxy-0 of fleet `g2`, stopping at
# the first that lands; prints PLACED <region> <type> or NONE. Attempts that fail cost nothing.
# Order: p5en then p5 in us-east-1 and us-east-2, then p5en in the other regions Scott approved
# (2026-10-20), cheapest first; spot before on-demand within each. Every region has a bench bucket
# (hack/bench-bucket.sh) and copies of the client and proxy AMIs. Proxies 1.. are added after the
# smoke test with `aditctl fleet up --clients 0 --proxy-start 1`.
#
# Outside us-east-1, us-east-2 and us-west-2 the Standard quotas (640 on-demand + 640 spot vCPU)
# hold at most 6 c8gn.48xlarge, so the ladder there tops out at 6 proxies.
#
#   AWS_PROFILE=aws ADIT_SPAWN=~/.xtmp/spawn-661-bin bash hack/g2-place.sh
set +e
set -uo pipefail
echo "opts=$-"
# region: p5en AZs | client AMI | proxy AMI | bucket | p5en $/hr | c8gn.48xlarge $/hr
declare -A REG=(
    [us-east-1]="us-east-1a us-east-1c|ami-074a42dcdc3e24113|ami-0fa5a88e250674e12|scttfrdmn-lith-bench|63.296|11.376"
    [us-east-2]="us-east-2a us-east-2b us-east-2c|ami-0e7becd51d15e4e52|ami-0def793130cdf7428|scttfrdmn-adit-bench-use2|63.296|11.376"
    [us-west-2]="us-west-2b us-west-2c us-west-2d|ami-0344cf181124f6e56|ami-0b72af9d934597be7|scttfrdmn-adit-bench-usw2|63.296|11.376"
    [eu-north-1]="eu-north-1a eu-north-1c|ami-0ad2390b5125d93ae|ami-05ff95952d0a1a004|scttfrdmn-adit-bench-eun1|67.7267|12.2189"
    [ap-south-1]="ap-south-1a ap-south-1b|ami-0fe32c4634c6ffcab|ami-06d16c49be590a254|scttfrdmn-adit-bench-aps1|75.9552|11.376"
    [us-west-1]="us-west-1b|ami-04eb76c4ab04b89ae|ami-05b518fe931b951e2|scttfrdmn-adit-bench-usw1|79.12|14.2195"
    [ap-northeast-1]="ap-northeast-1a|ami-0a4762fe2678dbf89|ami-02a60101243f4a1a6|scttfrdmn-adit-bench-apne1|79.12|14.3251"
    [ap-northeast-2]="ap-northeast-2a|ami-03a21f85036aa7b3f|ami-04a202f87f1350529|scttfrdmn-adit-bench-apne2|87.3485|12.8506"
)
TRIES=(
    "p5en.48xlarge us-east-1" "p5en.48xlarge us-east-2"
    "p5.48xlarge us-east-1" "p5.48xlarge us-east-2"
    "p5en.48xlarge us-west-2" "p5en.48xlarge eu-north-1" "p5en.48xlarge ap-south-1"
    "p5en.48xlarge us-west-1" "p5en.48xlarge ap-northeast-1" "p5en.48xlarge ap-northeast-2"
)
P5_AZS_USE1="us-east-1a us-east-1b us-east-1c us-east-1d us-east-1f"
P5_AZS_USE2="us-east-2a us-east-2b us-east-2c"
LOG=bench/runs/g2-place-$(date -u +%Y%m%dT%H%M%SZ).log
mkdir -p bench/runs
for tr in "${TRIES[@]}"; do
    read -r t r <<<"$tr"
    IFS='|' read -r azs cami pami bkt cprice pprice <<<"${REG[$r]}"
    if [[ $t == p5.48xlarge ]]; then
        cprice=55.04
        [[ $r == us-east-1 ]] && azs=$P5_AZS_USE1 || azs=$P5_AZS_USE2
    fi
    # Spot in every AZ, then on-demand in every AZ.
    c=$(for z in $azs; do printf '%s:spot,' "$z"; done; for z in $azs; do printf '%s,' "$z"; done)
    c=${c%,}
    # Cost limits: 2 h at the on-demand rate plus headroom (spored's hard kill; size for the market
    # the box may fall back to).
    ccap=$(awk -v p="$cprice" 'BEGIN{printf "%d", p*2+5}')
    pcap=$(awk -v p="$pprice" 'BEGIN{printf "%d", p*2+3}')
    echo "[$(date -u +%TZ)] try $t in $r (caps client \$$ccap proxy \$$pcap)" | tee -a "$LOG"
    out=$(timeout 2400 go run ./cmd/aditctl fleet up --name g2 --candidates "$c" --client-type "$t" \
        --proxy-type c8gn.48xlarge --proxies 1 --proxy-spot --ttl 2h --client-cost-limit "$ccap" \
        --proxy-cost-limit "$pcap" --client-ami "$cami" --proxy-ami "$pami" --bucket "$bkt" \
        --profile "${AWS_PROFILE:-aws}" 2>&1)
    rc=$?
    echo "$out" >>"$LOG"
    grep -E '^adit-g2-' <<<"$out" | tee -a "$LOG"
    if grep -q '^adit-g2-client-0' <<<"$out"; then
        [[ $rc -ne 0 ]] && echo "fleet up exit $rc after the client landed: check proxy-0 ($LOG)"
        echo "PLACED $r $t bucket=$bkt"
        exit 0
    fi
done
echo "NONE (log $LOG)"
