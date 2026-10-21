// SPDX-License-Identifier: Apache-2.0
//! Path E: read a plan of objects through adit proxies into GPU memory, verify every object on
//! the GPU against its manifest, and emit one results row per run with its counters.
//!
//! The timed window covers the transfers only; GPU verification runs after the window closes and
//! is reported in the `oracle` column. Objects are packed into the GPU buffer in waves when the
//! plan exceeds it; each wave is verified before the buffer is zeroed for the next.
//!
//! With `--devices`, every listed GPU gets its own buffer, all registered with one agent. Objects
//! go whole to the least-filled GPU, or, with `--spread`, each object is split into part-aligned
//! segments, one per GPU. Each GPU hashes its own segments; an object's part digests are the
//! concatenation of its segments' in order.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Range;
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
    /// Destination GPUs, comma-separated (overrides `--device`); one buffer of `--buffer-gib` each.
    #[arg(long, value_delimiter = ',')]
    devices: Vec<usize>,
    /// Split every object across all destination GPUs in part-aligned segments.
    #[arg(long)]
    spread: bool,
    /// One NIXL agent per GPU (`<agent-name>-g<dev>`), each registering only its GPU's buffer,
    /// instead of one agent holding every buffer (#48: that lost notifications under concurrency).
    #[arg(long)]
    agent_per_gpu: bool,
    /// With --agent-per-gpu: GPU g is served only by proxy g mod N and requests are not striped
    /// across proxies, so each agent hears from one initiator (#50/#51).
    #[arg(long)]
    pin_gpu_proxy: bool,
    /// GPU destination buffer per GPU, GiB.
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

/// One piece of an object: object bytes `range` into GPU `gpu`'s buffer at `off`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seg {
    gpu: usize,
    range: Range<u64>,
    off: u64,
}

/// Split `size` bytes into at most `n` contiguous pieces, each a whole number of `part`s (the
/// last may be short), so each piece's part digests are a run of the object's.
fn split(size: u64, part: u64, n: usize) -> Vec<Range<u64>> {
    let per = size.div_ceil(part).div_ceil(n as u64).max(1) * part;
    (0..n as u64).map(|k| k * per..((k + 1) * per).min(size)).filter(|r| r.start < r.end).collect()
}

/// Pack the plan into waves over `ngpu` buffers of `cap` bytes each: per object, its segments
/// (one whole piece on the least-filled GPU, or with `spread` one piece per GPU), each placed
/// page-aligned. A wave closes when an object no longer fits.
/// A wave: plan index and its segments, per object.
type Wave = Vec<(usize, Vec<Seg>)>;

fn waves(plan: &[PlanEntry], cap: u64, ngpu: usize, spread: bool) -> Result<Vec<Wave>, String> {
    let mut out = vec![];
    let mut cur: Wave = vec![];
    let mut used = vec![0u64; ngpu];
    for (i, p) in plan.iter().enumerate() {
        let pieces =
            if spread { split(p.m.size, p.m.part_size.max(1), ngpu) } else { split(p.m.size, p.m.size.max(1), 1) };
        if let Some(r) = pieces.iter().find(|r| r.end - r.start > cap) {
            return Err(format!("{} ({} bytes in a piece) exceeds the {cap}-byte buffer", p.m.key, r.end - r.start));
        }
        let place = |used: &[u64]| -> Option<Vec<Seg>> {
            let mut u = used.to_vec();
            let mut segs = vec![];
            for (k, r) in pieces.iter().enumerate() {
                let g = if spread { k } else { (0..ngpu).min_by_key(|&g| u[g]).unwrap_or(0) };
                // Keep each piece page-aligned in the buffer.
                let off = u[g].div_ceil(4096) * 4096;
                if off + (r.end - r.start) > cap {
                    return None;
                }
                u[g] = off + (r.end - r.start);
                segs.push(Seg { gpu: g, range: r.clone(), off });
            }
            Some(segs)
        };
        let segs = match place(&used) {
            Some(s) => s,
            None => {
                out.push(std::mem::take(&mut cur));
                used.iter_mut().for_each(|u| *u = 0);
                place(&used).ok_or_else(|| format!("{} does not fit an empty buffer", p.m.key))?
            }
        };
        for sg in &segs {
            used[sg.gpu] = sg.off + (sg.range.end - sg.range.start);
        }
        cur.push((i, segs));
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
    let devices = if a.devices.is_empty() { vec![a.device] } else { a.devices.clone() };
    let waves = waves(&plan, cap, devices.len(), a.spread)?;

    let gpus: Vec<Gpu> = devices.iter().map(|&d| Gpu::new(d)).collect::<Result<_, _>>()?;
    let mut bufs = gpus.iter().map(|g| g.alloc(cap as usize)).collect::<Result<Vec<_>, _>>()?;
    let dests: Vec<Dest> =
        bufs.iter().zip(&devices).map(|(b, &d)| Dest { addr: b.addr(), len: cap, dev: d as u64 }).collect();
    let regions: Vec<(u64, usize, u64)> = dests.iter().map(|d| (d.addr, cap as usize, d.dev)).collect();
    let agents = if a.agent_per_gpu {
        regions
            .iter()
            .map(|&r| {
                let name = format!("{}-g{}", a.agent_name, r.2);
                ClientAgent::with_regions(&name, &a.backend, &[r], true, a.nixl_progress_delay_us)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![ClientAgent::with_regions(&a.agent_name, &a.backend, &regions, true, a.nixl_progress_delay_us)?]
    };
    let mds = agents.iter().map(|g| g.metadata()).collect::<Result<Vec<_>, _>>()?;
    let agent = agents[0].clone();
    let md = mds[0].clone();
    let chunk = a.chunk_mib << 20;
    let cfg = Config {
        agent: a.agent_name.clone(),
        proxies: a.proxies.clone(),
        chunk,
        stripe: a.stripe,
        notif_grace: Duration::from_millis(a.notif_grace_ms),
        timeout: Duration::from_secs(3600),
        pump_idle: Duration::from_micros(a.pump_idle_us),
    };
    if a.pin_gpu_proxy && !a.agent_per_gpu {
        return Err("--pin-gpu-proxy needs --agent-per-gpu".into());
    }
    let client = if a.agent_per_gpu {
        let srcs = agents
            .iter()
            .map(|g| (g.name().to_string(), g.clone() as std::sync::Arc<dyn adit_client::completion::NotifSource>))
            .collect();
        let mut c = Client::with_agents(cfg, dests.clone(), srcs)?;
        if a.pin_gpu_proxy {
            c = c.pin_dest_proxy();
        }
        c.register_each(&mds).await?;
        c
    } else {
        let c = Client::with_dests(cfg, dests.clone(), agent.clone())?;
        c.register(&md).await?;
        c
    };
    eprintln!(
        "# gpus={}x{} devices={devices:?} agent={} metadata_bytes={} waves={} buffer_per_gpu={cap}",
        gpus.len(),
        gpus[0].name(),
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
        let mut host_ticks = 0f64;
        let (mut mem_rd, mut mem_wr, mut mem_ok) = (0u64, 0u64, true);
        let mut placed: Option<u64> = Some(0);
        let (mut bad, mut partial, mut notes) = (0usize, 0usize, Vec::<String>::new());
        let mut perf_note = String::new();
        let mut rails: BTreeMap<String, u64> = BTreeMap::new();
        for wave in &waves {
            for (g, b) in gpus.iter().zip(bufs.iter_mut()) {
                g.zero(b)?;
            }
            let win = Window::start(&a.perf_events);
            let t0 = Instant::now();
            let segs = wave.iter().enumerate().flat_map(|(w, (i, segs))| segs.iter().map(move |sg| (w, *i, sg)));
            let results: Vec<_> = futures::stream::iter(segs.map(|(w, i, sg)| {
                let (p, client) = (&plan[i], &client);
                async move {
                    let path = format!("{}/{}", p.m.bucket, p.m.key);
                    (w, client.get_into(sg.gpu, &path, &p.url, sg.range.clone(), sg.off).await)
                }
            }))
            .buffer_unordered(a.concurrency)
            .collect()
            .await;
            wall += t0.elapsed();
            let m = win.stop();
            host_cpu += m.host_cpu_s.unwrap_or(f64::NAN);
            host_ticks += m.host_cpu_ticks_s.unwrap_or(f64::NAN);
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
            for (d, v) in m.placed_by_dev() {
                *rails.entry(d).or_default() += v;
            }
            // An object is delivered when every one of its segments is.
            let mut failed = vec![None::<String>; wave.len()];
            let mut reqs = vec![0u64; wave.len()];
            for (w, r) in results {
                match r {
                    Ok(rep) => reqs[w] += rep.s3_requests,
                    Err(e) => failed[w] = Some(e.to_string()),
                }
            }
            for (w, (i, _)) in wave.iter().enumerate() {
                objects += 1;
                match &failed[w] {
                    None => {
                        bytes += plan[*i].m.size;
                        s3_requests += reqs[w];
                    }
                    Some(e) => {
                        partial += 1;
                        notes.push(format!("{}: {e}", plan[*i].m.key));
                    }
                }
            }
            // Verify: one launch per GPU over all its delivered segments, then each object's digests
            // in segment order.
            let mut digests: Vec<Vec<Vec<adit_client::oracle::Digest32>>> =
                wave.iter().map(|(_, segs)| vec![vec![]; segs.len()]).collect();
            for (g, gpu) in gpus.iter().enumerate() {
                let mine: Vec<(usize, usize, &Seg)> = wave
                    .iter()
                    .enumerate()
                    .filter(|(w, _)| failed[*w].is_none())
                    .flat_map(|(w, (_, segs))| segs.iter().enumerate().map(move |(k, sg)| (w, k, sg)))
                    .filter(|(_, _, sg)| sg.gpu == g)
                    .collect();
                let spans: Vec<_> = mine
                    .iter()
                    .map(|&(w, _, sg)| {
                        (dests[g].addr + sg.off, sg.range.end - sg.range.start, plan[wave[w].0].m.part_size)
                    })
                    .collect();
                for (&(w, k, _), parts) in mine.iter().zip(gpu.sha256_parts_many(&spans)?) {
                    digests[w][k] = parts;
                }
            }
            for (w, (i, _)) in wave.iter().enumerate() {
                if failed[w].is_some() {
                    continue;
                }
                let parts: Vec<_> = digests[w].concat();
                let v = plan[*i].m.check(&parts);
                if !v.matches {
                    bad += 1;
                    notes.push(format!("MISMATCH {} parts {:?}", plan[*i].m.key, v.bad_parts));
                }
            }
        }
        any_bad |= bad > 0 || partial > 0;
        let placement = adit_client::counters::placement(placed, bytes);
        notes
            .push(format!("placement={placement} nic_placed_bytes={}", placed.map_or("n/a".into(), |p| p.to_string())));
        notes.push(format!("host_cpu_ticks_s={host_ticks:.2}"));
        if !rails.is_empty() {
            notes.push(format!("rails={}", adit_client::counters::rails_note(&rails)));
        }
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
                "backend={};agent_per_gpu={};pin_gpu_proxy={};chunk={chunk};stripe={};concurrency={};buffer_gib={};waves={};gpu={};devices={};spread={};pump_idle_us={};nixl_progress_delay_us={}",
                a.backend,
                a.agent_per_gpu,
                a.pin_gpu_proxy,
                a.stripe,
                a.concurrency,
                a.buffer_gib,
                waves.len(),
                gpus[0].name(),
                devices.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" "),
                a.spread,
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
    // Teardown in dependency order (#51): stop the notification pumps and wait for them, release
    // every agent (deregistering its memory), and only then free the GPU buffers.
    client.shutdown().await;
    drop(agent);
    drop(agents);
    drop(bufs);
    if any_bad {
        return Err("oracle mismatch or partial transfer: see notes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, size: u64, part: u64) -> PlanEntry {
        let m = serde_json::json!({"bucket": "b", "key": key, "size": size, "part_size": part,
            "sha256": "", "parts": [], "composite": "", "url": "u"});
        serde_json::from_value(m).unwrap()
    }

    #[test]
    fn split_is_part_aligned_and_covers() {
        let r = split(100, 8, 3);
        assert_eq!(r, vec![0..40, 40..80, 80..100]);
        assert_eq!(split(8, 8, 4), vec![0..8]);
        assert_eq!(split(64, 8, 8).len(), 8);
    }

    #[test]
    fn whole_objects_balance_across_gpus() {
        let plan: Vec<_> = (0..4).map(|k| entry(&k.to_string(), 4096, 4096)).collect();
        let w = waves(&plan, 8192, 2, false).unwrap();
        assert_eq!(w.len(), 1);
        let gpus: Vec<usize> = w[0].iter().map(|(_, s)| s[0].gpu).collect();
        assert_eq!(gpus, vec![0, 1, 0, 1]);
        assert_eq!(w[0][2].1[0].off, 4096);
        // A fifth object starts a new wave.
        let plan: Vec<_> = (0..5).map(|k| entry(&k.to_string(), 4096, 4096)).collect();
        assert_eq!(waves(&plan, 8192, 2, false).unwrap().len(), 2);
    }

    #[test]
    fn spread_puts_one_segment_per_gpu() {
        let plan = vec![entry("t1", 64 << 20, 8 << 20)];
        let w = waves(&plan, 16 << 20, 4, true).unwrap();
        let segs = &w[0][0].1;
        assert_eq!(segs.len(), 4);
        for (k, sg) in segs.iter().enumerate() {
            assert_eq!(sg.gpu, k);
            assert_eq!(sg.off, 0);
            assert_eq!(sg.range, (k as u64) * (16 << 20)..(k as u64 + 1) * (16 << 20));
        }
        assert!(waves(&plan, 8 << 20, 4, true).is_err());
    }
}
