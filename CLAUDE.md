# adit — working rules

adit moves bytes from Amazon S3 into NVIDIA GPU memory on EC2 with zero copies on the GPU host:
GPU-less EFA proxies fetch S3 over ENA and RDMA-write into the client's registered HBM through
NIXL's LIBFABRIC backend. The project document and the pinned `design` issue hold the
architecture, protocol, hypotheses, paths and workloads; this file holds the rules.

**The one test.** Does this help answer: on AWS, can S3-resident data reach GPU memory with zero
copies on the GPU host, faster and cheaper than every current path — including FSx for Lustre
with GDS — and by how much on real workloads? Build what the gates need and nothing else.

## Laws

1. **Zero copies is measured, never asserted.** Every adit run reports GPU-host memory traffic and
   CPU-seconds attributable to the transfer, from counters (perf, `/proc`, NIXL stats), beside
   throughput. A number without its counters is not a result.
2. **Oracle.** Every path delivers the object's bytes to the GPU buffer; SHA-256 computed on the
   GPU equals the object's. Any mismatch is a defect that stops the gate.
3. **Baseline fairness.** Each competing path runs at its best known configuration, tuned before
   measured, configuration recorded in the results row.
4. **Real workloads.** Headline numbers only from R1–R4; toy workloads calibrate and find defects.
5. **Behind means a physical tie or an issue.** Every cell where adit is behind gets a mechanism:
   physical limit (client EFA, S3 first byte, per-prefix rate, per-connection throughput) is a
   documented tie; anything else is an issue with evidence — the measured gap and the
   competitor's number as the target.
6. **Per-lever attribution.** Never report one multiplier alone; decompose into fetch, write,
   completion, stripe.
7. **Hygiene.** Same AMI per box for every path; `drop_caches` before every cold rung (between
   rungs, not just the first); region check before I/O; cheapest rung first; logs streamed off
   the instance; `set +e` explicit, `$-` echoed; before trusting a null result, show the probe
   could have resolved the effect.
8. **Authority.** Scott decides scope, names, versions, releases, merges to `main`, and when
   anything is done. Report state and outcome only; never declare completion.
9. **No advocacy.** Numbers with their configuration; where adit loses, the row says so.

## Fixed decisions

- **Languages.** Rust (current stable) for `crates/`: `nixl-sys =1.2.0`, `mountpoint-s3-crt` for
  the CRT S3 client, PyO3 for Python. Go (current stable, no cgo) for `cmd/aditctl` and
  `internal/`. Third-party tools on paths A–D are driven by scripts in `bench/paths/`.
- **Versioning.** SemVer 2.0.0. No tags, no releases, no version numbers chosen by Claude Code.
  Crate and module versions stay `0.0.0` until Scott sets them.
- **Changelog.** Keep a Changelog 1.1.0, `[Unreleased]` only. Every PR adds its entry.
- **Planning.** GitHub Projects, milestones G0–G4, issues, labels. Decisions and corrections go in
  issue comments, not chat. No local plan, status or TODO files. Allowed local files: README,
  LICENSE, CHANGELOG, CLAUDE.md, code, tests, scripts, `hack/`, `bench/`.
- **Merges.** Open PRs. Claude Code may squash-merge a PR touching only `hack/`, `bench/`,
  `.github/`, docs and tests once CI is green. Anything under `crates/` or `cmd/` waits for Scott.

## Compute

- **All AWS runs go through spore.host** (`truffle` → `spawn` → `spored`), `AWS_PROFILE=aws`,
  `--region us-east-1` always passed (spawn otherwise picks the "lowest-latency" region). No raw
  `aws ec2` launches. One committed bootstrap script per role, one spawn invocation per box.
- **Every launch has `--ttl` and `--cost-limit`.** `--cost-limit` is a hard runtime kill, not a
  launch guard: size it as worst-case minutes / 60 × $/hr plus headroom. Size the TTL from a
  measured run (about 2× wall), then retighten.
- **Verify terminations with `spawn list`** after every session; never touch instances you did not
  launch. `make teardown` asserts none of ours remain.
- **Placement.** Proxies and clients in one AZ, one subnet, one self-referencing all-traffic
  security group (EFA requirement). Check `x-amz-bucket-region` equals the instance region
  before any I/O.
- **Bench bucket.** `s3://scttfrdmn-lith-bench` (7-day expiry) for anything written. Create no
  buckets.
- **spawn gotchas.**
  - `spawn launch --command` runs under `bash -e`; `set -uo pipefail` does not clear `-e`.
    Start every on-box script with `set +e; set -uo pipefail; echo "opts=$-"` and check statuses
    by hand. `zcat | head`, `wait $pid` and arithmetic on a failing `$(…)` die under `-e`.
  - Under `set -u` use `"${2-}"` for optional arguments.
  - `spawn connect` block-buffers piped stdout. Run long work with `bgrun` (installed by
    `hack/devbox.sh`) and poll its output file; `--tty` merges stdout and stderr.
  - `spawn launch --command` does not stage logs out: push results to the bench bucket after
    every step and in an `EXIT` trap.
  - `spawn extend` on Ubuntu: follow with `spawn connect <box> --user ubuntu -- sudo spored reload`.
  - `spawn cost` errors on a single instance; compute cost as Price List rate × wall.

## Measurement

- **No stub ever produces a number.** `nixl-sys` silently falls back to its stub API when NIXL is
  missing at build time. Builds on instances set `NIXL_NO_STUBS_FALLBACK=1`, and every binary
  checks `nixl_sys::is_stub()` at startup and refuses to measure if it is true.
- **Diagnostics exercise production paths.** Probes and benches call the same fetch engine,
  pipeline and client as the product, not a reimplementation.
- **Read the configuration back from the running process** (`/metrics`), don't trust the flag.
- **Reconcile against a neutral instrument** where one exists (CloudTrail `GetObject` counts, S3
  stored checksums), and read any self-made cross-check as adversarially as the measurement.
- **Cost every run** from the Price List API: every box's instance-hours, FSx provisioned hours,
  S3 requests and retrieval, as if we owned the bucket.
- **Results** live in `bench/results/*.csv`: `#` comment lines (instance, region, AMI, commit,
  flags, prices, data source), then a header row from `bench/schema/`, ending in `config`,
  `commit`, `notes`. `make schema-check` validates them.

## Reporting

A gate report has: what ran, results tables with configuration, hypotheses touched (pass / fail /
inconclusive with the probe's resolving power), behind-cells with mechanism or issue, changes from
design (for objection), needs-decision, compute (instances, $/hr, TTL, cost-limit, spend, teardown
verified via `spawn list`), open issues, unpushed commits. Before posting, a reviewer agent that
has not seen the code checks the report against the laws. Nothing is declared done — the call is
Scott's.
