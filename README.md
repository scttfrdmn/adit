# adit

S3 straight into GPU memory on EC2, with zero copies on the GPU host.

`adit` is a fleet of EFA-attached, GPU-less proxy instances. Each proxy fetches
ranges of an S3 object over ENA and RDMA-writes them into the client's registered
GPU buffer through [NIXL](https://github.com/ai-dynamo/nixl)'s LIBFABRIC backend.
The GPU host sees one PCIe traversal (NIC to GPU): no TCP, no TLS, no bounce
buffer, and no CPU on the data path.

**Why "adit"?** An adit is a horizontal passage driven straight into the seam: the
ore comes out level, not hauled up and lowered again.

**The question adit answers.** On AWS, can S3-resident data reach GPU memory faster
and cheaper than every current path — including FSx for Lustre with GPUDirect
Storage — and by how much on real workloads? The answer is measured against
registered hypotheses with pass and fail thresholds, on five paths run at their
best known configuration. Where adit loses, the results say so.

**Status: pre-release, under measurement.** Nothing here is a result until it is in
`bench/results/` with its configuration, counters and commit.

## What it is

- **One GET, two channels.** The control plane is an ordinary HTTP
  `GET /{bucket}/{key}` to a proxy, carrying a presigned S3 URL and a NIXL
  descriptor for the client's GPU buffer. The data plane is NIXL RDMA writes from
  the proxy into that buffer. The HTTP response is bodiless and carries the byte
  count.
- **Push, not pull.** The proxy initiates every write, at the offset the range
  belongs at, as the range lands. S3 ranges and SRD both complete out of order;
  one-sided writes carry their destination, so nothing is reassembled anywhere.
- **No credentials on the proxy.** The proxy reads only what the client's presigned
  URL lets it read.
- **Striping.** One object can be split by range across N proxies, all writing into
  the same buffer, until the client's EFA ingest is the limit.
- **Read-only over native key layout.** No filesystem, no POSIX. The sibling project
  [lith](https://github.com/scttfrdmn/lith) is the POSIX face of the same data; the
  two share the S3 fetch engine (`crates/adit-fetch`).

## Layout

| Path | What |
| --- | --- |
| `crates/adit-fetch` | Rust: S3 ranged-GET engine on the AWS CRT; no NIXL |
| `crates/adit-proxy` | Rust: HTTP control endpoint, NIXL agent, RDMA write pipeline |
| `crates/adit-client` | Rust: GPU buffer registration, requests, completion; C ABI |
| `crates/adit-py` | PyO3 binding over `adit-client` |
| `cmd/aditctl`, `internal/` | Go: fleet, presign, probes, bench runner, report |
| `bench/` | path runners A–E, workload definitions and oracles, results |
| `hack/` | instance bootstrap and fleet scripts |

## Build

```sh
make build   # Rust workspace (NIXL stub API) and aditctl
make test
make lint
```

On an instance with NIXL installed at `/opt/nvidia/nvda_nixl`, `make build-real`
builds against the real library and refuses to fall back to the stub.

## License

Apache 2.0. See [LICENSE](LICENSE).
