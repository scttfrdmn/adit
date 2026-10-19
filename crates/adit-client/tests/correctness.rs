// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::single_range_in_vec_init)]
//! G1 correctness on the loopback data plane: identity over many objects and runs, striping,
//! out-of-order completion, a proxy killed mid-transfer, and persistent S3 failure. The GPU is
//! stood in for by the loopback target's memory; the oracle check is the same code.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use adit_client::completion::ChannelSource;
use adit_client::oracle::{ObjectManifest, composite, sha256_parts_cpu};
use adit_client::{Client, Config, GetError};
use adit_fetch::Retrying;
use adit_fetch::crt::{CrtConfig, CrtSource};
use adit_fetch::mock::MockS3;
use adit_proto::Dest;
use adit_proxy::http::{App, router};
use adit_proxy::loopback::{Loopback, Target};
use adit_proxy::metrics::{ConfigInfo, Metrics};
use adit_proxy::pool::Pool;
use sha2::{Digest, Sha256};

const MIB: u64 = 1 << 20;
const BASE: u64 = 0x7f00_0000_0000;

fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn manifest(key: &str, data: &[u8], part: u64) -> ObjectManifest {
    let parts = sha256_parts_cpu(data, part as usize);
    ObjectManifest {
        bucket: "bench".into(),
        key: key.into(),
        size: data.len() as u64,
        sha256: hex::encode(Sha256::digest(data)),
        part_size: part,
        parts: parts.iter().map(hex::encode).collect(),
        composite: composite(&parts),
        seed: None,
    }
}

/// A proxy on its own runtime and thread, so a test can kill it mid-transfer.
struct Proxy {
    url: String,
    kill: Option<std::sync::mpsc::Sender<()>>,
}

impl Proxy {
    fn start(lb: &Arc<Loopback>, attempts: u32, slots: usize) -> Self {
        let lb = lb.clone();
        let (url_tx, url_rx) = std::sync::mpsc::channel();
        let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let src =
                    Retrying::new(CrtSource::new(&CrtConfig::default()).unwrap(), attempts, Duration::from_millis(5));
                let app = Arc::new(App {
                    source: Arc::new(src),
                    writer: Arc::new(lb),
                    pool: Arc::new(Pool::new(slots, MIB as usize)),
                    write_inflight: None,
                    metrics: Metrics::default(),
                    config: ConfigInfo {
                        backend: "loopback".into(),
                        chunk_bytes: MIB,
                        slots: slots as u64,
                        throughput_target_gbps: 10.0,
                        max_connections: 0,
                        write_inflight: 0,
                        stub: true,
                    },
                });
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                url_tx.send(format!("http://{}", l.local_addr().unwrap())).unwrap();
                tokio::spawn(async move { axum::serve(l, router(app)).await.unwrap() });
            });
            let _ = kill_rx.recv();
            // Dropping the runtime aborts every task: in-flight connections reset mid-transfer.
            rt.shutdown_background();
        });
        Self { url: url_rx.recv().unwrap(), kill: Some(kill_tx) }
    }

    fn kill(&mut self) {
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Rig {
    s3: MockS3,
    lb: Arc<Loopback>,
    target: Arc<Target>,
    client: Client,
    proxies: Vec<Proxy>,
}

async fn rig(dest_len: usize, n_proxies: usize, stripe: bool, attempts: u32) -> Rig {
    let s3 = MockS3::start().await;
    let lb = Loopback::new();
    let (target, rx) = lb.target("gpu0", dest_len, BASE);
    let proxies: Vec<Proxy> = (0..n_proxies).map(|_| Proxy::start(&lb, attempts, 4)).collect();
    let client = Client::new(
        Config {
            agent: "gpu0".into(),
            proxies: proxies.iter().map(|p| p.url.clone()).collect(),
            chunk: MIB,
            stripe,
            notif_grace: Duration::from_millis(500),
            timeout: Duration::from_secs(30),
        },
        Dest { addr: BASE, len: dest_len as u64, dev: 0 },
        Arc::new(ChannelSource(Mutex::new(rx))),
    )
    .unwrap();
    client.register(&Loopback::metadata("gpu0")).await.unwrap();
    Rig { s3, lb, target, client, proxies }
}

impl Rig {
    fn zero(&self) {
        self.target.mem.lock().unwrap().fill(0);
    }

    fn check(&self, m: &ObjectManifest, dest_offset: usize) -> bool {
        let mem = self.target.mem.lock().unwrap();
        let buf = &mem[dest_offset..dest_offset + m.size as usize];
        m.check(&sha256_parts_cpu(buf, m.part_size as usize)).matches
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_objects_ten_runs_byte_identical() {
    // A scaled-down T2: objects of uneven sizes, one destination each, ten runs.
    let sizes: Vec<usize> = (0..12).map(|i| (MIB as usize) * (1 + i % 3) + 4096 * i + 7).collect();
    let total: usize = sizes.iter().sum();
    let r = rig(total, 2, false, 3).await;
    let mut objs = Vec::new();
    let mut off = 0;
    for (i, &n) in sizes.iter().enumerate() {
        let key = format!("bench/t2/obj{i:04}");
        let data = bytes(n, i as u64 + 1);
        r.s3.put(&key, data.clone());
        objs.push((key.clone(), manifest(&key, &data, MIB), off, r.s3.url(&key)));
        off += n;
    }
    for run in 0..10 {
        r.zero();
        let futs = objs.iter().map(|(key, m, off, url)| r.client.get(key, url, 0..m.size, *off as u64));
        for (res, (key, ..)) in futures::future::join_all(futs).await.into_iter().zip(&objs) {
            res.unwrap_or_else(|e| panic!("run {run} {key}: {e}"));
        }
        for (key, m, off, _) in &objs {
            assert!(r.check(m, *off), "run {run}: {key} differs from manifest");
        }
    }
    assert_eq!(r.client.tracker().stray.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn striped_object_out_of_order_is_byte_identical() {
    let n = 9 * MIB as usize + 333;
    let r = rig(n, 3, true, 3).await;
    let data = bytes(n, 77);
    r.s3.put("bench/t1", data.clone());
    let m = manifest("bench/t1", &data, MIB);
    r.s3.delay(|start| Duration::from_millis(60u64.saturating_sub(start / MIB * 7)));
    r.lb.delay(|addr| Duration::from_millis((addr - BASE) / MIB % 4 * 5));
    for _ in 0..3 {
        r.zero();
        let rep = r.client.get("bench/t1", &r.s3.url("bench/t1"), 0..n as u64, 0).await.unwrap();
        assert_eq!(rep.stripes, 3);
        assert_eq!(rep.bytes, n as u64);
        assert!(r.check(&m, 0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_killed_mid_transfer_is_reported_partial() {
    let n = 12 * MIB as usize;
    let mut r = rig(n, 2, true, 3).await;
    r.s3.put("bench/big", bytes(n, 5));
    r.lb.delay(|_| Duration::from_millis(40));
    let url = r.s3.url("bench/big");
    let get = r.client.get("bench/big", &url, 0..n as u64, 0);
    let kill = async {
        tokio::time::sleep(Duration::from_millis(60)).await;
        r.proxies[1].kill();
    };
    let (res, ()) = tokio::join!(get, kill);
    match res {
        Err(GetError::Partial { want, covered_bytes, missing, proxy_errors, .. }) => {
            assert_eq!(want, n as u64);
            assert!(covered_bytes < want);
            assert!(!missing.is_empty());
            assert!(!proxy_errors.is_empty());
            let missing_bytes: u64 = missing.iter().map(|m| m.end - m.start).sum();
            assert_eq!(covered_bytes + missing_bytes, want);
        }
        other => panic!("a killed proxy must yield Partial, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistent_s3_failure_is_reported_partial() {
    let n = 4 * MIB as usize;
    let r = rig(n, 1, false, 2).await;
    r.s3.put("bench/x", bytes(n, 9));
    r.s3.poison(2 * MIB);
    match r.client.get("bench/x", &r.s3.url("bench/x"), 0..n as u64, 0).await {
        Err(GetError::Partial { missing, .. }) => assert_eq!(missing, vec![2 * MIB..3 * MIB]),
        other => panic!("expected Partial, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_s3_failure_still_completes() {
    let n = 3 * MIB as usize;
    let r = rig(n, 1, false, 4).await;
    let data = bytes(n, 13);
    r.s3.put("bench/y", data.clone());
    r.s3.fail_next(2);
    let rep = r.client.get("bench/y", &r.s3.url("bench/y"), 0..n as u64, 0).await.unwrap();
    assert_eq!(rep.s3_requests, r.s3.requests());
    assert!(r.check(&manifest("bench/y", &data, MIB), 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overrun_is_refused_before_any_io() {
    let r = rig(MIB as usize, 1, false, 1).await;
    r.s3.put("bench/z", bytes(MIB as usize, 1));
    let e = r.client.get("bench/z", &r.s3.url("bench/z"), 0..MIB, 1).await.unwrap_err();
    assert!(matches!(e, GetError::Invalid(_)), "{e:?}");
    assert_eq!(r.s3.requests(), 0);
}
