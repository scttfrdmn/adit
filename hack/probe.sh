#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G0b single-box facts for the capability matrix, as key=value lines on stdout (also written to
# /var/lib/adit/probe-<instance>.txt). The two-box measurements (nixlbench LIBFABRIC VRAM, the
# adit NIXL probe) are run from the operator with hack/g0.sh because they need a peer.
#
#   bash hack/probe.sh
set +e
set -uo pipefail
echo "opts=$-"
export PATH=/opt/amazon/efa/bin:/usr/local/cuda/bin:$PATH

tok=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 300')
md() { curl -s -H "X-aws-ec2-metadata-token: $tok" "http://169.254.169.254/latest/meta-data/$1"; }
it=$(md instance-type)
out=/var/lib/adit/probe-$it.txt
sudo mkdir -p /var/lib/adit && sudo chmod 1777 /var/lib/adit
exec > >(tee "$out")

say() { printf '%s=%s\n' "$1" "${2-}"; }
say instance "$it"
say region "$(md placement/region)"
say az "$(md placement/availability-zone)"
say ami "$(md ami-id)"
say kernel "$(uname -r)"
[[ -f /var/lib/adit/versions ]] && sed 's/^/version_/' /var/lib/adit/versions
say gpus "$(nvidia-smi -L 2>/dev/null | wc -l)"
say gpu "$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
say nvidia_peermem "$(lsmod | grep -c '^nvidia_peermem')"
say efa_devices "$(ls /sys/class/infiniband 2>/dev/null | wc -l)"

# EFA provider capabilities: RMA (one-sided read/write) and HMEM (device memory) support.
fi=$(fi_info -p efa -v 2>/dev/null)
say fi_efa_present "$([[ -n $fi ]] && echo 1 || echo 0)"
say fi_efa_rma "$(grep -c 'FI_RMA' <<<"$fi")"
say fi_efa_hmem "$(grep -o 'FI_HMEM[A-Z_]*' <<<"$fi" | sort -u | tr '\n' ' ')"
say fi_efa_hmem_cuda "$(FI_HMEM=cuda fi_info -p efa -c FI_HMEM 2>/dev/null | grep -c 'provider: efa')"
fi_info -p efa -v >/var/lib/adit/fi_info-$it.txt 2>&1

# GPUDirect Storage readiness (FSx path D) as gdscheck reports it.
gds=$(ls /usr/local/cuda/gds/tools/gdscheck 2>/dev/null || command -v gdscheck)
if [[ -n $gds ]]; then
    "$gds" -p >/var/lib/adit/gdscheck-$it.txt 2>&1
    say gdscheck_rc "$?"
    say gdscheck_gds "$(grep -iE 'GDS (release|version)|platform verification succeeded' /var/lib/adit/gdscheck-$it.txt | head -1 | tr -s ' ')"
    say gdscheck_efa "$(grep -iE 'efa|rdma' /var/lib/adit/gdscheck-$it.txt | head -3 | tr -s ' ' | tr '\n' '|')"
else
    say gdscheck_rc "absent"
fi
say probe_done "$(date -u +%FT%TZ)"
