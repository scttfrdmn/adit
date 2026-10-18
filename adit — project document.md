# adit — project document

Oct 16, 2026 · @Scott Friedman

## What adit is

adit moves bytes from Amazon S3 into NVIDIA GPU memory on EC2 with zero copies on the GPU host: a fleet of EFA-attached, GPU-less proxy instances fetches S3 over ENA and RDMA-writes the payload into the client's registered HBM through NIXL's LIBFABRIC backend. The GPU host sees one PCIe traversal (NIC to GPU), no TCP, no TLS, no bounce buffer, and no CPU on the data path.

**The question adit answers.** On AWS, can S3-resident data reach GPU memory faster and cheaper than every current path, including FSx for Lustre with GPUDirect Storage, and by how much on real workloads?

**Scope.** Read path first (S3 to HBM), write path (HBM to S3, checkpoints) as a later gate. Read-only semantics over native S3 key layout; no filesystem, no POSIX. Build what the gates need and nothing else. Scott decides scope, names, versions, releases, and when anything is done.

## Placement

adit is an independent repository under Playground Logic and a sibling of lith: lith is S3 as a filesystem, adit is S3 as memory; both read-only over native key layout. The S3 fetch engine (ranged GETs, prefix spreading, the access-hint and manifest prefetch design) is extracted into a library both consume, so lith and adit pull bytes the same way and present them two ways.

| Project | Role next to adit |
| --- | --- |
| lith | POSIX face of the same data; shares the fetch library; adit is the GDS-class path lith cannot have as a FUSE client |
| spore.host (truffle, spawn, spored) | Provisions, sizes and retires the proxy fleet and every test instance |
| plimsoll | Optional control-plane front end for quota and cost policy on the S3 side |
| CargoShip | Archive writer the checkpoint (write) path hands off to |
| scientific-codes-cookbook | Where the measured recipes and the copy-ladder comparison are published |

## Architecture

Three components, two languages. The data plane is Rust on [nixl-sys](https://docs.rs/crate/nixl-sys/1.2.0) (NIXL's maintained Rust bindings, the ones Dynamo uses); the control plane and fleet tooling are Go, matching every other Playground Logic project.

| Component | Language | Runs on | Does |
| --- | --- | --- | --- |
| adit-proxy | Rust | EFA + big-ENA instances, no GPU (c8gn/c7gn class; i8g class when caching) | S3-shaped HTTP control endpoint; S3 ranged GETs via the AWS CRT S3 client; NIXL agent that RDMA-writes received ranges into the client's registered GPU buffer over the LIBFABRIC backend |
| adit-client | Rust library, Python binding (PyO3) | GPU instance | Registers GPU buffers with NIXL (VRAM descriptors over EFA), issues GETs carrying the serialized descriptor, waits on completion counters; later a KvikIO RemoteFile endpoint |
| aditctl | Go | Operator's machine | Launches and sizes the fleet through spore.host, builds manifests, presigns URLs, collects metrics and bench results |

**Why NIXL rather than raw libfabric.** NIXL already abstracts "move bytes from a remote descriptor into my GPU memory", already has the [LIBFABRIC backend](https://docs.nvidia.com/nixl/user-guide/backend-selection/libfabric/) with multi-rail EFA and hwloc GPU-to-NIC placement, and is the transfer layer Dynamo, vLLM and LMCache already speak on AWS. An S3-fetching NIXL agent on the proxy gives every one of those consumers the path with no new client protocol.

**Why not cuObject.** NVIDIA's GPUDirect Storage for Objects needs a ConnectX HCA on the client and a cuObjServer-integrated object server; Amazon S3 does not speak its RDMA token protocol and EFA is not a ConnectX. adit plays the cuObject server role with S3 behind it, over the transport AWS actually has.

## Protocol and data path

One GET, two channels: the control plane is an ordinary HTTP GET to a proxy carrying a presigned S3 URL and a NIXL descriptor for the client's registered GPU buffer; the data plane is NIXL RDMA writes from the proxy into that buffer. The HTTP response is bodiless and carries the byte count.

1. **Register once.** The client allocates a GPU buffer (or a ring of them), registers it with its NIXL agent as VRAM over the LIBFABRIC backend, and serializes the agent metadata plus the descriptor. Registration is amortized across many objects.
2. **Request.** `GET /{bucket}/{key}` to a proxy with headers: the presigned S3 URL (so the proxy holds no credentials and can read nothing the client could not), the serialized NIXL descriptor, an optional range, and an optional stripe assignment.
3. **Fetch.** The proxy fans the range out as 8 to 16 MiB ranged GETs over ENA with the CRT client (parallel connections, prefix-aware), into pre-registered receive pages.
4. **Write.** Each range that lands is RDMA-written to its offset in the client buffer as it arrives: SRD delivers out of order and S3 ranges complete out of order, and one-sided writes carry their destination offset, so there is no reassembly anywhere.
5. **Complete.** A per-chunk write-with-immediate (or a tail message with a bitmap) advances a counter on the client; the request is done when bytes written equals bytes requested.
6. **Stripe.** A large object is split across N proxies by range; all N write into the same client buffer with the same descriptor. Aggregate S3 bandwidth scales with proxy count until the client's EFA ingest is the limit.

**Push, not pull.** The proxy initiates the write, so the client never polls remote state and the proxy paces itself against S3. RDMA write on EFA is a native `fi_writemsg` in NIXL's LIBFABRIC backend.

**Credentials.** Presigned URLs for both general-purpose buckets and S3 Express One Zone (session auth is handled by the signer). EFA requires same-AZ placement and an all-traffic security group between members, which coincides with Express One Zone co-location.

**Later gates, not v0.**

- Zero-copy receive on the proxy (kernel zero-copy TCP receive into the registered pages; plain HTTP to the S3 VPC endpoint where bucket policy allows), so the proxy's one remaining copy disappears.
- Manifest prefetch: the client posts keys and ranges in epoch order; proxies pre-pull from S3 ahead of demand, hiding S3 first-byte latency.
- NVMe cache on the proxies (i8g/i7ie class), consistent-hashed by key range, for multi-epoch and repeated model loads.
- Write path: proxy RDMA-reads from the client buffer and runs the multipart upload, so checkpoints leave HBM at NIC speed without touching the trainer's host.

## Fixed decisions

| Decision | Value |
| --- | --- |
| Name | adit (a horizontal passage driven straight into the seam: the ore comes out level, not hauled up and lowered again) |
| Repository | `github.com/scttfrdmn/adit`, later `playgroundlogic/adit`; Scott's call when |
| Languages | Rust for adit-proxy and adit-client (nixl-sys, CRT S3 client via the `mountpoint-s3-crt` bindings, PyO3 for Python); Go for aditctl and fleet tooling |
| License | Apache 2.0 |
| Versioning | SemVer 2.0.0; no tags, releases or version numbers chosen by Claude Code |
| Changelog | Keep a Changelog 1.1.0, `[Unreleased]` only until Scott tags |
| Planning | GitHub Projects, milestones, issues, labels; no local plan, status or TODO files; allowed local files are README, LICENSE, CHANGELOG, CLAUDE.md, code, tests, scripts, `bench/results/` |
| Execution | All AWS runs through spore.host (truffle, spawn, spored); every run has a TTL and a cost\_limit; terminations verified via `spawn list` |
| Region and AZ | us-east-1 to match the public datasets used; proxies and clients in one AZ, one subnet, one EFA security group |
| Bench bucket | `s3://scttfrdmn-lith-bench` (7-day expiry) for anything the benchmarks write; no new buckets without Scott |
| Client integration order | adit-client native API first; NIXL-consumer integration (Dynamo/vLLM model load) second; KvikIO endpoint third; cuFile integration parked |

## Hypotheses (registered)

Each hypothesis has a pass threshold and a fail threshold; a result between them is reported as inconclusive with the probe's resolving power stated.

| Id | Claim | Pass | Fail |
| --- | --- | --- | --- |
| H-zero | adit puts no copy of the payload in GPU-host memory and uses no GPU-host CPU on the data path | GPU-host memory bandwidth attributable to the transfer < 2% of bytes delivered; GPU-host CPU on the data path < 2 cores at the measured peak | Any host-memory copy of the payload, or > 10% of one core per GB/s |
| H-ceiling | Streaming loaders (path B) are capped by the GPU host's ENA; adit over EFA exceeds that cap | adit single-client cold read ≥ 2× the best path-B result on the same instance, with the gap tracking EFA vs ENA bandwidth | adit ≤ 1.2× path B |
| H-faster | adit beats FSx for Lustre DRA + GDS (path D) on a cold read of a dataset not yet hydrated | time-to-last-byte ≤ 0.5× path D's hydrate-plus-read; steady-state throughput ≥ path D at equal client | adit slower than path D on cold reads |
| H-cheaper | $/TB delivered for a read-once job is an order of magnitude below path D | ≤ 0.1× path D's cost (FSx provisioned hours + hydration) at ≥ 10 TiB | > 0.5× |
| H-scale | Throughput to one client scales with proxy count until the client's EFA ingest or S3's per-prefix rate | ≥ 0.8 linear from 1 to 8 proxies on a p5en/p6 client | Flat beyond 2 proxies with EFA and S3 both unsaturated |
| H-ident | Every path delivers byte-identical GPU buffers | SHA-256 of the GPU buffer equals the object's for every object on every path, every run | Any mismatch (a defect, stops the gate) |

## The five paths under test

Each path is run at its best known configuration, tuned before it is measured, and the configuration is recorded with the result. A path that loses on a physical limit (NIC, S3 first byte, per-prefix rate) is a documented tie; any other loss gets a mechanism and an issue.

| Path | Mechanism | Run at its best as |
| --- | --- | --- |
| A. Copy, then upload | `s5cmd` (best copier found in lith) to instance NVMe, then read and `cudaMemcpy` (pinned staging) | s5cmd tuned concurrency, NVMe RAID0 where multiple devices, pinned-memory staging, 8 MiB chunks |
| B. Streaming loader | S3 to pinned host buffer to GPU, no disk: KvikIO `RemoteFile` and the S3 Connector for PyTorch; Run:ai Model Streamer for the model-load workload | KvikIO thread pool and task size swept; connector with CRT; threads = vCPUs |
| C. FUSE mount + cuFile compat | lith and Mountpoint mounts read through cuFile in compatibility mode (POSIX read + bounce) | lith defaults from its v0.2 matrix; Mountpoint with `--maximum-throughput-gbps` set to the NIC |
| D. FSx for Lustre DRA + GDS | Persistent 2, 1000 MB/s/TiB, EFA enabled, data repository association to the bucket, lazy load, read via cuFile with GDS | File system sized to the working set; `--optimized-for-gds` client setup; cold = fresh DRA, warm = after hydration; metadata import time recorded separately |
| E. adit | Proxy fleet, NIXL LIBFABRIC RDMA write into registered HBM | Fleet width swept 1 to 8; chunk size swept 4 to 16 MiB; stripe on/off |

Every row records: wall-clock to last byte, throughput, GPU-host CPU-seconds, GPU-host memory traffic attributable to the transfer, bytes pulled from S3, S3 request count, and dollars (instance-hours for every box involved, FSx provisioned hours, S3 requests and retrieval) from the Price List API.

## Workloads

Headline numbers come only from real workloads where the bytes reach HBM and are used by a GPU program whose output is checked; toy workloads calibrate the paths and find defects. Every workload has an oracle: path A's result is the reference, and every other path must match it byte for byte (buffer SHA-256) or output for output.

**Toy (calibration, all five paths).**

| Id | Shape | Purpose |
| --- | --- | --- |
| T1 | 1 object, 64 GiB, random bytes | Single-stream ceiling per path; stripe scaling for adit |
| T2 | 1,000 objects × 1 GiB | Fan-out, prefix spreading, completion accounting |
| T3 | 100,000 objects × 16 MiB | Small-object regime; where S3 first byte dominates; manifest prefetch target |
| T4 | 8 clients × T1 concurrently | Fleet contention; per-client fairness |

**Real (headline, all five paths).**

| Id | Workload | Data | GPU consumer and check |
| --- | --- | --- | --- |
| R1 | Model weight load | A 70B-class safetensors checkpoint (\~140 GB BF16) staged once into the bench bucket; license recorded in the issue | vLLM or a plain safetensors loader on the client; load time to first token; logits on a fixed prompt identical across paths |
| R2 | Training data ingestion, one epoch | Public imagery shards in us-east-1 (SpaceNet, as lith's S4 planned) packed as WebDataset tars | PyTorch dataloader feeding a GPU model; samples/s; per-sample checksum stream identical across paths |
| R3 | GPU analytics over columnar data | A RODA Parquet dataset in us-east-1 (NYC TLC or equivalent; region verified via x-amz-bucket-region) | cuDF via KvikIO; a fixed aggregation whose result matches path A |
| R4 | Genomics large-object read | 1000 Genomes phase3 CRAMs (lith's heterogeneous 8-file set, 3.8–14 GB) | A GPU-side hash/scan kernel over the buffer; CRAM bytes identical; the lith comparison point |
| R5 | Checkpoint write (later gate) | Synthetic 140 GB state dict from HBM | Multipart object identical to a path-A write |

Run each real workload cold (first touch; for D a fresh DRA) and warm (second pass; for E with and without the NVMe cache tier once it exists), on the same client instance, same AMI, same day.

## Gates

Stop and report after each gate; Scott sequences the next. Budgets are proposals for Scott to set.

| Gate | Exit criterion | Boxes | Proposed budget |
| --- | --- | --- | --- |
| G0 Feasibility probes | (a) NIXL LIBFABRIC RDMA write from a GPU-less EFA instance into CUDA memory on a GPU instance succeeds and is verified by GPU-side hash; (b) capability matrix measured, not assumed: `fi_info -p efa`, `nixlbench --backend LIBFABRIC` VRAM, `gdscheck -p` on g6.12xlarge, g6e.12xlarge, g7.12xlarge, g7e.12xlarge, p5.48xlarge; (c) single proxy to single client throughput and the zero-copy counters (H-zero instrumentation) | c8gn proxy + g7e.12xlarge client; probes on the listed boxes, one hour each | $60 |
| G1 Correctness | adit-client reads T1 and T2 with SHA-256 identity on every object, 10 runs, including out-of-order and partial-failure injection; CI green on the Rust and Go trees | same as G0 | $40 |
| G2 Scaling | Stripe ladder 1/2/4/8 proxies on a p5en.48xlarge client; H-scale and H-ceiling answered; chunk and concurrency sweeps recorded | c8gn.48xlarge × 8, p5en.48xlarge × 1 | $150 |
| G3 The five paths | Toy T1–T4 and real R1–R4 on all five paths, cold and warm, with dollars; H-faster, H-cheaper, H-ident answered; results in `bench/results/paths-v0.csv` with a `config` and `commit` column | g7e.12xlarge for the full matrix, p5en.48xlarge for the headline rows; one FSx P2-1000 file system per cold run | $500 |
| G4 Levers (optional, one issue each) | Zero-copy receive on the proxy; manifest prefetch; NVMe cache tier; write path (R5); KvikIO endpoint; NIXL-consumer model load through Dynamo | as needed | per lever |

G0 and G1 are a Claude Code session each; G2 and G3 are run only after Scott reads the G1 report.

## Instance capability matrix

The cheapest client with both NIXL-over-EFA GPUDirect RDMA and FSx GDS is a multi-GPU G7e, so paths D and E can be measured on one box; G0 tests the G6/G6e rows because AWS documents neither capability for them. "Documented" means an AWS page says so; everything else is measured in G0.

| Family and sizes | EFA | GPUDirect RDMA to GPU memory (adit / NIXL) | FSx for Lustre GDS (path D) | Note |
| --- | --- | --- | --- | --- |
| g6 8xl, 12xl, 24xl, 48xl | yes on larger sizes | not documented by AWS; a re:Post answer claims 8xl/12xl; measure | not listed | cheapest probe |
| g6e 12xl (100 Gb/s), 24xl (200), 48xl (400) | yes | not documented; Dynamo's EKS guide uses g6e with TCP, not EFA; measure | not listed |  |
| g7 12xl (175 Gb/s), 24xl (350), 48xl (700) | yes | [documented](https://aws.amazon.com/blogs/aws/announcing-amazon-ec2-g7-instances-accelerated-by-nvidia-rtx-pro-4500-blackwell-server-edition-gpus/) (multi-GPU sizes) | documented | needs NVIDIA driver ≥ 595 |
| g7e 12xl and up (to 1,600 Gb/s) | yes | [documented](https://aws.amazon.com/ec2/instance-types/g7e/); Dynamo's EKS example targets g7e.12xlarge for NIXL LIBFABRIC over EFA | [documented](https://aws.amazon.com/blogs/aws/announcing-amazon-ec2-g7e-instances-accelerated-by-nvidia-rtx-pro-6000-blackwell-server-edition-gpus/), up to 1.2 Tb/s | primary G0–G3 client |
| g7 / g7e 4xl, 8xl (single GPU) | up to 100 Gb/s | no (no P2P, GDR listed for multi-GPU only) | no |  |
| p5.4xlarge | 100 Gb/s | [no](https://aws.amazon.com/ec2/instance-types/p5/) | no |  |
| p5 / p5e / p5en 48xl (3,200 Gb/s) | yes | yes | [yes](https://docs.aws.amazon.com/fsx/latest/LustreGuide/efa-file-systems.html) | headline client (p5en) |
| p6-b200 (3,200), p6-b300 (6,400) | yes | yes | yes | 300 Gb/s dedicated ENA on B300 makes H-ceiling sharpest here |
| p6e-gb200 | yes | yes | EFA configured, GDS not in the FSx list | arm64 Grace: libfabric 64K-page patch needed for `fi_mr_reg` on VRAM |

Proxies: c8gn (600 Gb/s) or c7gn (200 Gb/s) for bandwidth, i8g or i7ie when the NVMe cache tier is measured; all Graviton, all EFA, no GPU. The proxy registers host memory only, so the arm64 VRAM registration issue does not apply to it.

## Laws (copied into CLAUDE.md)

1. **Zero copies is measured, never asserted.** Every adit run reports GPU-host memory traffic and CPU-seconds attributable to the transfer, from counters (perf, `/proc`, NIXL stats), beside the throughput. A number without its counters is not a result.
2. **Oracle.** Every path delivers the object's bytes to the GPU buffer; SHA-256 computed on the GPU equals the object's. Any mismatch is a defect that stops the gate.
3. **Baseline fairness.** Each competing path runs at its best known configuration, tuned before it is measured, with the configuration in the results row. Mountpoint gets its NIC flag; FSx gets the 1000 tier and the GDS client setup; KvikIO gets its thread pool swept.
4. **Real workloads.** Headline numbers come only from R1–R4 on public or license-recorded data; toy workloads calibrate and find defects.
5. **Behind means a physical tie or an issue.** Every cell where adit is behind a competitor gets a mechanism: a physical limit (client EFA, S3 first byte, per-prefix rate, per-connection throughput) is a documented tie; anything else is an issue with the evidence.
6. **Per-lever attribution.** Never report one multiplier alone; decompose into fetch, write, completion and stripe contributions.
7. **Hygiene.** Same AMI for every path on a box; `drop_caches` before every cold rung; instance region equals bucket region checked via x-amz-bucket-region before any I/O; cheapest rung first; logs streamed off the instance; every run has a TTL and a cost\_limit; terminations verified; before trusting a null result, show the probe could have resolved the effect.
8. **Authority.** Scott decides scope, names, versions, releases, merges to `main`, and when anything is done. Claude Code opens PRs and reports state and outcome; it may squash-merge PRs touching only `hack/`, `bench/`, `.github/`, docs and tests once CI is green (the lith rule); crates and `cmd/` wait for Scott.
9. **No advocacy.** Results are stated as numbers with their configuration; where adit loses, the row says so.

## Risks and open questions

| Risk | Effect | How G0 settles it |
| --- | --- | --- |
| NIXL LIBFABRIC on a GPU-less sender writing into a remote GPU buffer is a less-travelled combination than GPU-to-GPU | The proxy side may need DRAM-to-VRAM specific handling | G0(a) is exactly this transfer, checked by GPU-side hash |
| EFA generation differences in one-sided RDMA write support | Older EFA gens expose RDMA read but not write; push would have to become pull | `fi_info` capability flags per box in G0(b); fall back to client-initiated NIXL READ if write is absent |
| NIXL descriptor and metadata size in an HTTP header | Large registrations or many descriptors may not fit a header | Carry them in the request body; measured in G1 |
| S3 per-prefix request rate and first-byte latency | Small-object workloads (T3, R2) stall without prefetch | Manifest prefetch is a G4 lever; T3 cold is reported as the floor, not hidden |
| G6/G6e GPUDirect RDMA unverified | Cheapest boxes may not run adit at all | Measured in G0(b); the matrix records the answer either way |
| arm64 (Grace) client VRAM registration under libfabric | p6e-gb200 excluded or patched | Out of scope until G3 unless Scott asks |
| FSx EFA/GDS client setup drift | Path D mis-measured low | Use AWS's `--optimized-for-gds` setup script; record driver and client versions in the row |
| Cost of G2/G3 (p5en, FSx P2-1000 at 4.8 TiB minimum) | Budget overrun | Stop limits per gate; cheapest rung first; FSx created and deleted inside the timed window |
| `mountpoint-s3-crt` API stability as the CRT binding | Build breaks on upgrade | Pin the crate; the fetch library hides it |

## Repository layout and mechanization

```
adit/
  crates/
    adit-fetch/      Rust: S3 ranged-GET engine (CRT), prefix spreading, manifests; shared with lith later
    adit-proxy/      Rust: HTTP control endpoint, NIXL agent, RDMA write pipeline
    adit-client/     Rust: GPU buffer registration, request, completion; C ABI
    adit-py/         PyO3 binding over adit-client
  cmd/aditctl/       Go: fleet launch/size/retire via spore.host, manifests, presign, metrics, bench runner
  internal/          Go packages for aditctl
  bench/
    paths/           one runner per path A–E, same CLI, same CSV schema
    workloads/       T1–T4, R1–R5 definitions and oracles
    results/         CSVs with config and commit columns
  hack/              devbox.sh, fleet.sh, drop-caches, region check
  .github/workflows/ Rust (fmt, clippy, test), Go (vet, lint, test), bench-schema check
  CLAUDE.md          the laws
```

Everything repeatable is a `make` target: `make build`, `make test`, `make probe` (G0 matrix), `make fleet N=4`, `make bench PATH=E WORKLOAD=T1`, `make report` (CSV to the comparison table), `make teardown` (verifies no instances remain). Claude Code uses subagents for independent work: one per path runner in G3, one per capability probe in G0, and a separate reviewer agent that has not seen the code to check results against the laws before a gate report.

## Sources

- [NIXL Libfabric backend](https://docs.nvidia.com/nixl/user-guide/backend-selection/libfabric/)
- [nixl-sys 1.2.0 (Rust bindings)](https://docs.rs/crate/nixl-sys/1.2.0)
- [Dynamo on EKS with EFA (NIXL LIBFABRIC, g7e.12xlarge)](https://docs.nvidia.com/dynamo/kubernetes-deployment/cloud-provider-guides/aws/eks-setup)
- [Dynamo EFA on EKS: LIBFABRIC required for EFA line rate](https://docs.nvidia.com/dynamo/dev/kubernetes/installation/rdma-setup/efa-on-aws)
- [Amazon EC2 G7e instances](https://aws.amazon.com/ec2/instance-types/g7e/)
- [Announcing EC2 G7 instances](https://aws.amazon.com/blogs/aws/announcing-amazon-ec2-g7-instances-accelerated-by-nvidia-rtx-pro-4500-blackwell-server-edition-gpus/)
- [Announcing EC2 G7e instances](https://aws.amazon.com/blogs/aws/announcing-amazon-ec2-g7e-instances-accelerated-by-nvidia-rtx-pro-6000-blackwell-server-edition-gpus/)
- [Amazon EC2 P5 instances](https://aws.amazon.com/ec2/instance-types/p5/)
- [FSx for Lustre: EFA-enabled file systems and GDS](https://docs.aws.amazon.com/fsx/latest/LustreGuide/efa-file-systems.html)
- [FSx for Lustre: configuring EFA clients](https://docs.aws.amazon.com/fsx/latest/LustreGuide/configure-efa-clients.html)
- [FSx for Lustre EFA/GDS launch](https://aws.amazon.com/blogs/aws/amazon-fsx-for-lustre-unlocks-full-network-bandwidth-and-gpu-performance/)
- [FSx for Lustre pricing per tier (LUG 2023)](https://wiki.lustre.org/images/1/13/LUG2023-Amazon_Lustre_FSX-Osborne.pdf)
- [NVIDIA cuObject client release notes](https://docs.nvidia.com/gpudirect-storage/cuobject/cuobject-client-release-notes/index.html)
- [S3 Express One Zone price reductions](https://aws.amazon.com/ko/blogs/aws/up-to-85-price-reductions-for-amazon-s3-express-one-zone/)
- [KvikIO remote I/O](https://developer.nvidia.com/blog/high-performance-remote-io-with-nvidia-kvikio)
- [S3 Connector for PyTorch](https://github.com/awslabs/s3-connector-for-pytorch)
