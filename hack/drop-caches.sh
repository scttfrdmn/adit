#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Drop the page cache before a cold rung (law 7). Between rungs, not just before the first.
# Prints one status line with the page cache before and after; exits non-zero if the drop failed
# (#70: callers used to discard this, leaving every cold rung unverified).
set +e
set -uo pipefail
cached() { awk '/^Cached:/{print $2}' /proc/meminfo; }
before=$(cached)
sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
st=$?
sleep 3
after=$(cached)
if (( st == 0 )); then
    echo "drop_caches ok cached_kib=$before->$after $(date -u +%FT%TZ)"
else
    echo "drop_caches FAILED status=$st cached_kib=$before->$after $(date -u +%FT%TZ)"
    exit 1
fi
