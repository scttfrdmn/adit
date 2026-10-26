# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`hack/g2.sh` `slotcmp` names each cycle's snapshots `c<cycle>-…`** (`CYCLE_BASE` in `interleave.sh`). They used to overwrite one another, leaving only the last cycle (#71).
- **NIXL and `nixl-sys` are pinned to upstream main `a9f1f7a7`** (Scott, 2026-10-23); they were v1.2.0. That commit has sender-keyed LIBFABRIC notifications (ai-dynamo/nixl#2294, the fix for #50) and the endpoint/CQ lock (#2308, the likely fix for #51); no release has either yet. `hack/install.sh` builds that commit and rebuilds when the ref changes.

### Added

- **Per-pair cap probe (#53).**
  - `bench/paths/interleave.sh` gains `G<g>P<ids>[-b<GiB>][-d<us>]` arms: the first g GPUs, served by the proxies at the listed indices, with optional per-arm buffer size and NIXL progress delay.
  - `hack/g2.sh` gains `pairs` and `pairslots` steps on a new T2-256 plan.
  - `hack/g2-place.sh` gains `CLIENT_TYPES` and `FLEET_EIPS`.
- **Every cold rung's `drop_caches` is now verified (#70).** `hack/drop-caches.sh` reports the page cache before and after and fails on error. `cold()` keeps each status line in `<out>.cold`, and the `collect` step fetches it.
- **G2 run 5 results (balanced ladder, one GPU per proxy)** in `bench/results/paths-g2r5.csv`: p5en with 7 × c8gn.48xlarge on both EFA rails, 4096 slots. There are 24 rows (slotcmp, ladder, csweep), and 23 are oracle ok. The partial row is the #51 abort. `hack/g2.sh` gains the `slotcmp` step and `DEVICES_PER_ARM`, which `bench/paths/interleave.sh` honours by giving E\<k\> the first k GPUs. `hack/g2-place.sh` tries proxies first in us-east-1c and us-east-1f.
- **G2 run 4 results (two-rail proxies)** in `bench/results/paths-g2r4.csv`: p5en with 7 × c8gn.48xlarge on both EFA rails, NIXL `a9f1f7a7`. `hack/g2-place.sh` gains `REGIONS` and `REQUIRE_ALL_RAILS`, and `hack/g2.sh` takes the proxy list from the fleet, so a proxy that never placed leaves no gap.
- **`aditctl teardown` releases orphaned spawn Elastic IPs.** In each region it runs `spawn cleanup --elastic-ips` (spawn 0.142.0, spawn#1232), then verifies on EC2 that no unassociated spawn launch Elastic IP remains, failing otherwise. `--no-eip-cleanup` skips this.
- **`hack/g2.sh` `csweep` step (#53)** and a wider path B grid (#54). **G2 run 3 results** are in `bench/results/paths-g2r3.csv`: p5en and 8 × c8gn.48xlarge (1 rail) on NIXL `a9f1f7a7`. All 45 rows have oracle ok, with 0 aborts.
- **Multi-rail reproduction with a NIXL control** in `bench/results/paths-r51mr.csv`, run on p5.48xlarge with 32 EFA rails. On NIXL v1.2.0, one agent shared by 2 proxies failed 10 of 10 runs, with 6,839 duplicate-fragment warnings and 1 #51 abort (the EFA "Local memory registration … IOVA" error). On upstream main `a9f1f7a7` the same runs failed 0 of 10, with no warnings and no aborts. One agent per GPU was clean on both.
- **#51/#50 small reproduction:** `bench/paths/repro51.sh`, the `hack/g2.sh` `repro51` step and `hack/r51-place.sh`. The results are in `bench/results/paths-r51.csv`. One NIXL agent shared by two proxies lost notifications in 9 of 10 runs, with NIXL's "Duplicate fragment received" warnings. One agent per GPU, each pinned to one proxy, lost none in 20 runs. The #51 abort did not reproduce on one rail (0 of 30).
- **G2 rerun results** in `bench/results/paths-g2r.csv` (#48): p5.48xlarge client and 8 × c8gn.48xlarge proxies with one rail each in us-east-2. It covers the T2 proxy ladder, the path B tuning grid with its best configuration, the #50 probe, and the client EFA abort backtrace (#51).
- **`aditctl fleet up --proxy-efa-interfaces N`** (spawn's `--efa-interfaces`). `hack/g2-place.sh` gives every proxy both rails only when the region's Elastic IPs hold the whole fleet, otherwise one rail each, and launches proxies on-demand.
- **Per-arm snapshots in `bench/paths/interleave.sh`.** Every proxy's `/metrics` and the client's per-device EFA counters are saved before and after each E arm, for per-lever attribution and stall forensics (#51).
- **EFA counters in proxy `/metrics`.** `adit-proxy` exposes its own EFA hw_counters per device (`adit_proxy_efa{dev,counter}`).
- **`hack/g2.sh` `bsweep`** tunes path B (threads × task size × objects in flight) on the client before H-ceiling (law 3).

- **`adit-get --agent-per-gpu`** gives each destination GPU its own NIXL agent. One agent holding 8 VRAM regions lost notifications for every region but the first under concurrent writes on p5en (#50).
- **`hack/g2-place.sh`** retries placement across every approved p5en region: proxy first where clients land without proxies, and a client that lands without a proxy is retired. **`hack/bench-bucket.sh`** creates regional bench buckets configured like the us-east-2 one.
- **G2 results** in `bench/results/paths-g2.csv` (#48): p5en client, 8 × c8gn.48xlarge proxies, T2 stripe ladder, path B on 8 GPUs, T1 spread. Partial rows are defects (#51), not results.

- **Multi-GPU destinations (G2, #48).**
  - `adit-get --devices 0,…,7` registers one buffer per GPU with one NIXL agent (`ClientAgent::with_regions`) and sends whole objects to the least-filled GPU. `--spread` instead splits each object into part-aligned segments, one per GPU.
  - Each GPU hashes its own segments. The object's part digests are their concatenation, checked against the manifest as before.
  - `Client::with_dests` / `get_into` address a request to one of several registered destinations, and notification offsets stay destination-relative.
  - Path B's loader (`gpuload.py --devices`) spreads objects round-robin over the same GPUs.
- **Per-EFA-device counters.** The counter window keeps deltas per device (`Measured::efa_by_dev`), and `adit-get` notes `rails=active/total` per run.
  - `hack/wseries.sh` sums counters over every EFA device; it previously broke on multi-card boxes. It also reports `rails`.
  - `hack/health.sh` with `ALL_RAILS=1` fails a proxy whose EFA devices did not all carry writes.
- **`aditctl fleet`:**
  - `--proxy-spot` (spot first, on-demand on capacity or quota errors);
  - proxies launch in parallel once the first is up;
  - `VerifyEFA` requires as many EFA interfaces as the instance type supports, not just one (spawn#661).
- **`hack/g2.sh`** raises each proxy's open-file limit to the hard limit. At 2,048 slots the default 1,024 turned fetches into `AWS_ERROR_MAX_FDS_EXCEEDED` 502s.
- **`hack/g2.sh`** drives the G2 session: refresh, per-proxy health, proxy start with configuration read back, presign, the stripe ladder, chunk and slot sweeps, T1 spread, H-ceiling and collect. **`hack/gen-box.sh`** is the generator role. `bench/paths/interleave.sh` takes `E<n>` arms over `PROXIES`, `B<threads>` arms, and `DEVICES`, `CHUNK_MIB` and `E_ARGS`.

- **`bench/paths/interleave.sh`** runs path arms in a rotated order every cycle, so time-of-session effects (S3 warm-up on a repeatedly read key) spread across arms instead of confounding them (#45).
- **`bench/paths/A/stages.sh`** splits path A into the s5cmd download (its own wall and rusage) and the local load, for per-stage attribution.

- **Path D sizing by target throughput.** `bench/paths/D/run.sh sizing` prints the PERSISTENT_2 configuration and $/hr needed to feed a client at a given rate. `plan` and `create` size for the larger of the working set and `TARGET_GBPS`, so path D runs at its best (law 3), not at the 4.8 TiB minimum. Path C pins lith (`LITH_VERSION`, default v1.18.1).

- **Path runners A–E** with one contract (`bench/paths/README.md`):
  - **A:** s5cmd → NVMe → pinned → `cudaMemcpy` via `adit-load`, with the copy inside the window.
  - **B:** KvikIO `RemoteFile` swept over threads and task size, and the S3 Connector for PyTorch.
  - **C:** lith and Mountpoint + cuFile compat. Scaffold.
  - **D:** FSx for Lustre PERSISTENT_2 + EFA + DRA + GDS, with a cost plan, CONFIRM-gated create and a deadline watchdog. Scaffold.
  - **E:** `adit-get`.
  
  B, C and D share one GPU loader (`bench/paths/lib/gpuload.py`) with the same counter window and GPU oracle as E. The client bootstrap installs s5cmd and a Python env with CuPy, KvikIO, PyTorch, the S3 Connector and adit-py.
- **Per-row NIC placement verdict** (#26). `adit-get` compares the NIC's one-sided placement
  counters (`rdma_write_recv_bytes` + `rdma_read_bytes`) with the payload and writes
  `placement=nic|emulated|unknown` into every row. On `emulated` it exits non-zero unless
  `--allow-emulated` is given. The GPU oracle checks bytes, not how they arrived; on g6e every
  emulated transfer passed it while copying through host memory (#20, #25).

- **Instruments for the G0-close round:**
  - per-chunk stage histograms on `/metrics`: slot wait, fetch, write wait and write, so pipeline throughput can be split by stage (#18);
  - per-chunk timings in `pipeline::Outcome`;
  - `adit-fetch-bench` reads many objects from a plan, rewrites the URL scheme for a TLS A/B, and logs every chunk;
  - a CRT `GetObject` request type alongside `Default`;
  - client knobs for the notification pump's back-off and NIXL's progress-thread delay (#17);
  - a write-probe health gate (`hack/health.sh`) that `hack/g0c.sh` runs before the ladder and every 5 rungs.

- **Client-initiated READ in the probe pair** (`--op read`). This is the design's fallback for EFA without hardware RDMA write. On g6e it completes correctly, but it is emulated as well (#20).
- **The capability matrix records hardware RDMA into VRAM from EFA counters.** `fi_info` lists `FI_RMA` even where libfabric emulates it, so its flags are not evidence (#20).

### Fixed

- **Lost notifications and stalled writes with many proxies (#50, #51).** `adit-get --pin-gpu-proxy` (with `--agent-per-gpu`) serves each destination GPU from exactly one proxy and does not stripe a request across proxies. NIXL 1.2.0's LIBFABRIC target matches notifications to writes by a 16-bit transfer id only, and every initiator numbers its transfers from 1. Two proxies writing into one agent collide: the second notification is dropped as a "duplicate", and the leaked placeholder entries grow the map the target scans on every completion. `hack/g2.sh` uses it by default; its `confirm50` step checks the mechanism.
- **Host CPU counts bursty work.** `host_cpu_s` is now online CPUs × wall − (idle + iowait), with idle accounted precisely under NOHZ. The tick-sampled sum missed most of a process that wakes every ~100 µs: G2 on p5en read host 1.49 s against 3.60 s for the process alone. It also counted `guest` twice and included `steal`. The old figure is kept as `host_cpu_ticks_s` (#27).
- **GPU verification left the GPU mostly idle.** `adit-get` hashed each object with its own
  kernel launch: 128 threads for a 1 GiB object, synchronized one at a time. On T2 (1,000 × 1 GiB)
  that made verification about 4× the transfer time. One `sha256_segs` launch now hashes every part
  of a wave (`Gpu::sha256_parts_many`). The CPU-shim check passes, and a hardware test compares the
  result with the per-object and CPU digests.

- **Host CPU could read below process CPU** (#27). The counter window now closes the process
  sample before the host sample, so the host window encloses the process window, and deltas
  saturate. A Linux test busy-loops 0.4 s and checks the two counters agree.

- **NIXL registrations of 16 GiB or more failed on the proxy** (#19). The EFA installer runs with
  `--skip-limit-conf`, which left `memlock` at the distro default of about 15 GiB.
  `hack/install.sh` now installs an unlimited `memlock` limit for new sessions.

- **Repository, laws and CI.** Apache 2.0 license, README, this changelog, `CLAUDE.md` holding
  the nine laws, and a single CI gate (`.github/workflows/gate.yml`) that runs the Rust tree
  (`cargo fmt`, `clippy`, `cargo test` against the NIXL stub API) and the Go tree (`go vet`,
  `golangci-lint`, `go test -race`) plus the bench CSV schema check.
- **The engine, end to end on a loopback data plane** (#5). `adit-fetch` issues ranged GETs
  against presigned URLs through the AWS CRT into caller buffers. `adit-proxy` turns one
  `GET /{bucket}/{key}` into chunked fetches, each RDMA-written by NIXL to its offset in the
  client's registered buffer with a completion notification. `adit-client` registers the GPU
  buffer, stripes objects across proxies, and accounts completion from notifications. A get
  succeeds only if every proxy answered `200` and the notifications cover exactly the requested
  bytes; otherwise it returns `Partial` with the covered and missing ranges. The tests cover a
  proxy killed mid-transfer, persistent and transient S3 503s, out-of-order fetches and writes,
  and ten-run identity over a scaled-down T2.
- **S3 request counts include the CRT's own retries.** The CRT retries 503s internally. Counting
  one request per range under-reported them, and the tests caught it. Requests are now counted
  from the CRT's per-request telemetry, failed attempts included, and the tests assert the count
  equals what the server saw.
- **GPU oracle.** A CUDA kernel compiled at runtime through NVRTC computes the SHA-256 of every
  part of a device buffer in parallel. The composite of those part digests is compared with
  S3's stored multipart `ChecksumSHA256` and with the generation manifest (design #1, item 1).
- **H-zero counters** around every transfer window: host and process CPU-seconds, EFA device
  counters, and memory-controller traffic through `perf` where the instance exposes it.
  Otherwise the column reads `unavailable`, never a guess.
- **Binaries for the boxes:** `adit-proxy`; `adit-get` (path E: results rows with counters);
  `adit-load` (path A's GPU half, with the copy step inside the measured window); and the G0a
  probe pair `adit-nixl-probe-init` / `adit-nixl-probe-target`. Every binary refuses to measure
  when built against the NIXL stub.
- **`aditctl`** (Go, no cgo):
  - `gen`: deterministic toy workloads uploaded multipart with SHA-256 checksums; refuses unless
    S3's composite matches.
  - `presign` and `region-check`: both refuse cross-region I/O.
  - `price`: on-demand from the Price List API, spot from history.
  - `fleet up/down/status`: through spawn. Candidates are tried in ranked order (region, AZ,
    spot or on-demand) because GPU capacity is scarce. Each fleet gets a self-referencing EFA
    security group shared by proxies and clients of different instance types.
  - `teardown`: verified via `spawn list`.
  - `report --check`: the CSV schema plus the mechanically checkable laws (configuration and
    commit on every row; counters on every adit row).
- **`hack/install.sh`** bootstraps a proxy or client box: EFA, NIXL `v1.2.0` with the LIBFABRIC
  plugin, nixlbench, Rust, and the adit build against the real NIXL. **`hack/probe.sh`** collects
  the single-box facts for the capability matrix.

### Removed

- The two planning documents committed by mistake in the first commit. The project document
  lives outside the repository and is linked from the pinned design issue.
