#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Per-thread CPU of one process (#53 pair split): every INTERVAL seconds until the process exits
# (or this is killed), one line per thread, `<unix time> <tid> <name> <utime ticks> <stime ticks>`,
# from /proc/<pid>/task/*/stat. No sysstat needed (offline boxes have none). Thread names separate
# the CRT event loops, tokio workers and NIXL/libfabric progress threads; deltas give CPU-s per
# class (ticks / getconf CLK_TCK).
#
#   bash bench/paths/threadcpu.sh <pid> <out> [interval=1]
set +e
set -uo pipefail
PID=${1:?pid} OUTF=${2:?out} IV=${3-1}
echo "# pid=$PID clk_tck=$(getconf CLK_TCK) interval=$IV" >"$OUTF"
while kill -0 "$PID" 2>/dev/null; do
    t=$(date +%s.%N)
    for f in /proc/"$PID"/task/*/stat; do
        # comm is in parentheses and may hold spaces; utime and stime are the 12th and 13th
        # fields after it.
        s=$(cat "$f" 2>/dev/null) || continue
        name=${s#*(}; name=${name%)*}
        rest=${s##*) }
        read -r -a fl <<<"$rest"
        tid=${f%/stat}; tid=${tid##*/}
        echo "$t $tid ${name// /_} ${fl[11]} ${fl[12]}"
    done >>"$OUTF"
    sleep "$IV"
done
