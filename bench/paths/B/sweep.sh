#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path B tuning grid on the GPU client (law 3: tuned before measured): KvikIO RemoteFile over
# presigned URLs into DEVICES, threads x task size x objects in flight, one cold read each, every
# row in OUT with its configuration. The best row's configuration is then used for H-ceiling.
#
#   PLAN=... DEVICES=0,...,7 THREADS="128 512 1024" TASK_MIB="16 64" CONC="8 32" bash bench/paths/B/sweep.sh T2-64
P=B
source "$(dirname "$0")/../lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
DEVICES=${DEVICES:-0}
BUF_GIB=${BUF_GIB:-64}
PY=${PY:-/opt/adit-venv/bin/python}
for th in ${THREADS:-128 512 1024}; do
    for ts in ${TASK_MIB:-16 64}; do
        for cc in ${CONC:-8 32}; do
            cold
            line=$($PY bench/paths/lib/gpuload.py --mode remotefile --plan "$PLAN" --threads "$th" --task-size-mib "$ts" \
                --concurrency "$cc" --devices "$DEVICES" --buffer-gib "$BUF_GIB" --region "$REGION" --runs 1 \
                "${META[@]}" --out "$OUT" 2>&1 | tail -1)
            echo "threads=$th task_mib=$ts conc=$cc: $(cut -d, -f13-15,23 <<<"$line")"
        done
    done
done
log "done: $OUT"
