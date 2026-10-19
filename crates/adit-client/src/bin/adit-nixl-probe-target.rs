// SPDX-License-Identifier: Apache-2.0
//! G0a probe, target side (runs on the GPU box): allocate and register a GPU buffer, hand its
//! descriptor and agent metadata to `adit-nixl-probe-init`, count the notifications, then hash
//! the buffer on the GPU and compare with the initiator's composite. Prints PASS or FAIL with the
//! H-zero counters for the transfer window.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adit_client::completion::Tracker;
use adit_client::counters::Window;
use adit_client::gpu::Gpu;
use adit_client::nixl::ClientAgent;
use adit_client::oracle::composite;
use adit_proto::Dest;
use clap::Parser;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8481")]
    listen: String,
    #[arg(long, default_value = "adit-probe-target")]
    agent_name: String,
    #[arg(long, default_value = "LIBFABRIC")]
    backend: String,
    #[arg(long, default_value_t = 1024)]
    mib: u64,
    #[arg(long, default_value_t = 0)]
    device: usize,
    #[arg(long, default_value_t = 8)]
    part_mib: u64,
    #[arg(long, default_value = "")]
    perf_events: String,
    #[arg(long, default_value_t = 120)]
    timeout_s: u64,
    #[arg(long, default_value_t = 20)]
    pump_idle_us: u64,
    #[arg(long, default_value_t = 0)]
    nixl_progress_delay_us: u64,
    /// write: the initiator pushes (default). read: this side pulls from the initiator's DRAM
    /// (client-initiated READ, for EFA without hardware RDMA write).
    #[arg(long, default_value = "write")]
    op: String,
    /// Reads in flight for --op read.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    if nixl_sys::is_stub() {
        return Err("built against the NIXL stub API".into());
    }
    let len = a.mib << 20;
    let gpu = Gpu::new(a.device)?;
    let buf = gpu.alloc(len as usize)?;
    let dest = Dest { addr: buf.addr(), len, dev: a.device as u64 };
    let agent = ClientAgent::with_progress_delay(
        &a.agent_name,
        &a.backend,
        dest.addr,
        len as usize,
        dest.dev,
        true,
        a.nixl_progress_delay_us,
    )?;
    let md = agent.metadata()?;
    let tracker = Arc::new(Tracker::default());
    tracker.open(0);
    let _pump = tracker.pump(agent.clone(), Duration::from_micros(a.pump_idle_us));
    eprintln!("# gpu={} dest={dest} metadata_bytes={} listening on {}", gpu.name(), md.len(), a.listen);

    let listener = TcpListener::bind(&a.listen)?;
    let (mut conn, peer) = listener.accept()?;
    eprintln!("# initiator {peer}");
    let win = Window::start(&a.perf_events);
    let t0 = Instant::now();
    writeln!(conn, "{dest}")?;
    conn.write_all(&(md.len() as u64).to_le_bytes())?;
    conn.write_all(&md)?;
    let mut rd = BufReader::new(conn.try_clone()?);
    let cov = if a.op == "read" {
        // The initiator answers with its metadata and the DRAM region holding the payload.
        let mut line = String::new();
        rd.read_line(&mut line)?;
        let src: Dest = line.trim().parse()?;
        let mut n = [0u8; 8];
        std::io::Read::read_exact(&mut rd, &mut n)?;
        let mut md = vec![0u8; u64::from_le_bytes(n) as usize];
        std::io::Read::read_exact(&mut rd, &mut md)?;
        let remote = agent.load_remote(&md)?;
        let chunk = (a.chunk_mib << 20) as usize;
        let n_chunks = (len as usize).div_ceil(chunk);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for _ in 0..a.concurrency {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= n_chunks {
                            break;
                        }
                        let off = (i * chunk) as u64;
                        let l = chunk.min(len as usize - i * chunk);
                        if let Err(e) =
                            agent.read(dest.addr + off, dest.dev, &remote, src.addr + off, l, Duration::from_secs(30))
                        {
                            eprintln!("# read chunk {i}: {e}");
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        let mut c = adit_proto::Coverage::default();
        if failed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            c.insert(0..len);
        }
        c
    } else {
        tracker.wait(0, 0..len, tokio::time::Instant::now() + Duration::from_secs(a.timeout_s)).await
    };
    let secs = t0.elapsed().as_secs_f64();
    let m = win.stop();
    let mut line = String::new();
    rd.read_line(&mut line)?;
    let theirs = line.split_whitespace().nth(1).unwrap_or_default().to_string();
    let parts = gpu.sha256_parts(dest.addr, len, a.part_mib << 20)?;
    let ours = composite(&parts);
    let complete = cov.is_complete(0..len);
    let pass = complete && ours == theirs;
    let verdict = format!(
        "{} op={} bytes={len} covered={} notifications={} secs_to_last_notif={secs:.3} gpu_composite={ours} init_composite={theirs} \
         host_cpu_s={:?} proc_cpu_s={:?} efa_rx_bytes={:?} mem_read_bytes={:?} mem_write_bytes={:?} perf={:?}",
        if pass { "PASS" } else { "FAIL" },
        a.op,
        cov.covered(),
        tracker.notifications.load(std::sync::atomic::Ordering::Relaxed),
        m.host_cpu_s,
        m.proc_cpu_s,
        m.efa_rx_bytes,
        m.mem_read_bytes,
        m.mem_write_bytes,
        m.perf_note,
    );
    println!("{verdict}");
    for (k, v) in &m.efa {
        println!("efa {k} {v}");
    }
    writeln!(conn, "{verdict}")?;
    if !pass {
        return Err("probe failed".into());
    }
    Ok(())
}
