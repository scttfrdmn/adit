# SPDX-License-Identifier: Apache-2.0
"""GPU loaders for paths B, C and D: the same window, oracle and row code for every path.

Modes:
- `remotefile`: KvikIO RemoteFile over the presigned HTTPS URL, straight into a CuPy device buffer.
  This is path B's streaming loader.
- `s3connector`: the S3 Connector for PyTorch reads into host memory, then `.cuda()`. This is path
  B's second loader.
- `cufile`: KvikIO CuFile over a filesystem path. In compatibility mode (POSIX read + bounce) it is
  path C on a FUSE mount; with GDS it is path D on FSx for Lustre.

Every run opens one `adit.Window` (the same counters the Rust runners use) around the reads only.
Every object is then checked on the GPU (`adit.Gpu.sha256_parts` + `adit.check` against the
manifest), and one row in `bench/schema/paths.csv` order is appended.

    python3 bench/paths/lib/gpuload.py --mode remotefile --plan plan.jsonl --meta path=B ...
"""

import argparse
import csv
import json
import os
import sys
import time


def schema(path):
    with open(path) as f:
        for line in f:
            if line.strip() and not line.startswith("#"):
                return line.strip().split(",")
    raise SystemExit(f"no header in {path}")


def load_lines(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", required=True, choices=["remotefile", "s3connector", "cufile"])
    ap.add_argument("--plan", help="manifest JSONL with presigned `url` (remotefile)")
    ap.add_argument("--manifest", help="manifest JSONL (s3connector, cufile)")
    ap.add_argument("--dir", help="file root: object key K is DIR/K (cufile)")
    ap.add_argument("--region", default="us-east-1")
    ap.add_argument("--device", type=int, default=0)
    ap.add_argument("--threads", type=int, default=0, help="KvikIO thread pool (0 = library default)")
    ap.add_argument("--task-size-mib", type=int, default=0, help="KvikIO task size (0 = default)")
    ap.add_argument("--compat", choices=["on", "off", "auto"], default="auto", help="KvikIO compat mode")
    ap.add_argument("--concurrency", type=int, default=1, help="objects in flight")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--buffer-gib", type=int, default=80, help="GPU memory per wave; objects are loaded in waves that fit")
    ap.add_argument("--schema", default="bench/schema/paths.csv")
    ap.add_argument("--meta", action="append", default=[], help="k=v fixed columns")
    ap.add_argument("--out")
    a = ap.parse_args()

    # KvikIO reads its configuration at import time.
    if a.compat != "auto":
        os.environ["KVIKIO_COMPAT_MODE"] = "ON" if a.compat == "on" else "OFF"
    if a.threads:
        os.environ["KVIKIO_NTHREADS"] = str(a.threads)
    if a.task_size_mib:
        os.environ["KVIKIO_TASK_SIZE"] = str(a.task_size_mib << 20)

    import cupy as cp
    import adit  # adit-py: the GPU oracle and counters
    from concurrent.futures import ThreadPoolExecutor

    cp.cuda.Device(a.device).use()
    gpu = adit.Gpu(a.device)
    objs = load_lines(a.plan or a.manifest)
    cols = schema(a.schema)

    if a.mode == "remotefile":
        import kvikio

        def read(o, buf):
            # KvikIO's own presigned-URL endpoint; pass the size so it does not probe the object
            # with a request the GET-only signature does not cover (403).
            with kvikio.RemoteFile.open_s3_presigned_url(o["url"], nbytes=o["size"]) as f:
                return f.read(buf)
    elif a.mode == "cufile":
        import kvikio

        def read(o, buf):
            with kvikio.CuFile(os.path.join(a.dir, o["key"]), "r") as f:
                return f.read(buf)
    else:
        import torch
        from s3torchconnector import S3ReaderConstructor  # noqa: F401  (version check)
        from s3torchconnector._s3client import S3Client

        client = S3Client(region=a.region)

        def read(o, buf):
            r = client.get_object(o["bucket"], o["key"])
            host = torch.frombuffer(bytearray(r.read()), dtype=torch.uint8)
            # Copy host -> the oracle's device buffer directly (no second device allocation).
            torch.as_tensor(buf, device=f"cuda:{a.device}").copy_(host)
            return host.numel()

    out = open(a.out, "a", newline="") if a.out else None
    if out and out.tell() == 0:
        out.write(",".join(cols) + "\n")
    print(",".join(cols))
    bad_any = False
    # Pack objects into waves that fit the GPU budget; each wave is verified and freed before the
    # next, and only the reads are inside the counter window.
    cap = a.buffer_gib << 30
    waves, cur, used = [], [], 0
    for o in objs:
        if o["size"] > cap:
            raise SystemExit(f"{o['key']} ({o['size']} bytes) exceeds --buffer-gib")
        if used + o["size"] > cap:
            waves.append(cur)
            cur, used = [], 0
        cur.append(o)
        used += o["size"]
    if cur:
        waves.append(cur)

    def merge(acc, m):
        for k in ("host_cpu_s", "proc_cpu_s", "host_mem_read_bytes", "host_mem_write_bytes", "efa_rx_bytes"):
            acc[k] = None if acc.get(k, 0) is None or m[k] is None else acc.get(k, 0) + m[k]
        acc["perf_note"] = m["perf_note"] or acc.get("perf_note", "")

    for run in range(1, a.runs + 1):
        secs, m, notes, bad = 0.0, {}, [], 0
        for wave in waves:
            bufs = [cp.zeros(o["size"], dtype=cp.uint8) for o in wave]
            cp.cuda.Device(a.device).synchronize()
            w = adit.Window("")
            t0 = time.monotonic()
            with ThreadPoolExecutor(max_workers=a.concurrency) as ex:
                got = list(ex.map(lambda ob: read(ob[0], ob[1]), zip(wave, bufs)))
            cp.cuda.Device(a.device).synchronize()
            secs += time.monotonic() - t0
            merge(m, w.stop())
            for o, b, n in zip(wave, bufs, got):
                if n != o["size"]:
                    notes.append(f"{o['key']}: read {n} of {o['size']}")
                    bad += 1
                    continue
                ok, comp, badparts = adit.check(json.dumps({k: o[k] for k in o if k != "url"}),
                                                gpu.sha256_parts(b.data.ptr, o["size"], o["part_size"]))
                if not ok:
                    bad += 1
                    notes.append(f"MISMATCH {o['key']} parts {badparts[:4]}")
            # Drop every reference (the loop variable `b` too) before freeing, or the next wave
            # (or run) allocates on top of this one.
            b = None
            bufs = None
            got = None
            cp.get_default_memory_pool().free_all_blocks()
        bad_any |= bad > 0
        total = sum(o["size"] for o in objs)
        row = {c: "" for c in cols}
        for kv in a.meta:
            k, v = kv.split("=", 1)
            if k not in row:
                raise SystemExit(f"--meta {k}: not a schema column")
            row[k] = v
        row.update({
            "run": run, "objects": len(objs), "bytes": total, "concurrency": a.concurrency,
            "wall_s": f"{secs:.3f}", "gbps": f"{total * 8 / secs / 1e9:.3f}",
            "host_cpu_s": f"{m['host_cpu_s']:.2f}" if m["host_cpu_s"] is not None else "unavailable",
            "proc_cpu_s": f"{m['proc_cpu_s']:.2f}" if m["proc_cpu_s"] is not None else "unavailable",
            "host_mem_read_bytes": m["host_mem_read_bytes"] if m["host_mem_read_bytes"] is not None else "unavailable",
            "host_mem_write_bytes": m["host_mem_write_bytes"] if m["host_mem_write_bytes"] is not None else "unavailable",
            "efa_rx_bytes": m["efa_rx_bytes"] if m["efa_rx_bytes"] is not None else 0,
            "oracle": "MISMATCH" if bad else "ok",
            "config": f"mode={a.mode};compat={a.compat};threads={a.threads or 'default'};"
                      f"task_size_mib={a.task_size_mib or 'default'};concurrency={a.concurrency};waves={len(waves)}",
            "notes": "; ".join(notes + ([m["perf_note"]] if m["perf_note"] else [])),
        })
        line = ",".join(f'"{v}"' if "," in str(v) else str(v) for v in (row[c] for c in cols))
        print(line)
        if out:
            out.write(line + "\n")
            out.flush()
    sys.exit(1 if bad_any else 0)


if __name__ == "__main__":
    main()
