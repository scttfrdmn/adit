# adit: Claude Code setup prompt (scaffold + G0–G1)

Build only what the gates need. Anything not required to answer the question below is out of scope.

Name: `adit`. Repo: `github.com/scttfrdmn/adit`.
Project document (read first): https://claude.ai/code/artifact/b05c9fff-d834-4b43-9716-69ddfd5454b9

## The question

On AWS, can S3-resident data reach NVIDIA GPU memory with zero copies on the GPU host, faster and cheaper than every current path — including FSx for Lustre with GPUDirect Storage — and by how much on real workloads?

adit is a fleet of EFA-attached, GPU-less proxy instances that fetch S3 over ENA and RDMA-write the payload into the client's registered GPU buffer through NIXL's LIBFABRIC backend. One GET, two channels: HTTP control plane (presigned S3 URL + NIXL descriptor), NIXL RDMA data plane. The GPU host sees one PCIe traversal and no CPU on the data path.

Related work to read before coding: `scttfrdmn/lith` (sibling; its `cmd/lith-s3bench` harness, `bench/results/`, and the standing "behind = physical tie or issue" law on issue #1), and the cookbook measurement `scientific-codes-cookbook/measurements/lith-kraken2-roda/README.md` for process failures that apply here.

## Fixed decisions

- **Languages.** Rust for the data plane (`adit-fetch`, `adit-proxy`, `adit-client`, `adit-py`): current stable toolchain, `nixl-sys` for NIXL, `mountpoint-s3-crt` for the CRT S3 client, PyO3 for Python. Go for `cmd/aditctl` and `internal/`: current stable, no cgo. No other languages.
- **License.** Apache 2.0.
- **Versioning.** SemVer 2.0.0. Don't create tags or releases, and don't choose version numbers.
- **Changelog.** Keep a Changelog 1.1.0, `[Unreleased]` only.
- **Planning.** GitHub Projects, milestones, issues, labels. Allowed local files: README, LICENSE, CHANGELOG, CLAUDE.md, code, tests, scripts, `hack/`, `bench/`. No plan, status or TODO files.
- **Execution.** All AWS runs through spore.host (`truffle` → `spawn` → `spored`), `AWS_PROFILE=aws`, us-east-1. Every run sets a TTL and a `cost_limit`; verify terminations with `spawn list` after every session. Known spawn gotchas from lith: `spawn extend` needs `--user ubuntu -- sudo spored reload` on Ubuntu; `spawn connect` block-buffers piped stdout (write to a file on the box and poll; see lith's `hack/devbox.sh bgrun`).
- **Placement.** Proxies and clients in one AZ, one subnet, one self-referencing all-traffic security group (EFA requirement). Verify instance region equals bucket region via `x-amz-bucket-region` before any I/O.
- **Bench bucket.** `s3://scttfrdmn-lith-bench` (7-day expiry) for anything written. Create no buckets.
- **Merges.** Open PRs. You may squash-merge PRs touching only `hack/`, `bench/`, `.github/`, docs and tests once CI is green. Anything under `crates/` or `cmd/` waits for Scott.

## Verified facts (sources in the project document)

- NIXL's LIBFABRIC backend is the EFA transport: multi-rail, hwloc GPU-to-NIC placement, native RDMA write (`fi_writemsg`). UCX does not support EFA. Dynamo's own EKS guide runs NIXL LIBFABRIC over EFA on `g7e.12xlarge`.
- `nixl-sys` 1.2.0 is the maintained Rust binding (used by `dynamo-llm`). Build with `NIXL_PREFIX=/opt/nvidia/nvda_nixl`; the `stub-api` feature compiles without NIXL for CI.
- GPUDirect RDMA with EFA and FSx GDS are documented by AWS for multi-GPU G7 and G7e sizes and for P5/P5e/P5en 48xl and P6. P5.4xlarge has no GDR. G6 and G6e have EFA on large sizes but AWS documents neither GDR nor GDS for them; that is measured in G0, not assumed.
- FSx for Lustre GDS requires Persistent 2, EFA enabled, 1000 MB/s/TiB, 4.8 TiB minimum, and the `--optimized-for-gds` client setup; $0.600/GB-month in us-east-1.
- NVIDIA cuObject does not apply: it needs a ConnectX HCA on the client and a cuObjServer-integrated object server. Amazon S3 is not one.

## Laws (copy into CLAUDE.md)

1. **Zero copies is measured, never asserted.** Every adit run reports GPU-host memory traffic and CPU-seconds attributable to the transfer, from counters (perf, `/proc`, NIXL stats), beside throughput. A number without its counters is not a result.
2. **Oracle.** Every path delivers the object's bytes to the GPU buffer; SHA-256 computed on the GPU equals the object's. Any mismatch is a defect that stops the gate.
3. **Baseline fairness.** Each competing path runs at its best known configuration, tuned before measured, configuration recorded in the results row.
4. **Real workloads.** Headline numbers only from R1–R4; toy workloads calibrate and find defects.
5. **Behind means a physical tie or an issue.** Every cell where adit is behind gets a mechanism: physical limit (client EFA, S3 first byte, per-prefix rate, per-connection throughput) is a documented tie; anything else is an issue with evidence.
6. **Per-lever attribution.** Never report one multiplier alone; decompose into fetch, write, completion, stripe.
7. **Hygiene.** Same AMI per box for every path; `drop_caches` before every cold rung; region check before I/O; cheapest rung first; logs streamed off the instance; `set +e` explicit, `$-` echoed; before trusting a null result, show the probe could have resolved the effect.
8. **Authority.** Scott decides scope, names, versions, releases, merges to `main`, and when anything is done. Report state and outcome only; never declare completion.
9. **No advocacy.** Numbers with their configuration; where adit loses, the row says so.

## Hypotheses (registered)

| Id | Claim | Pass | Fail |
|---|---|---|---|
| H-zero | No payload copy in GPU-host memory; no GPU-host CPU on the data path | host memory traffic < 2% of bytes delivered; < 2 cores at peak | any host copy, or > 10% core per GB/s |
| H-ceiling | Streaming loaders are capped by the GPU host's ENA; adit over EFA exceeds it | adit ≥ 2× best path-B result on the same box | adit ≤ 1.2× path B |
| H-faster | adit beats FSx DRA + GDS on a cold, unhydrated read | time-to-last-byte ≤ 0.5× path D's hydrate+read | slower than D cold |
| H-cheaper | $/TB delivered for read-once ≤ 0.1× path D | ≤ 0.1× at ≥ 10 TiB | > 0.5× |
| H-scale | Throughput scales with proxy count until client EFA or S3 prefix rate | ≥ 0.8 linear, 1→8 proxies | flat past 2 with both unsaturated |
| H-ident | Byte-identical GPU buffers on every path | SHA-256 equal every object, every run | any mismatch |

## The five paths (all measured in G3; runners scaffolded in G1)

| Path | Mechanism | Run at its best as |
|---|---|---|
| A | `s5cmd` → NVMe → read → pinned → `cudaMemcpy` | tuned concurrency, RAID0 NVMe, 8 MiB pinned chunks |
| B | KvikIO `RemoteFile`; S3 Connector for PyTorch; Run:ai Model Streamer for R1 | thread pool and task size swept; CRT on |
| C | lith and Mountpoint mounts via cuFile compat mode | lith v0.2 defaults; Mountpoint with NIC flag |
| D | FSx Lustre P2-1000 + EFA + DRA lazy load → cuFile GDS | `--optimized-for-gds`; cold = fresh DRA; metadata import timed separately |
| E | adit | fleet 1–8; chunk 4–16 MiB; stripe on/off |

Same CLI and CSV schema for every runner: wall to last byte, throughput, GPU-host CPU-s, GPU-host memory traffic, S3 bytes, S3 requests, dollars (Price List API), `config`, `commit`.

## Workloads

Toy: T1 one 64 GiB object; T2 1,000 × 1 GiB; T3 100,000 × 16 MiB; T4 eight clients × T1. Generate into the bench bucket with recorded seeds and per-object SHA-256 manifests.

Real (G3): R1 70B-class safetensors load (stage once, record license in the issue); R2 WebDataset epoch on us-east-1 SpaceNet shards; R3 cuDF over a us-east-1 RODA Parquet set; R4 1000 Genomes phase3 CRAMs (lith's heterogeneous 8-file set). R5 checkpoint write is a G4 lever.

## Engine (what to build for G0–G1)

- **`adit-fetch`.** Ranged GETs through the CRT client into caller-supplied, pre-registered page-aligned buffers; prefix-aware parallelism; per-range completion callbacks. This is the library lith will share later; keep it free of NIXL.
- **`adit-proxy`.** HTTP endpoint `GET /{bucket}/{key}` with headers for the presigned URL, serialized NIXL agent metadata + descriptor (fall back to request body if too large), optional range and stripe. Pipeline: range lands → NIXL WRITE to `offset` in the remote VRAM descriptor → per-chunk completion signal. Bodiless 200 with bytes written. Metrics on `/metrics`.
- **`adit-client`.** Register a CUDA buffer with a NIXL agent (LIBFABRIC backend, VRAM), serialize metadata, issue requests to one or more proxies, wait on completion counters, verify by GPU-side SHA-256. C ABI; `adit-py` over it.
- **`aditctl`.** `fleet up/down/status` through spore.host (c8gn/c7gn proxies, one SG, one subnet), `presign`, `probe` (G0 matrix), `bench` (drives the path runners), `report`.
- **Push, not pull.** Proxy initiates writes. If `fi_info` on the chosen client shows no RDMA-write capability, switch that pair to client-initiated NIXL READ and record it; don't silently fall back.

Deferred unless the data calls for them: zero-copy TCP receive on the proxy, manifest prefetch, NVMe cache tier, write path, KvikIO endpoint, cuFile integration.

## Gates

### G0: feasibility probes (budget $60; stop at $50)

- **G0a.** One c8gn proxy, one g7e.12xlarge client, same AZ/subnet/SG. Install EFA installer, CUDA, NIXL (LIBFABRIC plugin), nixl-sys. Prove a NIXL WRITE from proxy host memory into client CUDA memory, verified by GPU-side hash of a 1 GiB random buffer. Then adit-proxy end to end on one 1 GiB object. Record the H-zero counters.
- **G0b.** Capability matrix, measured: on g6.12xlarge, g6e.12xlarge, g7.12xlarge, g7e.12xlarge, p5.48xlarge run `fi_info -p efa` (capabilities incl. RDMA read/write), `nixlbench --backend LIBFABRIC` with VRAM, `gdscheck -p`. One hour max each, cheapest first; p5 only if budget remains after the G-series. Commit to `bench/results/capabilities.csv`. Use `truffle` to pick sizes and prices.
- **G0c.** Single proxy → single client throughput ladder on T1: chunk 4/8/16 MiB × concurrency 16–256. Commit to `bench/results/g0c.csv`.

### G1: correctness (budget $40)

- T1 and T2 through adit-client with SHA-256 identity on every object, 10 runs.
- Fault injection: out-of-order range completion, one proxy killed mid-transfer with stripe on, S3 503 injected via a mock endpoint. Define and test the failure semantics (partial buffer is reported, never silently returned).
- Scaffold all five path runners with the shared CSV schema; paths A and B must run T1 end to end on the client box (C and D are scaffolded, not run, until G3).
- CI green: `cargo fmt`, `clippy`, `cargo test` (stub-api on CI, real tests tagged), `go vet`, `golangci-lint`, `go test`, bench CSV schema check.

Stop after G1 and report: capability matrix, G0c ladder, H-zero counters from G0a, correctness results, open issues, spend. Scott sequences G2 and G3.

## Mechanization and subagents

Every repeatable step is a `make` target: `build`, `test`, `probe`, `fleet N=`, `bench PATH= WORKLOAD=`, `report`, `teardown` (asserts no instances remain). Use subagents for independent work: one per probe box in G0b, one per path runner scaffold in G1, and a reviewer agent that has not seen the code to check every gate report against the laws before you post it. Record decisions and corrections as issue comments, not in chat.

## GitHub setup (gh CLI)

1. Create the public repo with Apache 2.0, README, CHANGELOG (`[Unreleased]`), CLAUDE.md (the laws), `.github/workflows`.
2. Create a GitHub Project linked to it; milestones G0, G1, G2, G3, G4.
3. Labels: `gate`, `oracle`, `measurement`, `defect`, `path:A` … `path:E`, `lever`, `design`.
4. Pin a `design` issue holding the project document's architecture, protocol, hypotheses and laws (link the document; don't duplicate the tables).
5. One issue per gate item above; one `lever` issue each for zero-copy receive, manifest prefetch, NVMe cache, write path, KvikIO endpoint, Dynamo model-load integration, with no implementation.

Stop after G1 is green and report. Scott sequences what follows.
