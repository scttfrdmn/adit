# Path C: FUSE mount + cuFile compatibility mode

**Scaffold: not run until G3.** Nothing here has produced a number yet; flags marked UNVERIFIED
have not been checked against the installed binary on the client AMI.

## What C measures

The workload's bucket is mounted read-only in its native key layout by a FUSE file system, and
`lib/gpuload.py --mode cufile --compat on` reads every manifest object through KvikIO `CuFile`
with `KVIKIO_COMPAT_MODE=ON`. In compat mode, cuFile does a POSIX `pread` into a host bounce
buffer and then copies it to the GPU. Every object is checked on the GPU against the manifest
(law 2). The path costs the GPU host:

- one FUSE daemon's S3 fetch;
- a kernel-to-user copy through `/dev/fuse` and the page cache;
- a copy into KvikIO's bounce buffer;
- a host-to-device copy.

Path E removes those copies, and C exists to measure them. The measured window covers only the
reads: mount, index build and unmount happen outside it, and the header comments record them.

Two file systems:

| fs | what it is |
|---|---|
| `lith` | read-only S3 FUSE file system with a prebuilt local index (`/Users/scttfrdmn/src/lith`, v1.18.1 at the time of writing) |
| `mountpoint` | Mountpoint for Amazon S3 (`mount-s3`) |

## Usage

```
MANIFEST=... BUCKET=... REGION=us-east-1 bash bench/paths/C/run.sh <workload>
```

The common contract is in `../README.md`. Path knobs are listed at the top of `run.sh`.

## Layout: key K is the file `$MNT/<fs>/K`

`gpuload --dir D` opens `D/<key>`. Both file systems can root a mount at a key prefix. The runner
mounts the prefix at the matching subdirectory, `$MNT/<fs>/<PREFIX>`, and passes
`--dir $MNT/<fs>`, so full keys resolve unchanged. `PREFIX` defaults to the longest common
directory of the manifest's keys, and the runner refuses to start if any key falls outside it.

## Mount commands, as `run.sh` issues them

**lith.** Commands are quoted from lith's `README.md`, `docs/index.md` and `cmd/lith/cmd_mount.go`.
The index is built once, before any run:

```
lith index build s3://$BUCKET/$PREFIX --index-file $LITH_INDEX --region $REGION
lith mount s3://$BUCKET/$PREFIX $MNT/lith/$PREFIX --daemon --region $REGION \
    --index-file $LITH_INDEX --metrics 127.0.0.1:9101 $LITH_FLAGS
lith umount $MNT/lith/$PREFIX            # falls back to fusermount3 -u
```

The flags behave as follows:

- **`--daemon`**: lith forks into the background once the mount is ready. Without it, lith runs in
  the foreground.
- **`--index-file`**: lith answers listing and stat from the index with zero S3 calls. If the index
  is absent, lith builds it at mount time when the key count is under `--auto-index-limit`. The
  runner builds it explicitly so the build stays outside the window.
- **`--metrics`**: serves `/metrics` only. After every run the runner scrapes it into
  `<OUT>.lith-metrics-*.txt`. This is how we read lith's configuration back from the running
  process, as CLAUDE.md requires, along with cache tiers, S3 bytes and S3 requests.
- **Mount point ownership**: the user that runs `lith` must own the mount point, so the runner
  `chown`s `$MNT`.

**Mountpoint.** Flags are from awslabs/mountpoint-s3 `doc/CONFIGURATION.md`:

```
mount-s3 $BUCKET $MNT/mountpoint/$PREFIX --read-only --region $REGION --prefix $PREFIX \
    --maximum-throughput-gbps $NIC_GBPS $MP_FLAGS
fusermount3 -u $MNT/mountpoint/$PREFIX
```

The flags behave as follows:

- **`--prefix`**: must end in `/`.
- **`--maximum-throughput-gbps`**: Mountpoint auto-detects the available network bandwidth on EC2,
  and the runner sets it explicitly anyway.
- **`--cache`**: off by default and not passed. Without `--cache`, `--metadata-ttl` defaults to
  `minimal`.
- **Daemonizing**: `mount-s3` daemonizes by default (UNVERIFIED on the installed version).

## Tuning knobs and where their best values come from (law 3)

| knob | value used | source |
|---|---|---|
| lith flags | lith defaults (`LITH_FLAGS` empty), lith pinned by `LITH_VERSION` (default v1.18.1), and `lith version` recorded in the results header | The project prompt's "lith defaults from its v0.2 matrix" predates lith 1.x (now 1.18.x). The current defaults (`--mem-cache` 25% RAM, `--disk-cache 0`, `--s3-concurrency 128`, `--inflight-bytes` 2 × NIC × 100 ms, readahead sized to the detected NIC; `--nic-gbps` overrides detection) are lith's own tuned configuration, so they are path C at its best (law 3). |
| `mount-s3 --maximum-throughput-gbps` | `NIC_GBPS`: the instance type's `NetworkInfo.NetworkCards[0].BaselineBandwidthInGbps` (UNVERIFIED field name), floored to an integer (UNVERIFIED that the flag rejects fractions) | Project prompt: "set to the NIC". Card 0 carries the primary ENA, the interface S3 traffic uses on multi-card EFA instances. |
| `mount-s3 --max-threads`, `--read-part-size` | Mountpoint defaults (16 concurrent FUSE ops; 8 MiB part), overridable via `MP_FLAGS` | Mountpoint `CONFIGURATION.md`. lith's matrix ran mount-s3 at `--read-part-size 8388608`. With `CONCURRENCY` above 16, `--max-threads` probably binds and should be raised before measuring. |
| `KVIKIO_THREADS`, `TASK_MIB`, `CONCURRENCY` | swept: `16 64 128` × `4 16` × `8` | Same sweep as path B. Pick the best cell per file system and record it. |

## Caveats

- **The FUSE daemon's cache vs `COLD`.** `drop_caches` empties the kernel page cache but not a FUSE
  daemon's own memory. lith's in-process memory tier (25% of RAM by default) survives it and
  would serve a "cold" rerun from RAM. With `COLD=1` the runner therefore remounts before every
  run (`REMOUNT=1`) and drops caches after mounting. `COLD=0 REMOUNT=0` measures warm.
- **The kernel page cache.** lith opens files with `FOPEN_KEEP_CACHE` (lith `docs/knobs.md`), so
  kernel pages survive close and reopen within one mount. That is another reason to remount per
  cold run. Whether Mountpoint without `--cache` uses `direct_io` (bypassing the page cache) is
  UNVERIFIED. The answer determines whether compat-mode reads on Mountpoint pay one more host copy
  than on lith.
- **Concurrent readers of one object on lith.** If cuFile splits one file into parallel tasks
  (`TASK_MIB` < object size, threads > 1), lith's handles may see each other's reads only as holes
  in the shared page cache. lith `docs/knobs.md` (#316) measured this pattern at up to 243× slower.
  One cuFile handle per object reading in parallel tasks may or may not trigger it; watch
  `lith_prefetch_low_coverage_total` in the metrics scrape. `--prefetch-sibling-coverage` is lith's
  experimental fix and is off by default.
- **Mountpoint metadata.** Mountpoint has no index. Every `open` does a lookup against S3 (LIST
  and/or HEAD), and with `--metadata-ttl minimal` those lookups are not cached across runs. That
  is part of the path's cost, and it is inside the window. lith's index build is outside the
  window and recorded once in the header. On many-small-object workloads this difference
  dominates; note it in the row for any such cell.
- **GPU memory.** `gpuload` allocates one device buffer per manifest object at once, so the
  manifest's total size must fit in GPU memory.
- **Row attribution.** `gpuload` writes its own `config`. `run.sh` appends `;fs=<fs>;mount=<flags>`
  and sets `run` to the runner's run number. Each file system's rows follow a
  `# mount: fs=... cmd=...` comment line in `OUT`.
- **Oracle.** `gpuload` exits 1 on any mismatch or short read. The runner stops the gate there
  (law 2) and unmounts in its `EXIT` trap.
