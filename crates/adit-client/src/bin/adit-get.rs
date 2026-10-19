// SPDX-License-Identifier: Apache-2.0
//! Path E: read a plan of objects through adit proxies into GPU memory, verify every object on
//! the GPU against its manifest, and emit one results row per run with its counters.
//!
//! The timed window covers the transfers only; GPU verification runs after the window closes and
//! is reported in the `oracle` column. Objects are packed into the GPU buffer in waves when the
//! plan exceeds it; each wave is verified before the buffer is zeroed for the next.

use std::io::Write;
use std::time::{Duration, Instant};

use adit_client::counters::Window;
use adit_client::csvrow::Row;
use adit_client::gpu::Gpu;
use adit_client::nixl::ClientAgent;
use adit_client::oracle::ObjectManifest;
use adit_client::{Client, Config};
use adit_proto::Dest;
use clap::Parser;
use futures::StreamExt;
use serde::Deserialize;

#[derive(Parser, Debug)]
struct Args {
    /// Proxy base URLs, comma-separated.
    #[arg(long, value_delimiter = ',', required = true)]
    proxies: Vec<String>,
    /// Plan: JSONL of object manifests each with a presigned `url` (from `aditctl presign`).
    #[arg(long)]
    plan: String,
    #[arg(long, default_value = "adit-client")]
    agent_name: String,
    #[arg(long, default_value = "LIBFABRIC")]
    backend: String,
    #[arg(long, default_value_t = 0)]
    device: usize,
    /// GPU destination buffer, GiB.
    #[arg(long, default_value_t = 16)]
    buffer_gib: u64,
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    #[arg(long)]
    stripe: bool,
    /// Objects in flight.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    #[arg(long, default_value_t = 1)]
    runs: u32,
    #[arg(long, default_value = "bench/schema/paths.csv")]
    schema: String,
    /// Fixed columns for every row, e.g. `--meta path=E --meta workload=T1 --meta cold=1`.
    #[arg(long)]
    meta: Vec<String>,
    /// perf events counting memory-controller reads and writes; empty disables.
    #[arg(long, default_value = "")]
    perf_events: String,
    /// Append rows here (header written if the file is new); always echoed to stdout.
    #[arg(long)]
    out: Option<String>,
    #[arg(long, default_value_t = 2000)]
    notif_grace_ms: u64,
    /// Notification pump idle back-off, microseconds (#17).
    #[arg(long, default_value_t = 50)]
    pump_idle_us: u64,
    /// NIXL progress-thread delay between polls, microseconds; 0 = NIXL default busy poll (#17).
    #[arg(long, default_value_t = 0)]
    nixl_progress_delay_us: u64,
    /// Accept a run whose bytes were not placed by the NIC (EFA emulating RDMA through the host,
    /// #26). Without it such a run exits non-zero even when the oracle passes.
    #[arg(long)]
    allow_emulated: bool,
}

#[derive(Deserialize)]
struct PlanEntry {
    #[serde(flatten)]
    m: ObjectManifest,
    url: String,
}

fn waves(plan: &[PlanEntry], cap: u64) -> Result<Vec<Vec<(usize, u64)>>, String> {
    let mut out = vec![];
    let mut cur = vec![];
    let mut off = 0u64;
    for (i, p) in plan.iter().enumerate() {
        if p.m.size > cap {
            return Err(format!("{} ({} bytes) exceeds the {cap}-byte buffer", p.m.key, p.m.size));
        }
        // Keep each object page-aligned in the buffer.
        let aligned = off.div_ceil(4096) * 4096;
        if aligned + p.m.size > cap {
            out.push(std::mem::take(&mut cur));
            off = 0;
            cur.push((i, 0));
            off += p.m.size;
        } else {
            cur.push((i, aligned));
            off = aligned + p.m.size;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let a = Args::parse();
    if nixl_sys::is_stub() {
        return Err("built against the NIXL stub API: refusing to measure".into());
    }
    let plan: Vec<PlanEntry> = std::fs::read_to_string(&a.plan)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let cap = a.buffer_gib << 30;
    let waves = waves(&plan, cap)?;

    let gpu = Gpu::new(a.device)?;
    let mut buf = gpu.alloc(cap as usize)?;
    let dest = Dest { addr: buf.addr(), len: cap, dev: a.device as u64 };
    let agent = ClientAgent::with_progress_delay(
        &a.agent_name,
        &a.backend,
        dest.addr,
        cap as usize,
        dest.dev,
        true,
        a.nixl_progress_delay_us,
    )?;
    let md = agent.metadata()?;
    let chunk = a.chunk_mib << 20;
    let client = Client::new(
        Config {
            agent: a.agent_name.clone(),
            proxies: a.proxies.clone(),
            chunk,
            stripe: a.stripe,
            notif_grace: Duration::from_millis(a.notif_grace_ms),
            timeout: Duration::from_secs(3600),
            pump_idle: Duration::from_micros(a.pump_idle_us),
        },
        dest,
        agent.clone(),
    )?;
    client.register(&md).await?;
    eprintln!(
        "# gpu={} agent={} metadata_bytes={} waves={} buffer={cap}",
        gpu.name(),
        agent.name(),
        md.len(),
        waves.len()
    );

    let mut out = match &a.out {
        Some(p) => {
            let new = !std::path::Path::new(p).exists();
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            if new {
                writeln!(f, "{}", Row::from_schema(&a.schema)?.header())?;
            }
            Some(f)
        }
        None => None,
    };
    println!("{}", Row::from_schema(&a.schema)?.header());

    let mut any_bad = false;
    for run in 1..=a.runs {
        let (mut wall, mut bytes, mut s3_requests, mut objects) = (Duration::ZERO, 0u64, 0u64, 0usize);
        let (mut host_cpu, mut proc_cpu, mut efa_rx) = (0f64, 0f64, 0u64);
        let (mut mem_rd, mut mem_wr, mut mem_ok) = (0u64, 0u64, true);
        let mut placed: Option<u64> = Some(0);
        let (mut bad, mut partial, mut notes) = (0usize, 0usize, Vec::<String>::new());
        let mut perf_note = String::new();
        for wave in &waves {
            gpu.zero(&mut buf)?;
            let win = Window::start(&a.perf_events);
            let t0 = Instant::now();
            let results: Vec<_> = futures::stream::iter(wave.iter().map(|&(i, off)| {
                let (p, client) = (&plan[i], &client);
                async move {
                    let path = format!("{}/{}", p.m.bucket, p.m.key);
                    (i, off, client.get(&path, &p.url, 0..p.m.size, off).await)
                }
            }))
            .buffer_unordered(a.concurrency)
            .collect()
            .await;
            wall += t0.elapsed();
            let m = win.stop();
            host_cpu += m.host_cpu_s.unwrap_or(f64::NAN);
            proc_cpu += m.proc_cpu_s.unwrap_or(f64::NAN);
            efa_rx += m.efa_rx_bytes.unwrap_or(0);
            placed = placed.zip(m.nic_placed_bytes).map(|(a, b)| a + b);
            match (m.mem_read_bytes, m.mem_write_bytes) {
                (Some(r), Some(w)) => {
                    mem_rd += r;
                    mem_wr += w;
                }
                _ => mem_ok = false,
            }
            perf_note = m.perf_note.clone();
            // Verify every delivered object of the wave in one GPU launch.
            let mut delivered = Vec::new();
            for (i, off, r) in results {
                let p = &plan[i];
                objects += 1;
                match r {
                    Ok(rep) => {
                        bytes += rep.bytes;
                        s3_requests += rep.s3_requests;
                        delivered.push((i, off));
                    }
                    Err(e) => {
                        partial += 1;
                        notes.push(format!("{}: {e}", p.m.key));
                    }
                }
            }
            let spans: Vec<_> =
                delivered.iter().map(|&(i, off)| (dest.addr + off, plan[i].m.size, plan[i].m.part_size)).collect();
            for (&(i, _), parts) in delivered.iter().zip(gpu.sha256_parts_many(&spans)?) {
                let v = plan[i].m.check(&parts);
                if !v.matches {
                    bad += 1;
                    notes.push(format!("MISMATCH {} parts {:?}", plan[i].m.key, v.bad_parts));
                }
            }
        }
        any_bad |= bad > 0 || partial > 0;
        let placement = adit_client::counters::placement(placed, bytes);
        notes
            .push(format!("placement={placement} nic_placed_bytes={}", placed.map_or("n/a".into(), |p| p.to_string())));
        if placement == "emulated" && !a.allow_emulated {
            eprintln!("run {run}: bytes were not placed by the NIC (placement=emulated); see #26");
            any_bad = true;
        }
        let t = client.tracker();
        let stray = t.stray.load(std::sync::atomic::Ordering::Relaxed);
        if stray > 0 {
            notes.push(format!("{stray} stray notifications"));
        }
        if !perf_note.is_empty() {
            notes.push(perf_note);
        }
        let mut row = Row::from_schema(&a.schema)?;
        for kv in &a.meta {
            let (k, v) = kv.split_once('=').ok_or_else(|| format!("--meta {kv:?} is not k=v"))?;
            row.set(k, v)?;
        }
        let secs = wall.as_secs_f64();
        row.set("run", run)?;
        row.set("proxies", a.proxies.len())?;
        row.set("chunk_bytes", chunk)?;
        row.set("concurrency", a.concurrency)?;
        row.set("stripe", a.stripe as u8)?;
        row.set("objects", objects)?;
        row.set("bytes", bytes)?;
        row.set("wall_s", format!("{secs:.3}"))?;
        row.set("gbps", format!("{:.3}", bytes as f64 * 8.0 / secs / 1e9))?;
        row.set("host_cpu_s", format!("{host_cpu:.2}"))?;
        row.set("proc_cpu_s", format!("{proc_cpu:.2}"))?;
        row.set("host_mem_read_bytes", if mem_ok { mem_rd.to_string() } else { "unavailable".into() })?;
        row.set("host_mem_write_bytes", if mem_ok { mem_wr.to_string() } else { "unavailable".into() })?;
        row.set("efa_rx_bytes", efa_rx)?;
        row.set("s3_bytes", bytes)?;
        row.set("s3_requests", s3_requests)?;
        row.set(
            "oracle",
            if bad > 0 {
                "MISMATCH"
            } else if partial > 0 {
                "partial"
            } else {
                "ok"
            },
        )?;
        row.set(
            "config",
            format!(
                "backend={};chunk={chunk};stripe={};concurrency={};buffer_gib={};waves={};gpu={};pump_idle_us={};nixl_progress_delay_us={}",
                a.backend,
                a.stripe,
                a.concurrency,
                a.buffer_gib,
                waves.len(),
                gpu.name(),
                a.pump_idle_us,
                a.nixl_progress_delay_us
            ),
        )?;
        row.set("notes", notes.join("; "))?;
        println!("{}", row.line());
        if let Some(f) = out.as_mut() {
            writeln!(f, "{}", row.line())?;
        }
    }
    if any_bad {
        return Err("oracle mismatch or partial transfer: see notes".into());
    }
    Ok(())
}
