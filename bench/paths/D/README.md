# Path D: FSx for Lustre DRA + GDS

**Scaffold: not run until G3.** Nothing in this directory has touched AWS. Every item marked
UNVERIFIED below must be checked against the AWS and NVIDIA docs (or a cheap probe) before the first
create.

Definition (project document): Persistent 2, 1000 MB/s/TiB, EFA enabled, a data repository
association (DRA) to the bucket, lazy load, read via cuFile with GDS; the file system sized to the
working set; the `--optimized-for-gds` client setup; cold = a fresh DRA, warm = after hydration;
metadata import time recorded separately.

## Files

| File | Role |
|---|---|
| `run.sh` | the runner: operator phases (create and delete billable resources) and client phases (on the GPU box) |
| `gdsprobe.py` | preflight: KvikIO compat mode, cuFile GDS availability, a CuFile handle on the mount, as JSON |
| `annotate.py` | stamps each row `gpuload.py` appends with the FSx config, the nvidia-fs GDS byte check and the EFA byte check |

Reads go through `bench/paths/lib/gpuload.py --mode cufile --compat off` like every other path: the
same window, counters, GPU SHA-256 oracle and row code.

## Procedure

Operator (laptop, `AWS_PROFILE=aws`, region `us-east-1`); client (GPU box, repository root). The
client box is launched through spawn as usual, in the same subnet and the same self-referencing
all-traffic security group. EFA needs that group, and FSx EFA needs it too.

```
# operator
MANIFEST=m.jsonl bash bench/paths/D/run.sh plan R1                         # size + cost, no AWS writes
CONFIRM=1 MANIFEST=m.jsonl SUBNET_ID=subnet-… SG_ID=sg-… bash bench/paths/D/run.sh create R1
CONFIRM=1 MANIFEST=m.jsonl bash bench/paths/D/run.sh dra R1                # times the metadata import
#   copy bench/runs/D-R1.fsx.env to the client's bench/runs/
# client
bash bench/paths/D/run.sh setup R1          # Lustre client, EFA/GDS client config, nvidia-fs counters
bash bench/paths/D/run.sh mount R1          # mount, record LNet NIDs (efa or tcp-only)
bash bench/paths/D/run.sh gdscheck R1       # gdscheck -p, nvidia_fs, KvikIO state -> bench/runs/D-R1-gds.txt
MANIFEST=m.jsonl bash bench/paths/D/run.sh read R1   # pass 1 hydrates (a legitimate cold row)
MANIFEST=m.jsonl bash bench/paths/D/run.sh tune R1   # law 3: warm sweep of threads x task size x concurrency
# operator
CONFIRM=1 MANIFEST=m.jsonl bash bench/paths/D/run.sh redra R1              # fresh DRA = a new cold, new import time
# client
MANIFEST=m.jsonl KVIKIO_THREADS=… TASK_MIB=… CONCURRENCY=… bash bench/paths/D/run.sh read R1
bash bench/paths/D/run.sh umount R1
# operator, inside the window
CONFIRM=1 bash bench/paths/D/run.sh delete R1
bash bench/paths/D/run.sh status R1         # must list no live adit:fleet FS
```

`read` runs one cold pass and then `RUNS` warm passes. Before the cold pass it checks with
`lfs hsm_state` that every manifest file is still released (unhydrated). If they are not, it skips
the cold pass and writes a comment saying so. Before the warm passes it checks that they are all
hydrated. That is the law 7 check that the cold probe can tell cold from warm. Every pass drops the
client page cache and the Lustre client locks (`ldlm…lru_size=clear`). The FSx servers are never
touched, because the difference between cold and warm is supposed to be server-side hydration.
`COLD_RESET=hsm_release` releases every file with `lfs hsm_release` instead of using a fresh DRA.
That deviates from the design, so it is recorded in `config` as `cold_reset=hsm_release`.

## Why not spawn

The project requires AWS resources to go through spawn where it can. `spawn launch --fsx-create`
(spawn `pkg/aws/fsx.go`, `cmd/launch_single.go`) cannot create this file system:

- `buildFSxCreateInput` sets no `EfaEnabled` or `MetadataConfiguration`, and it hard-codes LZ4
  compression;
- it adds only `spawn:*` tags, so it cannot add `adit:fleet`;
- the DRA (`associateFSxS3`) is fixed at `FileSystemPath=/` with auto import and auto export of
  NEW/CHANGED/DELETED. That is wrong for a read-only benchmark, and the import is not timed;
- the mount (`pkg/agent/sys_linux.go`) is `mount -t lustre -o noatime,flock …@tcp:/…` with no EFA
  client configuration;
- the file system is tied to an instance launch and reclaimed by spawn's reaper on its own schedule,
  not inside our timed window.

So `create`, `dra`, `redra` and `delete` use raw `aws fsx`, each guarded by `CONFIRM=1`. The
instances still go through spawn. Making spawn do this would take an EFA flag, a metadata mode, extra
tags, a configurable DRA and an EFA mount. That is a spawn change for Scott to decide. **Needs
decision:** accept raw `aws fsx` for path D, or wait for spawn support.

## Cost model

| Item | Rate | Note |
|---|---|---|
| FSx storage | `FS_GIB × $0.600 / GB-month / 730 h` | PERSISTENT_2 SSD at 1000 MB/s/TiB, us-east-1. Throughput is priced into the storage rate |
| minimum FS | 4800 GiB → **$3.945/hr** | the EFA/GDS minimum (4.8 TiB) |
| default window | 120 min → **$7.89** | `WINDOW_MIN`; create (5–25 min) + DRA + runs + delete |
| hydration | S3 GET requests + any cross-AZ data | lazy load issues GETs against the bucket. Reconcile against CloudTrail `GetObject` counts |
| client box | spawn Price List rate × wall | as for every path |

`plan` prints the estimate, and `create` prints it again before it creates anything. **spawn's
`--cost-limit` does not cover FSx.** Only `delete`, or the watchdog, stops the bill:

- `create` tags the file system `adit:fleet=<FLEET>`, `adit:role=path-d` and `adit:deadline=<UTC>`.
- `create` starts a background watchdog (`nohup run.sh watchdog`). At the deadline it deletes the
  file system if it still exists.
- `delete` waits until the file system is gone, prints FS-hours × $/hr, warns if this happened after
  the deadline, and lists every remaining `adit:fleet` file system.
- `aditctl teardown` checks instances only, so run `run.sh status` after every session as well.

The `usd` column stays empty in path D rows, because the file system bills by the hour and not by
the run. Per-run cost = FS $/hr × the run's share of the window, plus the client box. The gate report
computes it from the header comments.

Discrepancy: spawn's rate table (`cmd/fsx_cost.go`) lists 1000 MB/s/TiB at **$1.160**/GiB-month, a
linear doubling of the 125 tier. The rate given for this project is $0.600. Confirm it from the Price
List API before G3 (CLAUDE.md: cost every run from the Price List). If spawn's table is wrong, file it
against spawn.

## Throughput ceiling (law 5 mechanism, before any run)

A PERSISTENT_2 file system delivers `capacity TiB × 1000 MB/s`. At the 4.8 TiB minimum that is about
4.69 GB/s, or **~37.5 Gb/s aggregate**. That is below the client's EFA line rate (175–400 Gb/s
across g7/g7e/p5). "Sized to the working set" therefore caps path D at the FS baseline whenever the
working set is small. Any cell where D is behind on a small working set is an FS-throughput tie
caused by sizing. Law 3 (best known configuration) may require a larger file system than the working
set needs. If so, record `fs_gib` in `config` and cost it. **Needs decision:** whether to size D by
the working set (the definition) or by throughput (fairness), or to run both.

## GDS verification (never a silent fallback)

`gdscheck`, which `read` and `tune` run first, records each of the following in
`bench/runs/D-<workload>-gds.txt`:

- `gdscheck -p` must show Lustre as `Supported`;
- the `nvidia_fs` module must be loaded;
- KvikIO compat mode must resolve to `OFF`;
- KvikIO / cuFile must report GDS available.

Any failed condition sets `gds=INACTIVE` with the reasons. With `REQUIRE_GDS=1` (the default) the
runner refuses to measure. With `REQUIRE_GDS=0` it measures anyway: the OUT header carries
`# GDS NOT ACTIVE: these rows are NOT path D (GDS) numbers`, and every row's `config` and `notes`
say `gds=INACTIVE`.

Two fallbacks are guarded, at different layers:

- KvikIO compat OFF makes KvikIO raise rather than fall back to POSIX.
- libcufile has its own `allow_compat_mode`. The runner writes `bench/runs/D-cufile.json` from the
  system `cufile.json` with `allow_compat_mode=false`, `logging.level=INFO` into
  `bench/runs/D-cufile-log/` and `fs.lustre.posix_gds_min_kb=0`. It exports `CUFILE_ENV_PATH_JSON`.

There are also two checks on every pass, done by `annotate.py`:

- the nvidia-fs `readMiB` delta must be at least 99% of the bytes read. If it is short, the row says
  `GDS NOT USED for X% of bytes`;
- `efa_rx_bytes` must be at least 99% of the bytes read. If it is short, the row says
  `Lustre traffic not (all) on EFA`.

## Caveats

- The lazy load: a cold pass measures S3 → FSx hydration plus FSx → GPU, concurrently. The project
  document wants per-lever attribution (law 6). The warm pass isolates FSx → GPU, and the metadata
  import is reported apart from `wall_s`. Hydration alone is cold minus warm, which is an inference
  and not a counter.
- One cold sample per DRA. More cold samples cost a `redra` each: a DRA delete with its data, a
  recreate and a fresh metadata import, all on the FS-hour clock. Size `WINDOW_MIN` for it.
- `CHUNK_MIB` (DRA `ImportedFileChunkSize`) sets how imported files are striped across OSTs, which
  bounds single-file parallelism. It is a law 3 knob, recorded in `config`. It can only change at
  DRA creation.
- The DRA maps `s3://BUCKET/DRA_PREFIX/` to `/DRA_PREFIX`, so `MNT/<key>` is the object. The prefix
  is the manifest keys' common directory. A DRA on the bucket root would import the metadata of
  every object in the bench bucket.
- `hack/drop-caches.sh` runs between passes, not only before the first (law 7). It clears the client
  only.
- The client box needs a Lustre client for its exact kernel. The DLAMI used in G0 runs kernel
  `7.0.0-1014-aws`, so check that the FSx Ubuntu repository has `lustre-client-modules-$(uname -r)`
  before booking G3 time.
- The g7e.12xlarge has one EFA device, shared by the Lustre LNet efa network and anything else that
  uses EFA. Path D runs alone on the box.

## UNVERIFIED (check before G3)

1. The `aws fsx create-file-system` shape: `EfaEnabled=true` and
   `MetadataConfiguration={Mode=AUTOMATIC}` inside `--lustre-configuration`, whether EFA requires the
   metadata configuration, `--file-system-type-version 2.15`, and `DataCompressionType=NONE`
   alongside GDS.
2. The capacity rules for EFA-enabled 1000 MB/s/TiB file systems: the 4800 GiB minimum is given, but
   the 2400 GiB increment used by `FS_GIB` auto-sizing is assumed.
3. The $0.600/GB-month rate. It conflicts with spawn's $1.160. Does EFA enablement add any charge?
4. Which instance types FSx supports for GDS (is g7e on the list?), and GDS-over-EFA support on the
   Lustre client.
5. The EFA client setup archive URL
   (`https://docs.aws.amazon.com/fsx/latest/LustreGuide/samples/configure-efa-fsx-lustre-client.zip`),
   its entry-point script name (`setup.sh` is assumed) and the `--optimized-for-gds` flag spelling.
   Pin `EFA_SETUP_SHA256` once it is verified.
6. The mount: whether the mount NID stays `<dns>@tcp:/<mountname>` with EFA, and whether
   `lnetctl net show` shows an `efa` net when EFA is in use.
7. The FSx Ubuntu client repository URL and keyring path, the package names, and kernel coverage for
   `7.0.0-*-aws`.
8. Whether the DRA metadata import appears as a data repository task (the runner filters
   `describe-data-repository-tasks` on `Type` containing `IMPORT`), and whether DRA `AVAILABLE` marks
   the end of the batch import. Today `META_IMPORT_S` is the time from create to `AVAILABLE`.
9. A nested `FileSystemPath` (`/adit/r1`) for a DRA.
10. The nvidia-fs stats format (`/proc/driver/nvidia-fs/stats`, the `readMiB=` field, the
    `rw_stats_enabled` parameter) on GDS 1.17, and whether nvidia-fs is required at all for Lustre
    GDS on this driver/CUDA (13.2).
11. The `gdscheck -p` output line for Lustre (`Lustre … : Supported`).
12. The KvikIO Python API in `/opt/adit-venv`: `kvikio.defaults.compat_mode()`,
    `kvikio.cufile_driver.properties.is_gds_available`, `CuFile.is_compat_mode_preferred`.
13. The cufile.json keys: `properties.allow_compat_mode`, `logging.dir` / `logging.level` and
    `fs.lustre.posix_gds_min_kb`.
14. Whether a GDS read of a released (unhydrated) file blocks correctly on the HSM restore, and
    stays on GDS instead of failing or going through POSIX.
15. `lfs hsm_state` / `lfs hsm_release` on FSx DRA files (the `released` flag).
