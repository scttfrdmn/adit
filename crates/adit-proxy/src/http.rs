// SPDX-License-Identifier: Apache-2.0
//! The control endpoint.
//!
//! - `PUT /_adit/agents/{name}`: body is the client's NIXL agent metadata; loaded once. With
//!   [`App::retire_idle`] set, registering an agent first retires every other agent idle that long
//!   (#98); a retired agent's requests get `428` until it registers again.
//! - `GET /{bucket}/{key}` with the `x-adit-*` headers (see `adit-proto`): runs one transfer and
//!   answers with no body: `200` when every chunk was written, `502` with the failed ranges
//!   otherwise. `400` for malformed headers, `428` if the agent has not registered.
//! - `GET /metrics`: Prometheus text, including the configuration in force.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adit_fetch::RangeSource;
use adit_proto as proto;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::{get, put};

use crate::metrics::{ConfigInfo, Metrics};
use crate::pipeline::{self, Transfer};
use crate::pool::Pool;
use crate::writer::RemoteWriter;

pub struct App {
    pub source: Arc<dyn RangeSource>,
    pub writer: Arc<dyn RemoteWriter>,
    pub pool: Arc<Pool>,
    pub metrics: Metrics,
    pub config: ConfigInfo,
    /// Cap on outstanding RDMA writes across all transfers; `None` = one per slot.
    pub write_inflight: Option<tokio::sync::Semaphore>,
    /// Retire registered agents idle at least this long whenever another agent registers (#98: an
    /// initiator that had served an earlier, exited client and then idled about 4 min failed its
    /// first writes to the next client, and the client aborted in the EFA provider). `None`
    /// keeps every agent.
    pub retire_idle: Option<Duration>,
    /// Registered agents' last activity and transfers in progress.
    pub agents: Agents,
}

/// Last activity and in-progress transfers per registered agent. An agent is idle once its last
/// transfer has finished; one with a transfer in progress is never retired.
#[derive(Default)]
pub struct Agents(Mutex<HashMap<String, AgentUse>>);

struct AgentUse {
    last: Instant,
    active: usize,
}

/// One transfer for `agent` in progress; dropping it ends the transfer and stamps its time.
struct Active<'a> {
    agents: &'a Agents,
    agent: String,
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        if let Some(u) = self.agents.0.lock().unwrap().get_mut(&self.agent) {
            u.active -= 1;
            u.last = Instant::now();
        }
    }
}

impl App {
    /// Record `agent` as registered now, first retiring every other agent idle for at least
    /// [`App::retire_idle`]. Returns the agents retired.
    fn registered(&self, agent: &str) -> Vec<String> {
        let mut m = self.agents.0.lock().unwrap();
        let now = Instant::now();
        let mut retired = vec![];
        if let Some(after) = self.retire_idle {
            let idle: Vec<String> = m
                .iter()
                .filter(|(n, u)| n.as_str() != agent && u.active == 0 && now.duration_since(u.last) >= after)
                .map(|(n, _)| n.clone())
                .collect();
            for n in idle {
                match self.writer.forget_agent(&n) {
                    Ok(()) => {
                        m.remove(&n);
                        self.metrics.agents_retired.fetch_add(1, Relaxed);
                        retired.push(n);
                    }
                    Err(e) => tracing::warn!(agent = n, error = %e, "could not retire idle agent"),
                }
            }
        }
        m.entry(agent.to_string()).or_insert(AgentUse { last: now, active: 0 }).last = now;
        retired
    }

    /// Start a transfer for `agent`, or `None` if it is not registered (or was retired). Checked
    /// under the same lock as retirement, so an agent is never retired with a transfer starting.
    fn begin(&self, agent: &str) -> Option<Active<'_>> {
        let mut m = self.agents.0.lock().unwrap();
        if !self.writer.knows(agent) {
            return None;
        }
        let u = m.entry(agent.to_string()).or_insert(AgentUse { last: Instant::now(), active: 0 });
        u.active += 1;
        u.last = Instant::now();
        Some(Active { agents: &self.agents, agent: agent.to_string() })
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route(&format!("{}/{{name}}", proto::AGENTS_PATH), put(register))
        .route("/{*path}", get(transfer))
        .with_state(app)
}

fn reply(status: StatusCode, headers: &[(&str, String)], body: impl Into<Body>) -> Response<Body> {
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    b.body(body.into()).unwrap()
}

async fn metrics(State(app): State<Arc<App>>) -> String {
    app.metrics.render(&app.config)
}

async fn register(State(app): State<Arc<App>>, Path(name): Path<String>, body: Bytes) -> Response<Body> {
    match app.writer.load_agent(&body) {
        Ok(loaded) if loaded == name => {
            let retired = app.registered(&name);
            if !retired.is_empty() {
                tracing::info!(agent = name, ?retired, "retired idle agents");
            }
            reply(StatusCode::NO_CONTENT, &[], Body::empty())
        }
        Ok(loaded) => {
            reply(StatusCode::BAD_REQUEST, &[], format!("metadata is for agent {loaded:?}, path says {name:?}"))
        }
        Err(e) => reply(StatusCode::BAD_REQUEST, &[], e.to_string()),
    }
}

fn header<'h>(h: &'h HeaderMap, name: &str) -> Result<&'h str, String> {
    h.get(name).ok_or_else(|| format!("missing {name}"))?.to_str().map_err(|_| format!("non-ASCII {name}"))
}

fn parse_transfer(h: &HeaderMap, default_chunk: u64) -> Result<Transfer, String> {
    let num = |name: &str| -> Result<u64, String> { header(h, name)?.parse().map_err(|_| format!("malformed {name}")) };
    let t = Transfer {
        url: header(h, proto::H_PRESIGNED_URL)?.to_string(),
        range: proto::parse_range(header(h, proto::H_RANGE)?).map_err(|e| e.to_string())?,
        agent: header(h, proto::H_AGENT)?.to_string(),
        dest: header(h, proto::H_DEST)?.parse().map_err(|e: proto::ParseError| e.to_string())?,
        dest_offset: num(proto::H_DEST_OFFSET)?,
        req: num(proto::H_REQUEST_ID)?,
        chunk: if h.contains_key(proto::H_CHUNK) { num(proto::H_CHUNK)? } else { default_chunk },
    };
    t.validate()?;
    Ok(t)
}

async fn transfer(State(app): State<Arc<App>>, Path(path): Path<String>, h: HeaderMap) -> Response<Body> {
    app.metrics.requests.fetch_add(1, Relaxed);
    let t = match parse_transfer(&h, app.config.chunk_bytes) {
        Ok(t) => t,
        Err(e) => {
            app.metrics.requests_failed.fetch_add(1, Relaxed);
            return reply(StatusCode::BAD_REQUEST, &[], e);
        }
    };
    let Some(_active) = app.begin(&t.agent) else {
        app.metrics.requests_failed.fetch_add(1, Relaxed);
        return reply(StatusCode::PRECONDITION_REQUIRED, &[], format!("agent {:?} not registered", t.agent));
    };
    let out = pipeline::run_bounded(
        &t,
        app.source.as_ref(),
        app.writer.as_ref(),
        &app.pool,
        &app.metrics,
        app.write_inflight.as_ref(),
    )
    .await;
    let mut hs = vec![
        (proto::H_BYTES_WRITTEN, out.bytes_written.to_string()),
        (proto::H_S3_REQUESTS, out.s3_requests.to_string()),
    ];
    if out.failed.is_empty() {
        tracing::debug!(path, req = t.req, bytes = out.bytes_written, "transfer complete");
        reply(StatusCode::OK, &hs, Body::empty())
    } else {
        app.metrics.requests_failed.fetch_add(1, Relaxed);
        tracing::warn!(path, req = t.req, failed = ?out.failed, errors = ?out.errors, "transfer partial");
        hs.push((proto::H_FAILED_RANGES, proto::format_ranges(&out.failed)));
        reply(StatusCode::BAD_GATEWAY, &hs, out.errors.join("\n"))
    }
}
