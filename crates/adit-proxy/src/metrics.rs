// SPDX-License-Identifier: Apache-2.0
//! Counters for `/metrics` (Prometheus text). Configuration is exported too, so a results row can
//! record what the running process actually used rather than what a flag said.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

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
}

pub struct ConfigInfo {
    pub backend: String,
    pub chunk_bytes: u64,
    pub slots: u64,
    pub throughput_target_gbps: f64,
    pub max_connections: u64,
    pub write_inflight: u64,
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
        if let Some(cpu) = process_cpu_seconds() {
            let _ = writeln!(o, "# TYPE adit_proxy_cpu_seconds_total counter\nadit_proxy_cpu_seconds_total {cpu}");
        }
        let _ = writeln!(
            o,
            "# HELP adit_proxy_config Configuration in force.\n# TYPE adit_proxy_config gauge\n\
             adit_proxy_config{{backend=\"{}\",chunk_bytes=\"{}\",slots=\"{}\",throughput_target_gbps=\"{}\",max_connections=\"{}\",write_inflight=\"{}\",stub=\"{}\"}} 1",
            cfg.backend,
            cfg.chunk_bytes,
            cfg.slots,
            cfg.throughput_target_gbps,
            cfg.max_connections,
            cfg.write_inflight,
            cfg.stub
        );
        o
    }
}
