#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path C: FUSE mount + cuFile compatibility mode. The workload's bucket is mounted read-only in its
# native key layout by lith and by Mountpoint for Amazon S3; gpuload.py reads every manifest object
# through KvikIO CuFile with KVIKIO_COMPAT_MODE=ON (POSIX pread into a host bounce buffer, then a
# copy to the GPU) and checks it on the GPU against the manifest. Scaffold: not run until G3.
#
# Path knobs (tune before measuring, law 3; see C/README.md for where each best value comes from):
#   MOUNTS          file systems to run (default "lith mountpoint")
#   MNT             mount root (default /mnt/adit-C); each FS mounts at $MNT/<fs>/<PREFIX> so that
#                   object key K is the file $MNT/<fs>/K, which is what gpuload --dir expects
#   PREFIX          key prefix to root the mount at (default: the longest common directory of the
#                   manifest's keys; "" mounts the bucket root)
#   NIC_GBPS        Mountpoint --maximum-throughput-gbps (default: the instance type's baseline
#                   network bandwidth from DescribeInstanceTypes; unset/empty = Mountpoint auto)
#   LITH_FLAGS      extra `lith mount` flags (default empty: lith defaults, sized by lith itself)
#   MP_FLAGS        extra `mount-s3` flags (default empty)
#   LITH_INDEX      lith index file (default $MNT/lith-<workload>.lithidx; built before any run,
#                   outside the measured window, build time recorded in the header)
#   KVIKIO_THREADS  KvikIO thread pools to sweep (default "16 64 128")
#   TASK_MIB        KvikIO task sizes to sweep (default "4 16")
#   CONCURRENCY     objects in flight, swept (default "8")
#   REMOUNT         1 = unmount and remount before every run (default: 1 when COLD=1). drop_caches
#                   clears the kernel page cache but not a FUSE daemon's own memory: lith's
#                   in-process block cache survives it, so a cold run needs a fresh daemon.
#   INSTALL         1 = install missing lith / mount-s3 binaries (install_tools below); default 0
#
# Every row's config gets ";fs=<fs>;mount=<flags>" appended by this script (gpuload's own config
# column is left as gpuload wrote it, then suffixed), and each mount writes a "# mount: ..." line
# into OUT before its rows.
P=C
source "$(dirname "$0")/../lib/common.sh"
MANIFEST=${MANIFEST:?manifest JSONL}
MOUNTS=${MOUNTS:-lith mountpoint}
MNT=${MNT:-/mnt/adit-C}
KVIKIO_THREADS=${KVIKIO_THREADS:-16 64 128}
TASK_MIB=${TASK_MIB:-4 16}
CONCURRENCY=${CONCURRENCY:-8}
LITH_FLAGS=${LITH_FLAGS-}
MP_FLAGS=${MP_FLAGS-}
LITH_INDEX=${LITH_INDEX:-$MNT/lith-$WORKLOAD.lithidx}
REMOUNT=${REMOUNT:-$COLD}
INSTALL=${INSTALL:-0}
PY=${PY:-/opt/adit-venv/bin/python}

# --- tools -----------------------------------------------------------------------------------------

arch_deb() { case $(uname -m) in aarch64) echo arm64 ;; *) echo x86_64 ;; esac; }
arch_go() { case $(uname -m) in aarch64) echo arm64 ;; *) echo amd64 ;; esac; }

# Only ever called with INSTALL=1. Sources: lith README ("Download the static binary"), Mountpoint
# release bucket (the same URL lith's hack/devbox.sh uses, per-arch).
install_tools() {
    if ! command -v lith >/dev/null 2>&1; then
        log "installing lith (latest release, linux_$(arch_go))"
        curl -fsSL "https://github.com/scttfrdmn/lith/releases/latest/download/lith_linux_$(arch_go)" -o /tmp/lith \
            && chmod +x /tmp/lith && sudo mv /tmp/lith /usr/local/bin/lith
        [[ $? == 0 ]] || { log "lith install failed"; return 1; }
    fi
    if ! command -v mount-s3 >/dev/null 2>&1; then
        log "installing mount-s3 (latest, $(arch_deb))"
        curl -fsSL "https://s3.amazonaws.com/mountpoint-s3-release/latest/$(arch_deb)/mount-s3.deb" -o /tmp/mount-s3.deb \
            && sudo apt-get install -y -qq /tmp/mount-s3.deb >/dev/null
        [[ $? == 0 ]] || { log "mount-s3 install failed"; return 1; }
    fi
}

need_tool() { # need_tool <fs>
    local bin; case $1 in lith) bin=lith ;; mountpoint) bin=mount-s3 ;; *) log "unknown fs $1"; return 1 ;; esac
    command -v "$bin" >/dev/null 2>&1 && return 0
    log "$bin not found. Install it, or rerun with INSTALL=1:"
    case $1 in
        lith) log "  curl -L https://github.com/scttfrdmn/lith/releases/latest/download/lith_linux_$(arch_go) -o lith && chmod +x lith && sudo mv lith /usr/local/bin/" ;;
        mountpoint) log "  curl -fsSL -o mount-s3.deb https://s3.amazonaws.com/mountpoint-s3-release/latest/$(arch_deb)/mount-s3.deb && sudo apt-get install -y ./mount-s3.deb" ;;
    esac
    return 1
}

if [[ $INSTALL == 1 ]]; then install_tools || exit 1; fi
command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 || { log "no fusermount3/fusermount (apt-get install fuse3)"; exit 1; }
for fs in $MOUNTS; do need_tool "$fs" || exit 1; done
[[ -x $PY ]] || { log "no $PY"; exit 1; }

# --- layout ----------------------------------------------------------------------------------------

mbucket=$(jq -r -s 'map(.bucket)|unique|if length==1 then .[0] else "MIXED" end' "$MANIFEST")
[[ $mbucket == "$BUCKET" ]] || { log "manifest bucket '$mbucket' != BUCKET '$BUCKET'; refusing"; exit 1; }
if [[ -z ${PREFIX+x} ]]; then
    # Longest common directory (ends in "/") of every key; "" if the keys share none.
    PREFIX=$(jq -r -s '[.[].key|split("/")|.[:-1]] as $d
        | [range(0; ($d|map(length)|min) + 1) | . as $n | select(($d|map(.[:$n])|unique|length) == 1)] | max as $n
        | $d[0][:$n] | if length==0 then "" else join("/")+"/" end' "$MANIFEST")
fi
[[ -z $PREFIX || $PREFIX == */ ]] || PREFIX="$PREFIX/"
bad=$(jq -r --arg p "$PREFIX" 'select(.key|startswith($p)|not)|.key' "$MANIFEST" | head -1)
[[ -z $bad ]] || { log "key '$bad' is not under PREFIX '$PREFIX'; refusing"; exit 1; }
sudo mkdir -p "$MNT" && sudo chown "$(id -u):$(id -g)" "$MNT" || { log "cannot create $MNT"; exit 1; }

if [[ -z ${NIC_GBPS+x} ]]; then
    # UNVERIFIED field: NetworkInfo.NetworkCards[0].BaselineBandwidthInGbps. Card 0 carries the
    # primary ENA interface, the one S3 traffic leaves through on a multi-card (EFA) instance.
    NIC_GBPS=$(aws ec2 describe-instance-types --region "$REGION" --instance-types "$CLIENT_INSTANCE" \
        --query 'InstanceTypes[0].NetworkInfo.NetworkCards[0].BaselineBandwidthInGbps' --output text 2>/dev/null)
    [[ $NIC_GBPS =~ ^[0-9.]+$ ]] || { log "NIC baseline unavailable; Mountpoint will auto-detect"; NIC_GBPS=; }
fi
NIC_GBPS=${NIC_GBPS%%.*} # --maximum-throughput-gbps takes an integer (UNVERIFIED)

LITH_VER=$(lith version 2>/dev/null | head -1)
MP_VER=$(mount-s3 --version 2>/dev/null | head -1)
log "prefix='$PREFIX' mnt=$MNT nic_gbps=${NIC_GBPS:-auto} remount=$REMOUNT lith='$LITH_VER' mount-s3='$MP_VER'"

# OUT starts with its own comment block and the schema header, so gpuload (which writes a header
# only into an empty file) never writes one mid-file.
if [[ ! -s $OUT ]]; then
    {
        echo "# path C: FUSE + cuFile compat. instance=$CLIENT_INSTANCE region=$REGION bucket=$BUCKET prefix=$PREFIX commit=$COMMIT"
        echo "# manifest=$MANIFEST cold=$COLD remount=$REMOUNT lith=$LITH_VER mount-s3=$MP_VER"
        grep -v '^#' bench/schema/paths.csv | grep -m1 .
    } >"$OUT"
fi

# --- mount / unmount -------------------------------------------------------------------------------

MOUNTED= # mountpoint currently mounted by this script, for the EXIT trap
mp_of() { echo "$MNT/$1${PREFIX:+/${PREFIX%/}}"; }

mount_opts() { # the flags in force for <fs>, as recorded in config
    case $1 in
        lith) echo "lith mount --daemon --region $REGION --index-file $LITH_INDEX --metrics 127.0.0.1:9101 $LITH_FLAGS" ;;
        mountpoint) echo "mount-s3 --read-only --region $REGION${PREFIX:+ --prefix $PREFIX}${NIC_GBPS:+ --maximum-throughput-gbps $NIC_GBPS} $MP_FLAGS" ;;
    esac
}

wait_mounted() { # wait_mounted <mp> <want 0|1>
    local i
    for i in $(seq 1 60); do
        if mountpoint -q "$1"; then [[ $2 == 1 ]] && return 0; else [[ $2 == 0 ]] && return 0; fi
        sleep 0.5
    done
    return 1
}

do_mount() { # do_mount <fs>
    local mp; mp=$(mp_of "$1")
    mkdir -p "$mp" || { log "mkdir $mp failed"; return 1; }
    mountpoint -q "$mp" && { log "$mp already mounted; refusing"; return 1; }
    local url="s3://$BUCKET${PREFIX:+/${PREFIX%/}}"
    case $1 in
        # lith runs in the foreground unless --daemon; --daemon returns once the mount is ready.
        lith) lith mount "$url" "$mp" --daemon --region "$REGION" --index-file "$LITH_INDEX" \
                  --metrics 127.0.0.1:9101 $LITH_FLAGS ;;
        # mount-s3 daemonizes by default.
        mountpoint) mount-s3 "$BUCKET" "$mp" --read-only --region "$REGION" ${PREFIX:+--prefix "$PREFIX"} \
                  ${NIC_GBPS:+--maximum-throughput-gbps "$NIC_GBPS"} $MP_FLAGS ;;
    esac
    local st=$?
    [[ $st == 0 ]] || { log "$1 mount exited $st"; return 1; }
    wait_mounted "$mp" 1 || { log "$1 mount not ready at $mp"; return 1; }
    MOUNTED="$1 $mp"
}

do_umount() { # unmounts $MOUNTED, if any
    [[ -n $MOUNTED ]] || return 0
    local fs=${MOUNTED%% *} mp=${MOUNTED#* }
    if [[ $fs == lith ]]; then lith umount "$mp" >/dev/null 2>&1; fi
    mountpoint -q "$mp" && { fusermount3 -u "$mp" 2>/dev/null || fusermount -u "$mp" 2>/dev/null || sudo umount "$mp"; }
    wait_mounted "$mp" 0 || { log "could not unmount $mp"; return 1; }
    MOUNTED=
}
trap 'do_umount' EXIT

# lith index: built once, before any run, outside the measured window (lith answers listing and
# stat from it with zero S3 calls; Mountpoint has no equivalent and LISTs/HEADs on lookup).
if [[ " $MOUNTS " == *" lith "* && ! -s $LITH_INDEX ]]; then
    t0=$(date +%s.%N)
    lith index build "s3://$BUCKET${PREFIX:+/${PREFIX%/}}" --index-file "$LITH_INDEX" --region "$REGION"
    st=$?
    [[ $st == 0 ]] || { log "lith index build exited $st"; exit 1; }
    echo "# lith index: built $LITH_INDEX in $(awk -v a="$t0" -v b="$(date +%s.%N)" 'BEGIN{printf "%.1f", b-a}') s (outside the measured window)" >>"$OUT"
fi

# Append gpuload's rows from $1 to OUT with ";fs=<fs>;mount=<flags>" suffixed to config, and the
# run column set to this script's run number (gpuload numbers its own --runs loop, always 1 here).
append_rows() { # append_rows <tmpcsv> <fs> <flags> <run>
    $PY -I -c '
import csv, sys
src, out, extra, run = sys.argv[1:5]
rows = [r for r in csv.reader(l for l in open(src) if not l.startswith("#"))]
if len(rows) < 2:
    sys.exit("no rows")
i, j = rows[0].index("config"), rows[0].index("run")
with open(out, "a", newline="") as f:
    w = csv.writer(f, lineterminator="\n")
    for r in rows[1:]:
        r[i], r[j] = r[i] + extra, run
        w.writerow(r)
' "$1" "$OUT" ";fs=$2;mount=$(echo "$3" | tr ',' ' ' | tr -s ' ')" "$4"
}

# --- runs ------------------------------------------------------------------------------------------

rc=0
for fs in $MOUNTS; do
    flags=$(mount_opts "$fs")
    echo "# mount: fs=$fs mp=$(mp_of "$fs") dir=$MNT/$fs cmd=$flags" >>"$OUT"
    [[ $REMOUNT == 1 ]] || { do_mount "$fs" || { rc=1; continue; }; }
    for th in $KVIKIO_THREADS; do for ts in $TASK_MIB; do for cc in $CONCURRENCY; do
        for r in $(seq 1 "$RUNS"); do
            if [[ $REMOUNT == 1 ]]; then do_mount "$fs" || { rc=1; break 4; }; fi
            cold
            tmp=$(mktemp "${TMPDIR:-/tmp}/adit-C.XXXXXX.csv")
            $PY bench/paths/lib/gpuload.py --mode cufile --compat on --dir "$MNT/$fs" --manifest "$MANIFEST" \
                --threads "$th" --task-size-mib "$ts" --concurrency "$cc" --region "$REGION" \
                "${META[@]}" --out "$tmp" | tail -1 | sed "s/^/$fs r$r /"
            st=${PIPESTATUS[0]}
            if [[ $fs == lith ]]; then
                curl -s http://127.0.0.1:9101/metrics >"${OUT%.csv}.lith-metrics-t$th-ts$ts-c$cc-r$r.txt"
            fi
            append_rows "$tmp" "$fs" "$flags" "$r" || { log "no row from gpuload (exit $st)"; rc=1; }
            rm -f "$tmp"
            if [[ $st != 0 ]]; then
                # gpuload exits 1 on an oracle mismatch or short read: law 2 stops the gate.
                log "gpuload exited $st ($fs threads=$th task=$ts conc=$cc run=$r); stopping"
                exit 1
            fi
            if [[ $REMOUNT == 1 ]]; then do_umount || { rc=1; break 4; }; fi
        done
    done; done; done
    do_umount || rc=1
done
log "done: $OUT (rc=$rc)"
exit $rc
