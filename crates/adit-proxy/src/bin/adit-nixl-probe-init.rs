// SPDX-License-Identifier: Apache-2.0
//! G0a probe, initiator side (runs on the GPU-less proxy box): NIXL WRITE from registered host
//! DRAM into a remote GPU buffer, with no HTTP and no S3 in the way. Uses the production
//! `NixlWriter` and `Pool`.
//!
//! Connects to `adit-nixl-probe-target`, receives its destination and agent metadata, fills a
//! buffer with seeded random bytes, writes it in chunks (each with a notification), then sends
//! back the S3-style composite SHA-256 of what it sent so the target can compare against the
//! digest it computes on the GPU.
//!
//! `--idle-before 300,0,300` (#98, ai-dynamo/nixl#2505) keeps this one agent alive across several
//! rounds, one per entry plus the first: before round k+1 it sleeps the k-th number of seconds,
//! then connects to a *new* target process (a new agent) at `--target` and writes again. A
//! round's failure is reported, not fatal. The exit status is non-zero if any round failed.

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
    /// write: push into the target (default). read: hand the target our metadata and region and
    /// let it pull (client-initiated READ).
    #[arg(long, default_value = "write")]
    op: String,
    /// Part size of the composite digest (must match the target).
    #[arg(long, default_value_t = 8)]
    part_mib: u64,
    /// Comma-separated seconds to idle before each further round, each against a new target
    /// process (#98). Empty (default): one round.
    #[arg(long, value_delimiter = ',')]
    idle_before: Vec<u64>,
    /// Seconds to keep retrying the connection to each round's target.
    #[arg(long, default_value_t = 600)]
    connect_wait_s: u64,
}

/// Connect to `target`, retrying until it listens or `wait` passes (each round's target is a new
/// process started separately).
fn connect(target: &str, wait: std::time::Duration) -> std::io::Result<TcpStream> {
    let until = Instant::now() + wait;
    loop {
        match TcpStream::connect(target) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() >= until => return Err(e),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
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
    let writer = NixlWriter::new(&a.agent_name, &a.backend, &pool)?.with_deadline(std::time::Duration::from_secs(30));

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

    let rounds = 1 + a.idle_before.len();
    if rounds > 1 && a.op != "write" {
        return Err("--idle-before needs --op write".into());
    }
    let mut any_failed = false;
    for round in 1..=rounds {
        if round > 1 {
            let idle = a.idle_before[round - 2];
            eprintln!("# round {round}: idle {idle}s");
            tokio::time::sleep(std::time::Duration::from_secs(idle)).await;
        }
        let idle_s = if round > 1 { a.idle_before[round - 2] } else { 0 };
        match one_round(&a, &writer, &held, chunk, total, &composite, &pool, round, rounds).await {
            Ok(ok) => any_failed |= !ok,
            Err(e) => {
                println!("ROUND {round} idle_before_s={idle_s} ERROR {e}");
                any_failed = true;
            }
        }
        if rounds > 1 {
            eprintln!("# round {round} done");
        }
    }
    if any_failed {
        return Err("probe failed".into());
    }
    Ok(())
}

/// One target: receive its destination and metadata, write (or, for `--op read`, hand over our
/// region), print the verdict. Returns whether the round passed.
#[allow(clippy::too_many_arguments)]
async fn one_round(
    a: &Args,
    writer: &NixlWriter,
    held: &[(usize, usize, adit_proxy::pool::Slot<'_>)],
    chunk: usize,
    total: usize,
    composite: &str,
    pool: &Pool,
    round: usize,
    rounds: usize,
) -> Result<bool, Box<dyn std::error::Error>> {
    let idle_s = if round > 1 { a.idle_before[round - 2] } else { 0 };
    let mut conn = connect(&a.target, std::time::Duration::from_secs(a.connect_wait_s))?;
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

    if a.op == "read" {
        let md = writer.local_metadata()?;
        let (ptr, plen) = pool.region();
        writeln!(conn, "{}", Dest { addr: ptr as u64, len: plen as u64, dev: 0 })?;
        conn.write_all(&(md.len() as u64).to_le_bytes())?;
        conn.write_all(&md)?;
        writeln!(conn, "composite {composite} bytes {total} secs 0 failed 0")?;
        println!("INIT op=read bytes={total} chunk={chunk} composite={composite} (target pulls)");
        let mut verdict = String::new();
        rd.read_line(&mut verdict)?;
        println!("TARGET {}", verdict.trim());
        return Ok(verdict.contains("PASS"));
    }
    let t0 = Instant::now();
    let results: Vec<_> = futures::stream::iter(held.iter().map(|(i, n, s)| {
        let (writer, remote) = (writer, &remote);
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
    // Wait for the target's verdict before tearing down the agent (or the next round). A target
    // that aborted (#98) sends none.
    let mut verdict = String::new();
    let got = rd.read_line(&mut verdict).unwrap_or(0);
    let verdict = if got == 0 { "none (target closed the connection)".to_string() } else { verdict.trim().to_string() };
    println!("TARGET {verdict}");
    let ok = failed == 0 && verdict.contains("PASS");
    if rounds > 1 {
        println!(
            "ROUND {round} idle_before_s={idle_s} remote={remote} failed={failed} {}",
            if ok { "PASS" } else { "FAIL" }
        );
    }
    Ok(ok)
}
