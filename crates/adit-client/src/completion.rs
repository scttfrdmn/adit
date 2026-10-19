// SPDX-License-Identifier: Apache-2.0
//! Completion accounting from data-plane notifications.
//!
//! A pump task polls the notification source (the NIXL agent, or a test channel) and routes each
//! `adit1 <req> <offset> <len>` to its request's [`Coverage`]. Notifications for unknown requests
//! (late arrivals after a partial request closed) and overlapping coverage are counted: both
//! indicate a defect or a fault and are reported, never ignored.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adit_proto::{Coverage, Notif};
use tokio::sync::Notify;
use tokio::time::Instant;

pub trait NotifSource: Send + Sync + 'static {
    /// Every notification that has arrived since the last call; empty if none.
    fn poll(&self) -> Result<Vec<Vec<u8>>, String>;
}

/// A channel-backed source, for tests and the loopback writer.
pub struct ChannelSource(pub Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>);

impl NotifSource for ChannelSource {
    fn poll(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut rx = self.0.lock().unwrap();
        let mut out = Vec::new();
        while let Ok(n) = rx.try_recv() {
            out.push(n);
        }
        Ok(out)
    }
}

#[derive(Default)]
struct Entry {
    cov: Coverage,
    overlap: u64,
}

#[derive(Default)]
pub struct Tracker {
    open: Mutex<HashMap<u64, Entry>>,
    changed: Notify,
    pub stray: AtomicU64,
    pub malformed: AtomicU64,
    pub notifications: AtomicU64,
}

impl Tracker {
    pub fn open(&self, req: u64) {
        self.open.lock().unwrap().insert(req, Entry::default());
    }

    /// Close a request; returns its coverage and overlapping bytes.
    pub fn close(&self, req: u64) -> (Coverage, u64) {
        let e = self.open.lock().unwrap().remove(&req).unwrap_or_default();
        (e.cov, e.overlap)
    }

    pub fn feed(&self, raw: &[u8]) {
        self.notifications.fetch_add(1, Relaxed);
        let Ok(n) = Notif::decode(raw) else {
            self.malformed.fetch_add(1, Relaxed);
            return;
        };
        let mut open = self.open.lock().unwrap();
        match open.get_mut(&n.req) {
            Some(e) => e.overlap += e.cov.insert(n.offset..n.offset + n.len),
            None => {
                self.stray.fetch_add(1, Relaxed);
            }
        }
        drop(open);
        self.changed.notify_waiters();
    }

    fn snapshot(&self, req: u64) -> Coverage {
        self.open.lock().unwrap().get(&req).map(|e| e.cov.clone()).unwrap_or_default()
    }

    /// Wait until `within` is covered for `req`, or until `deadline`. Returns the coverage seen.
    pub async fn wait(&self, req: u64, within: Range<u64>, deadline: Instant) -> Coverage {
        loop {
            let notified = self.changed.notified();
            let cov = self.snapshot(req);
            if cov.is_complete(within.clone()) || Instant::now() >= deadline {
                return cov;
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    /// Poll `src` forever, feeding notifications in. Polls back off to `idle` when quiet.
    pub fn pump(self: &Arc<Self>, src: Arc<dyn NotifSource>, idle: Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match src.poll() {
                    Ok(v) if v.is_empty() => tokio::time::sleep(idle).await,
                    Ok(v) => v.iter().for_each(|n| me.feed(n)),
                    Err(e) => {
                        tracing::error!(%e, "notification poll failed");
                        tokio::time::sleep(idle * 100).await;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_counts_strays_and_overlap() {
        let t = Tracker::default();
        t.open(1);
        t.feed(&Notif { req: 1, offset: 8, len: 8 }.encode());
        t.feed(&Notif { req: 1, offset: 0, len: 8 }.encode());
        t.feed(&Notif { req: 1, offset: 4, len: 4 }.encode());
        t.feed(&Notif { req: 9, offset: 0, len: 8 }.encode());
        t.feed(b"junk");
        let (cov, overlap) = t.close(1);
        assert!(cov.is_complete(0..16));
        assert_eq!(overlap, 4);
        assert_eq!(t.stray.load(Relaxed), 1);
        assert_eq!(t.malformed.load(Relaxed), 1);
    }

    #[tokio::test]
    async fn wait_returns_on_completion_or_deadline() {
        let t = Arc::new(Tracker::default());
        t.open(2);
        let t2 = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            t2.feed(&Notif { req: 2, offset: 0, len: 4 }.encode());
        });
        let cov = t.wait(2, 0..4, Instant::now() + Duration::from_secs(5)).await;
        assert!(cov.is_complete(0..4));
        let start = Instant::now();
        let cov = t.wait(2, 0..8, Instant::now() + Duration::from_millis(30)).await;
        assert!(!cov.is_complete(0..8));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }
}
