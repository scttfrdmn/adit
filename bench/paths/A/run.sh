#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path A: copy, then upload. s5cmd copies the workload to instance NVMe inside the measured window,
# then adit-load reads each file through a pinned staging buffer into GPU memory and checks it on the
# GPU. The window covers copy + load: the whole cost of the path.
#
# Path knobs (tune before measuring, law 3):
#   NVME_DIR     local NVMe root (default: the DLAMI instance-store mount /opt/dlami/nvme)
#   S5_WORKERS   s5cmd --numworkers (default 256)
#   S5_CONC      s5cmd cp --concurrency, parts per object (default 16)
#   CHUNK_MIB    pinned staging chunk (default 8)
P=A
source "$(dirname "$0")/../lib/common.sh"
MANIFEST=${MANIFEST:?manifest JSONL}
NVME_DIR=${NVME_DIR:-/opt/dlami/nvme}/adit-$WORKLOAD
S5_WORKERS=${S5_WORKERS:-256}
S5_CONC=${S5_CONC:-16}
CHUNK_MIB=${CHUNK_MIB:-8}
BUF_GIB=${BUF_GIB:-$(( ($(jq -s 'map(.size)|max' "$MANIFEST") >> 30) + 1 ))}
sudo mkdir -p "$NVME_DIR" && sudo chmod 1777 "$NVME_DIR"
jq -r --arg d "$NVME_DIR" '"cp --concurrency '"$S5_CONC"' s3://\(.bucket)/\(.key) \($d)/\(.key)"' "$MANIFEST" >/tmp/s5cmd-A.txt
for r in $(seq 1 "$RUNS"); do
    rm -rf "${NVME_DIR:?}"/*
    cold
    adit-load --manifests "$MANIFEST" --dir "$NVME_DIR" --chunk-mib "$CHUNK_MIB" --buffer-gib "$BUF_GIB" \
        --fetch-cmd "s5cmd --numworkers $S5_WORKERS run /tmp/s5cmd-A.txt" "${META[@]}" --meta "run=$r" \
        --meta "config=s5cmd_workers=$S5_WORKERS;s5cmd_concurrency=$S5_CONC;chunk_mib=$CHUNK_MIB;nvme=$NVME_DIR" \
        --out "$OUT" | tail -1
done
log "done: $OUT"
