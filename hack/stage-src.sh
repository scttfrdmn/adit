#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Stage adit's source for boxes with no internet route (`--no-public-ip`, #60): run on the operator
# machine, it puts a `git archive` of REF (with a COMMIT file) and `cargo vendor` of its locked
# dependencies (crates.io and the pinned nixl-sys git dependency) under
# s3://$BUCKET/adit/src/<sha>/, and prints that prefix. It also stages the NIXL source at that
# commit's NIXL_REF, with its meson subprojects downloaded, under s3://$BUCKET/adit/src/nixl-<ref>/:
# the AMIs predate the pin (v1.2.0), so every refresh rebuilds NIXL. On the box, hack/install.sh
# with ADIT_SRC_S3=<prefix> builds both from these (meson --wrap-mode=nodownload, cargo --frozen);
# the rest of the toolchain comes from the AMI's markers (install.sh refuses offline if one is
# missing). Needs meson on the operator machine (MESON, default meson on PATH).
#
#   AWS_PROFILE=aws BUCKET=scttfrdmn-lith-bench REGION=us-east-1 bash hack/stage-src.sh [ref=main]
#
# REF must be on a remote branch, so every staged build is a commit others can check out. Staging a
# sha or NIXL ref already in the bucket is a no-op.
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
MESON=${MESON:-meson}
aws() { AWS_PROFILE=${AWS_PROFILE-aws} command aws "$@" --region "$REGION"; }
staged() { local f; for f in "$@"; do aws s3 ls "$f" >/dev/null 2>&1 || return 1; done; }
scratch=$HOME/.xtmp
[[ -d $scratch ]] || scratch=/tmp
W=$scratch/adit-stage-$sha
rm -rf "$W" && mkdir -p "$W" || fail "scratch $W"
trap 'rm -rf "$W"' EXIT

nixl_ref=$(git show "$sha:hack/install.sh" | sed -n 's/^NIXL_REF=\${NIXL_REF:-\([0-9a-f]*\)}$/\1/p')
[[ -n $nixl_ref ]] || fail "no NIXL_REF in $sha:hack/install.sh"
NDEST=s3://$BUCKET/adit/src/nixl-$nixl_ref
if staged "$NDEST/nixl.tar.gz"; then
    log "nixl $nixl_ref already staged"
else
    command -v "$MESON" >/dev/null || fail "no meson ($MESON) to download NIXL's subprojects"
    mkdir -p "$W/nixl" || fail "nixl dir"
    (cd "$W/nixl" && git init -q && git remote add origin https://github.com/ai-dynamo/nixl.git &&
        git fetch -q --depth 1 origin "$nixl_ref" && git checkout -q FETCH_HEAD) || fail "nixl fetch $nixl_ref"
    # Download every subproject, then keep what meson on the box needs offline: wrap-file archives
    # (and patches) in packagecache/, which it unpacks under --wrap-mode=nodownload, and wrap-git
    # clones (patches already applied), which it cannot fetch. The unpacked wrap-file trees are
    # dropped: unpacking on a case-insensitive filesystem (macOS) breaks liburing's man-page
    # symlinks, so download's own exit status is not the check. Crate wraps (meson reads NIXL's
    # Cargo.lock) are dropped too; the box's build has rust=false.
    sp=$W/nixl/subprojects
    (cd "$W/nixl" && "$MESON" subprojects download) >"$W/nixl-subprojects.log" 2>&1
    keep=() files=()
    for w in "$sp"/*.wrap; do
        if grep -q '^\[wrap-git\]' "$w"; then
            d=$(awk -F' *= *' '$1=="directory"{print $2}' "$w")
            keep+=("${d:-$(basename "$w" .wrap)}")
        else
            read -ra more <<<"$(awk -F' *= *' '$1=="source_filename" || $1=="patch_filename"{print $2}' "$w" | paste -sd' ' -)"
            files+=("${more[@]}")
        fi
    done
    for d in "$sp"/*/; do
        d=$(basename "$d")
        [[ $d == packagecache || $d == packagefiles || " ${keep[*]} " == *" $d "* ]] || rm -rf "${sp:?}/$d"
    done
    for f in "$sp"/packagecache/*; do
        [[ " ${files[*]} " == *" $(basename "$f") "* ]] || rm -f "$f"
    done
    for d in "${keep[@]}"; do [[ -d $sp/$d ]] || fail "nixl subproject $d not cloned"; done
    for f in "${files[@]}"; do [[ -f $sp/packagecache/$f ]] || fail "nixl subproject archive $f not downloaded"; done
    log "nixl subprojects: git ${keep[*]}; archives ${files[*]}"
    rm -rf "$W/nixl/.git" && COPYFILE_DISABLE=1 tar czf "$W/nixl.tar.gz" -C "$W" nixl || fail "nixl tarball"
    aws s3 cp --only-show-errors "$W/nixl.tar.gz" "$NDEST/nixl.tar.gz" || fail "upload nixl"
    log "nixl $nixl_ref staged: $(du -h "$W/nixl.tar.gz" | cut -f1) -> $NDEST"
fi

if staged "$DEST/adit.tar.gz" "$DEST/vendor.tar.gz" "$DEST/cargo-config.toml"; then
    log "$sha already staged"
    echo "$DEST"
    exit 0
fi
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
