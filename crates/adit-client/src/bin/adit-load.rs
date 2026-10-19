// SPDX-License-Identifier: Apache-2.0
//! Path A's GPU half, and the oracle for any path that lands files on local disk: optionally run
//! a fetch command (e.g. `s5cmd cp ...` to NVMe) inside the measured window, then read each file
//! through a pinned staging buffer into GPU memory with `cudaMemcpy`-equivalent copies, then
//! verify every object on the GPU against its manifest.
//!
//! The measured window spans the fetch command and the load, so the row carries the whole cost
//! of "copy, then upload" — including the host memory traffic that path exists to show.

use std::io::{Read, Write};
use std::time::Instant;

use adit_client::counters::Window;
use adit_client::csvrow::Row;
use adit_client::gpu::Gpu;
use adit_client::oracle::ObjectManifest;
use clap::Parser;

#[derive(Parser, Debug)]
struct Args {
    /// Manifests (JSONL); file for object `key` is `<dir>/<key>`.
    #[arg(long)]
    manifests: String,
    #[arg(long)]
    dir: String,
    /// Run this shell command first, inside the measured window (the copy step).
    #[arg(long)]
    fetch_cmd: Option<String>,
    #[arg(long, default_value_t = 0)]
    device: usize,
    /// Pinned staging chunk, MiB.
    #[arg(long, default_value_t = 8)]
    chunk_mib: u64,
    #[arg(long, default_value_t = 16)]
    buffer_gib: u64,
    #[arg(long, default_value = "bench/schema/paths.csv")]
    schema: String,
    #[arg(long)]
    meta: Vec<String>,
    #[arg(long, default_value = "")]
    perf_events: String,
    #[arg(long)]
    out: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    let ms: Vec<ObjectManifest> = std::fs::read_to_string(&a.manifests)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let gpu = Gpu::new(a.device)?;
    let cap = (a.buffer_gib << 30) as usize;
    let mut buf = gpu.alloc(cap)?;
    let chunk = (a.chunk_mib << 20) as usize;
    let staging = gpu.alloc_pinned(chunk)?;
    let mut notes = vec![];

    // Window 1: the copy step. Then one window per wave of loads; verification of each wave runs
    // between windows, so neither its time nor its counters are attributed to the transfer.
    let mut acc = Acc::default();
    let win = Window::start(&a.perf_events);
    let t0 = Instant::now();
    if let Some(cmd) = &a.fetch_cmd {
        let st = std::process::Command::new("bash").args(["-c", cmd]).status()?;
        if !st.success() {
            notes.push(format!("fetch command exited {st}"));
        }
    }
    acc.wall += t0.elapsed();
    acc.add(win.stop());

    let (mut bytes, mut bad) = (0u64, 0usize);
    let mut i = 0;
    while i < ms.len() {
        gpu.zero(&mut buf)?;
        let mut placed = vec![];
        let mut off = 0usize;
        let win = Window::start(&a.perf_events);
        let t = Instant::now();
        while i < ms.len() {
            let m = &ms[i];
            if m.size as usize > cap {
                return Err(format!("{} exceeds the buffer", m.key).into());
            }
            let at = off.div_ceil(4096) * 4096;
            if at + m.size as usize > cap {
                break;
            }
            let mut f = std::fs::File::open(format!("{}/{}", a.dir, m.key))?;
            let mut pos = at;
            loop {
                // SAFETY: staging is `chunk` bytes of pinned host memory owned by this process.
                let host = unsafe { std::slice::from_raw_parts_mut(staging.as_ptr()? as *mut u8, chunk) };
                let n = f.read(host)?;
                if n == 0 {
                    break;
                }
                gpu.upload(&staging, n, &mut buf, pos)?;
                pos += n;
            }
            if (pos - at) as u64 != m.size {
                notes.push(format!("{}: file has {} bytes, manifest {}", m.key, pos - at, m.size));
            }
            bytes += (pos - at) as u64;
            placed.push((i, at));
            off = pos;
            i += 1;
        }
        acc.wall += t.elapsed();
        acc.add(win.stop());
        for (j, at) in placed {
            let m = &ms[j];
            let v = m.check(&gpu.sha256_parts(buf.addr() + at as u64, m.size, m.part_size)?);
            if !v.matches {
                bad += 1;
                notes.push(format!("MISMATCH {} parts {:?}", m.key, v.bad_parts));
            }
        }
    }
    if !acc.perf_note.is_empty() {
        notes.push(acc.perf_note.clone());
    }
    let secs = acc.wall.as_secs_f64();
    let mut row = Row::from_schema(&a.schema)?;
    for kv in &a.meta {
        let (k, v) = kv.split_once('=').ok_or("meta must be k=v")?;
        row.set(k, v)?;
    }
    row.set("objects", ms.len())?;
    row.set("bytes", bytes)?;
    row.set("wall_s", format!("{secs:.3}"))?;
    row.set("gbps", format!("{:.3}", bytes as f64 * 8.0 / secs / 1e9))?;
    row.set("host_cpu_s", format!("{:.2}", acc.host_cpu))?;
    row.set("proc_cpu_s", format!("{:.2}", acc.proc_cpu))?;
    let mem = |v: Option<u64>| v.map_or("unavailable".to_string(), |v| v.to_string());
    row.set("host_mem_read_bytes", mem(acc.mem_rd))?;
    row.set("host_mem_write_bytes", mem(acc.mem_wr))?;
    row.set("chunk_bytes", chunk)?;
    row.set("oracle", if bad > 0 { "MISMATCH" } else { "ok" })?;
    row.set("notes", notes.join("; "))?;
    println!("{}\n{}", row.header(), row.line());
    if let Some(p) = &a.out {
        let new = !std::path::Path::new(p).exists();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
        if new {
            writeln!(f, "{}", row.header())?;
        }
        writeln!(f, "{}", row.line())?;
    }
    if bad > 0 {
        return Err("oracle mismatch: see notes".into());
    }
    Ok(())
}

struct Acc {
    wall: std::time::Duration,
    host_cpu: f64,
    proc_cpu: f64,
    /// `None` once any window lacked memory counters: a partial sum is not a measurement.
    mem_rd: Option<u64>,
    mem_wr: Option<u64>,
    perf_note: String,
}

impl Default for Acc {
    fn default() -> Self {
        Self {
            wall: Default::default(),
            host_cpu: 0.0,
            proc_cpu: 0.0,
            mem_rd: Some(0),
            mem_wr: Some(0),
            perf_note: String::new(),
        }
    }
}

impl Acc {
    fn add(&mut self, m: adit_client::counters::Measured) {
        self.host_cpu += m.host_cpu_s.unwrap_or(f64::NAN);
        self.proc_cpu += m.proc_cpu_s.unwrap_or(f64::NAN);
        self.mem_rd = self.mem_rd.zip(m.mem_read_bytes).map(|(a, b)| a + b);
        self.mem_wr = self.mem_wr.zip(m.mem_write_bytes).map(|(a, b)| a + b);
        if !m.perf_note.is_empty() {
            self.perf_note = m.perf_note;
        }
    }
}
