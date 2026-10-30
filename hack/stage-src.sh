#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Stage adit's source for boxes with no internet route (`--no-public-ip`, #60): run on the operator
# machine, it puts a `git archive` of REF (with a COMMIT file) and `cargo vendor` of its locked
# dependencies (crates.io and the pinned nixl-sys git dependency) under
# s3://$BUCKET/adit/src/<sha>/, and prints that prefix. On the box, hack/install.sh with
# ADIT_SRC_S3=<prefix> builds from it with cargo --frozen; the rest of the toolchain comes from the
# AMI's markers (install.sh refuses offline if one is missing).
#
#   AWS_PROFILE=aws BUCKET=scttfrdmn-lith-bench REGION=us-east-1 bash hack/stage-src.sh [ref=main]
#
# REF must be on a remote branch, so every staged build is a commit others can check out. Staging a
# sha already in the bucket is a no-op.
set +e
set -uo pipefail
echo "opts=$-" >&2
REF=${1-main}
BUCKET=${BUCKET:-scttfrdmn-lith-bench}
REGION=${REGION:-us-east-1}
log() { echo "[$(date -u +%FT%TZ)] stage-src: $*" >&2; }
fail() { log "FAILED: $*"; exit 1; }

git fetch -q origin || fail "git fetch"
# A branch name means the remote's branch, as install.sh's online refresh checks out origin/<ref>.
sha=$(git rev-parse --verify -q "origin/$REF^{commit}" || git rev-parse --verify -q "$REF^{commit}") || fail "no commit $REF"
[[ -n $(git branch -r --contains "$sha" 2>/dev/null) ]] || fail "$sha is on no remote branch; push it first"
DEST=s3://$BUCKET/adit/src/$sha
aws() { AWS_PROFILE=${AWS_PROFILE-aws} command aws "$@" --region "$REGION"; }
if aws s3 ls "$DEST/vendor.tar.gz" >/dev/null 2>&1 && aws s3 ls "$DEST/adit.tar.gz" >/dev/null 2>&1; then
    log "$sha already staged"
    echo "$DEST"
    exit 0
fi
scratch=$HOME/.xtmp
[[ -d $scratch ]] || scratch=/tmp
W=$scratch/adit-stage-$sha
rm -rf "$W" && mkdir -p "$W" || fail "scratch $W"
trap 'rm -rf "$W"' EXIT
git archive --format=tar --prefix=adit/ --add-virtual-file="adit/COMMIT:$sha" "$sha" | gzip >"$W/adit.tar.gz" || fail "git archive"
tar xzf "$W/adit.tar.gz" -C "$W" || fail "extract"
# --versioned-dirs keeps two versions of one crate apart; the printed source-replacement config is
# kept beside the tarball (install.sh rewrites its directory to the box's vendor path). macOS tar
# would add AppleDouble files without COPYFILE_DISABLE.
(cd "$W/adit" && cargo vendor --locked --versioned-dirs "$W/vendor" 2>/dev/null >"$W/cargo-config.toml") || fail "cargo vendor"
COPYFILE_DISABLE=1 tar czf "$W/vendor.tar.gz" -C "$W" vendor || fail "vendor tarball"
for f in adit.tar.gz vendor.tar.gz cargo-config.toml; do
    aws s3 cp --only-show-errors "$W/$f" "$DEST/$f" || fail "upload $f"
done
log "$sha staged: adit $(du -h "$W/adit.tar.gz" | cut -f1), vendor $(du -h "$W/vendor.tar.gz" | cut -f1) -> $DEST"
echo "$DEST"
