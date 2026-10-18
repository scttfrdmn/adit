#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# G0b, the two-process half on one GPU box: nixlbench LIBFABRIC WRITE DRAM -> VRAM with both ranks
# on this box over its EFA device (shm transfer disabled). nixlbench needs CUDA on every rank once
# VRAM is named, so the CUDA-less proxy cannot be a nixlbench peer; the cross-box GDR check is the
# adit NIXL probe from the proxy instead (hack/wseries.sh).
set +e
set -uo pipefail
echo "opts=$-"
ARGS="--etcd_endpoints http://127.0.0.1:2379 --backend LIBFABRIC --initiator_seg_type DRAM --target_seg_type VRAM --op_type WRITE --start_block_size 1048576 --max_block_size 67108864 --start_batch_size 1 --max_batch_size 1 --num_iter 64 --warmup_iter 16"
pkill -x nixlbench; pkill -x etcd; sleep 1; rm -rf /tmp/etcd-data
nohup etcd --data-dir /tmp/etcd-data >/tmp/etcd.out 2>&1 &
sleep 3
export FI_EFA_ENABLE_SHM_TRANSFER=0
nohup nixlbench $ARGS >/tmp/nb-i.out 2>&1 &
sleep 4
timeout 400 nixlbench $ARGS >/tmp/nb-t.out 2>&1
sleep 2
grep -E '^ *[0-9]+ +1 ' /tmp/nb-i.out | awk '{printf "nixlbench block=%s GBps=%s\n", $1, $3}'
grep -q -E 'not supported|rror' /tmp/nb-i.out /tmp/nb-t.out && grep -h -E 'not supported|rror' /tmp/nb-i.out /tmp/nb-t.out | head -3
pkill -x etcd
