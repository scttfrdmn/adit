// SPDX-License-Identifier: Apache-2.0
//! Fetch-only diagnostic (#18): the production fetch engine and pipeline with the RDMA write
//! replaced by a no-op, so fetch and write can be measured apart. Same CRT source, same slot pool,
//! same chunking as `adit-proxy`.
//!
//!   adit-fetch-bench --url "$PRESIGNED" --size 68719476736 --chunk-mib 8 --slots 128
//!   adit-fetch-bench --plan plan.jsonl --slots 256 --chunk-log chunks.csv   # many objects
//!
//! `--scheme http` rewrites each presigned URL to plain HTTP (SigV4 does not sign the scheme) to
//! measure TLS cost; a bucket policy that denies insecure transport shows up as 403s.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use adit_fetch::Retrying;
use adit_fetch::crt::{CrtConfig, CrtSource, MetaType};
use adit_proto::Dest;
use adit_proxy::metrics::{Metrics, process_cpu_seconds};
use adit_proxy::pipeline::{self, ChunkTiming, Transfer};
use adit_proxy::pool::Pool;
use adit_proxy::writer::{RemoteWriter, WriteError};
use clap::Parser;
use futures::StreamExt;
use futures::future::BoxFuture;

struct Discard;

impl RemoteWriter for Discard {
    fn load_agent(&self, _: &[u8]) -> Result<String, WriteError> {
        Ok("discard".into())
    }
    fn knows(&self, _: &str) -> bool {
        true
    }
    fn write<'a>(
        &'a self,
        _: &'a [u8],
        _: &'a str,
        _: u64,
        _: u64,
        _: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), WriteError>> {
        Box::pin(async { Ok(()) })
    }
    fn backend(&self) -> &str {
        "discard"
    }
}

#[derive(Parser, Debug)]
struct Args {
    /// One object: presigned URL (with --size).
    #[arg(long)]
    url: Option<String>,
    #[arg(long)]
    size: Option<u64>,
    /// Many objects: plan JSONL (manifest lines with `url`), fetched `--objects-inflight` at a time.
    #[arg(long)]
    plan: Option<String>,
    #[arg(long, default_value_t = 8)]
    objects_inflight: usize,
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    /// Receive slots shared by all objects in flight.
    #[arg(long, default_value_t = 64)]
    slots: usize,
    #[arg(long, default_value_t = 200.0)]
    throughput_gbps: f64,
    #[arg(long)]
    max_connections: Option<u32>,
    #[arg(long)]
    event_loop_threads: Option<u16>,
    /// Bind S3 connections to these interfaces, comma-separated (multi-ENA boxes, #53); unset
    /// means the default route.
    #[arg(long, value_delimiter = ',')]
    network_interfaces: Vec<String>,
    /// default | get-object
    #[arg(long, default_value = "default")]
    meta_type: MetaType,
    /// CRT part size for get-object, MiB.
    #[arg(long)]
    part_mib: Option<usize>,
    /// https (as presigned) | http
    #[arg(long, default_value = "https")]
    scheme: String,
    #[arg(long, default_value = "us-east-1")]
    region: String,
    #[arg(long, default_value_t = 1)]
    runs: u32,
    /// Append one CSV line per chunk (stage times relative to its object's start).
    #[arg(long)]
    chunk_log: Option<String>,
    /// Label written into every chunk-log line and result line.
    #[arg(long, default_value = "")]
    label: String,
}

/// Rewrite a presigned https URL to http. SigV4 signs host, path and query, not the scheme.
pub fn with_scheme(url: &str, scheme: &str) -> String {
    match (scheme, url.strip_prefix("https://")) {
        ("http", Some(rest)) => format!("http://{rest}"),
        _ => url.to_string(),
    }
}

#[derive(serde::Deserialize)]
struct PlanLine {
    key: String,
    size: u64,
    url: String,
}

fn log_line(label: &str, run: u32, key: &str, t: &ChunkTiming) -> String {
    format!(
        "{label},{run},{key},{},{},{},{},{},{},{},{}",
        t.offset, t.len, t.requests, t.slot_ns, t.first_byte_ns, t.fetched_ns, t.written_ns, t.ok as u8
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    let objects: Vec<(String, u64, String)> = match (&a.plan, &a.url, a.size) {
        (Some(p), _, _) => std::fs::read_to_string(p)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<PlanLine>(l).map(|p| (p.key, p.size, with_scheme(&p.url, &a.scheme))))
            .collect::<Result<_, _>>()?,
        (None, Some(u), Some(n)) => vec![("object".into(), n, with_scheme(u, &a.scheme))],
        _ => return Err("give --plan, or --url with --size".into()),
    };
    let chunk = a.chunk_mib << 20;
    let pool = Arc::new(Pool::new(a.slots, chunk as usize));
    let src = Arc::new(Retrying::new(
        CrtSource::new(&CrtConfig {
            throughput_target_gbps: a.throughput_gbps,
            max_connections: a.max_connections,
            event_loop_threads: a.event_loop_threads,
            network_interfaces: a.network_interfaces.clone(),
            region: a.region.clone(),
            meta_type: a.meta_type,
            part_size: a.part_mib.map(|m| m << 20),
        })?,
        4,
        Duration::from_millis(50),
    ));
    let mut log = match &a.chunk_log {
        Some(p) => {
            let new = !std::path::Path::new(p).exists();
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            if new {
                writeln!(f, "label,run,key,offset,len,requests,slot_ns,first_byte_ns,fetched_ns,done_ns,ok")?;
            }
            Some(f)
        }
        None => None,
    };
    println!(
        "label,run,objects,chunk_bytes,slots,objects_inflight,meta_type,scheme,throughput_gbps,bytes,wall_s,gbps,s3_requests,cpu_s,avg_fetch_ms,failed"
    );
    for run in 1..=a.runs {
        let m = Arc::new(Metrics::default());
        let cpu0 = process_cpu_seconds().unwrap_or(f64::NAN);
        let t0 = Instant::now();
        let outs: Vec<_> = futures::stream::iter(objects.iter().map(|(key, size, url)| {
            let (src, pool, m) = (src.clone(), pool.clone(), m.clone());
            async move {
                let t = Transfer {
                    url: url.clone(),
                    range: 0..*size,
                    agent: "discard".into(),
                    dest: Dest { addr: 0, len: *size, dev: 0 },
                    dest_offset: 0,
                    req: 0,
                    chunk,
                };
                (key.clone(), pipeline::run(&t, src.as_ref(), &Discard, &pool, &m).await)
            }
        }))
        .buffer_unordered(a.objects_inflight)
        .collect()
        .await;
        let secs = t0.elapsed().as_secs_f64();
        let cpu = process_cpu_seconds().unwrap_or(f64::NAN) - cpu0;
        let (mut bytes, mut reqs, mut failed) = (0u64, 0u64, 0usize);
        for (key, o) in &outs {
            bytes += o.bytes_written;
            reqs += o.s3_requests;
            failed += o.failed.len();
            if let Some(f) = log.as_mut() {
                for t in &o.timings {
                    writeln!(f, "{}", log_line(&a.label, run, key, t))?;
                }
            }
        }
        let chunks = m.chunks_ok.load(Relaxed).max(1);
        println!(
            "{},{run},{},{chunk},{},{},{:?},{},{},{bytes},{secs:.3},{:.3},{reqs},{cpu:.2},{:.1},{failed}",
            a.label,
            objects.len(),
            a.slots,
            a.objects_inflight,
            a.meta_type,
            a.scheme,
            a.throughput_gbps,
            bytes as f64 * 8.0 / secs / 1e9,
            m.fetch_ns.load(Relaxed) as f64 / chunks as f64 / 1e6,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_rewrite_keeps_signature() {
        let u = "https://b.s3.us-east-1.amazonaws.com/k?X-Amz-Signature=abc&X-Amz-Date=1";
        assert_eq!(with_scheme(u, "http"), "http://b.s3.us-east-1.amazonaws.com/k?X-Amz-Signature=abc&X-Amz-Date=1");
        assert_eq!(with_scheme(u, "https"), u);
        assert_eq!(with_scheme("http://x/y", "http"), "http://x/y");
    }

    #[test]
    fn chunk_log_line_has_every_field() {
        let t = ChunkTiming {
            offset: 8,
            len: 4,
            requests: 2,
            slot_ns: 1,
            first_byte_ns: 3,
            fetched_ns: 5,
            write_start_ns: 6,
            written_ns: 7,
            ok: true,
        };
        assert_eq!(log_line("L", 1, "k", &t), "L,1,k,8,4,2,1,3,5,7,1");
        assert_eq!(
            log_line("L", 1, "k", &t).split(',').count(),
            "label,run,key,offset,len,requests,slot_ns,first_byte_ns,fetched_ns,done_ns,ok".split(',').count()
        );
    }
}
