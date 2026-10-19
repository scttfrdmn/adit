// SPDX-License-Identifier: Apache-2.0
//! An in-process [`RemoteWriter`] for tests: "remote" agents are byte vectors in this process and
//! notifications arrive on a channel. Faults: fail every write after the first N, or hold writes
//! to chosen addresses so they complete out of order.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::writer::{RemoteWriter, WriteError};

type DelayFn = dyn Fn(u64) -> Duration + Send + Sync;

pub struct Target {
    pub base: u64,
    pub mem: Mutex<Vec<u8>>,
    tx: UnboundedSender<Vec<u8>>,
}

#[derive(Default)]
pub struct Loopback {
    targets: Mutex<HashMap<String, Arc<Target>>>,
    known: Mutex<HashSet<String>>,
    writes_left: AtomicI64,
    delay: Mutex<Option<Arc<DelayFn>>>,
}

impl Loopback {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { writes_left: AtomicI64::new(i64::MAX), ..Default::default() })
    }

    /// Create a remote agent `name` with `len` bytes at fake base address `base`.
    pub fn target(&self, name: &str, len: usize, base: u64) -> (Arc<Target>, UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = unbounded_channel();
        let t = Arc::new(Target { base, mem: Mutex::new(vec![0; len]), tx });
        self.targets.lock().unwrap().insert(name.to_string(), t.clone());
        (t, rx)
    }

    /// The metadata a loopback client sends to `PUT /_adit/agents/{name}`.
    pub fn metadata(name: &str) -> Vec<u8> {
        format!("loopback:{name}").into_bytes()
    }

    /// Every write after the next `n` fails.
    pub fn fail_writes_after(&self, n: i64) {
        self.writes_left.store(n, Ordering::SeqCst);
    }

    pub fn delay(&self, f: impl Fn(u64) -> Duration + Send + Sync + 'static) {
        *self.delay.lock().unwrap() = Some(Arc::new(f));
    }
}

impl RemoteWriter for Arc<Loopback> {
    fn load_agent(&self, metadata: &[u8]) -> Result<String, WriteError> {
        let s = std::str::from_utf8(metadata).map_err(|e| WriteError::BadMetadata(e.to_string()))?;
        let name = s.strip_prefix("loopback:").ok_or_else(|| WriteError::BadMetadata(s.into()))?;
        if !self.targets.lock().unwrap().contains_key(name) {
            return Err(WriteError::BadMetadata(format!("no loopback target {name:?}")));
        }
        self.known.lock().unwrap().insert(name.to_string());
        Ok(name.to_string())
    }

    fn knows(&self, agent: &str) -> bool {
        self.known.lock().unwrap().contains(agent)
    }

    fn write<'a>(
        &'a self,
        src: &'a [u8],
        agent: &'a str,
        remote_addr: u64,
        _dev: u64,
        notif: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), WriteError>> {
        Box::pin(async move {
            let delay = self.delay.lock().unwrap().clone();
            if let Some(f) = delay {
                tokio::time::sleep(f(remote_addr)).await;
            }
            if self.writes_left.fetch_sub(1, Ordering::SeqCst) <= 0 {
                return Err(WriteError::Failed("injected write failure".into()));
            }
            if !self.knows(agent) {
                return Err(WriteError::UnknownAgent(agent.into()));
            }
            let t = self
                .targets
                .lock()
                .unwrap()
                .get(agent)
                .cloned()
                .ok_or_else(|| WriteError::UnknownAgent(agent.into()))?;
            let off = remote_addr.checked_sub(t.base).ok_or_else(|| WriteError::Failed("below base".into()))? as usize;
            {
                let mut mem = t.mem.lock().unwrap();
                let dst =
                    mem.get_mut(off..off + src.len()).ok_or_else(|| WriteError::Failed("beyond region".into()))?;
                dst.copy_from_slice(src);
            }
            let _ = t.tx.send(notif);
            Ok(())
        })
    }

    fn backend(&self) -> &str {
        "loopback"
    }
}
