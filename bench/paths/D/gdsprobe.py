# SPDX-License-Identifier: Apache-2.0
"""Path D preflight: what KvikIO and cuFile will actually do on this box, as JSON.

Run with KVIKIO_COMPAT_MODE=OFF (run.sh does). Prints compat mode as KvikIO resolved it, whether
cuFile reports GDS available, and whether a CuFile handle on a file under the mount opened in
GDS mode. KvikIO's Python API has moved between releases, so every probe is tolerant and reports
"unknown" rather than guessing (UNVERIFIED against the installed KvikIO until G3).

    KVIKIO_COMPAT_MODE=OFF python bench/paths/D/gdsprobe.py /fsx
"""

import json
import os
import sys


def probe(fn):
    try:
        return fn()
    except Exception as e:  # noqa: BLE001 - every failure is a recorded fact
        return f"unknown ({type(e).__name__}: {e})"


def main():
    mnt = sys.argv[1] if len(sys.argv) > 1 else "/fsx"
    import kvikio
    import kvikio.defaults as d

    out = {"kvikio": getattr(kvikio, "__version__", "unknown"),
           "env_KVIKIO_COMPAT_MODE": os.environ.get("KVIKIO_COMPAT_MODE"),
           "env_CUFILE_ENV_PATH_JSON": os.environ.get("CUFILE_ENV_PATH_JSON")}

    def compat():
        v = d.compat_mode()
        name = getattr(v, "name", None)  # CompatMode enum (newer) or bool (older)
        if name is not None:
            return name.upper()
        return "ON" if v else "OFF"
    out["compat_mode"] = probe(compat)

    def gds_available():
        cd = kvikio.cufile_driver
        props = getattr(cd, "properties", None)
        if props is not None and hasattr(props, "is_gds_available"):
            return bool(props.is_gds_available)
        return bool(cd.get("is_gds_available"))
    out["gds_available"] = probe(gds_available)

    # one file under the mount, opened the way gpuload.py opens it
    sample = None
    for root, _, files in os.walk(mnt):
        if files:
            sample = os.path.join(root, files[0])
            break
    out["sample"] = sample
    if sample:
        def handle():
            f = kvikio.CuFile(sample, "r")
            try:
                fn = getattr(f, "is_compat_mode_preferred", None)
                return {"compat_mode_preferred": fn() if fn else "unknown"}
            finally:
                f.close()
        out["handle"] = probe(handle)
    print(json.dumps(out, indent=1, default=str))


if __name__ == "__main__":
    main()
