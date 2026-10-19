// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::single_range_in_vec_init)]
//! The proxy end to end over HTTP: mock S3 behind, loopback "RDMA" in front.

use std::sync::Arc;
use std::time::Duration;

use adit_fetch::Retrying;
use adit_fetch::crt::{CrtConfig, CrtSource};
use adit_fetch::mock::MockS3;
use adit_proto::{self as proto, Coverage, Dest, Notif};
use adit_proxy::http::{App, router};
use adit_proxy::loopback::Loopback;
use adit_proxy::metrics::{ConfigInfo, Metrics};
use adit_proxy::pool::Pool;
use tokio::sync::mpsc::UnboundedReceiver;

const MIB: u64 = 1 << 20;
const BASE: u64 = 0x7000_0000_0000;

fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn proxy(lb: &Arc<Loopback>, attempts: u32, slots: usize) -> String {
    let src = Retrying::new(CrtSource::new(&CrtConfig::default()).unwrap(), attempts, Duration::from_millis(5));
    let app = Arc::new(App {
        source: Arc::new(src),
        writer: Arc::new(lb.clone()),
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
            meta_type: "Default".into(),
            stub: true,
        },
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(app)).await.unwrap() });
    format!("http://{addr}")
}

struct Fixture {
    s3: MockS3,
    lb: Arc<Loopback>,
    obj: Vec<u8>,
    target: Arc<adit_proxy::loopback::Target>,
    notifs: UnboundedReceiver<Vec<u8>>,
    http: reqwest::Client,
}

async fn fixture(len: usize) -> Fixture {
    let s3 = MockS3::start().await;
    let obj = bytes(len, 99);
    s3.put("bench/obj", obj.clone());
    let lb = Loopback::new();
    let (target, notifs) = lb.target("client", len, BASE);
    Fixture { s3, lb, obj, target, notifs, http: reqwest::Client::new() }
}

impl Fixture {
    async fn register(&self, proxy: &str) {
        let r = self
            .http
            .put(format!("{proxy}{}/client", proto::AGENTS_PATH))
            .body(Loopback::metadata("client"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }

    async fn get(&self, proxy: &str, range: std::ops::Range<u64>, dest_offset: u64, req: u64) -> reqwest::Response {
        let dest = Dest { addr: BASE, len: self.obj.len() as u64, dev: 0 };
        self.http
            .get(format!("{proxy}/bench/obj"))
            .header(proto::H_PRESIGNED_URL, self.s3.url("bench/obj"))
            .header(proto::H_AGENT, "client")
            .header(proto::H_DEST, dest.to_string())
            .header(proto::H_DEST_OFFSET, dest_offset.to_string())
            .header(proto::H_RANGE, proto::format_range(&range))
            .header(proto::H_REQUEST_ID, req.to_string())
            .send()
            .await
            .unwrap()
    }

    fn drain(&mut self, req: u64) -> Coverage {
        let mut c = Coverage::default();
        while let Ok(n) = self.notifs.try_recv() {
            let n = Notif::decode(&n).unwrap();
            assert_eq!(n.req, req);
            assert_eq!(c.insert(n.offset..n.offset + n.len), 0, "overlapping notification");
        }
        c
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whole_object_lands_byte_identical() {
    let mut f = fixture(5 * MIB as usize + 123).await;
    let p = proxy(&f.lb, 4, 3).await;
    f.register(&p).await;
    let n = f.obj.len() as u64;
    let r = f.get(&p, 0..n, 0, 7).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()[proto::H_BYTES_WRITTEN], n.to_string().as_str());
    assert!(*f.target.mem.lock().unwrap() == f.obj);
    assert!(f.drain(7).is_complete(0..n));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_fetch_and_write_still_land_in_place() {
    let mut f = fixture(6 * MIB as usize).await;
    f.s3.delay(|start| Duration::from_millis(120u64.saturating_sub(start / MIB * 20)));
    f.lb.delay(|addr| Duration::from_millis(((addr - BASE) / MIB) % 3 * 15));
    let p = proxy(&f.lb, 4, 6).await;
    f.register(&p).await;
    let n = f.obj.len() as u64;
    assert_eq!(f.get(&p, 0..n, 0, 1).await.status(), 200);
    assert!(*f.target.mem.lock().unwrap() == f.obj);
    assert!(f.drain(1).is_complete(0..n));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_range_goes_to_its_dest_offset() {
    let mut f = fixture(4 * MIB as usize).await;
    let p = proxy(&f.lb, 4, 2).await;
    f.register(&p).await;
    let r = MIB + 17..3 * MIB + 5;
    assert_eq!(f.get(&p, r.clone(), r.start, 2).await.status(), 200);
    let mem = f.target.mem.lock().unwrap().clone();
    assert_eq!(&mem[r.start as usize..r.end as usize], &f.obj[r.start as usize..r.end as usize]);
    assert!(mem[..r.start as usize].iter().all(|&b| b == 0));
    assert!(f.drain(2).is_complete(r));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_503s_are_absorbed() {
    let mut f = fixture(3 * MIB as usize).await;
    f.s3.fail_next(3);
    let p = proxy(&f.lb, 5, 1).await;
    f.register(&p).await;
    let n = f.obj.len() as u64;
    let r = f.get(&p, 0..n, 0, 3).await;
    assert_eq!(r.status(), 200);
    let s3_reqs: u64 = r.headers()[proto::H_S3_REQUESTS].to_str().unwrap().parse().unwrap();
    assert_eq!(s3_reqs, f.s3.requests(), "S3 request count must match what the server saw");
    assert_eq!(s3_reqs, 3 + 3, "three chunks plus three retried 503s");
    assert!(*f.target.mem.lock().unwrap() == f.obj);
    assert!(f.drain(3).is_complete(0..n));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistent_503_reports_partial_never_complete() {
    let mut f = fixture(4 * MIB as usize).await;
    let p = proxy(&f.lb, 1, 1).await;
    f.register(&p).await;
    f.s3.poison(MIB); // the second chunk never succeeds
    let n = f.obj.len() as u64;
    let r = f.get(&p, 0..n, 0, 4).await;
    assert_eq!(r.status(), 502);
    let failed = proto::parse_ranges(r.headers()[proto::H_FAILED_RANGES].to_str().unwrap()).unwrap();
    let written: u64 = r.headers()[proto::H_BYTES_WRITTEN].to_str().unwrap().parse().unwrap();
    assert_eq!(failed, vec![MIB..2 * MIB]);
    assert_eq!(written, n - MIB);
    let s3_reqs: u64 = r.headers()[proto::H_S3_REQUESTS].to_str().unwrap().parse().unwrap();
    assert_eq!(s3_reqs, f.s3.requests(), "failed attempts are counted too");
    let cov = f.drain(4);
    assert_eq!(cov.missing(0..n), failed, "notifications and HTTP answer must agree on what is missing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_failure_reports_partial() {
    let mut f = fixture(4 * MIB as usize).await;
    let p = proxy(&f.lb, 1, 1).await;
    f.register(&p).await;
    f.lb.fail_writes_after(2);
    let n = f.obj.len() as u64;
    let r = f.get(&p, 0..n, 0, 5).await;
    assert_eq!(r.status(), 502);
    let failed = proto::parse_ranges(r.headers()[proto::H_FAILED_RANGES].to_str().unwrap()).unwrap();
    assert_eq!(failed, vec![2 * MIB..3 * MIB, 3 * MIB..4 * MIB]);
    assert_eq!(f.drain(5).missing(0..n), vec![2 * MIB..4 * MIB]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unregistered_agent_and_overrun_are_refused() {
    let f = fixture(MIB as usize).await;
    let p = proxy(&f.lb, 1, 1).await;
    assert_eq!(f.get(&p, 0..MIB, 0, 6).await.status(), 428);
    f.register(&p).await;
    assert_eq!(f.get(&p, 0..MIB, 1, 6).await.status(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_export_counters_and_config() {
    let f = fixture(2 * MIB as usize).await;
    let p = proxy(&f.lb, 1, 2).await;
    f.register(&p).await;
    let n = f.obj.len() as u64;
    assert_eq!(f.get(&p, 0..n, 0, 8).await.status(), 200);
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains(&format!("adit_proxy_bytes_written_total {n}")), "{m}");
    assert!(m.contains("chunk_bytes=\"1048576\""), "{m}");
    assert!(m.contains("backend=\"loopback\""), "{m}");
    // Two 1 MiB chunks: every stage histogram saw both.
    for h in ["chunk_slot_wait", "chunk_fetch", "chunk_write_wait", "chunk_write"] {
        assert!(m.contains(&format!("adit_proxy_{h}_seconds_count 2")), "{h}: {m}");
        assert!(m.contains(&format!("adit_proxy_{h}_seconds_bucket{{le=\"+Inf\"}} 2")), "{h}: {m}");
    }
}
