// SPDX-License-Identifier: Apache-2.0
//! Counters for `/metrics` (Prometheus text). Configuration is exported too, so a results row can
//! record what the running process actually used rather than what a flag said.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Upper bounds of the latency histogram buckets, milliseconds (Prometheus `le`).
pub const BOUNDS_MS: [f64; 14] =
    [0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0];

/// A fixed-bucket latency histogram: per-chunk stage times, so throughput can be decomposed into
/// fetch, write wait and write (law 6) from the running process.
#[derive(Default)]
pub struct Hist {
    buckets: [AtomicU64; BOUNDS_MS.len()],
    count: AtomicU64,
    sum_ns: AtomicU64,
}

impl Hist {
    pub fn observe(&self, d: Duration) {
        let ms = d.as_secs_f64() * 1e3;
        if let Some(i) = BOUNDS_MS.iter().position(|b| ms <= *b) {
            self.buckets[i].fetch_add(1, Relaxed);
        }
        self.count.fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(d.as_nanos() as u64, Relaxed);
    }

    fn render(&self, o: &mut String, name: &str, help: &str) {
        let _ = writeln!(o, "# HELP adit_proxy_{name}_seconds {help}\n# TYPE adit_proxy_{name}_seconds histogram");
        let mut cum = 0;
        for (b, n) in BOUNDS_MS.iter().zip(&self.buckets) {
            cum += n.load(Relaxed);
            let _ = writeln!(o, "adit_proxy_{name}_seconds_bucket{{le=\"{}\"}} {cum}", b / 1e3);
        }
        let count = self.count.load(Relaxed);
        let _ = writeln!(o, "adit_proxy_{name}_seconds_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(o, "adit_proxy_{name}_seconds_sum {}", self.sum_ns.load(Relaxed) as f64 / 1e9);
        let _ = writeln!(o, "adit_proxy_{name}_seconds_count {count}");
    }
}

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub requests_failed: AtomicU64,
    pub chunks_ok: AtomicU64,
    pub chunks_failed: AtomicU64,
    pub bytes_fetched: AtomicU64,
    pub bytes_written: AtomicU64,
    pub s3_requests: AtomicU64,
    pub fetch_ns: AtomicU64,
    pub write_ns: AtomicU64,
    pub first_byte_ns: AtomicU64,
    /// Per chunk: waiting for a free slot, the ranged GET, waiting for a write permit, the write.
    pub h_slot_wait: Hist,
    pub h_fetch: Hist,
    pub h_write_wait: Hist,
    pub h_write: Hist,
}

pub struct ConfigInfo {
    pub backend: String,
    pub chunk_bytes: u64,
    pub slots: u64,
    pub throughput_target_gbps: f64,
    pub max_connections: u64,
    /// Interfaces the CRT binds to, comma-separated; empty for the default route.
    pub network_interfaces: String,
    pub write_inflight: u64,
    pub meta_type: String,
    pub stub: bool,
}

/// CPU seconds (user + system) of this process, from `/proc/self/stat`; `None` off Linux.
pub fn process_cpu_seconds() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    let (utime, stime): (f64, f64) = (f.get(11)?.parse().ok()?, f.get(12)?.parse().ok()?);
    Some((utime + stime) / 100.0)
}

impl Metrics {
    pub fn render(&self, cfg: &ConfigInfo) -> String {
        let mut o = String::new();
        let c = |o: &mut String, name: &str, help: &str, v: u64| {
            let _ =
                writeln!(o, "# HELP adit_proxy_{name} {help}\n# TYPE adit_proxy_{name} counter\nadit_proxy_{name} {v}");
        };
        c(&mut o, "requests_total", "Object GETs served.", self.requests.load(Relaxed));
        c(&mut o, "requests_failed_total", "Object GETs answered 502.", self.requests_failed.load(Relaxed));
        c(&mut o, "chunks_ok_total", "Chunks fetched and written.", self.chunks_ok.load(Relaxed));
        c(&mut o, "chunks_failed_total", "Chunks that failed fetch or write.", self.chunks_failed.load(Relaxed));
        c(&mut o, "bytes_fetched_total", "Payload bytes received from S3.", self.bytes_fetched.load(Relaxed));
        c(&mut o, "bytes_written_total", "Payload bytes RDMA-written to clients.", self.bytes_written.load(Relaxed));
        c(&mut o, "s3_requests_total", "S3 GET requests issued, including retries.", self.s3_requests.load(Relaxed));
        c(&mut o, "fetch_ns_total", "Summed per-chunk fetch time.", self.fetch_ns.load(Relaxed));
        c(&mut o, "write_ns_total", "Summed per-chunk write time.", self.write_ns.load(Relaxed));
        c(&mut o, "first_byte_ns_total", "Summed per-chunk S3 first-byte latency.", self.first_byte_ns.load(Relaxed));
        self.h_slot_wait.render(&mut o, "chunk_slot_wait", "Per chunk: time waiting for a free receive slot.");
        self.h_fetch.render(&mut o, "chunk_fetch", "Per chunk: ranged GET, issue to last body byte.");
        self.h_write_wait.render(&mut o, "chunk_write_wait", "Per chunk: time waiting for a write permit.");
        self.h_write.render(&mut o, "chunk_write", "Per chunk: RDMA write, post to completion.");
        // The proxy's own EFA device counters (errors, retransmits, RDMA bytes), so a client-side
        // runner can snapshot them per arm with the rest of /metrics (#51).
        let efa = efa_counters();
        if !efa.is_empty() {
            let _ = writeln!(
                o,
                "# HELP adit_proxy_efa EFA hw_counters per device (port 1).\n# TYPE adit_proxy_efa counter"
            );
            for (dev, name, v) in efa {
                let _ = writeln!(o, "adit_proxy_efa{{dev=\"{dev}\",counter=\"{name}\"}} {v}");
            }
        }
        if let Some(cpu) = process_cpu_seconds() {
            let _ = writeln!(o, "# TYPE adit_proxy_cpu_seconds_total counter\nadit_proxy_cpu_seconds_total {cpu}");
        }
        let _ = writeln!(
            o,
            "# HELP adit_proxy_config Configuration in force.\n# TYPE adit_proxy_config gauge\n\
             adit_proxy_config{{backend=\"{}\",chunk_bytes=\"{}\",slots=\"{}\",throughput_target_gbps=\"{}\",max_connections=\"{}\",network_interfaces=\"{}\",write_inflight=\"{}\",meta_type=\"{}\",stub=\"{}\"}} 1",
            cfg.backend,
            cfg.chunk_bytes,
            cfg.slots,
            cfg.throughput_target_gbps,
            cfg.max_connections,
            cfg.network_interfaces,
            cfg.write_inflight,
            cfg.meta_type,
            cfg.stub
        );
        o
    }
}

/// `(device, counter, value)` for every readable `hw_counters` file under
/// `/sys/class/infiniband/<dev>/ports/1`; empty off EFA hosts.
fn efa_counters() -> Vec<(String, String, u64)> {
    let mut out = vec![];
    let Ok(devs) = std::fs::read_dir("/sys/class/infiniband") else { return out };
    for d in devs.flatten() {
        let dev = d.file_name().to_string_lossy().into_owned();
        let Ok(cs) = std::fs::read_dir(d.path().join("ports/1/hw_counters")) else { continue };
        for c in cs.flatten() {
            if let Some(v) = std::fs::read_to_string(c.path()).ok().and_then(|t| t.trim().parse::<u64>().ok()) {
                out.push((dev.clone(), c.file_name().to_string_lossy().into_owned(), v));
            }
        }
    }
    out.sort();
    out
}
