# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Path runners A–E** with one contract (`bench/paths/README.md`):
  - **A:** s5cmd → NVMe → pinned → `cudaMemcpy` via `adit-load`, with the copy inside the window.
  - **B:** KvikIO `RemoteFile` swept over threads and task size, and the S3 Connector for PyTorch.
  - **C:** lith and Mountpoint + cuFile compat. Scaffold.
  - **D:** FSx for Lustre PERSISTENT_2 + EFA + DRA + GDS, with a cost plan, CONFIRM-gated create and a deadline watchdog. Scaffold.
  - **E:** `adit-get`.
  
  B, C and D share one GPU loader (`bench/paths/lib/gpuload.py`) with the same counter window and GPU oracle as E. The client bootstrap installs s5cmd and a Python env with CuPy, KvikIO, PyTorch, the S3 Connector and adit-py.

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
