# Path runners

One runner per path, one contract, one results schema (`bench/schema/paths.csv`).

| Path | Mechanism | Runner |
|---|---|---|
| A | copy then upload: `s5cmd` → instance NVMe → read → pinned staging → `cudaMemcpy` | `A/run.sh` |
| B | streaming loader: KvikIO `RemoteFile` (and the S3 Connector for PyTorch) → GPU, no disk | `B/run.sh` |
| C | FUSE mount + cuFile compatibility mode (lith, Mountpoint) | `C/run.sh` (scaffold, not yet run: [C/README.md](C/README.md)) |
| D | FSx for Lustre DRA + GDS | `D/run.sh` (scaffold, not yet run: [D/README.md](D/README.md)) |
| E | adit: proxy fleet, NIXL RDMA write into registered HBM | `E/run.sh` |

## Contract

```
bash bench/paths/<P>/run.sh <workload>
```

Each runner runs **on the GPU client box**, from the repository root, and is configured by
environment variables:

| Variable | Meaning |
|---|---|
| `MANIFEST` | manifest JSONL for the workload (`aditctl gen` or a staged real workload); paths A, C and D |
| `PLAN` | the same with presigned URLs (`aditctl presign`); paths B and E |
| `BUCKET`, `REGION` | where the objects are (default `scttfrdmn-lith-bench`, `us-east-1`); the runner refuses to start if `aditctl region-check` fails |
| `RUNS` | runs per configuration (default 3) |
| `OUT` | results CSV to append to (default `bench/runs/<P>-<workload>-<utc>.csv`) |
| `CLIENT_INSTANCE` | recorded in every row (the runner reads the instance type from IMDS if unset) |
| `COMMIT` | recorded in every row (the runner reads `adit=` from `/var/lib/adit/versions` if unset) |
| `COLD` | `1` (default) drops the page cache before every run (`hack/drop-caches.sh`) and logs each drop's status line to `<OUT>.cold`; `0` reuses the cache |
| path-specific | listed at the top of each `run.sh` |

The shared prologue is `lib/common.sh`. `make bench P=<P> WORKLOAD=<W>` and `aditctl bench --path <P>
--workload <W>` run the same command.

Every row carries:
- the counters (`host_cpu_s`, `proc_cpu_s`, `efa_rx_bytes`, `host_mem_*` or `unavailable`) from the
  same window code (`adit-client::counters`, or `adit.Window` from `adit-py` in Python);
- the oracle verdict, from the GPU per-part SHA-256 composite compared with the manifest, on every
  path whose bytes reach the GPU;
- `config` (every knob in force) and `commit`.

Laws 3 and 7 apply: tune each path before it is measured, and record the configuration in the row.

## Other scripts here

The shell scripts except `threadcpu.sh` source the same prologue, so the contract variables above
apply to them too. Usage and knobs are in each script's header.

| Script | What | Runs on |
|---|---|---|
| `lib/gpuload.py` | the Python reader for paths B, C and D (KvikIO `RemoteFile`, `CuFile`, S3 Connector), with the `adit-py` oracle and counter window | client |
| `A/stages.sh` | path A split into download and load, with wall time and CPU per stage (#45) | client |
| `B/sweep.sh` | path B tuning grid: threads × task size × objects in flight (law 3) | client |
| `interleave.sh` | arms (`B<threads>`, `E<n>`, `G<g>P<ids>`) in a rotated order per cycle, with per-arm proxy and EFA snapshots; most `hack/g2.sh` steps call it | client |
| `confirm50.sh` | #50 check: one E read unpinned vs pinned, counting NIXL duplicate-fragment warnings | client |
| `repro51.sh` | #51 reproduction: repeated E reads per agent mode, counting client aborts | client |
| `fetchprobe.sh` | fetch only (`adit-fetch-bench`), arms rotated, with ENA, EFA and thread snapshots (#53); `hack/fetchprobe.sh` drives it from the operator machine | proxy |
| `threadcpu.sh` | per-thread CPU of one process from `/proc` | any |

Runner output lands in `bench/runs/` (not committed). Results that are kept go to `bench/results/`
with their header comments; `make schema-check` validates them.
