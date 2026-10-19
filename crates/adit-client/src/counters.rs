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

fn host_cpu_jiffies() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().next()?;
    let f: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal ...: everything except idle and iowait.
    Some(f.iter().enumerate().filter(|(i, _)| *i != 3 && *i != 4).map(|(_, v)| v).sum())
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
    host0: Option<u64>,
    proc0: Option<u64>,
    efa0: DevCounters,
    perf: Option<(Child, std::path::PathBuf)>,
}

#[derive(Debug, Clone, Default)]
pub struct Measured {
    pub host_cpu_s: Option<f64>,
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
        Self { host0: host_cpu_jiffies(), proc0: proc_cpu_jiffies(), efa0: efa_counters(), perf }
    }

    pub fn stop(self) -> Measured {
        // Close the process window before the host window (start opened host first), so the host
        // window encloses the process window and host CPU cannot read below process CPU (#27).
        let proc1 = proc_cpu_jiffies();
        let host1 = host_cpu_jiffies();
        let mut m = Measured {
            host_cpu_s: host1.zip(self.host0).map(|(b, a)| b.saturating_sub(a) as f64 / CLK_TCK),
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
}
