# SPDX-License-Identifier: Apache-2.0
"""Path D: stamp the row gpuload.py just appended with the FSx configuration and the GDS evidence.

gpuload.py writes its own `config` and `notes`; this appends path D's knobs to `config` and the
per-pass GDS verdict to `notes`, in place. A row is "new" while its config lacks `fsx_state=`.
GDS evidence is the nvidia-fs read MiB delta across the pass against the bytes read: below 99% means
some bytes did not go through GDS, and the row says so. Likewise efa_rx_bytes below the bytes read
means LNet did not carry the transfer over EFA. If no row was appended (gpuload failed
before writing), a `#` comment records the failure.
"""

import argparse
import csv
import io


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--rc", type=int, required=True)
    ap.add_argument("--gds", required=True)
    ap.add_argument("--gds-why", default="")
    ap.add_argument("--nvfs-before", default="unknown")
    ap.add_argument("--nvfs-after", default="unknown")
    ap.add_argument("--config", required=True)
    a = ap.parse_args()

    with open(a.out) as f:
        lines = f.read().splitlines()
    header_i = next(i for i, l in enumerate(lines) if l and not l.startswith("#"))
    cols = next(csv.reader([lines[header_i]]))
    ci, ni, bi, ei = cols.index("config"), cols.index("notes"), cols.index("bytes"), cols.index("efa_rx_bytes")
    touched = 0
    for i in range(header_i + 1, len(lines)):
        if not lines[i] or lines[i].startswith("#"):
            continue
        row = next(csv.reader([lines[i]]))
        if "fsx_state=" in row[ci]:
            continue
        nbytes = int(row[bi] or 0)
        try:
            delta = int(a.nvfs_after) - int(a.nvfs_before)
            want = nbytes / (1 << 20)
            frac = delta / want if want else 0.0
            ev = f"nvfs_read_mib_delta={delta}"
            verdict = "gds_bytes=ok" if frac >= 0.99 else f"GDS NOT USED for {100 * (1 - frac):.1f}% of bytes"
        except ValueError:
            ev, verdict = "nvfs_read_mib_delta=unavailable", "gds_bytes=unverified (no nvidia-fs counters)"
        row[ci] = f"{row[ci]};{a.config};gds={a.gds};{ev}"
        gds_note = verdict if a.gds == "active" else f"GDS INACTIVE ({a.gds_why}); {verdict}"
        # EFA evidence: Lustre over EFA must show the bytes on the client EFA counters.
        try:
            efa = int(row[ei] or 0)
            efa_note = "" if efa >= 0.99 * nbytes else f"efa_rx {efa} < bytes: Lustre traffic not (all) on EFA"
        except ValueError:
            efa_note = "efa_rx unavailable"
        row[ni] = "; ".join(x for x in (gds_note, efa_note, row[ni]) if x)
        buf = io.StringIO()
        csv.writer(buf, lineterminator="").writerow(row)
        lines[i] = buf.getvalue()
        touched += 1
    if touched == 0:
        lines.append(f"# pass failed before writing a row: gpuload rc={a.rc} gds={a.gds} ({a.gds_why})")
    elif a.rc != 0:
        lines.append(f"# gpuload rc={a.rc} for the row above (oracle mismatch or read error)")
    with open(a.out, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
