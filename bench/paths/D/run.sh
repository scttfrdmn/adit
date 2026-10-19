#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Path D: FSx for Lustre + GPUDirect Storage. A PERSISTENT_2, 1000 MB/s/TiB, EFA-enabled file system
# with a data repository association (DRA) to the bench bucket, lazy load. The GPU client mounts it
# over EFA (LNet efa), and bench/paths/lib/gpuload.py reads every object with KvikIO CuFile, compat
# mode OFF, into GPU buffers and checks each on the GPU.
#
# SCAFFOLD: not run until G3. Nothing here has touched AWS. See D/README.md for the UNVERIFIED list.
#
#   operator side (laptop, AWS_PROFILE=aws; creates and deletes billable resources):
#     run.sh plan     <workload>   size + cost estimate, no AWS writes
#     run.sh create   <workload>   CONFIRM=1: create the FS (tag adit:fleet), wait AVAILABLE, start the deadline watchdog
#     run.sh dra      <workload>   CONFIRM=1: create the DRA, time the metadata import (recorded separately)
#     run.sh redra    <workload>   CONFIRM=1: delete the DRA with its data, recreate it (a fresh DRA = a new cold)
#     run.sh status   <workload>   every FSx file system tagged adit:fleet, any region-1 state
#     run.sh delete   <workload>   CONFIRM=1: delete the FS, wait until it is gone, print FS-hours
#   client side (GPU box, repository root; FSX_DNS/FSX_MOUNT_NAME from the operator's state file):
#     run.sh setup    <workload>   Lustre client + EFA/GDS client configuration (--optimized-for-gds)
#     run.sh mount    <workload>   mount over EFA, prove LNet uses efa
#     run.sh gdscheck <workload>   gdscheck -p, nvidia-fs, KvikIO compat/GDS state -> D-gds.txt
#     run.sh read     <workload>   one cold pass (fresh DRA, unhydrated) + RUNS warm passes
#     run.sh tune     <workload>   warm-only sweep of KvikIO threads x task size x concurrency (law 3)
#     run.sh umount   <workload>
#   run.sh <workload>              = read (the bench/paths/README.md contract)
#
# Operator knobs:
#   CONFIRM         must be 1 for create/dra/redra/delete (FSx costs ~$3.95/hr at the 4.8 TiB minimum)
#   FLEET           adit:fleet tag value (default d-<workload>)
#   SUBNET_ID       the client's subnet (one AZ, one subnet: required for create)
#   SG_ID           the fleet's self-referencing all-traffic security group (EFA requirement; required)
#   MANIFEST        manifest JSONL (sizes the FS and derives the DRA prefix)
#   FS_GIB          storage capacity in GiB (default: max(4800, working set x 1.1 rounded up to 2400))
#   THROUGHPUT      MB/s/TiB (default 1000; GDS requires 1000)
#   CHUNK_MIB       DRA ImportedFileChunkSize, the per-OST stripe of imported files (default 1024, the AWS default)
#   DRA_PREFIX      S3 prefix under BUCKET to associate (default: the manifest keys' common directory)
#   USD_GB_MONTH    storage price (default 0.600, PERSISTENT_2 1000 MB/s/TiB us-east-1)
#   WINDOW_MIN      timed window from create to delete (default 120); the watchdog deletes at the deadline
#   STATE           operator state file (default bench/runs/D-<workload>.fsx.env)
# Client knobs:
#   FSX_DNS, FSX_MOUNT_NAME   from STATE
#   MNT             mount point (default /fsx)
#   EFA_SETUP_URL   AWS EFA client configuration archive (UNVERIFIED default below)
#   EFA_SETUP_SHA256  if set, the archive must match it
#   KVIKIO_THREADS, TASK_MIB, CONCURRENCY   read config (tune sweeps lists of them)
#   COLD_RESET      how the cold pass gets an unhydrated FS: fresh (default: operator ran dra/redra) or
#                   hsm_release (lfs hsm_release every file; a design deviation, recorded in config)
#   REQUIRE_GDS     1 (default): refuse to measure when GDS is not active. 0: measure anyway; the OUT
#                   header and every row say gds=INACTIVE
P=D
HERE=$(cd "$(dirname "$0")" && pwd)
PHASE=${1-}
case $PHASE in
    plan|create|dra|redra|status|delete|watchdog|setup|mount|gdscheck|read|tune|umount) shift ;;
    *) PHASE="read" ;;
esac
WORKLOAD=${1-}

# ---------------------------------------------------------------- operator side -----------------
operator_prologue() {
    set +e
    set -uo pipefail
    echo "opts=$-"
    [[ -n $WORKLOAD ]] || { echo "usage: $0 $PHASE <workload>" >&2; exit 2; }
    export AWS_PROFILE=${AWS_PROFILE:-aws}
    REGION=${REGION:-us-east-1}
    BUCKET=${BUCKET:-scttfrdmn-lith-bench}
    FLEET=${FLEET:-d-$WORKLOAD}
    THROUGHPUT=${THROUGHPUT:-1000}
    CHUNK_MIB=${CHUNK_MIB:-1024}
    USD_GB_MONTH=${USD_GB_MONTH:-0.600}
    WINDOW_MIN=${WINDOW_MIN:-120}
    mkdir -p bench/runs
    STATE=${STATE:-bench/runs/D-$WORKLOAD.fsx.env}
    AWSF=(--region "$REGION" --output text)
    olog() { echo "[$(date -u +%FT%TZ)] D $WORKLOAD $PHASE: $*" | tee -a "${STATE%.env}.log"; }
    [[ -f $STATE ]] && source "$STATE"
}

confirm() {
    [[ ${CONFIRM-} == 1 ]] || { olog "refusing: $PHASE creates or deletes billable FSx resources; rerun with CONFIRM=1"; exit 3; }
}

region_check() {
    # x-amz-bucket-region must equal REGION before any I/O (CLAUDE.md, Placement).
    local br
    if command -v aditctl >/dev/null; then
        aditctl region-check --bucket "$BUCKET" --region "$REGION" || { olog "region check failed"; exit 1; }
        return
    fi
    br=$(aws s3api head-bucket --bucket "$BUCKET" --region "$REGION" --query BucketRegion --output text 2>&1)
    [[ $br == "$REGION" ]] || { olog "bucket $BUCKET region '$br' != $REGION; refusing"; exit 1; }
}

save() { # save KEY VALUE: append to the state file (later lines win when sourced)
    echo "$1=$(printf %q "$2")" >>"$STATE"
    printf -v "$1" %s "$2"
}

size_plan() {
    MANIFEST=${MANIFEST:?manifest JSONL (sizes the FS and the DRA prefix)}
    WS_BYTES=$(jq -s 'map(.size)|add' "$MANIFEST")
    OBJECTS=$(jq -s length "$MANIFEST")
    local ws_gib=$(( (WS_BYTES * 11 / 10 + (1 << 30) - 1) >> 30 ))
    local auto=$(( ((ws_gib + 2399) / 2400) * 2400 ))
    (( auto < 4800 )) && auto=4800
    FS_GIB=${FS_GIB:-$auto}
    if [[ -z ${DRA_PREFIX-} ]]; then
        # longest common directory of every key; the DRA maps s3://BUCKET/DRA_PREFIX/ to /DRA_PREFIX
        # so file DIR/<key> == MNT/<key> and gpuload.py runs with --dir MNT unchanged.
        DRA_PREFIX=$(jq -r .key "$MANIFEST" | awk -F/ '
            NR==1 { n=NF-1; for (i=1;i<=n;i++) p[i]=$i; next }
            { m=0; for (i=1;i<=n && i<NF;i++) { if ($i!=p[i]) break; m=i } n=m }
            END { s=""; for (i=1;i<=n;i++) s=s (i>1?"/":"") p[i]; print s }')
    fi
    [[ -n $DRA_PREFIX ]] || { olog "manifest keys share no directory; set DRA_PREFIX (a DRA on the bucket root imports every object's metadata)"; exit 2; }
    # $/hr = GiB x $/GB-month / 730 h. FSx bills GB-months prorated hourly; the price quoted is per GB.
    USD_HR=$(awk -v g="$FS_GIB" -v p="$USD_GB_MONTH" 'BEGIN{printf "%.4f", g*p/730}')
    USD_WINDOW=$(awk -v h="$USD_HR" -v m="$WINDOW_MIN" 'BEGIN{printf "%.2f", h*m/60}')
}

print_plan() {
    olog "working set: $OBJECTS objects, $WS_BYTES bytes; DRA s3://$BUCKET/$DRA_PREFIX/ -> /$DRA_PREFIX"
    olog "FS: PERSISTENT_2 ${FS_GIB} GiB @ ${THROUGHPUT} MB/s/TiB, EFA on, imported chunk ${CHUNK_MIB} MiB, region $REGION"
    olog "aggregate FS throughput (baseline): $(( FS_GIB * THROUGHPUT / 1024 )) MB/s = $(awk -v g="$FS_GIB" -v t="$THROUGHPUT" 'BEGIN{printf "%.1f", g/1024*t*8/1000}') Gb/s"
    olog "cost: ${FS_GIB} GiB x \$${USD_GB_MONTH}/GB-month / 730 h = \$${USD_HR}/hr; window ${WINDOW_MIN} min = \$${USD_WINDOW} (FS only; add S3 GETs for hydration and the client box)"
    olog "NOT covered by spawn --cost-limit: only delete (or the watchdog) stops this bill"
}

fs_state() { aws fsx describe-file-systems "${AWSF[@]}" --file-system-ids "$1" --query 'FileSystems[0].Lifecycle' 2>/dev/null; }

op_plan() { size_plan; print_plan; }

op_create() {
    confirm
    region_check
    size_plan
    print_plan
    [[ -n ${FS_ID-} && $(fs_state "$FS_ID") != "" && $(fs_state "$FS_ID") != None ]] && { olog "state already has $FS_ID ($(fs_state "$FS_ID")); delete it first"; exit 1; }
    SUBNET_ID=${SUBNET_ID:?client subnet (same AZ as the client)}
    SG_ID=${SG_ID:?self-referencing all-traffic security group}
    local deadline=$(( $(date +%s) + WINDOW_MIN * 60 ))
    local deadline_iso
    deadline_iso=$(date -u -r "$deadline" +%FT%TZ 2>/dev/null || date -u -d "@$deadline" +%FT%TZ)
    # spawn cannot create this FS (it has no EFA, metadata-configuration or custom-tag option, and its
    # DRA is fixed to auto import+export with no timing); see README. Raw `aws fsx`, guarded by CONFIRM.
    local t0 out
    t0=$(date +%s)
    out=$(aws fsx create-file-system "${AWSF[@]}" --query 'FileSystem.FileSystemId' \
        --file-system-type LUSTRE --file-system-type-version 2.15 \
        --storage-capacity "$FS_GIB" --storage-type SSD \
        --subnet-ids "$SUBNET_ID" --security-group-ids "$SG_ID" \
        --lustre-configuration "DeploymentType=PERSISTENT_2,PerUnitStorageThroughput=$THROUGHPUT,EfaEnabled=true,DataCompressionType=NONE,MetadataConfiguration={Mode=AUTOMATIC}" \
        --tags "Key=adit:fleet,Value=$FLEET" "Key=adit:role,Value=path-d" "Key=adit:deadline,Value=$deadline_iso" "Key=Name,Value=adit-$FLEET" 2>&1)
    local rc=$?
    [[ $rc == 0 && $out == fs-* ]] || { olog "create-file-system failed rc=$rc: $out"; exit 1; }
    : >"$STATE"
    save FS_ID "$out"; save CREATED_AT "$t0"; save DEADLINE "$deadline"; save FS_GIB "$FS_GIB"
    save THROUGHPUT "$THROUGHPUT"; save USD_HR "$USD_HR"; save REGION "$REGION"; save FLEET "$FLEET"
    olog "created $FS_ID; deadline $deadline_iso; starting watchdog"
    nohup bash "$HERE/run.sh" watchdog "$WORKLOAD" >>"${STATE%.env}.watchdog.log" 2>&1 &
    save WATCHDOG_PID "$!"
    local s
    while :; do
        s=$(fs_state "$FS_ID")
        [[ $s == AVAILABLE ]] && break
        [[ $s == FAILED || $s == MISCONFIGURED ]] && { olog "$FS_ID is $s: $(aws fsx describe-file-systems "${AWSF[@]}" --file-system-ids "$FS_ID" --query 'FileSystems[0].FailureDetails.Message')"; olog "delete it: CONFIRM=1 $0 delete $WORKLOAD"; exit 1; }
        (( $(date +%s) > deadline )) && { olog "not AVAILABLE before the deadline"; exit 1; }
        sleep 30
    done
    save CREATE_S "$(( $(date +%s) - t0 ))"
    save FSX_DNS "$(aws fsx describe-file-systems "${AWSF[@]}" --file-system-ids "$FS_ID" --query 'FileSystems[0].DNSName')"
    save FSX_MOUNT_NAME "$(aws fsx describe-file-systems "${AWSF[@]}" --file-system-ids "$FS_ID" --query 'FileSystems[0].LustreConfiguration.MountName')"
    save FSX_EFA "$(aws fsx describe-file-systems "${AWSF[@]}" --file-system-ids "$FS_ID" --query 'FileSystems[0].LustreConfiguration.EfaEnabled')"
    olog "AVAILABLE after ${CREATE_S}s: dns=$FSX_DNS mount=$FSX_MOUNT_NAME efa=$FSX_EFA"
    [[ $FSX_EFA == True ]] || olog "WARNING: EfaEnabled reads back '$FSX_EFA', not True: this FS cannot serve path D as defined"
}

op_dra() {
    confirm
    region_check
    size_plan
    [[ -n ${FS_ID-} && $(fs_state "$FS_ID") == AVAILABLE ]] || { olog "no AVAILABLE FS in $STATE"; exit 1; }
    # Read-only benchmark: no auto import/export policy (nothing is written back to the bucket),
    # metadata imported in one batch at creation, file data lazy-loaded on first read.
    local t0 id s
    t0=$(date +%s)
    id=$(aws fsx create-data-repository-association "${AWSF[@]}" --query 'Association.AssociationId' \
        --file-system-id "$FS_ID" --file-system-path "/$DRA_PREFIX" \
        --data-repository-path "s3://$BUCKET/$DRA_PREFIX/" \
        --batch-import-meta-data-on-create --imported-file-chunk-size "$CHUNK_MIB" \
        --tags "Key=adit:fleet,Value=$FLEET" 2>&1)
    [[ $id == dra-* ]] || { olog "create-data-repository-association failed: $id"; exit 1; }
    save DRA_ID "$id"; save DRA_PREFIX "$DRA_PREFIX"; save CHUNK_MIB "$CHUNK_MIB"
    while :; do
        s=$(aws fsx describe-data-repository-associations "${AWSF[@]}" --association-ids "$id" --query 'Associations[0].Lifecycle' 2>/dev/null)
        [[ $s == AVAILABLE ]] && break
        [[ $s == FAILED || $s == MISCONFIGURED ]] && { olog "DRA $id is $s"; exit 1; }
        (( $(date +%s) > DEADLINE )) && { olog "DRA not AVAILABLE before the deadline"; exit 1; }
        sleep 5
    done
    local wall=$(( $(date +%s) - t0 ))
    # The DRA wall includes provisioning. The batch import also shows up as a data repository task
    # (UNVERIFIED: task type name); record its own start/end and counts when present.
    local task
    task=$(aws fsx describe-data-repository-tasks --region "$REGION" --output json \
        --filters "Name=file-system-id,Values=$FS_ID" 2>/dev/null |
        jq -c '[.DataRepositoryTasks[] | select(.Type|test("IMPORT")) | {id:.TaskId,type:.Type,start:.StartTime,end:.EndTime,status:.Status}] | last')
    save DRA_CREATED_AT "$t0"; save META_IMPORT_S "$wall"; save META_IMPORT_TASK "${task:-none}"
    save DRA_GEN "$(( ${DRA_GEN:-0} + 1 ))"
    olog "DRA $id AVAILABLE: metadata import wall ${wall}s for $OBJECTS objects (task: ${task:-none}); generation $DRA_GEN"
    # results row-side record; the client copies META_IMPORT_S into each row's config.
}

op_redra() {
    confirm
    [[ -n ${DRA_ID-} ]] || { olog "no DRA in $STATE"; exit 1; }
    # delete-data-in-file-system removes the imported files, so the next DRA is a fresh, unhydrated one.
    aws fsx delete-data-repository-association "${AWSF[@]}" --association-id "$DRA_ID" --delete-data-in-file-system >/dev/null ||
        { olog "delete-data-repository-association failed"; exit 1; }
    while [[ -n $(aws fsx describe-data-repository-associations "${AWSF[@]}" --association-ids "$DRA_ID" --query 'Associations[0].Lifecycle' 2>/dev/null) ]]; do
        (( $(date +%s) > DEADLINE )) && { olog "DRA delete not done before the deadline"; exit 1; }
        sleep 10
    done
    olog "DRA $DRA_ID deleted with its data"
    op_dra
}

op_status() {
    aws fsx describe-file-systems --region "$REGION" --output json |
        jq -r '.FileSystems[] | select(any(.Tags[]?; .Key=="adit:fleet")) |
            [.FileSystemId, .Lifecycle, .StorageCapacity, (.Tags[]|select(.Key=="adit:fleet").Value),
             ((.Tags[]|select(.Key=="adit:deadline").Value) // "-"), .CreationTime] | @tsv'
}

op_delete() {
    confirm
    [[ -n ${FS_ID-} ]] || { olog "no FS in $STATE"; op_status; exit 1; }
    local s
    s=$(fs_state "$FS_ID")
    if [[ -n $s && $s != None && $s != DELETING ]]; then
        aws fsx delete-file-system "${AWSF[@]}" --file-system-id "$FS_ID" >/dev/null ||
            { olog "delete-file-system $FS_ID failed: the bill continues; fix by hand"; exit 1; }
    fi
    while [[ -n $(fs_state "$FS_ID") && $(fs_state "$FS_ID") != None ]]; do sleep 30; done
    local gone hours
    gone=$(date +%s)
    hours=$(awk -v a="$CREATED_AT" -v b="$gone" 'BEGIN{printf "%.3f", (b-a)/3600}')
    save DELETED_AT "$gone"
    olog "$FS_ID gone; created->gone ${hours} h x \$${USD_HR}/hr = \$$(awk -v h="$hours" -v r="$USD_HR" 'BEGIN{printf "%.2f", h*r}') (FS only)"
    (( gone <= DEADLINE )) || olog "WARNING: deleted after the deadline"
    [[ -n ${WATCHDOG_PID-} ]] && kill "$WATCHDOG_PID" 2>/dev/null
    olog "remaining adit FSx in $REGION:"; op_status
}

op_watchdog() {
    # Started by create. Sleeps to the deadline, then deletes the FS if it still exists. FSx is not
    # covered by spawn's --cost-limit, so this is the only automatic stop.
    while (( $(date +%s) < DEADLINE )); do
        [[ -n ${DELETED_AT-} ]] && exit 0
        sleep 60
        source "$STATE"
    done
    local s
    s=$(fs_state "$FS_ID")
    if [[ -n $s && $s != None && $s != DELETING ]]; then
        olog "watchdog: deadline reached, deleting $FS_ID"
        aws fsx delete-file-system "${AWSF[@]}" --file-system-id "$FS_ID" >/dev/null || olog "watchdog: delete FAILED for $FS_ID"
    fi
}

# ---------------------------------------------------------------- client side -------------------
client_prologue() {
    if [[ $PHASE == read || $PHASE == tune ]]; then
        # the shared contract: region check, OUT, RUNS, META, CLIENT_INSTANCE, COMMIT
        source "$HERE/../lib/common.sh" "$WORKLOAD"
    else
        set +e; set -uo pipefail; echo "opts=$-"
        [[ -n $WORKLOAD ]] || { echo "usage: $0 $PHASE <workload>" >&2; exit 2; }
        REGION=${REGION:-us-east-1}
        BUCKET=${BUCKET:-scttfrdmn-lith-bench}
        log() { echo "[$(date -u +%FT%TZ)] $P $WORKLOAD: $*"; }
    fi
    # The operator's state file, copied to the box (FS_ID, FSX_DNS, FSX_MOUNT_NAME, DRA_*, META_IMPORT_S).
    STATE=${STATE:-bench/runs/D-$WORKLOAD.fsx.env}
    [[ -f $STATE ]] && source "$STATE"
    MNT=${MNT:-/fsx}
    PY=${PY:-/opt/adit-venv/bin/python}
    GDS_TXT=${GDS_TXT:-bench/runs/D-$WORKLOAD-gds.txt}
    CUFILE_JSON=${CUFILE_JSON:-bench/runs/D-cufile.json}
    CUFILE_LOGDIR=${CUFILE_LOGDIR:-bench/runs/D-cufile-log}
    GDSCHECK=${GDSCHECK:-$(ls /usr/local/cuda/gds/tools/gdscheck 2>/dev/null || command -v gdscheck)}
    mkdir -p bench/runs
}

cl_setup() {
    # 1. Lustre client for the running kernel (Ubuntu DLAMI). Repository and key per the FSx Lustre
    #    User Guide "Installing the Lustre client" (Ubuntu). UNVERIFIED for kernel 7.0.0-*-aws: the FSx
    #    repo ships lustre-client-modules per AWS kernel and may lag the DLAMI's kernel.
    local codename
    codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
    wget -qO - https://fsx-lustre-client-repo-public-keys.s3.amazonaws.com/fsx-ubuntu-public-key.asc |
        gpg --dearmor | sudo tee /usr/share/keyrings/fsx-ubuntu-public-key.gpg >/dev/null
    echo "deb [signed-by=/usr/share/keyrings/fsx-ubuntu-public-key.gpg] https://fsx-lustre-client-repo.s3.amazonaws.com/ubuntu $codename main" |
        sudo tee /etc/apt/sources.list.d/fsxlustreclientrepo.list >/dev/null
    sudo apt-get update -q >/dev/null
    sudo apt-get install -y -q "lustre-client-modules-$(uname -r)" lustre-client-utils ||
        { log "no lustre-client-modules for kernel $(uname -r): path D cannot run on this AMI"; exit 1; }
    # 2. EFA + GDS client configuration. AWS publishes a setup script for EFA-enabled FSx clients;
    #    `--optimized-for-gds` is the GDS variant. UNVERIFIED: archive URL, entry point name and flag
    #    spelling; check the FSx Lustre User Guide "Configuring EFA clients" before G3.
    EFA_SETUP_URL=${EFA_SETUP_URL:-https://docs.aws.amazon.com/fsx/latest/LustreGuide/samples/configure-efa-fsx-lustre-client.zip}
    local d=/tmp/adit-d-efa
    rm -rf "$d"; mkdir -p "$d"
    curl -fsSL -o "$d/efa.zip" "$EFA_SETUP_URL" || { log "cannot fetch $EFA_SETUP_URL"; exit 1; }
    local sum
    sum=$(sha256sum "$d/efa.zip" | cut -d' ' -f1)
    log "EFA client setup archive sha256=$sum"
    [[ -z ${EFA_SETUP_SHA256-} || $sum == "$EFA_SETUP_SHA256" ]] || { log "sha256 mismatch (want $EFA_SETUP_SHA256)"; exit 1; }
    (cd "$d" && unzip -q efa.zip)
    local setup
    setup=$(find "$d" -name 'setup.sh' -o -name 'configure-efa*.sh' | head -1)
    [[ -n $setup ]] || { log "no setup script in the archive: $(cd "$d" && find . -type f | head)"; exit 1; }
    sudo bash "$setup" --optimized-for-gds 2>&1 | tee bench/runs/D-efa-setup.log
    local rc=${PIPESTATUS[0]}
    [[ $rc == 0 ]] || { log "EFA client setup exited $rc (bench/runs/D-efa-setup.log)"; exit 1; }
    echo "efa_setup_url=$EFA_SETUP_URL sha256=$sum" >bench/runs/D-efa-setup.id
    # 3. nvidia-fs: GDS on Lustre goes through the nvidia-fs kernel module (UNVERIFIED that the
    #    DLAMI's GDS 1.17 ships it built for this kernel). Turn on its read counters: the proof of GDS.
    sudo modprobe nvidia_fs || log "WARNING: nvidia_fs did not load: GDS cannot be active"
    [[ -w /sys/module/nvidia_fs/parameters/rw_stats_enabled ]] || sudo chmod a+w /sys/module/nvidia_fs/parameters/rw_stats_enabled 2>/dev/null
    echo 1 | sudo tee /sys/module/nvidia_fs/parameters/rw_stats_enabled >/dev/null 2>&1
    log "setup done"
}

cl_mount() {
    FSX_DNS=${FSX_DNS:?from the operator state file}
    FSX_MOUNT_NAME=${FSX_MOUNT_NAME:?from the operator state file}
    sudo mkdir -p "$MNT"
    mountpoint -q "$MNT" || sudo mount -t lustre -o relatime,flock "$FSX_DNS@tcp:/$FSX_MOUNT_NAME" "$MNT" ||
        { log "mount failed"; exit 1; }
    # The mount NID is @tcp; with EFA configured LNet must also carry an efa network and the server
    # NIDs must be reached through it. Record both; a tcp-only LNet means the run is over ENA.
    sudo lnetctl net show 2>&1 | tee bench/runs/D-lnet.txt
    sudo lctl list_nids 2>&1 | tee -a bench/runs/D-lnet.txt
    grep -q 'efa' bench/runs/D-lnet.txt && LNET=efa || LNET=tcp-only
    echo "lnet=$LNET" >>bench/runs/D-lnet.txt
    lfs df -h "$MNT" | tee -a bench/runs/D-lnet.txt
    log "mounted $MNT lnet=$LNET"
}

nvfs_read_mib() { # sum of readMiB in nvidia-fs stats (UNVERIFIED field name); "unknown" if absent
    local v
    v=$(grep -oE 'readMiB=[0-9]+' /proc/driver/nvidia-fs/stats 2>/dev/null | head -1 | cut -d= -f2)
    echo "${v:-unknown}"
}

write_cufile_json() {
    # cuFile must not fall back to POSIX on its own: KvikIO compat OFF only governs KvikIO, while
    # libcufile has its own allow_compat_mode. Start from the system config, force it off, log to a
    # known dir. posix_gds_min_kb=0 keeps small reads on GDS too. (UNVERIFIED key paths for 1.17.)
    mkdir -p "$CUFILE_LOGDIR"
    local src=/etc/cufile.json
    [[ -r $src ]] || src=/usr/local/cuda/gds/cufile.json
    sed -E 's://.*$::' "$src" 2>/dev/null | jq --arg d "$(pwd)/$CUFILE_LOGDIR" '
        .properties.allow_compat_mode = false
        | .logging.dir = $d | .logging.level = "INFO"
        | .fs.lustre.posix_gds_min_kb = 0' >"$CUFILE_JSON" ||
        { log "cannot build $CUFILE_JSON from $src"; return 1; }
    export CUFILE_ENV_PATH_JSON=$(pwd)/$CUFILE_JSON
}

cl_gdscheck() {
    write_cufile_json
    {
        echo "# gdscheck -p ($GDSCHECK)"
        [[ -n $GDSCHECK ]] && "$GDSCHECK" -p 2>&1 || echo "gdscheck: not found"
        echo "# lsmod nvidia_fs"; lsmod | grep -E '^nvidia_fs' || echo "nvidia_fs: not loaded"
        echo "# nvidia-fs stats"; cat /proc/driver/nvidia-fs/stats 2>&1 | head -30
        echo "# kvikio"
        KVIKIO_COMPAT_MODE=OFF "$PY" "$HERE/gdsprobe.py" "$MNT" 2>&1
    } >"$GDS_TXT"
    # Verdict. Every condition must hold; any missing one is recorded by name.
    local why=()
    grep -qiE 'lustre[^:]*:[[:space:]]*supported' "$GDS_TXT" || why+=("gdscheck:lustre-not-supported")
    grep -qE '^nvidia_fs' "$GDS_TXT" || why+=("nvidia_fs-not-loaded")
    grep -q '"compat_mode": "OFF"' "$GDS_TXT" || why+=("kvikio-compat-not-off")
    grep -q '"gds_available": true' "$GDS_TXT" || why+=("kvikio-gds-unavailable")
    if (( ${#why[@]} == 0 )); then GDS=active; GDS_WHY=preflight-ok; else GDS=INACTIVE; GDS_WHY=$(IFS=+; echo "${why[*]}"); fi
    echo "gds=$GDS why=$GDS_WHY" | tee -a "$GDS_TXT"
}

drop_client() {
    # Client side only: page cache and Lustre client locks/cached pages. FSx server hydration is NOT
    # touched: that is what separates cold from warm.
    sudo bash hack/drop-caches.sh >/dev/null
    sudo lctl set_param -q 'ldlm.namespaces.*.lru_size=clear' >/dev/null 2>&1
}

hsm_count() { # hsm_count released|hydrated: how many manifest files are in that state
    local n=0 f st
    while read -r f; do
        st=$(lfs hsm_state "$MNT/$f" 2>/dev/null)
        if [[ $1 == released ]]; then [[ $st == *released* ]] && n=$((n + 1))
        else [[ -n $st && $st != *released* ]] && n=$((n + 1)); fi
    done < <(jq -r .key "$MANIFEST")
    echo "$n"
}

out_header() {
    [[ -s $OUT ]] && return
    local efa_id=$(cat bench/runs/D-efa-setup.id 2>/dev/null)
    {
        echo "# path D: FSx for Lustre PERSISTENT_2 ${FS_GIB-?} GiB @ ${THROUGHPUT-?} MB/s/TiB, EFA ${FSX_EFA-?}, fs=${FS_ID-?} region=$REGION"
        echo "# DRA ${DRA_ID-?}: s3://$BUCKET/${DRA_PREFIX-?}/ -> $MNT/${DRA_PREFIX-?}, imported chunk ${CHUNK_MIB-?} MiB, lazy load, generation ${DRA_GEN-?}"
        echo "# metadata import (recorded separately, not in wall_s): ${META_IMPORT_S-?} s; task ${META_IMPORT_TASK-?}"
        echo "# client: $CLIENT_INSTANCE kernel $(uname -r); lnet=$(awk -F= '/^lnet=/{print $2}' bench/runs/D-lnet.txt 2>/dev/null); $efa_id"
        echo "# GDS: gds=$GDS ($GDS_WHY); cufile.json $CUFILE_JSON (allow_compat_mode=false); full preflight $GDS_TXT"
        [[ $GDS == active ]] || echo "# GDS NOT ACTIVE: these rows are NOT path D (GDS) numbers; reason $GDS_WHY"
        echo "# reader: bench/paths/lib/gpuload.py --mode cufile --compat off; cold = first read after a fresh DRA (COLD_RESET=$COLD_RESET), warm = after hydration; client cache dropped before every pass"
        echo "# FS cost: \$${USD_HR-?}/hr (${FS_GIB-?} GiB x \$${USD_GB_MONTH:-0.600}/GB-month / 730); row usd left empty: the FS bills by the hour, not by the run"
        grep -v '^#' bench/schema/paths.csv | head -1
    } >>"$OUT"
}

one_pass() { # one_pass <cold 1|0> <label> <threads> <task_mib> <concurrency>
    local cold=$1 label=$2 th=$3 ts=$4 cc=$5 before after rc fstate
    drop_client
    fstate=$([[ $cold == 1 ]] && echo unhydrated || echo hydrated)
    before=$(nvfs_read_mib)
    "$PY" bench/paths/lib/gpuload.py --mode cufile --compat off --dir "$MNT" --manifest "$MANIFEST" \
        --threads "$th" --task-size-mib "$ts" --concurrency "$cc" --region "$REGION" \
        "${META[@]}" --meta "cold=$cold" --out "$OUT" | tail -1 | sed "s/^/$label /"
    rc=${PIPESTATUS[0]}
    after=$(nvfs_read_mib)
    # GDS evidence per pass: nvidia-fs read MiB delta against the bytes read. compat OFF makes KvikIO
    # raise rather than fall back; the delta catches libcufile's own fallback.
    "$PY" "$HERE/annotate.py" --out "$OUT" --rc "$rc" --gds "$GDS" --gds-why "$GDS_WHY" \
        --nvfs-before "$before" --nvfs-after "$after" \
        --config "fsx_state=$fstate;cold_reset=$COLD_RESET;fs_gib=${FS_GIB-?};throughput=${THROUGHPUT-?};efa=${FSX_EFA-?};lnet=$(awk -F= '/^lnet=/{print $2}' bench/runs/D-lnet.txt 2>/dev/null);chunk_mib=${CHUNK_MIB-?};dra=${DRA_ID-?};dra_gen=${DRA_GEN-?};meta_import_s=${META_IMPORT_S-?};client_cache=dropped"
}

client_gate() {
    MANIFEST=${MANIFEST:?manifest JSONL}
    COLD_RESET=${COLD_RESET:-fresh}
    REQUIRE_GDS=${REQUIRE_GDS:-1}
    mountpoint -q "$MNT" || { log "$MNT is not mounted (run.sh mount)"; exit 1; }
    cl_gdscheck
    if [[ $GDS != active ]]; then
        log "GDS is NOT active ($GDS_WHY)"
        [[ $REQUIRE_GDS == 1 ]] && { log "refusing to measure (REQUIRE_GDS=1); see $GDS_TXT"; exit 1; }
        log "REQUIRE_GDS=0: measuring anyway; OUT header and every row say gds=INACTIVE"
    fi
    local missing
    missing=$(jq -r '"\(.key)\t\(.size)"' "$MANIFEST" | while IFS=$'\t' read -r k s; do
        [[ $(stat -c %s "$MNT/$k" 2>/dev/null) == "$s" ]] || echo "$k"; done | head -3)
    [[ -z $missing ]] || { log "manifest objects missing or wrong size under $MNT: $missing"; exit 1; }
}

cl_read() {
    client_gate
    KVIKIO_THREADS=${KVIKIO_THREADS:-64}
    TASK_MIB=${TASK_MIB:-16}
    CONCURRENCY=${CONCURRENCY:-8}
    local n
    n=$(jq -s length "$MANIFEST")
    if [[ $COLD_RESET == hsm_release ]]; then
        jq -r .key "$MANIFEST" | while read -r k; do sudo lfs hsm_release "$MNT/$k" || echo "hsm_release failed: $k"; done
    fi
    # Law 7: show the cold probe can resolve. Every file must be unhydrated before the cold pass.
    local rel
    rel=$(hsm_count released)
    out_header
    if [[ $rel != "$n" ]]; then
        log "only $rel of $n files are unhydrated: not a cold FS. Operator: run.sh redra (or COLD_RESET=hsm_release). Running warm passes only."
        echo "# cold pass skipped: $rel of $n files released before it" >>"$OUT"
    else
        echo "# cold pass: $rel of $n files released before it" >>"$OUT"
        one_pass 1 cold "$KVIKIO_THREADS" "$TASK_MIB" "$CONCURRENCY"
    fi
    local hyd
    hyd=$(hsm_count hydrated)
    echo "# warm passes: $hyd of $n files hydrated before them" >>"$OUT"
    [[ $hyd == "$n" ]] || log "WARNING: $hyd of $n hydrated; warm rows include lazy loads"
    for r in $(seq 1 "$RUNS"); do
        one_pass 0 "warm$r" "$KVIKIO_THREADS" "$TASK_MIB" "$CONCURRENCY"
    done
    log "done: $OUT"
}

cl_tune() {
    # Law 3: tune before measuring. Warm only (hydrated); a cold pass cannot be repeated per DRA.
    client_gate
    OUT=${OUT%.csv}-tune.csv
    local hyd n
    n=$(jq -s length "$MANIFEST"); hyd=$(hsm_count hydrated)
    [[ $hyd == "$n" ]] || log "WARNING: only $hyd of $n hydrated: the first sweep cell absorbs lazy loads"
    out_header
    for th in ${KVIKIO_THREADS:-16 64 128}; do for ts in ${TASK_MIB:-4 16 64}; do for cc in ${CONCURRENCY:-1 8 32}; do
        one_pass 0 "t$th-s$ts-c$cc" "$th" "$ts" "$cc"
    done; done; done
    log "tune done: $OUT (pick the best cell; then operator redra; then read with it)"
}

cl_umount() { sudo umount "$MNT" && log "unmounted $MNT"; }

case $PHASE in
    plan)     operator_prologue; op_plan ;;
    create)   operator_prologue; op_create ;;
    dra)      operator_prologue; op_dra ;;
    redra)    operator_prologue; op_redra ;;
    status)   operator_prologue; op_status ;;
    delete)   operator_prologue; op_delete ;;
    watchdog) operator_prologue; op_watchdog ;;
    setup)    client_prologue; cl_setup ;;
    mount)    client_prologue; cl_mount ;;
    gdscheck) client_prologue; cl_gdscheck ;;
    read)     client_prologue; cl_read ;;
    tune)     client_prologue; cl_tune ;;
    umount)   client_prologue; cl_umount ;;
esac
