#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path B: streaming loaders, no disk. KvikIO RemoteFile reads presigned HTTPS URLs straight into
# CuPy device buffers (runs repeat within one process; there is no page cache on this path); the S3 Connector for PyTorch reads to host then copies to the GPU. Every
# object is checked on the GPU by the same oracle as path E.
#
# Path knobs (swept before measuring, law 3):
#   LOADERS        which loaders (default "remotefile s3connector")
#   KVIKIO_THREADS thread pools to sweep (default "16 64 128")
#   TASK_MIB       KvikIO task sizes to sweep (default "4 16")
#   CONCURRENCY    objects in flight (default 8)
P=B
source "$(dirname "$0")/../lib/common.sh"
PLAN=${PLAN:?plan JSONL with presigned urls}
LOADERS=${LOADERS:-remotefile s3connector}
KVIKIO_THREADS=${KVIKIO_THREADS:-16 64 128}
TASK_MIB=${TASK_MIB:-4 16}
CONCURRENCY=${CONCURRENCY:-8}
PY=${PY:-/opt/adit-venv/bin/python}
for loader in $LOADERS; do
    if [[ $loader == remotefile ]]; then
        for th in $KVIKIO_THREADS; do for ts in $TASK_MIB; do
            cold
            $PY bench/paths/lib/gpuload.py --mode remotefile --plan "$PLAN" --threads "$th" --task-size-mib "$ts" \
                --concurrency "$CONCURRENCY" --region "$REGION" --runs "$RUNS" "${META[@]}" --out "$OUT" | tail -n +2
        done; done
    else
        cold
        $PY bench/paths/lib/gpuload.py --mode s3connector --manifest "$PLAN" --concurrency "$CONCURRENCY" \
            --region "$REGION" --runs "$RUNS" "${META[@]}" --out "$OUT" | tail -n +2
    fi
done
log "done: $OUT"
