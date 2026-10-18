// SPDX-License-Identifier: Apache-2.0
//! The fetch abstraction: get one byte range of one object into a caller buffer.

use std::ops::Range;
use std::time::Duration;

use futures::future::BoxFuture;

/// What one successful ranged fetch cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Payload bytes written into the destination.
    pub bytes: u64,
    /// HTTP requests issued, including retried attempts.
    pub requests: u32,
    /// Time from issuing the (last) request to its first body byte.
    pub first_byte: Duration,
    /// Time from issuing the first request to the last body byte.
    pub elapsed: Duration,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("short body: expected {expected} bytes, got {got}")]
    Short { expected: u64, got: u64 },
    #[error("body byte outside the requested range at offset {offset}")]
    OutOfRange { offset: u64 },
    #[error("transport: {0}")]
    Transport(String),
    #[error("canceled")]
    Canceled,
}

/// A failed fetch and the HTTP requests it cost, so failures are counted in the S3 request
/// column too.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{error} (after {requests} requests)")]
pub struct FetchFailure {
    pub error: FetchError,
    pub requests: u32,
}

impl FetchError {
    pub fn after(self, requests: u32) -> FetchFailure {
        FetchFailure { error: self, requests }
    }

    /// Whether a fresh attempt could succeed: throttling, server errors, transport failures.
    pub fn retryable(&self) -> bool {
        match self {
            FetchError::Status { status, .. } => *status == 429 || *status >= 500,
            FetchError::Short { .. } | FetchError::Transport(_) => true,
            FetchError::OutOfRange { .. } | FetchError::Canceled => false,
        }
    }
}

/// A source of object bytes addressed by presigned URL.
///
/// `get_into` writes exactly `range.end - range.start` bytes into `dst` (which has that length)
/// or fails. On failure the contents of `dst` are unspecified.
pub trait RangeSource: Send + Sync + 'static {
    fn get_into<'a>(
        &'a self,
        url: &'a str,
        range: Range<u64>,
        dst: &'a mut [u8],
    ) -> BoxFuture<'a, Result<FetchStats, FetchFailure>>;
}

impl<S: RangeSource + ?Sized> RangeSource for std::sync::Arc<S> {
    fn get_into<'a>(
        &'a self,
        url: &'a str,
        range: Range<u64>,
        dst: &'a mut [u8],
    ) -> BoxFuture<'a, Result<FetchStats, FetchFailure>> {
        (**self).get_into(url, range, dst)
    }
}

/// Retries retryable failures with exponential backoff. Request counts accumulate across
/// attempts so the S3 request column stays honest.
pub struct Retrying<S> {
    inner: S,
    attempts: u32,
    backoff: Duration,
}

impl<S> Retrying<S> {
    pub fn new(inner: S, attempts: u32, backoff: Duration) -> Self {
        assert!(attempts > 0);
        Self { inner, attempts, backoff }
    }
}

impl<S: RangeSource> RangeSource for Retrying<S> {
    fn get_into<'a>(
        &'a self,
        url: &'a str,
        range: Range<u64>,
        dst: &'a mut [u8],
    ) -> BoxFuture<'a, Result<FetchStats, FetchFailure>> {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let mut requests = 0;
            let mut delay = self.backoff;
            let mut attempt = 0;
            loop {
                attempt += 1;
                match self.inner.get_into(url, range.clone(), dst).await {
                    Ok(mut s) => {
                        s.requests += requests;
                        s.elapsed = start.elapsed();
                        return Ok(s);
                    }
                    Err(e) if e.error.retryable() && attempt < self.attempts => {
                        requests += e.requests;
                        tracing::debug!(%e, attempt, ?range, "retrying ranged GET");
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                    Err(mut e) => {
                        e.requests += requests;
                        return Err(e);
                    }
                }
            }
        })
    }
}
