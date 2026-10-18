// SPDX-License-Identifier: Apache-2.0
//! Fetch-only diagnostic (#18): the production fetch engine and pipeline with the RDMA write
//! replaced by a no-op, so fetch and write can be measured apart. Same CRT source, same slot pool,
//! same chunking as `adit-proxy`.
//!
//!   adit-fetch-bench --url "$PRESIGNED" --size 68719476736 --chunk-mib 8 --slots 128

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use adit_fetch::Retrying;
use adit_fetch::crt::{CrtConfig, CrtSource};
use adit_proto::Dest;
use adit_proxy::metrics::{Metrics, process_cpu_seconds};
use adit_proxy::pipeline::{self, Transfer};
use adit_proxy::pool::Pool;
use adit_proxy::writer::{RemoteWriter, WriteError};
use clap::Parser;
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
    #[arg(long)]
    url: String,
    #[arg(long)]
    size: u64,
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    #[arg(long, default_value_t = 64)]
    slots: usize,
    #[arg(long, default_value_t = 200.0)]
    throughput_gbps: f64,
    #[arg(long)]
    max_connections: Option<u32>,
    #[arg(long)]
    event_loop_threads: Option<u16>,
    #[arg(long, default_value_t = 1)]
    runs: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    let chunk = a.chunk_mib << 20;
    let pool = Pool::new(a.slots, chunk as usize);
    let src = Retrying::new(
        CrtSource::new(&CrtConfig {
            throughput_target_gbps: a.throughput_gbps,
            max_connections: a.max_connections,
            event_loop_threads: a.event_loop_threads,
            ..CrtConfig::default()
        })?,
        4,
        Duration::from_millis(50),
    );
    let t = Transfer {
        url: a.url.clone(),
        range: 0..a.size,
        agent: "discard".into(),
        dest: Dest { addr: 0, len: a.size, dev: 0 },
        dest_offset: 0,
        req: 0,
        chunk,
    };
    println!(
        "run,chunk_bytes,slots,throughput_gbps,max_connections,event_loop_threads,bytes,wall_s,gbps,s3_requests,cpu_s,avg_fetch_ms,failed"
    );
    for run in 1..=a.runs {
        let m = Arc::new(Metrics::default());
        let cpu0 = process_cpu_seconds().unwrap_or(f64::NAN);
        let t0 = Instant::now();
        let out = pipeline::run(&t, &src, &Discard, &pool, &m).await;
        let secs = t0.elapsed().as_secs_f64();
        let cpu = process_cpu_seconds().unwrap_or(f64::NAN) - cpu0;
        let chunks = m.chunks_ok.load(Relaxed).max(1);
        println!(
            "{run},{chunk},{},{},{},{},{},{secs:.3},{:.3},{},{cpu:.2},{:.1},{}",
            a.slots,
            a.throughput_gbps,
            a.max_connections.map_or("".into(), |v| v.to_string()),
            a.event_loop_threads.map_or("".into(), |v| v.to_string()),
            out.bytes_written,
            out.bytes_written as f64 * 8.0 / secs / 1e9,
            out.s3_requests,
            m.fetch_ns.load(Relaxed) as f64 / chunks as f64 / 1e6,
            out.failed.len()
        );
    }
    Ok(())
}
