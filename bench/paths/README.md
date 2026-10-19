# Path runners

One runner per path, one contract, one results schema (`bench/schema/paths.csv`).

| Path | Mechanism | Runner |
|---|---|---|
| A | copy then upload: `s5cmd` → instance NVMe → read → pinned staging → `cudaMemcpy` | `A/run.sh` |
| B | streaming loader: KvikIO `RemoteFile` (and the S3 Connector for PyTorch) → GPU, no disk | `B/run.sh` |
| C | FUSE mount + cuFile compatibility mode (lith, Mountpoint) | `C/run.sh` (scaffold; runs in G3) |
| D | FSx for Lustre DRA + GDS | `D/run.sh` (scaffold; runs in G3) |
| E | adit: proxy fleet, NIXL RDMA write into registered HBM | `E/run.sh` |

## Contract

```
bash bench/paths/<P>/run.sh <workload>
```

Each runner runs **on the GPU client box**, from the repository root. It is configured by environment variables:

| Variable | Meaning |
|---|---|
| `MANIFEST` | manifest JSONL for the workload (`aditctl gen` or a staged real workload) |
| `PLAN` | the same with presigned URLs (`aditctl presign`); paths B and E |
| `BUCKET`, `REGION` | where the objects are; the runner refuses to start if `aditctl region-check` fails |
| `RUNS` | runs per configuration (default 3) |
| `OUT` | results CSV to append to (default `bench/runs/<P>-<workload>-<utc>.csv`) |
| `CLIENT_INSTANCE` | recorded in every row (the runner reads it from IMDS if unset) |
| `COMMIT` | recorded in every row (the runner reads `/var/lib/adit/versions` if unset) |
| `COLD` | `1` drops the page cache before every run (`hack/drop-caches.sh`); `0` reuses it |
| path-specific | listed at the top of each `run.sh` |

Every row carries:
- the counters (`host_cpu_s`, `proc_cpu_s`, `efa_rx_bytes`, `host_mem_*` or `unavailable`) from the same window code (`adit-client::counters` or `adit.Window` in Python);
- the oracle verdict, from the GPU per-part SHA-256 composite compared with the manifest, where every path's bytes reach the GPU;
- `config` (every knob in force) and `commit`.

Laws 3 and 7 apply: tune each path before it is measured, and record the configuration in the row.
