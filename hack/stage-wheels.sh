#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Stage the client's path tools for boxes with no internet route (#60 follow-up, H-ceiling): run on
# a box booted from the client AMI that does have one (any small x86 instance; no GPU needed), it
# installs the same packages as hack/install.sh's pathtools stage into a scratch venv, downloads
# exactly those versions as wheels, checks that a fresh venv installs them with --no-index and
# imports them, and uploads the wheelhouse, its requirements.txt and s5cmd to
# s3://$BUCKET/adit/src/wheels-py<XY>-cu<N>/, where install.sh looks for it offline.
#
#   BUCKET=scttfrdmn-lith-bench REGION=us-east-1 bash hack/stage-wheels.sh
set +e
set -uo pipefail
echo "opts=$-"
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
REGION=${REGION:-us-east-1}
log() { echo "[$(date -u +%FT%TZ)] stage-wheels: $*"; }
fail() { log "FAILED: $*"; exit 1; }

export PATH=/usr/local/cuda/bin:$PATH
cu=$(nvcc --version 2>/dev/null | sed -n 's/.*release \([0-9]*\)\..*/\1/p')
[[ -n $cu ]] || fail "no nvcc: run this on a box from the client AMI"
py=$(python3 -c 'import sys; print("%d%d" % sys.version_info[:2])')
DEST=s3://$BUCKET/adit/src/wheels-py$py-cu$cu
W=${W:-/tmp/adit-wheels}
rm -rf "$W" /tmp/whbuild /tmp/whcheck && mkdir -p "$W" || fail "scratch"
log "python $py, CUDA $cu -> $DEST"

# The same list, names and fallbacks as install.sh's pathtools stage.
python3 -m venv /tmp/whbuild || fail venv
V=/tmp/whbuild/bin/pip
$V install -q --upgrade pip maturin patchelf || fail pip
$V install -q "cupy-cuda${cu}x" || $V install -q cupy-cuda12x || fail cupy
$V install -q "kvikio-cu${cu}" || $V install -q kvikio-cu12 || fail kvikio
TORCH_INDEX=https://download.pytorch.org/whl/cu${cu}0
$V install -q torch --index-url "$TORCH_INDEX" || $V install -q torch || fail torch
$V install -q s3torchconnector || fail s3torchconnector
$V freeze --all | grep -vE '^(pip|setuptools|wheel)==' >"$W/requirements.txt" || fail freeze
log "$(wc -l <"$W/requirements.txt") packages"

# Exactly those versions, as wheels for this platform: `pip wheel` builds any that PyPI has only as
# a source distribution (a plain download left one that needs setuptools to build offline).
$V wheel -q --no-deps -w "$W" -r "$W/requirements.txt" --extra-index-url "$TORCH_INDEX" || fail "pip wheel"
v=2.3.0
curl -fsSL "https://github.com/peak/s5cmd/releases/download/v$v/s5cmd_${v}_Linux-64bit.tar.gz" | tar xz -C "$W" s5cmd || fail s5cmd

# A fresh venv must install from the wheelhouse alone and import every tool.
python3 -m venv /tmp/whcheck || fail "check venv"
/tmp/whcheck/bin/pip install -q --no-index --find-links "$W" -r "$W/requirements.txt" || fail "offline install check"
# Imports are logged, not required: on a box without a GPU driver loaded a CUDA library may refuse.
/tmp/whcheck/bin/python -c 'import cupy, kvikio, torch, s3torchconnector, maturin; print("imports ok: torch", torch.__version__, "kvikio", kvikio.__version__, "cupy", cupy.__version__)' ||
    log "WARNING: import check failed on this box (no GPU driver?); the install check passed"

aws s3 cp --recursive --only-show-errors "$W" "$DEST/" --region "$REGION" || fail upload
log "staged $(du -sh "$W" | cut -f1) in $(ls "$W" | wc -l) files -> $DEST"
