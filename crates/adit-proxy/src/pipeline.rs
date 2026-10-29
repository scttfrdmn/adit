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

/// One chunk's life, nanoseconds from the start of its transfer. Zero where a stage never ran.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChunkTiming {
    pub offset: u64,
    pub len: u64,
    pub requests: u32,
    pub slot_ns: u64,
    pub first_byte_ns: u64,
    pub fetched_ns: u64,
    pub write_start_ns: u64,
    pub written_ns: u64,
    pub ok: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub bytes_written: u64,
    pub s3_requests: u64,
    /// Object ranges not written.
    pub failed: Vec<Range<u64>>,
    /// First few error messages, for the log and the response.
    pub errors: Vec<String>,
    /// Every chunk's stage times, in completion order.
    pub timings: Vec<ChunkTiming>,
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

/// Counts a held slot in [`Metrics::slots_in_use`] and its hold time in
/// [`Metrics::slot_hold_ns`]; dropped with the chunk's future, alongside the slot, on every path.
struct Held<'a> {
    m: &'a Metrics,
    since: Instant,
}

impl<'a> Held<'a> {
    fn new(m: &'a Metrics, since: Instant) -> Self {
        m.slots_in_use.fetch_add(1, Relaxed);
        Self { m, since }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.m.slot_hold_ns.fetch_add(self.since.elapsed().as_nanos() as u64, Relaxed);
        self.m.slots_in_use.fetch_sub(1, Relaxed);
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
    let start = Instant::now();
    let ns = move |i: Instant| i.duration_since(start).as_nanos() as u64;
    let results = futures::stream::iter(plan)
        .map(|c| async move {
            let len = c.len as usize;
            let mut tm = ChunkTiming { offset: c.offset, len: c.len, ..Default::default() };
            let q = Instant::now();
            let mut slot = pool.acquire().await;
            let t0 = Instant::now();
            let _held = Held::new(m, t0);
            m.h_slot_wait.observe(t0 - q);
            tm.slot_ns = ns(t0);
            let fetched = src.get_into(&t.url, c.range(), &mut slot[..len]).await;
            let st = match fetched {
                Ok(st) => st,
                Err(e) => {
                    tm.requests = e.requests;
                    return (c, tm, Err(format!("fetch {:?}: {e}", c.range())));
                }
            };
            let t_f = Instant::now();
            tm.requests = st.requests;
            tm.first_byte_ns = ns(t0 + st.first_byte);
            tm.fetched_ns = ns(t_f);
            m.h_fetch.observe(t_f - t0);
            m.fetch_ns.fetch_add((t_f - t0).as_nanos() as u64, Relaxed);
            m.first_byte_ns.fetch_add(st.first_byte.as_nanos() as u64, Relaxed);
            m.bytes_fetched.fetch_add(st.bytes, Relaxed);
            let off = t.dest_offset + (c.offset - t.range.start);
            let notif = Notif { req: t.req, offset: off, len: c.len }.encode();
            let _permit = match write_inflight {
                Some(sem) => Some(sem.acquire().await.expect("write semaphore closed")),
                None => None,
            };
            let t1 = Instant::now();
            m.h_write_wait.observe(t1 - t_f);
            tm.write_start_ns = ns(t1);
            let wrote = writer.write(&slot[..len], &t.agent, t.dest.addr + off, t.dest.dev, notif).await;
            let t2 = Instant::now();
            tm.written_ns = ns(t2);
            m.h_write.observe(t2 - t1);
            m.write_ns.fetch_add((t2 - t1).as_nanos() as u64, Relaxed);
            tm.ok = wrote.is_ok();
            (c, tm, wrote.map_err(|e| format!("write {:?}: {e}", c.range())))
        })
        .buffer_unordered(pool.slots())
        .collect::<Vec<_>>()
        .await;

    let mut out = Outcome::default();
    for (c, tm, r) in results {
        out.s3_requests += tm.requests as u64;
        out.timings.push(tm);
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
