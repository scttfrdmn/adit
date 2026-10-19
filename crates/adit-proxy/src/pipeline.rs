// SPDX-License-Identifier: Apache-2.0
//! One transfer: split the owned object range into chunks; for each, take a slot, fetch the range
//! from S3 into it, RDMA-write it to its offset in the client buffer with a notification, release
//! the slot. Chunks run concurrently up to the pool size and complete in any order.

use std::ops::Range;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use adit_fetch::{RangeSource, chunks};
use adit_proto::{Dest, Notif};
use futures::StreamExt;

use crate::metrics::Metrics;
use crate::pool::Pool;
use crate::writer::RemoteWriter;

#[derive(Debug, Clone)]
pub struct Transfer {
    pub url: String,
    /// Object bytes this proxy owns.
    pub range: Range<u64>,
    pub agent: String,
    pub dest: Dest,
    /// Offset in `dest` of the first byte of `range`.
    pub dest_offset: u64,
    pub req: u64,
    pub chunk: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub bytes_written: u64,
    pub s3_requests: u64,
    /// Object ranges not written.
    pub failed: Vec<Range<u64>>,
    /// First few error messages, for the log and the response.
    pub errors: Vec<String>,
}

impl Transfer {
    /// The transfer stays inside the registered destination.
    pub fn validate(&self) -> Result<(), String> {
        let len = self.range.end.checked_sub(self.range.start).ok_or("empty or inverted range")?;
        let end = self.dest_offset.checked_add(len).ok_or("dest offset overflow")?;
        if end > self.dest.len {
            return Err(format!(
                "range of {len} bytes at dest offset {} overruns dest of {}",
                self.dest_offset, self.dest.len
            ));
        }
        if self.chunk == 0 {
            return Err("chunk size 0".into());
        }
        Ok(())
    }
}

pub async fn run<S: RangeSource + ?Sized, W: RemoteWriter + ?Sized>(
    t: &Transfer,
    src: &S,
    writer: &W,
    pool: &Pool,
    m: &Metrics,
) -> Outcome {
    run_bounded(t, src, writer, pool, m, None).await
}

/// As [`run`], with at most `write_inflight` RDMA writes outstanding at once (fetches still use
/// every slot). NIXL write throughput falls as outstanding writes grow (#18), so the two
/// concurrencies are separate knobs.
pub async fn run_bounded<S: RangeSource + ?Sized, W: RemoteWriter + ?Sized>(
    t: &Transfer,
    src: &S,
    writer: &W,
    pool: &Pool,
    m: &Metrics,
    write_inflight: Option<&tokio::sync::Semaphore>,
) -> Outcome {
    let chunk = t.chunk.min(pool.slot_size() as u64);
    let plan = chunks(t.range.clone(), chunk);
    let results = futures::stream::iter(plan)
        .map(|c| async move {
            let len = c.len as usize;
            let mut slot = pool.acquire().await;
            let t0 = Instant::now();
            let fetched = src.get_into(&t.url, c.range(), &mut slot[..len]).await;
            let st = match fetched {
                Ok(st) => st,
                Err(e) => {
                    return (c, e.requests as u64, Err(format!("fetch {:?}: {e}", c.range())));
                }
            };
            m.fetch_ns.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
            m.first_byte_ns.fetch_add(st.first_byte.as_nanos() as u64, Relaxed);
            m.bytes_fetched.fetch_add(st.bytes, Relaxed);
            let off = t.dest_offset + (c.offset - t.range.start);
            let notif = Notif { req: t.req, offset: off, len: c.len }.encode();
            let _permit = match write_inflight {
                Some(sem) => Some(sem.acquire().await.expect("write semaphore closed")),
                None => None,
            };
            let t1 = Instant::now();
            let wrote = writer.write(&slot[..len], &t.agent, t.dest.addr + off, t.dest.dev, notif).await;
            m.write_ns.fetch_add(t1.elapsed().as_nanos() as u64, Relaxed);
            (c, st.requests as u64, wrote.map_err(|e| format!("write {:?}: {e}", c.range())))
        })
        .buffer_unordered(pool.slots())
        .collect::<Vec<_>>()
        .await;

    let mut out = Outcome::default();
    for (c, reqs, r) in results {
        out.s3_requests += reqs;
        match r {
            Ok(()) => {
                out.bytes_written += c.len;
                m.chunks_ok.fetch_add(1, Relaxed);
            }
            Err(e) => {
                m.chunks_failed.fetch_add(1, Relaxed);
                out.failed.push(c.range());
                if out.errors.len() < 4 {
                    out.errors.push(e);
                }
            }
        }
    }
    out.failed.sort_by_key(|r| r.start);
    m.s3_requests.fetch_add(out.s3_requests, Relaxed);
    m.bytes_written.fetch_add(out.bytes_written, Relaxed);
    out
}
