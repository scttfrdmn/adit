# adit

S3 straight into GPU memory on EC2, with zero copies on the GPU host.

`adit` is a fleet of EFA-attached, GPU-less proxy instances. Each proxy fetches
ranges of an S3 object over ENA and RDMA-writes them into the client's registered
GPU buffer through [NIXL](https://github.com/ai-dynamo/nixl)'s LIBFABRIC backend.
By design, the bytes cross the GPU host once, NIC to GPU: no TCP, no TLS, no bounce
buffer and no CPU copy on the GPU host. Whether that holds is measured, not assumed:
every results row carries the GPU host's CPU and memory counters beside its throughput,
or says which were unavailable.

**Why "adit"?** An adit is a horizontal passage driven straight into the seam: the
ore comes out level, not hauled up and lowered again.

**The question adit answers.** On AWS, can S3-resident data reach GPU memory faster
and cheaper than every current path, including FSx for Lustre with GPUDirect
Storage, and by how much on real workloads? The answer is measured against
registered hypotheses with pass and fail thresholds, on five paths, each run at its
best known configuration. Where adit loses, the results say so.

Nothing here is a result until it is in `bench/results/` with its configuration,
counters and commit. The architecture, protocol and hypotheses are in the pinned
[design issue (#1)](https://github.com/scttfrdmn/adit/issues/1). The project's
rules are in [CLAUDE.md](CLAUDE.md), and the work in progress is in the
[issues](https://github.com/scttfrdmn/adit/issues).

## How it works

- **One GET, two channels.** A client registers once with each proxy
  (`PUT /_adit/agents/{name}`, body = its serialized NIXL agent metadata). Each
  object, or each stripe of one, is then an ordinary HTTP `GET /{bucket}/{key}`
  to a proxy, carrying a presigned S3 URL and the destination in the client's GPU
  buffer as `x-adit-*` headers. The data plane is NIXL RDMA writes from the proxy
  into that buffer. The HTTP response has no body and reports the bytes written.
  The wire protocol is documented in `crates/adit-proto`.
- **Push, not pull.** The proxy initiates every write, at the offset the range
  belongs at, as the range lands. S3 ranges and SRD both complete out of order;
  one-sided writes carry their destination, so nothing is reassembled anywhere.
- **Completion needs both channels.** Every chunk write carries a NIXL
  notification. A request is complete only when the notified coverage equals the
  requested range and every proxy involved answered `200`.
- **No credentials on the proxy.** The proxy reads only what the client's presigned
  URL lets it read.
- **Striping.** One object can be split by range across N proxies, all writing into
  the same buffer, until the client's EFA ingest is the limit.
- **Read-only over native key layout.** No filesystem, no POSIX. The sibling project
  [lith](https://github.com/scttfrdmn/lith) is a POSIX face of the same data, and
  one of the file systems path C measures.

## Layout

| Path | What |
| --- | --- |
| `crates/adit-proto` | Rust: the wire protocol (headers, notification format) shared by proxy and client |
| `crates/adit-fetch` | Rust: S3 ranged-GET engine on the AWS CRT; no NIXL |
| `crates/adit-proxy` | Rust: `adit-proxy` (HTTP control endpoint on `:8480`, NIXL agent, RDMA write pipeline, `/metrics`); `adit-fetch-bench` (fetch only, write a no-op); `adit-nixl-probe-init` |
| `crates/adit-client` | Rust: GPU buffer registration, requests, completion, GPU SHA-256 oracle, counters, C ABI; `adit-get` (path E), `adit-load` (path A's GPU half), `adit-nixl-probe-target` |
| `crates/adit-py` | PyO3 binding over `adit-client`: the GPU oracle and counter window for the Python runners |
| `cmd/aditctl`, `internal/` | Go operator tool: workload generation, presign, region check, prices, fleet placement through spawn, teardown check, results schema check (`aditctl help`) |
| `bench/paths/` | path runners A–E and the diagnostics built on them ([bench/paths/README.md](bench/paths/README.md)) |
| `bench/schema/` | CSV schemas for results |
| `bench/results/` | committed results |
| `hack/` | box bootstrap, source staging, placement and session scripts; each documents its use in its header |

## Build and test

```sh
make build         # Rust workspace against the NIXL stub API, and bin/aditctl
make test          # unit and integration tests: no AWS, no GPU
make lint          # cargo fmt --check, clippy -D warnings, go vet, golangci-lint
make schema-check  # bench/results against bench/schema
make help          # every target
```

A stub build compiles and tests but never measures: `adit-proxy`, `adit-get` and
the NIXL probes check `nixl_sys::is_stub()` at startup and refuse to run on a stub
build. On macOS, `make` adds a link shim under `~/.xtmp/adit-macshim` for the stub's
`-lstdc++` and the PyO3 extension.

On instances, adit is built against the real NIXL at `/opt/nvidia/nvda_nixl` with
`NIXL_NO_STUBS_FALLBACK=1`, so a missing NIXL fails the build instead of falling
back to the stub. `make build-real` and `make test-hw` (GPU tests, `#[ignore]`d
elsewhere) do this. `hack/install.sh proxy|client` bootstraps a box: EFA, NIXL at
the commit pinned in the script and in `Cargo.toml`, Rust, the path A/B tools on
clients, and the release binaries in `/usr/local/bin`. The installed versions go to
`/var/lib/adit/versions`.

## Running on AWS

Every launch goes through [spawn](https://github.com/spore-host/spore-host) with a
TTL and a cost limit, as CLAUDE.md requires. `aditctl fleet up` places a fleet's
boxes (`adit-<fleet>-client-<i>`, `adit-<fleet>-proxy-<i>`) in one AZ, one subnet
and one self-referencing security group. `aditctl fleet down` terminates them, and
`make teardown` asserts that no adit instance remains.

A G2 session (#48), from the operator machine at the repository root:

1. `hack/g2-place.sh` lands the GPU client and `proxy-0` of fleet `g2` from a
   ranked list of instance types and regions, and prints `PLACED <region> <type> …`
   or `NONE`.
2. `aditctl fleet up --name g2 --clients 0 --proxy-start 1 --proxies <n>
   --candidates <the client's AZ> …` adds the remaining proxies.
3. `STEPS="refresh health start plans …" hack/g2.sh g2 <proxies>` rebuilds every
   box at `ADIT_REF`, gates each proxy–client pair with `hack/health.sh`, starts the
   proxies and runs the selected steps. The step list is in the script's header.
   Results go to `bench/runs/g2-<utc>/` and to the bench bucket after every step.
4. `aditctl fleet down --name g2`, then `make teardown` and `spawn list`.

**Boxes with no internet route** (`--no-public-ip`) build from source staged in the
bench bucket. `hack/stage-src.sh [ref]` stages a `git archive` of the ref, its
vendored crates and the pinned NIXL source. `hack/stage-wheels.sh`, run once on a
box from the client AMI that has a route, stages the path tools as a wheelhouse.
`OFFLINE=1` makes `hack/g2.sh` and `hack/fetchprobe.sh` stage and build from
there, through `hack/install.sh` with `ADIT_SRC_S3`.

## Results

Runners write to `bench/runs/` (not committed). A results file that is kept goes to
`bench/results/*.csv`: `#` comment lines (instance, region, AMI, commit, flags,
prices, data source), then the header row of its schema in `bench/schema/`, ending
in `config`, `commit` and `notes`. `make schema-check` validates every file. What
each set of results shows is discussed in the gate issues, not here.

## License

Apache 2.0. See [LICENSE](LICENSE).
