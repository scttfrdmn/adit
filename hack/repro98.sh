#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# #98 / ai-dynamo/nixl#2505 reproducer with NIXL only: no S3, no HTTP, no adit-proxy. One
# long-lived initiator (adit-nixl-probe-init on the proxy box, host DRAM) writes to a sequence of
# fresh target processes (adit-nixl-probe-target on the GPU box, VRAM on GPU 0, a new agent name
# each), idling IDLE_BEFORE seconds before each further round. Prediction (#98): a round that
# follows an idle of more than about 4 min after an earlier round fails its first writes, and the
# target aborts in the EFA provider; a round right after another passes.
#
#   ADIT_SPAWN=... bash hack/repro98.sh <client-box> <proxy-box> <client-ip> [idle-before=300,0,300] [mib=4096]
#
# Both boxes need adit built against the real NIXL (hack/install.sh). Outputs land in
# bench/runs/repro98-<utc>/: the initiator's ROUND lines and every target's PASS/FAIL line.
set +e
set -uo pipefail
echo "opts=$-"
CLIENT=${1:?client box} PROXY=${2:?proxy box} CIP=${3:?client private ip}
IDLE=${4-300,0,300} MIB=${5-4096}
SPAWN=${ADIT_SPAWN:-spawn}
OUT=${OUT:-bench/runs/repro98-$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$OUT"
on() { local box=$1; shift; timeout "${ON_TIMEOUT:-300}" "$SPAWN" connect "$box" --user ubuntu -- "$*" 2>&1 | grep -v -E 'newer version|releases/tag|^$|Looking up|Found instance|Connecting|Permanently|no public IP \(private only\)|via SSM RunCommand'; }
log() { echo "[$(date -u +%FT%TZ)] $*" | tee -a "$OUT/run.log"; }
rounds=$(( 1 + $(tr ',' '\n' <<<"$IDLE" | grep -c .) ))
max_idle=$(tr ',' '\n' <<<"$IDLE" | sort -n | tail -1)
log "repro98: client=$CLIENT proxy=$PROXY ($CIP) rounds=$rounds idle_before=$IDLE mib=$MIB"

# Targets: one new process (and agent) per round, each waiting for its initiator connection.
on "$CLIENT" "pkill -f adit-nixl-probe-target; rm -f /tmp/r98t.out; bgrun -o /tmp/r98t.out bash -c 'for k in \$(seq 1 $rounds); do adit-nixl-probe-target --mib $MIB --agent-name r98-\$k-\$\$ --timeout-s $((max_idle + 600)); echo TARGET_ROUND \$k exit=\$?; done'; sleep 8" | tee -a "$OUT/run.log"
# The initiator: one agent for every round.
on "$PROXY" "pkill -x adit-proxy; rm -f /tmp/r98i.out; bgrun -o /tmp/r98i.out adit-nixl-probe-init --target $CIP:8481 --mib $MIB --chunk-mib 8 --concurrency 32 --idle-before $IDLE" | tee -a "$OUT/run.log"
total=$(( $(tr ',' '\n' <<<"$IDLE" | paste -sd+ - | bc) + rounds * 120 + 120 ))
t0=$SECONDS
while (( SECONDS - t0 < total )); do
    sleep 30
    on "$PROXY" "grep -cE '^ROUND' /tmp/r98i.out; pgrep -x adit-nixl-probe-init >/dev/null && echo running" >"$OUT/poll.txt"
    log "  rounds done: $(head -1 "$OUT/poll.txt")"
    grep -q running "$OUT/poll.txt" || break
done
on "$PROXY" "cat /tmp/r98i.out" >"$OUT/initiator.txt"
on "$CLIENT" "cat /tmp/r98t.out" >"$OUT/targets.txt"
log "initiator: $(grep -E '^ROUND' "$OUT/initiator.txt" | tr '\n' ';')"
log "targets: $(grep -E 'PASS|FAIL|TARGET_ROUND|EFA internal error' "$OUT/targets.txt" | tr '\n' ';' | cut -c1-600)"
