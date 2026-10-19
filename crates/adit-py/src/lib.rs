// SPDX-License-Identifier: Apache-2.0
//! `import adit`: the same oracle and counters the Rust runners use, for the Python-driven paths
//! (KvikIO, the S3 Connector for PyTorch, Run:ai Model Streamer), plus the adit client itself.
//!
//! ```python
//! import adit, cupy
//! gpu = adit.Gpu(0)
//! w = adit.Window("")                    # H-zero counters around the transfer
//! ...load into a cupy array `a` ...
//! m = w.stop()
//! ok, composite, bad = adit.check(manifest_json, gpu.sha256_parts(a.data.ptr, a.nbytes, part))
//! ```

use std::sync::Arc;
use std::time::Duration;

use adit_client::counters::Window as RWindow;
use adit_client::gpu::Gpu as RGpu;
use adit_client::nixl::ClientAgent;
use adit_client::oracle::{ObjectManifest, composite};
use adit_proto::Dest;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

#[pyclass(unsendable)]
struct Gpu(RGpu);

#[pymethods]
impl Gpu {
    #[new]
    fn new(device: usize) -> PyResult<Self> {
        RGpu::new(device).map(Gpu).map_err(err)
    }

    /// Per-part SHA-256 (32-byte `bytes` each) of `length` bytes at device pointer `ptr`.
    fn sha256_parts(&self, ptr: u64, length: u64, part: u64) -> PyResult<Vec<Vec<u8>>> {
        Ok(self.0.sha256_parts(ptr, length, part).map_err(err)?.into_iter().map(|d| d.to_vec()).collect())
    }

    fn name(&self) -> String {
        self.0.name()
    }
}

/// `(matches, composite, bad_part_ranges)`.
type CheckResult = (bool, String, Vec<(u64, u64)>);

/// `(matches, composite, bad_part_ranges)` for parts against a manifest (JSON object).
#[pyfunction]
fn check(manifest_json: &str, parts: Vec<Vec<u8>>) -> PyResult<CheckResult> {
    let m: ObjectManifest = serde_json::from_str(manifest_json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    let parts: Vec<[u8; 32]> = parts
        .into_iter()
        .map(|p| p.try_into().map_err(|_| PyValueError::new_err("digest must be 32 bytes")))
        .collect::<PyResult<_>>()?;
    let v = m.check(&parts);
    Ok((v.matches, v.composite, v.bad_parts.into_iter().map(|r| (r.start, r.end)).collect()))
}

#[pyfunction(name = "composite")]
fn py_composite(parts: Vec<Vec<u8>>) -> PyResult<String> {
    let parts: Vec<[u8; 32]> = parts
        .into_iter()
        .map(|p| p.try_into().map_err(|_| PyValueError::new_err("digest must be 32 bytes")))
        .collect::<PyResult<_>>()?;
    Ok(composite(&parts))
}

#[pyclass(unsendable)]
struct Window(Option<RWindow>);

#[pymethods]
impl Window {
    #[new]
    #[pyo3(signature = (perf_events = ""))]
    fn new(perf_events: &str) -> Self {
        Window(Some(RWindow::start(perf_events)))
    }

    /// Close the window: dict of host_cpu_s, proc_cpu_s, efa_rx_bytes, host_mem_read_bytes,
    /// host_mem_write_bytes (None where unavailable) and perf_note.
    fn stop<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let m = self.0.take().ok_or_else(|| PyRuntimeError::new_err("window already stopped"))?.stop();
        let d = PyDict::new(py);
        d.set_item("host_cpu_s", m.host_cpu_s)?;
        d.set_item("proc_cpu_s", m.proc_cpu_s)?;
        d.set_item("efa_rx_bytes", m.efa_rx_bytes)?;
        d.set_item("host_mem_read_bytes", m.mem_read_bytes)?;
        d.set_item("host_mem_write_bytes", m.mem_write_bytes)?;
        d.set_item("perf_note", m.perf_note)?;
        Ok(d)
    }
}

/// The adit client over a caller-owned device buffer.
#[pyclass(unsendable)]
struct Client {
    rt: tokio::runtime::Runtime,
    inner: adit_client::Client,
    _agent: Arc<ClientAgent>,
}

#[pymethods]
impl Client {
    #[new]
    #[pyo3(signature = (agent, proxies, ptr, length, device=0, chunk=8<<20, stripe=false, backend="LIBFABRIC"))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        agent: &str,
        proxies: Vec<String>,
        ptr: u64,
        length: u64,
        device: u64,
        chunk: u64,
        stripe: bool,
        backend: &str,
    ) -> PyResult<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(err)?;
        let a = ClientAgent::new(agent, backend, ptr, length as usize, device, true).map_err(err)?;
        let md = a.metadata().map_err(err)?;
        let cfg = adit_client::Config {
            agent: agent.into(),
            proxies,
            chunk,
            stripe,
            notif_grace: Duration::from_secs(2),
            timeout: Duration::from_secs(3600),
            pump_idle: Duration::from_micros(50),
        };
        let inner = rt.block_on(async {
            let c = adit_client::Client::new(cfg, Dest { addr: ptr, len: length, dev: device }, a.clone())?;
            c.register(&md).await?;
            Ok::<_, adit_client::GetError>(c)
        });
        Ok(Client { inner: inner.map_err(err)?, rt, _agent: a })
    }

    /// Fetch object bytes `[start, end)` into the buffer at `dest_offset`. Raises on partial.
    fn get(
        &self,
        py: Python<'_>,
        path: &str,
        url: &str,
        start: u64,
        end: u64,
        dest_offset: u64,
    ) -> PyResult<(u64, u64)> {
        let r = py.detach(|| self.rt.block_on(self.inner.get(path, url, start..end, dest_offset)));
        r.map(|rep| (rep.bytes, rep.s3_requests)).map_err(err)
    }
}

#[pymodule]
fn adit(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Gpu>()?;
    m.add_class::<Window>()?;
    m.add_class::<Client>()?;
    m.add_function(wrap_pyfunction!(check, m)?)?;
    m.add_function(wrap_pyfunction!(py_composite, m)?)?;
    Ok(())
}
