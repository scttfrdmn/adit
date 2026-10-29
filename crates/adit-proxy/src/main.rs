// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use adit_fetch::Retrying;
use adit_fetch::crt::{CrtConfig, CrtSource};
use adit_proxy::http::{App, router};
use adit_proxy::metrics::{ConfigInfo, Metrics};
use adit_proxy::nixl::NixlWriter;
use adit_proxy::pool::Pool;
use clap::Parser;

/// adit proxy: S3 ranges into a client's GPU memory over NIXL.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// HTTP control endpoint.
    #[arg(long, default_value = "0.0.0.0:8480")]
    listen: String,
    /// NIXL agent name; defaults to the hostname.
    #[arg(long)]
    agent_name: Option<String>,
    /// NIXL backend plugin.
    #[arg(long, default_value = "LIBFABRIC")]
    backend: String,
    /// Default chunk (one ranged GET, one RDMA write), MiB.
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    /// Receive slots = maximum chunks in flight per proxy.
    #[arg(long, default_value_t = 64)]
    slots: usize,
    /// CRT throughput target: set to the proxy's ENA bandwidth.
    #[arg(long, default_value_t = 100.0)]
    throughput_gbps: f64,
    /// Hard cap on CRT connections.
    #[arg(long)]
    max_connections: Option<u32>,
    /// Bind S3 connections to these interfaces, comma-separated (multi-ENA boxes, #53); unset
    /// means the default route.
    #[arg(long, value_delimiter = ',')]
    network_interfaces: Vec<String>,
    /// Attempts per ranged GET on 5xx/throttle/transport errors.
    #[arg(long, default_value_t = 4)]
    attempts: u32,
    #[arg(long, default_value = "us-east-1")]
    region: String,
    /// Seconds before an in-progress RDMA write is reported failed.
    #[arg(long, default_value_t = 30)]
    write_timeout_s: u64,
    /// CRT request type: default | get-object (#18).
    #[arg(long, default_value = "default")]
    meta_type: adit_fetch::crt::MetaType,
    /// CRT part size for get-object, MiB.
    #[arg(long)]
    part_mib: Option<usize>,
    /// Cap on outstanding RDMA writes (0 = one per slot). See #18.
    #[arg(long, default_value_t = 0)]
    write_inflight: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let a = Args::parse();
    if nixl_sys::is_stub() {
        return Err("built against the NIXL stub API; rebuild with NIXL_PREFIX set and NIXL_NO_STUBS_FALLBACK=1".into());
    }
    let name = a.agent_name.clone().unwrap_or_else(|| {
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "adit-proxy".into())
    });
    let chunk = a.chunk_mib << 20;
    let pool = Arc::new(Pool::new(a.slots, chunk as usize));
    let writer = NixlWriter::new(&name, &a.backend, &pool)?.with_deadline(Duration::from_secs(a.write_timeout_s));
    let crt = CrtSource::new(&CrtConfig {
        throughput_target_gbps: a.throughput_gbps,
        max_connections: a.max_connections,
        network_interfaces: a.network_interfaces.clone(),
        region: a.region.clone(),
        meta_type: a.meta_type,
        part_size: a.part_mib.map(|m| m << 20),
        ..CrtConfig::default()
    })?;
    let app = Arc::new(App {
        source: Arc::new(Retrying::new(crt, a.attempts, Duration::from_millis(50))),
        writer: Arc::new(writer),
        pool: pool.clone(),
        metrics: Metrics::default(),
        config: ConfigInfo {
            backend: a.backend.clone(),
            chunk_bytes: chunk,
            slots: a.slots as u64,
            throughput_target_gbps: a.throughput_gbps,
            max_connections: a.max_connections.unwrap_or(0) as u64,
            network_interfaces: a.network_interfaces.join(","),
            write_inflight: a.write_inflight as u64,
            meta_type: format!("{:?}", a.meta_type),
            stub: false,
        },
        write_inflight: (a.write_inflight > 0).then(|| tokio::sync::Semaphore::new(a.write_inflight)),
    });
    let listener = tokio::net::TcpListener::bind(&a.listen).await?;
    tracing::info!(listen = a.listen, agent = name, backend = a.backend, chunk, slots = a.slots, "adit-proxy serving");
    axum::serve(listener, router(app)).await?;
    Ok(())
}
