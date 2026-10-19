// SPDX-License-Identifier: Apache-2.0
//! Requests: one object (or a range of one) into a registered destination, from one proxy or
//! striped across several. A client may hold several destinations (one per GPU, G2); each request
//! lands in one of them.
//!
//! **Failure semantics.** A get returns `Ok` only when (a) every proxy involved answered `200`
//! and (b) notifications cover exactly the requested destination bytes, with no overlap. Anything
//! else is [`GetError::Partial`] carrying what is covered and what is missing: a partial buffer is
//! reported, never silently returned.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use adit_fetch::stripe;
use adit_proto::{self as proto, Dest};
use tokio::time::Instant;

use crate::completion::{NotifSource, Tracker};

#[derive(Debug, Clone)]
pub struct Config {
    /// Registered NIXL agent name.
    pub agent: String,
    /// Proxy base URLs, e.g. `http://10.0.0.5:8480`.
    pub proxies: Vec<String>,
    /// Chunk size requested of proxies; stripes align to it.
    pub chunk: u64,
    /// Split each object across all proxies by range.
    pub stripe: bool,
    /// After the proxies answer, how long to wait for straggling notifications.
    pub notif_grace: Duration,
    /// Whole-request HTTP timeout.
    pub timeout: Duration,
    /// Notification pump back-off when no notification is waiting (#17).
    pub pump_idle: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub bytes: u64,
    pub s3_requests: u64,
    pub stripes: usize,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum GetError {
    #[error("bad request: {0}")]
    Invalid(String),
    #[error("registration with {proxy} failed: {msg}")]
    Register { proxy: String, msg: String },
    #[error("partial: {covered_bytes} of {want} bytes covered; missing {missing:?}; proxy errors {proxy_errors:?}")]
    Partial {
        want: u64,
        covered_bytes: u64,
        /// Destination ranges covered by notifications.
        covered: Vec<Range<u64>>,
        /// Destination ranges not covered.
        missing: Vec<Range<u64>>,
        proxy_errors: Vec<String>,
    },
    #[error("overlapping writes: {0} bytes notified twice (defect)")]
    Overlap(u64),
}

pub struct Client {
    cfg: Config,
    http: reqwest::Client,
    dests: Vec<Dest>,
    tracker: Arc<Tracker>,
    next_req: AtomicU64,
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

struct StripeAnswer {
    ok: bool,
    bytes: u64,
    s3_requests: u64,
    error: Option<String>,
}

impl Client {
    /// `dest` is the registered destination region; `notifs` delivers its notifications.
    pub fn new(cfg: Config, dest: Dest, notifs: Arc<dyn NotifSource>) -> Result<Self, GetError> {
        Self::with_dests(cfg, vec![dest], notifs)
    }

    /// As [`Client::new`] with several registered destinations, addressed by index in
    /// [`Client::get_into`]. Notification offsets are relative to the request's destination.
    pub fn with_dests(cfg: Config, dests: Vec<Dest>, notifs: Arc<dyn NotifSource>) -> Result<Self, GetError> {
        if cfg.proxies.is_empty() || cfg.chunk == 0 || dests.is_empty() {
            return Err(GetError::Invalid("need at least one proxy, one destination and a positive chunk".into()));
        }
        let http =
            reqwest::Client::builder().timeout(cfg.timeout).build().map_err(|e| GetError::Invalid(e.to_string()))?;
        let tracker = Arc::new(Tracker::default());
        let pump = tracker.pump(notifs, cfg.pump_idle);
        // Request ids start from the clock so a restarted client never reuses a live id.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(1);
        Ok(Self { cfg, http, dests, tracker, next_req: AtomicU64::new(seed), pump })
    }

    pub fn tracker(&self) -> &Tracker {
        &self.tracker
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn dests(&self) -> &[Dest] {
        &self.dests
    }

    /// Send our agent metadata to every proxy.
    pub async fn register(&self, metadata: &[u8]) -> Result<(), GetError> {
        for p in &self.cfg.proxies {
            let url = format!("{p}{}/{}", proto::AGENTS_PATH, self.cfg.agent);
            let r = self.http.put(&url).body(metadata.to_vec()).send().await;
            match r {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    return Err(GetError::Register { proxy: p.clone(), msg: format!("{status}: {body}") });
                }
                Err(e) => return Err(GetError::Register { proxy: p.clone(), msg: e.to_string() }),
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn one(
        &self,
        dest: &Dest,
        proxy: &str,
        path: &str,
        url: &str,
        range: Range<u64>,
        dest_offset: u64,
        req: u64,
    ) -> StripeAnswer {
        let r = self
            .http
            .get(format!("{proxy}/{path}"))
            .header(proto::H_PRESIGNED_URL, url)
            .header(proto::H_AGENT, &self.cfg.agent)
            .header(proto::H_DEST, dest.to_string())
            .header(proto::H_DEST_OFFSET, dest_offset.to_string())
            .header(proto::H_RANGE, proto::format_range(&range))
            .header(proto::H_REQUEST_ID, req.to_string())
            .header(proto::H_CHUNK, self.cfg.chunk.to_string())
            .send()
            .await;
        let num = |r: &reqwest::Response, h: &str| -> u64 {
            r.headers().get(h).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0)
        };
        match r {
            Ok(r) => {
                let (ok, bytes, s3) =
                    (r.status() == 200, num(&r, proto::H_BYTES_WRITTEN), num(&r, proto::H_S3_REQUESTS));
                let error = (!ok).then(|| format!("{proxy}: {}", r.status()));
                StripeAnswer { ok, bytes, s3_requests: s3, error }
            }
            Err(e) => StripeAnswer { ok: false, bytes: 0, s3_requests: 0, error: Some(format!("{proxy}: {e}")) },
        }
    }

    /// Fetch object bytes `range` (via presigned `url`; `path` is `bucket/key`, for logs and
    /// proxy routing) into the first destination at `dest_offset`.
    pub async fn get(&self, path: &str, url: &str, range: Range<u64>, dest_offset: u64) -> Result<Report, GetError> {
        self.get_into(0, path, url, range, dest_offset).await
    }

    /// As [`Client::get`], into destination `dest` (an index into [`Client::dests`]).
    pub async fn get_into(
        &self,
        dest: usize,
        path: &str,
        url: &str,
        range: Range<u64>,
        dest_offset: u64,
    ) -> Result<Report, GetError> {
        let d = self.dests.get(dest).ok_or_else(|| GetError::Invalid(format!("no destination {dest}")))?;
        let want = range.end.checked_sub(range.start).ok_or_else(|| GetError::Invalid("inverted range".into()))?;
        if dest_offset.checked_add(want).is_none_or(|e| e > d.len) {
            return Err(GetError::Invalid(format!("{want} bytes at offset {dest_offset} overrun dest of {}", d.len)));
        }
        let start = Instant::now();
        let req = self.next_req.fetch_add(1, Relaxed);
        let within = dest_offset..dest_offset + want;
        self.tracker.open(req);

        let n = self.cfg.proxies.len();
        let stripes = if self.cfg.stripe { stripe(range.clone(), n, self.cfg.chunk) } else { vec![range.clone()] };
        let futs = stripes.iter().enumerate().map(|(i, s)| {
            let proxy = &self.cfg.proxies[(req as usize + i) % n];
            self.one(d, proxy, path, url, s.clone(), dest_offset + (s.start - range.start), req)
        });
        let answers = futures::future::join_all(futs).await;
        let all_ok = answers.iter().all(|a| a.ok);
        self.tracker.wait(req, within.clone(), Instant::now() + self.cfg.notif_grace).await;
        let (cov, overlap) = self.tracker.close(req);
        if overlap > 0 {
            return Err(GetError::Overlap(overlap));
        }
        let mut proxy_errors: Vec<String> = answers.iter().filter_map(|a| a.error.clone()).collect();
        let reported: u64 = answers.iter().map(|a| a.bytes).sum();
        if reported != cov.covered() {
            proxy_errors
                .push(format!("proxies reported {reported} bytes written, notifications cover {}", cov.covered()));
        }
        if all_ok && cov.is_complete(within.clone()) && proxy_errors.is_empty() {
            return Ok(Report {
                bytes: want,
                s3_requests: answers.iter().map(|a| a.s3_requests).sum(),
                stripes: stripes.len(),
                elapsed: start.elapsed(),
            });
        }
        Err(GetError::Partial {
            want,
            covered_bytes: cov.covered(),
            covered: cov.ranges().to_vec(),
            missing: cov.missing(within),
            proxy_errors,
        })
    }
}
