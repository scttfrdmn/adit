// SPDX-License-Identifier: Apache-2.0
//! [`RangeSource`] on the AWS Common Runtime S3 client (via `mountpoint-s3-crt`).
//!
//! Each ranged GET is one CRT `Default` meta request against the presigned URL as given: the
//! signature is already in the query string, so the client carries no credentials (anonymous
//! provider: requests go out unsigned). The CRT owns connection pooling, DNS fan-out across S3 front ends, and TLS. Body parts
//! are copied straight into the caller's buffer at their offset as they arrive; that copy is the
//! proxy's one remaining copy (see the zero-copy-receive lever).

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use mountpoint_s3_crt::auth::credentials::CredentialsProvider;
use mountpoint_s3_crt::common::allocator::Allocator;
use mountpoint_s3_crt::common::uri::Uri;
use mountpoint_s3_crt::http::request_response::{Header, Message};
use mountpoint_s3_crt::io::channel_bootstrap::{ClientBootstrap, ClientBootstrapOptions};
use mountpoint_s3_crt::io::event_loop::EventLoopGroup;
use mountpoint_s3_crt::io::host_resolver::{HostResolver, HostResolverDefaultOptions};
use mountpoint_s3_crt::s3::client::{
    Client, ClientConfig, MetaRequest, MetaRequestOptions, MetaRequestType, init_signing_config,
};

use crate::source::{FetchError, FetchFailure, FetchStats, RangeSource};

/// CRT client tuning. Every field is recorded in the results row's `config` column.
#[derive(Debug, Clone)]
pub struct CrtConfig {
    /// Hint the CRT sizes its connection pool from; set to the proxy's ENA bandwidth.
    pub throughput_target_gbps: f64,
    /// Hard cap on concurrent connections, overriding the CRT's own sizing.
    pub max_connections: Option<u32>,
    /// Event loop threads; `None` lets the CRT choose (one per core).
    pub event_loop_threads: Option<u16>,
    /// Bind connections to these interfaces (multi-ENA instances); empty means the default route.
    pub network_interfaces: Vec<String>,
    /// Bucket region. The CRT requires one; presigned URLs carry their own endpoint.
    pub region: String,
}

impl Default for CrtConfig {
    fn default() -> Self {
        Self {
            throughput_target_gbps: 100.0,
            max_connections: None,
            event_loop_threads: None,
            network_interfaces: vec![],
            region: "us-east-1".into(),
        }
    }
}

pub struct CrtSource {
    allocator: Allocator,
    client: Client,
    // Kept alive for the client's lifetime.
    _event_loop_group: EventLoopGroup,
}

impl CrtSource {
    pub fn new(cfg: &CrtConfig) -> Result<Self, FetchError> {
        let crt = |e: mountpoint_s3_crt::common::error::Error| FetchError::Transport(e.to_string());
        let allocator = Allocator::default();
        let mut elg = EventLoopGroup::new_default(&allocator, cfg.event_loop_threads, || {}).map_err(crt)?;
        let mut resolver = HostResolver::new_default(
            &allocator,
            &HostResolverDefaultOptions { max_entries: 64, event_loop_group: &mut elg },
        )
        .map_err(crt)?;
        let bootstrap = ClientBootstrap::new(
            &allocator,
            &ClientBootstrapOptions { event_loop_group: &mut elg, host_resolver: &mut resolver },
        )
        .map_err(crt)?;
        // The CRT insists on a signing config. Anonymous credentials make it send requests
        // unsigned, which is what a presigned URL needs: the signature is already in the query.
        let anonymous = CredentialsProvider::new_anonymous(&allocator).map_err(crt)?;
        let mut config = ClientConfig::new();
        config.signing_config(init_signing_config(&cfg.region, anonymous, None, None, None));
        config.client_bootstrap(bootstrap).throughput_target_gbps(cfg.throughput_target_gbps).region(&cfg.region);
        if let Some(n) = cfg.max_connections {
            config.max_active_connections_override(n);
        }
        if !cfg.network_interfaces.is_empty() {
            config.network_interface_names(cfg.network_interfaces.clone());
        }
        let client = Client::new(&allocator, config).map_err(crt)?;
        Ok(Self { allocator, client, _event_loop_group: elg })
    }
}

/// The destination slice, smuggled into CRT callbacks. Valid until the request finishes: the
/// owning future either awaits `on_finish` or, if dropped early, cancels and blocks for it.
#[derive(Clone, Copy)]
struct Dst {
    ptr: *mut u8,
    len: usize,
}
// SAFETY: writes go to disjoint offsets of a buffer the caller lent exclusively for the request.
unsafe impl Send for Dst {}

/// Cancels the meta request and waits for its finish callback if the future is dropped first,
/// so no CRT thread can write into a buffer that has been handed back.
struct InFlight {
    req: Option<MetaRequest>,
    finished: Option<mpsc::Receiver<()>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let (Some(req), Some(rx)) = (self.req.take(), self.finished.take()) {
            req.cancel();
            let _ = rx.recv();
        }
    }
}

fn request_for(alloc: &Allocator, url: &str, range: &Range<u64>) -> Result<(Message, Uri), FetchError> {
    let crt = |e: mountpoint_s3_crt::common::error::Error| FetchError::Transport(e.to_string());
    let uri = Uri::new_from_str(alloc, url).map_err(crt)?;
    let mut path = uri.path().to_os_string();
    if !uri.query_string().is_empty() {
        path.push("?");
        path.push(uri.query_string());
    }
    let mut msg = Message::new_request(alloc).map_err(crt)?;
    msg.set_request_method("GET").map_err(crt)?;
    msg.set_request_path(path).map_err(crt)?;
    msg.set_header(&Header::new("Host", uri.authority())).map_err(crt)?;
    msg.set_header(&Header::new("Range", format!("bytes={}-{}", range.start, range.end - 1))).map_err(crt)?;
    Ok((msg, uri))
}

impl RangeSource for CrtSource {
    fn get_into<'a>(
        &'a self,
        url: &'a str,
        range: Range<u64>,
        dst: &'a mut [u8],
    ) -> BoxFuture<'a, Result<FetchStats, FetchFailure>> {
        Box::pin(async move {
            let want = range.end - range.start;
            assert_eq!(dst.len() as u64, want, "destination length must equal range length");
            if want == 0 {
                return Ok(FetchStats::default());
            }
            let (msg, uri) = request_for(&self.allocator, url, &range).map_err(|e| e.after(0))?;
            let start = Instant::now();
            let landed = Arc::new(AtomicU64::new(0));
            let first_byte = Arc::new(Mutex::new(None::<Duration>));
            let out_of_range = Arc::new(Mutex::new(None::<u64>));
            let status = Arc::new(AtomicU64::new(0));
            // One telemetry callback per HTTP request the CRT sends, so its internal retries
            // are counted too.
            let sent = Arc::new(AtomicU64::new(0));
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let (fin_tx, fin_rx) = mpsc::channel();
            let d = Dst { ptr: dst.as_mut_ptr(), len: dst.len() };

            let start_req = || -> Result<MetaRequest, FetchError> {
                let mut opts = MetaRequestOptions::new();
                opts.message(msg).endpoint(uri).request_type(MetaRequestType::Default).operation_name("GetObject");
                {
                    let sent = sent.clone();
                    opts.on_telemetry(move |_m| {
                        sent.fetch_add(1, Ordering::Relaxed);
                    });
                }
                {
                    let status = status.clone();
                    opts.on_headers(move |_h, code| status.store(code as u64, Ordering::Relaxed));
                }
                {
                    let (landed, first_byte, out_of_range) = (landed.clone(), first_byte.clone(), out_of_range.clone());
                    opts.on_body(move |offset, buf| {
                        let d = d;
                        // A Default meta request reports offsets within the response body.
                        let off = offset as usize;
                        let n = buf.len();
                        if off.checked_add(n).is_none_or(|end| end > d.len) {
                            out_of_range.lock().unwrap().get_or_insert(offset);
                            return;
                        }
                        first_byte.lock().unwrap().get_or_insert_with(|| start.elapsed());
                        // SAFETY: `off..off+n` is within the lent destination (checked above), and the
                        // buffer outlives the request (see `InFlight`).
                        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), d.ptr.add(off), n) };
                        landed.fetch_add(n as u64, Ordering::Relaxed);
                    });
                }
                opts.on_finish(move |result| {
                    let outcome = if result.is_canceled() {
                        Err(FetchError::Canceled)
                    } else if result.response_status >= 300 {
                        let body =
                            result.error_response_body.map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                        Err(FetchError::Status { status: result.response_status as u16, body })
                    } else if result.is_err() {
                        Err(FetchError::Transport(result.crt_error.to_string()))
                    } else {
                        Ok(())
                    };
                    let _ = done_tx.send(outcome);
                    let _ = fin_tx.send(());
                });

                self.client.make_meta_request(opts).map_err(|e| FetchError::Transport(e.to_string()))
            };
            let req = start_req().map_err(|e| e.after(0))?;
            let mut guard = InFlight { req: Some(req), finished: Some(fin_rx) };
            let outcome = done_rx.await.unwrap_or(Err(FetchError::Canceled));
            // Finished: nothing left to cancel.
            guard.req.take();
            guard.finished.take();
            let requests = (sent.load(Ordering::Relaxed) as u32).max(1);
            outcome.map_err(|e| e.after(requests))?;

            if let Some(offset) = *out_of_range.lock().unwrap() {
                return Err(FetchError::OutOfRange { offset }.after(requests));
            }
            let got = landed.load(Ordering::Relaxed);
            if got != want {
                return Err(FetchError::Short { expected: want, got }.after(requests));
            }
            let code = status.load(Ordering::Relaxed);
            if code != 206 && !(code == 200 && range.start == 0) {
                return Err(
                    FetchError::Status { status: code as u16, body: "range not honoured".into() }.after(requests)
                );
            }
            Ok(FetchStats {
                bytes: got,
                requests,
                first_byte: first_byte.lock().unwrap().unwrap_or_default(),
                elapsed: start.elapsed(),
            })
        })
    }
}
