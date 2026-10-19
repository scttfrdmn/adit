#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Drop the page cache before a cold rung (law 7). Between rungs, not just before the first.
set +e
set -uo pipefail
sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
sleep 3
echo "drop_caches ok $(date -u +%FT%TZ)"
