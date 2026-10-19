// SPDX-License-Identifier: Apache-2.0
//! An in-process S3-shaped HTTP server for tests.
//!
//! Serves ranged GETs of in-memory objects at `http://127.0.0.1:<port>/<bucket>/<key>?<anything>`
//! (the query string stands in for a presigned signature and is ignored). Faults are injected per
//! server: the next N GETs answer 503 SlowDown, and an optional delay function holds each range
//! for a chosen time so completions arrive out of order.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response, StatusCode, header};

type DelayFn = dyn Fn(u64) -> Duration + Send + Sync;

#[derive(Default)]
struct Inner {
    objects: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    fail_next: AtomicU32,
    requests: AtomicU64,
    delay: Mutex<Option<Arc<DelayFn>>>,
    // Ranges starting at these object offsets always answer 503.
    poisoned: Mutex<Vec<u64>>,
}

/// A running mock server; dropped servers keep running until the runtime stops.
#[derive(Clone)]
pub struct MockS3 {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl MockS3 {
    /// Bind to an ephemeral loopback port and serve on the current tokio runtime.
    pub async fn start() -> Self {
        let inner = Arc::new(Inner::default());
        let app = Router::new().route("/{*path}", axum::routing::get(get)).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock S3");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.expect("mock S3 serve") });
        Self { inner, addr }
    }

    pub fn put(&self, bucket_key: &str, data: Vec<u8>) {
        self.inner.objects.lock().unwrap().insert(bucket_key.to_string(), Arc::new(data));
    }

    /// A URL shaped like a presigned one.
    pub fn url(&self, bucket_key: &str) -> String {
        format!("http://{}/{}?X-Amz-Signature=mock", self.addr, bucket_key)
    }

    /// The next `n` GETs answer `503 SlowDown`.
    pub fn fail_next(&self, n: u32) {
        self.inner.fail_next.store(n, Ordering::SeqCst);
    }

    /// Hold each range starting at object offset `start` for `f(start)` before answering.
    pub fn delay(&self, f: impl Fn(u64) -> Duration + Send + Sync + 'static) {
        *self.inner.delay.lock().unwrap() = Some(Arc::new(f));
    }

    /// Every GET of a range starting at object offset `start` answers `503`, however often retried.
    pub fn poison(&self, start: u64) {
        self.inner.poisoned.lock().unwrap().push(start);
    }

    /// GET requests received, including the ones answered with a fault.
    pub fn requests(&self) -> u64 {
        self.inner.requests.load(Ordering::SeqCst)
    }
}

fn parse_range(h: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    let v = h.get(header::RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (a, b) = v.split_once('-')?;
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { len - 1 } else { b.parse().ok()? };
    (a <= b && b < len).then_some((a, b))
}

async fn get(State(s): State<Arc<Inner>>, Path(path): Path<String>, headers: HeaderMap) -> Response<Body> {
    s.requests.fetch_add(1, Ordering::SeqCst);
    let failed = s.fail_next.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok();
    if failed {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("<Error><Code>SlowDown</Code></Error>"))
            .unwrap();
    }
    let Some(obj) = s.objects.lock().unwrap().get(&path).cloned() else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("<Error><Code>NoSuchKey</Code></Error>"))
            .unwrap();
    };
    let len = obj.len() as u64;
    let (status, a, b) = match headers.get(header::RANGE) {
        None => (StatusCode::OK, 0, len.saturating_sub(1)),
        Some(_) => match parse_range(&headers, len) {
            Some((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b),
            None => {
                return Response::builder().status(StatusCode::RANGE_NOT_SATISFIABLE).body(Body::empty()).unwrap();
            }
        },
    };
    if s.poisoned.lock().unwrap().contains(&a) {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("<Error><Code>SlowDown</Code></Error>"))
            .unwrap();
    }
    let delay = s.delay.lock().unwrap().clone();
    if let Some(f) = delay {
        tokio::time::sleep(f(a)).await;
    }
    let body = if len == 0 { Vec::new() } else { obj[a as usize..=b as usize].to_vec() };
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"))
        // The CRT's GetObject meta request requires an ETag to keep its parts consistent.
        .header(header::ETAG, format!("\"mock-{len}\""))
        .body(Body::from(body))
        .unwrap()
}
