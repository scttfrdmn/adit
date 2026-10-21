// SPDX-License-Identifier: Apache-2.0
//! H-zero instrumentation (law 1): counters read on the GPU host around a transfer window.
//!
//! - CPU-seconds, whole host (`/proc/stat`) and this process (`/proc/self/stat`).
//! - EFA device counters (`/sys/class/infiniband/*/ports/*/hw_counters/*`): bytes the NIC wrote
//!   into memory, a cross-check of bytes delivered.
//! - Host memory traffic from the integrated memory controllers, via a `perf stat -a` child
//!   spanning exactly the window, when the instance exposes uncore IMC events. When it does not,
//!   the row says `unavailable` rather than a number.

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};

const CLK_TCK: f64 = 100.0;

/// One read of `/proc/stat`: busy ticks (user nice system irq softirq; `guest*` are already in
/// `user`, `steal` is not this host's work), idle + iowait ticks, online CPUs, and when.
#[derive(Debug, Clone, Copy)]
struct HostStat {
    busy: u64,
    idle: u64,
    cpus: u64,
    at: std::time::Instant,
}

fn host_stat_from(text: &str, at: std::time::Instant) -> Option<HostStat> {
    let line = text.lines().next()?;
    let f: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let g = |i: usize| f.get(i).copied().unwrap_or(0);
    let cpus =
        text.lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)).count();
    Some(HostStat { busy: g(0) + g(1) + g(2) + g(5) + g(6), idle: g(3) + g(4), cpus: cpus as u64, at })
}

fn host_stat() -> Option<HostStat> {
    let at = std::time::Instant::now();
    host_stat_from(&std::fs::read_to_string("/proc/stat").ok()?, at)
}

/// Host CPU-seconds busy between two reads, two ways (#27).
/// - `precise`: online CPUs × wall − (idle + iowait). Under NOHZ the kernel accounts idle precisely,
///   so this counts short bursts that run between ticks.
/// - `ticks`: the tick-sampled user/system/irq sum. On a mostly idle many-core host, work that wakes
///   every ~100 µs (NIXL progress threads, notification pumps) is largely missed by ticks: G2 on
///   p5en read host 1.49 s against 3.60 s for the process alone.
fn host_busy(a: &HostStat, b: &HostStat) -> (f64, f64) {
    let wall = b.at.duration_since(a.at).as_secs_f64();
    let idle = b.idle.saturating_sub(a.idle) as f64 / CLK_TCK;
    let precise = (b.cpus.min(a.cpus) as f64 * wall - idle).max(0.0);
    (precise, b.busy.saturating_sub(a.busy) as f64 / CLK_TCK)
}

fn proc_cpu_jiffies() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// Counters per EFA device (`rdmap…`), summed over its ports.
type DevCounters = BTreeMap<String, BTreeMap<String, u64>>;

fn efa_counters() -> DevCounters {
    let mut out = DevCounters::new();
    let Ok(devs) = std::fs::read_dir("/sys/class/infiniband") else { return out };
    for dev in devs.flatten() {
        let Ok(ports) = std::fs::read_dir(dev.path().join("ports")) else { continue };
        let per = out.entry(dev.file_name().to_string_lossy().into_owned()).or_default();
        for port in ports.flatten() {
            let Ok(ctrs) = std::fs::read_dir(port.path().join("hw_counters")) else { continue };
            for c in ctrs.flatten() {
                if let Ok(v) = std::fs::read_to_string(c.path()).map(|s| s.trim().parse::<u64>()) {
                    let name = c.file_name().to_string_lossy().into_owned();
                    *per.entry(name).or_default() += v.unwrap_or(0);
                }
            }
        }
    }
    out
}

/// Per-device deltas `after - before`.
fn delta(before: &DevCounters, after: &DevCounters) -> DevCounters {
    after
        .iter()
        .map(|(dev, ctrs)| {
            let b = before.get(dev);
            let d = ctrs
                .iter()
                .map(|(k, v)| (k.clone(), v.saturating_sub(b.and_then(|b| b.get(k)).copied().unwrap_or(0))))
                .collect();
            (dev.clone(), d)
        })
        .collect()
}

pub struct Window {
    host0: Option<HostStat>,
    proc0: Option<u64>,
    efa0: DevCounters,
    perf: Option<(Child, std::path::PathBuf)>,
}

#[derive(Debug, Clone, Default)]
pub struct Measured {
    /// Host CPU-seconds busy, precise (online CPUs × wall − idle − iowait; #27).
    pub host_cpu_s: Option<f64>,
    /// The tick-sampled host busy figure, kept as a cross-check: it undercounts bursty work.
    pub host_cpu_ticks_s: Option<f64>,
    pub proc_cpu_s: Option<f64>,
    /// Bytes the EFA device(s) received by RDMA write, if the counter exists.
    pub efa_rx_bytes: Option<u64>,
    /// Bytes the NIC itself placed into registered memory by one-sided RDMA (write receive plus
    /// read), if the device exposes those counters. Compared with the payload, this is the per-row
    /// evidence that no host copy happened (#26): an EFA that emulates RDMA moves the same bytes
    /// as two-sided receives and leaves these at zero.
    pub nic_placed_bytes: Option<u64>,
    /// Counter deltas summed over every EFA device.
    pub efa: BTreeMap<String, u64>,
    /// Counter deltas per EFA device: which rails carried the transfer (G2, multi-card hosts).
    pub efa_by_dev: DevCounters,
    pub mem_read_bytes: Option<u64>,
    pub mem_write_bytes: Option<u64>,
    pub perf_note: String,
}

/// IMC CAS events count 64-byte lines.
const CAS_BYTES: u64 = 64;

impl Window {
    /// Start a window. `perf_events` is a comma-separated perf event list counting memory
    /// controller reads and writes (e.g. `uncore_imc/cas_count_read/,uncore_imc/cas_count_write/`);
    /// empty disables perf.
    pub fn start(perf_events: &str) -> Self {
        let perf = (!perf_events.is_empty())
            .then(|| {
                let out = std::env::temp_dir().join(format!("adit-perf-{}.csv", std::process::id()));
                Command::new("perf")
                    .args(["stat", "-a", "-x", ",", "-e", perf_events, "-o"])
                    .arg(&out)
                    .args(["--", "sleep", "86400"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
                    .map(|c| (c, out))
            })
            .flatten();
        // Give perf a moment to attach before the window opens.
        if perf.is_some() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Self { host0: host_stat(), proc0: proc_cpu_jiffies(), efa0: efa_counters(), perf }
    }

    pub fn stop(self) -> Measured {
        // Close the process window before the host window (start opened host first), so the host
        // window encloses the process window and host CPU cannot read below process CPU (#27).
        let proc1 = proc_cpu_jiffies();
        let host1 = host_stat();
        let busy = host1.zip(self.host0).map(|(b, a)| host_busy(&a, &b));
        let mut m = Measured {
            host_cpu_s: busy.map(|b| b.0),
            host_cpu_ticks_s: busy.map(|b| b.1),
            proc_cpu_s: proc1.zip(self.proc0).map(|(b, a)| b.saturating_sub(a) as f64 / CLK_TCK),
            ..Default::default()
        };
        m.efa_by_dev = delta(&self.efa0, &efa_counters());
        for ctrs in m.efa_by_dev.values() {
            for (k, v) in ctrs {
                *m.efa.entry(k.clone()).or_default() += v;
            }
        }
        m.efa_rx_bytes = m.efa.get("rdma_write_recv_bytes").or_else(|| m.efa.get("rx_bytes")).copied();
        let (w, r) = (m.efa.get("rdma_write_recv_bytes").copied(), m.efa.get("rdma_read_bytes").copied());
        m.nic_placed_bytes = if w.is_none() && r.is_none() { None } else { Some(w.unwrap_or(0) + r.unwrap_or(0)) };
        match self.perf {
            None => m.perf_note = "perf disabled".into(),
            Some((mut child, path)) => {
                let _ = Command::new("kill").args(["-INT", &child.id().to_string()]).status();
                let _ = child.wait();
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let _ = std::fs::remove_file(&path);
                let (mut rd, mut wr, mut seen) = (0u64, 0u64, false);
                for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
                    let f: Vec<&str> = line.split(',').collect();
                    let (Some(v), Some(ev)) = (f.first(), f.get(2)) else { continue };
                    let Ok(v) = v.parse::<f64>() else {
                        m.perf_note = format!("perf: {ev} {v}");
                        continue;
                    };
                    seen = true;
                    if ev.contains("read") {
                        rd += v as u64 * CAS_BYTES;
                    } else if ev.contains("write") {
                        wr += v as u64 * CAS_BYTES;
                    }
                }
                if seen {
                    m.mem_read_bytes = Some(rd);
                    m.mem_write_bytes = Some(wr);
                } else if m.perf_note.is_empty() {
                    m.perf_note = "perf: no counts (events unsupported on this instance?)".into();
                }
            }
        }
        m
    }
}

impl Measured {
    /// Bytes NIC-placed per device (RDMA write receive plus read), every device listed.
    pub fn placed_by_dev(&self) -> BTreeMap<String, u64> {
        self.efa_by_dev
            .iter()
            .map(|(d, c)| {
                (
                    d.clone(),
                    c.get("rdma_write_recv_bytes").copied().unwrap_or(0)
                        + c.get("rdma_read_bytes").copied().unwrap_or(0),
                )
            })
            .collect()
    }
}

/// `active/total` devices with a non-zero count, and the per-device list, e.g.
/// `2/16 rdmap0s1=1234,rdmap1s1=0,…`; for the notes column.
pub fn rails_note(per_dev: &BTreeMap<String, u64>) -> String {
    let active = per_dev.values().filter(|&&v| v > 0).count();
    let list: Vec<String> = per_dev.iter().map(|(d, v)| format!("{d}={v}")).collect();
    format!("{active}/{} {}", per_dev.len(), list.join(","))
}

/// How the payload reached registered memory, from the NIC counters (#26).
pub fn placement(nic_placed: Option<u64>, payload: u64) -> &'static str {
    match nic_placed {
        None => "unknown",
        Some(n) if n >= payload => "nic",
        Some(_) => "emulated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_busy_counts_idle_precisely_and_skips_guest_and_steal() {
        let t0 = std::time::Instant::now();
        // user nice system idle iowait irq softirq steal guest guest_nice; 4 CPUs.
        let a =
            host_stat_from("cpu 100 0 50 1000 10 5 5 7 30 0\ncpu0 1\ncpu1 1\ncpu2 1\ncpu3 1\nintr 9\n", t0).unwrap();
        assert_eq!((a.busy, a.idle, a.cpus), (160, 1010, 4));
        let t1 = t0 + std::time::Duration::from_secs(2);
        // 2 s × 4 CPUs = 800 ticks; idle+iowait grew by 500, busy ticks by only 60.
        let b = host_stat_from("cpu 140 0 70 1490 20 5 5 9 60 0\ncpu0 1\ncpu1 1\ncpu2 1\ncpu3 1\n", t1).unwrap();
        let (precise, ticks) = host_busy(&a, &b);
        assert!((precise - 3.0).abs() < 1e-9, "{precise}");
        assert!((ticks - 0.6).abs() < 1e-9, "{ticks}");
    }

    #[test]
    fn per_device_deltas_and_rails() {
        let mk = |v: &[(&str, &str, u64)]| {
            let mut m = DevCounters::new();
            for &(d, k, x) in v {
                m.entry(d.into()).or_default().insert(k.into(), x);
            }
            m
        };
        let before = mk(&[("a", "rdma_write_recv_bytes", 10), ("b", "rdma_write_recv_bytes", 5)]);
        let after =
            mk(&[("a", "rdma_write_recv_bytes", 30), ("b", "rdma_write_recv_bytes", 5), ("b", "rdma_read_bytes", 7)]);
        let m = Measured { efa_by_dev: delta(&before, &after), ..Default::default() };
        let placed = m.placed_by_dev();
        assert_eq!(placed["a"], 20);
        assert_eq!(placed["b"], 7);
        assert_eq!(rails_note(&placed), "2/2 a=20,b=7");
    }

    #[test]
    fn placement_verdicts() {
        assert_eq!(placement(None, 10), "unknown");
        assert_eq!(placement(Some(10), 10), "nic");
        assert_eq!(placement(Some(3), 10), "emulated");
        assert_eq!(placement(Some(0), 10), "emulated");
    }

    /// Host CPU over a window must not read below this process's own CPU (#27). Linux only:
    /// elsewhere both counters are unavailable.
    #[test]
    #[cfg(target_os = "linux")]
    fn host_cpu_encloses_process_cpu() {
        let w = Window::start("");
        let t = std::time::Instant::now();
        let mut x = 0u64;
        while t.elapsed() < std::time::Duration::from_millis(400) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        std::hint::black_box(x);
        let m = w.stop();
        let (h, p) = (m.host_cpu_s.unwrap(), m.proc_cpu_s.unwrap());
        assert!(p >= 0.3, "busy loop of 0.4 s measured {p} s of process CPU");
        assert!(h + 0.02 >= p, "host {h} s < process {p} s");
    }

    /// The G2 shape (#27): many threads waking every ~100 µs for a short burst. Tick sampling misses
    /// most of it; the precise host figure must still enclose the process's own CPU (allowing ±10 ms
    /// of idle truncation per CPU at each end).
    #[test]
    #[cfg(target_os = "linux")]
    fn host_cpu_encloses_bursty_process_cpu() {
        let w = Window::start("");
        let hs: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let t = std::time::Instant::now();
                    let mut x = 0u64;
                    while t.elapsed() < std::time::Duration::from_millis(600) {
                        let b = std::time::Instant::now();
                        while b.elapsed() < std::time::Duration::from_micros(40) {
                            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                        }
                        std::thread::sleep(std::time::Duration::from_micros(100));
                    }
                    std::hint::black_box(x);
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        let m = w.stop();
        let (h, p) = (m.host_cpu_s.unwrap(), m.proc_cpu_s.unwrap());
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        assert!(p > 0.1, "bursty threads measured only {p} s of process CPU");
        assert!(h + 0.02 * cpus.sqrt() + 0.02 >= p, "host {h} s < process {p} s (ticks {:?})", m.host_cpu_ticks_s);
    }
}
