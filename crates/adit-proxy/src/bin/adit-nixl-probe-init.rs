// SPDX-License-Identifier: Apache-2.0
//! G0a probe, initiator side (runs on the GPU-less proxy box): NIXL WRITE from registered host
//! DRAM into a remote GPU buffer, with no HTTP and no S3 in the way. Uses the production
//! `NixlWriter` and `Pool`.
//!
//! Connects to `adit-nixl-probe-target`, receives its destination and agent metadata, fills a
//! buffer with seeded random bytes, writes it in chunks (each with a notification), then sends
//! back the S3-style composite SHA-256 of what it sent so the target can compare against the
//! digest it computes on the GPU.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use adit_proto::{Dest, Notif};
use adit_proxy::nixl::NixlWriter;
use adit_proxy::pool::Pool;
use adit_proxy::writer::RemoteWriter;
use clap::Parser;
use futures::StreamExt;
use sha2::{Digest, Sha256};

#[derive(Parser, Debug)]
struct Args {
    /// Target `host:port`.
    #[arg(long)]
    target: String,
    #[arg(long, default_value = "adit-probe-init")]
    agent_name: String,
    #[arg(long, default_value = "LIBFABRIC")]
    backend: String,
    #[arg(long, default_value_t = 1024)]
    mib: u64,
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    /// Writes in flight.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Part size of the composite digest (must match the target).
    #[arg(long, default_value_t = 8)]
    part_mib: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    if nixl_sys::is_stub() {
        return Err("built against the NIXL stub API".into());
    }
    let (total, chunk) = ((a.mib << 20) as usize, (a.chunk_mib << 20) as usize);
    // The whole payload lives in one registered pool, one slot per chunk, so writes come straight
    // from registered memory as they would in the proxy.
    let slots = total.div_ceil(chunk);
    let pool = Arc::new(Pool::new(slots, chunk));
    let writer = NixlWriter::new(&a.agent_name, &a.backend, &pool)?;

    let mut held = Vec::with_capacity(slots);
    let mut x = a.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    for i in 0..slots {
        let mut s = pool.acquire().await;
        let n = chunk.min(total - i * chunk);
        for b in s[..n].iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        held.push((i, n, s));
    }
    // Composite of parts, as the target will compute it on the GPU.
    let part = (a.part_mib << 20) as usize;
    let mut all = Vec::with_capacity(total);
    for (_, n, s) in &held {
        all.extend_from_slice(&s[..*n]);
    }
    let parts: Vec<[u8; 32]> = all.chunks(part).map(|c| Sha256::digest(c).into()).collect();
    let mut h = Sha256::new();
    parts.iter().for_each(|d| h.update(d));
    use base64::Engine;
    let composite = format!("{}-{}", base64::engine::general_purpose::STANDARD.encode(h.finalize()), parts.len());
    drop(all);

    let mut conn = TcpStream::connect(&a.target)?;
    let mut rd = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    rd.read_line(&mut line)?;
    let dest: Dest = line.trim().parse()?;
    let mut len = [0u8; 8];
    rd.read_exact(&mut len)?;
    let mut md = vec![0u8; u64::from_le_bytes(len) as usize];
    rd.read_exact(&mut md)?;
    let remote = writer.load_agent(&md)?;
    if dest.len < total as u64 {
        return Err(format!("target buffer {} < {total}", dest.len).into());
    }
    eprintln!("# remote agent={remote} dest={dest} metadata_bytes={}", md.len());

    let t0 = Instant::now();
    let results: Vec<_> = futures::stream::iter(held.iter().map(|(i, n, s)| {
        let (writer, remote) = (&writer, &remote);
        async move {
            let off = (*i * chunk) as u64;
            let notif = Notif { req: 0, offset: off, len: *n as u64 }.encode();
            writer.write(&s[..*n], remote, dest.addr + off, dest.dev, notif).await
        }
    }))
    .buffer_unordered(a.concurrency)
    .collect()
    .await;
    let secs = t0.elapsed().as_secs_f64();
    let failed = results.iter().filter(|r| r.is_err()).count();
    if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
        eprintln!("# first write error: {e}");
    }
    writeln!(conn, "composite {composite} bytes {total} secs {secs:.6} failed {failed}")?;
    println!(
        "INIT bytes={total} chunk={chunk} concurrency={} secs={secs:.3} gbps={:.2} failed={failed} composite={composite}",
        a.concurrency,
        total as f64 * 8.0 / secs / 1e9
    );
    // Wait for the target's verdict before tearing down the agent.
    let mut verdict = String::new();
    rd.read_line(&mut verdict)?;
    println!("TARGET {}", verdict.trim());
    if failed > 0 || !verdict.contains("PASS") {
        return Err("probe failed".into());
    }
    Ok(())
}
