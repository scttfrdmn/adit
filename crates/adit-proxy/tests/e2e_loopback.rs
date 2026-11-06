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
    proxy_retiring(lb, attempts, slots, None).await
}

async fn proxy_retiring(lb: &Arc<Loopback>, attempts: u32, slots: usize, retire_idle: Option<Duration>) -> String {
    proxy_cfg(lb, attempts, slots, retire_idle, None).await
}

async fn proxy_cfg(
    lb: &Arc<Loopback>,
    attempts: u32,
    slots: usize,
    retire_idle: Option<Duration>,
    recycle_idle: Option<Duration>,
) -> String {
    let src = Retrying::new(CrtSource::new(&CrtConfig::default()).unwrap(), attempts, Duration::from_millis(5));
    let app = Arc::new(App {
        source: Arc::new(src),
        writer: Arc::new(lb.clone()),
        pool: Arc::new(Pool::new(slots, MIB as usize)),
        write_inflight: None,
        retire_idle,
        recycle_idle,
        agents: Default::default(),
        metrics: Metrics::default(),
        config: ConfigInfo {
            backend: "loopback".into(),
            chunk_bytes: MIB,
            slots: slots as u64,
            throughput_target_gbps: 10.0,
            max_connections: 0,
            network_interfaces: String::new(),
            write_inflight: 0,
            meta_type: "Default".into(),
            stub: true,
            retire_idle_s: retire_idle.map_or(0, |d| d.as_secs()),
            recycle_idle_s: recycle_idle.map_or(0, |d| d.as_secs()),
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

fn metric(text: &str, name: &str) -> u64 {
    text.lines().find_map(|l| l.strip_prefix(name)?.trim().parse().ok()).unwrap_or_else(|| panic!("{name} missing"))
}

/// Slot occupancy (#86): the gauge is back to 0 once a transfer completes, on success and on
/// failure, and the hold counter has summed time for every chunk held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_occupancy_is_released_and_hold_time_counted() {
    let mut f = fixture(4 * MIB as usize).await;
    let p = proxy(&f.lb, 1, 3).await;
    f.register(&p).await;
    let n = f.obj.len() as u64;
    assert_eq!(f.get(&p, 0..n, 0, 11).await.status(), 200);
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(metric(&m, "adit_proxy_slots_in_use "), 0);
    assert!(metric(&m, "adit_proxy_slot_hold_ns_total ") > 0);
    f.drain(11);
    f.s3.poison(MIB);
    assert_eq!(f.get(&p, 0..n, 0, 12).await.status(), 502);
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(metric(&m, "adit_proxy_slots_in_use "), 0);
}

/// #98: registering a new agent retires every other agent idle at least `retire_idle`, whose
/// requests then get 428 until it registers again; an agent idle for less is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_agents_are_retired_when_another_registers() {
    let mut f = fixture(MIB as usize).await;
    let (_t2, _rx2) = f.lb.target("client2", MIB as usize, BASE);
    let p = proxy_retiring(&f.lb, 1, 2, Some(Duration::from_millis(200))).await;
    let (http, pc) = (f.http.clone(), p.clone());
    let put = |name: &'static str| {
        let (http, p) = (http.clone(), pc.clone());
        async move {
            http.put(format!("{p}{}/{name}", proto::AGENTS_PATH))
                .body(Loopback::metadata(name))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    f.register(&p).await;
    assert_eq!(f.get(&p, 0..MIB, 0, 21).await.status(), 200);
    f.drain(21);
    // Idle for less than the threshold: a newcomer leaves the agent alone.
    assert_eq!(put("client2").await, 204);
    assert_eq!(f.get(&p, 0..MIB, 0, 22).await.status(), 200);
    f.drain(22);
    // Past it: client re-registering retires client2 (idle since its registration), never itself.
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.register(&p).await;
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(metric(&m, "adit_proxy_agents_retired_total "), 1, "{m}");
    // client idles past it in turn; client2 registering retires it, and its next request is 428.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(put("client2").await, 204);
    assert_eq!(f.get(&p, 0..MIB, 0, 23).await.status(), 428);
    f.register(&p).await;
    assert_eq!(f.get(&p, 0..MIB, 0, 24).await.status(), 200);
    assert!(f.drain(24).is_complete(0..MIB));
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(metric(&m, "adit_proxy_agents_retired_total "), 2, "{m}");
    assert!(m.contains("retire_idle_s=\"0\""), "{m}");
}

/// #98: a newcomer registering after the proxy served an agent and then idled `recycle_idle`
/// recreates the transport first: every agent is forgotten (428 until it registers again) and the
/// recycle is counted. A fresh proxy, or one recently active, is not recycled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_is_recycled_after_idle_before_a_newcomer() {
    let mut f = fixture(MIB as usize).await;
    let (_t2, _rx2) = f.lb.target("client2", MIB as usize, BASE);
    let p = proxy_cfg(&f.lb, 1, 2, None, Some(Duration::from_millis(200))).await;
    let (http, pc) = (f.http.clone(), p.clone());
    let put = |name: &'static str| {
        let (http, p) = (http.clone(), pc.clone());
        async move {
            http.put(format!("{p}{}/{name}", proto::AGENTS_PATH))
                .body(Loopback::metadata(name))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    let recycled = |m: &str| metric(m, "adit_proxy_transport_recycled_total ");
    // Fresh proxy: nothing served yet, so no recycle however long it idled.
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.register(&p).await;
    assert_eq!(f.get(&p, 0..MIB, 0, 31).await.status(), 200);
    f.drain(31);
    // Served and still recent: a newcomer loads with no recycle.
    assert_eq!(put("client2").await, 204);
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(recycled(&m), 0, "{m}");
    // Served, then idle past the threshold: the next newcomer recycles first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(put("client2").await, 204);
    let m = f.http.get(format!("{p}/metrics")).send().await.unwrap().text().await.unwrap();
    assert_eq!(recycled(&m), 1, "{m}");
    assert!(m.contains("recycle_idle_s=\"0\""), "{m}");
    // The earlier agent was forgotten with the old transport; registering again restores it.
    assert_eq!(f.get(&p, 0..MIB, 0, 32).await.status(), 428);
    f.register(&p).await;
    assert_eq!(f.get(&p, 0..MIB, 0, 33).await.status(), 200);
    assert!(f.drain(33).is_complete(0..MIB));
}
