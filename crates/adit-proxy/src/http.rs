// SPDX-License-Identifier: Apache-2.0
//! The control endpoint.
//!
//! - `PUT /_adit/agents/{name}`: body is the client's NIXL agent metadata; loaded once.
//! - `GET /{bucket}/{key}` with the `x-adit-*` headers (see `adit-proto`): runs one transfer and
//!   answers with no body: `200` when every chunk was written, `502` with the failed ranges
//!   otherwise. `400` for malformed headers, `428` if the agent has not registered.
//! - `GET /metrics`: Prometheus text, including the configuration in force.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

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
        Ok(loaded) if loaded == name => reply(StatusCode::NO_CONTENT, &[], Body::empty()),
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
    if !app.writer.knows(&t.agent) {
        app.metrics.requests_failed.fetch_add(1, Relaxed);
        return reply(StatusCode::PRECONDITION_REQUIRED, &[], format!("agent {:?} not registered", t.agent));
    }
    let out = pipeline::run(&t, app.source.as_ref(), app.writer.as_ref(), &app.pool, &app.metrics).await;
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
