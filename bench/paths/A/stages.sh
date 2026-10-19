#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path A split into its stages (#45): the s5cmd download alone (wall and its own CPU, from
# /usr/bin/time), then the local load + GPU check alone (adit-load with no fetch command), so the
# path's time and CPU are attributed per stage and the window counter can be reconciled against
# s5cmd's own rusage.
#
#   MANIFEST=... CONCS="16 64" RUNS=3 bash bench/paths/A/stages.sh T1
P=A
source "$(dirname "$0")/../lib/common.sh"
MANIFEST=${MANIFEST:?manifest JSONL}
NVME_DIR=${NVME_DIR:-/opt/dlami/nvme}/adit-$WORKLOAD
CONCS=${CONCS:-16 64}
S5_WORKERS=${S5_WORKERS:-256}
BUF_GIB=${BUF_GIB:-$(( ($(jq -s 'map(.size)|max' "$MANIFEST") >> 30) + 1 ))}
sudo mkdir -p "$NVME_DIR" && sudo chmod 1777 "$NVME_DIR"
STAGES=${OUT%.csv}.stages
echo "conc,run,s5_wall_s,s5_user_s,s5_sys_s,s5_maxrss_kb,load_row" >"$STAGES"
for c in $CONCS; do
    jq -r --arg d "$NVME_DIR" '"cp --concurrency '"$c"' s3://\(.bucket)/\(.key) \($d)/\(.key)"' "$MANIFEST" >/tmp/s5cmd-A.txt
    for r in $(seq 1 "$RUNS"); do
        rm -rf "${NVME_DIR:?}"/*
        cold
        /usr/bin/time -f '%e,%U,%S,%M' -o /tmp/s5.time s5cmd --numworkers "$S5_WORKERS" run /tmp/s5cmd-A.txt >/dev/null
        cold
        row=$(adit-load --manifests "$MANIFEST" --dir "$NVME_DIR" --chunk-mib 8 --buffer-gib "$BUF_GIB" \
            "${META[@]}" --meta "config=stage=load;after_s5cmd_concurrency=$c" --out "$OUT" | tail -1)
        echo "$c,$r,$(cat /tmp/s5.time),\"$(cut -d, -f13,14,15,16,23 <<<"$row")\"" | tee -a "$STAGES"
    done
done
log "done: $STAGES"
