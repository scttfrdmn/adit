// SPDX-License-Identifier: Apache-2.0
//! [`RemoteWriter`] on NIXL: one agent, one backend (LIBFABRIC on EFA), the slot pool registered
//! once as DRAM, and one WRITE transfer request per chunk with a notification attached.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use nixl_sys::{
    Agent, AgentConfig, Backend, MemType, MemoryRegion, NixlDescriptor, OptArgs, RegistrationHandle, ThreadSync,
    XferDescList, XferOp, XferRequest, XferStatus,
};

use crate::pool::Pool;
use crate::writer::{RemoteWriter, WriteError};

fn nixl(e: nixl_sys::NixlError) -> WriteError {
    WriteError::Failed(format!("nixl: {e:?}"))
}

#[derive(Debug)]
struct PoolRegion {
    ptr: usize,
    len: usize,
}

impl MemoryRegion for PoolRegion {
    unsafe fn as_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }
    fn size(&self) -> usize {
        self.len
    }
}

impl NixlDescriptor for PoolRegion {
    fn mem_type(&self) -> MemType {
        MemType::Dram
    }
    fn device_id(&self) -> u64 {
        0
    }
}

/// One NIXL agent with its backend and the pool's registration; replaced whole by
/// [`RemoteWriter::recycle`].
struct Inner {
    agent: Agent,
    backend: Backend,
    _registration: RegistrationHandle,
    known: Mutex<HashSet<String>>,
}

impl Inner {
    fn new(name: &str, backend_name: &str, region: (usize, usize)) -> Result<Self, WriteError> {
        let cfg = AgentConfig { thread_sync: ThreadSync::Rw, ..AgentConfig::default() };
        let agent = Agent::new_configured(name, &cfg).map_err(nixl)?;
        let (_mems, params) = agent.get_plugin_params(backend_name).map_err(nixl)?;
        let backend = agent.create_backend(backend_name, &params).map_err(nixl)?;
        let mut opt = OptArgs::new().map_err(nixl)?;
        opt.add_backend(&backend).map_err(nixl)?;
        let registration =
            agent.register_memory(&PoolRegion { ptr: region.0, len: region.1 }, Some(&opt)).map_err(nixl)?;
        Ok(Self { agent, backend, _registration: registration, known: Mutex::default() })
    }
}

pub struct NixlWriter {
    /// `None` only if a recycle failed to create the new agent.
    inner: Mutex<Option<Arc<Inner>>>,
    name: String,
    backend_name: String,
    region: (usize, usize),
    /// A write still in progress after this long is reported failed rather than polled forever
    /// (#18: an EFA write error left a transfer `InProgress` indefinitely).
    deadline: Duration,
}

impl NixlWriter {
    /// Create agent `name` with backend `backend_name` (e.g. `LIBFABRIC`) and register `pool`.
    /// The pool must outlive the writer.
    pub fn new(name: &str, backend_name: &str, pool: &Pool) -> Result<Self, WriteError> {
        let (ptr, len) = pool.region();
        let region = (ptr as usize, len);
        let inner = Inner::new(name, backend_name, region)?;
        Ok(Self {
            inner: Mutex::new(Some(Arc::new(inner))),
            name: name.into(),
            backend_name: backend_name.into(),
            region,
            deadline: Duration::from_secs(30),
        })
    }

    fn cur(&self) -> Result<Arc<Inner>, WriteError> {
        self.inner.lock().unwrap().clone().ok_or_else(|| WriteError::Failed("no NIXL agent (recycle failed)".into()))
    }

    /// Fail writes still in progress after `d`.
    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }

    /// This proxy's own metadata (for diagnostics; clients do not need it for push).
    pub fn local_metadata(&self) -> Result<Vec<u8>, WriteError> {
        self.cur()?.agent.get_local_md().map_err(nixl)
    }

    fn post(
        &self,
        inner: &Inner,
        src: &[u8],
        agent: &str,
        remote_addr: u64,
        dev: u64,
        notif: &[u8],
    ) -> Result<Option<XferRequest>, WriteError> {
        let mut local = XferDescList::new(MemType::Dram).map_err(nixl)?;
        local.add_desc(src.as_ptr() as usize, src.len(), 0);
        let mut remote = XferDescList::new(MemType::Vram).map_err(nixl)?;
        remote.add_desc(remote_addr as usize, src.len(), dev);
        let mut opt = OptArgs::new().map_err(nixl)?;
        opt.add_backend(&inner.backend).map_err(nixl)?;
        opt.set_notification_message(notif).map_err(nixl)?;
        opt.set_has_notification(true).map_err(nixl)?;
        let req = inner.agent.create_xfer_req(XferOp::Write, &local, &remote, agent, Some(&opt)).map_err(nixl)?;
        let in_progress = inner.agent.post_xfer_req(&req, Some(&opt)).map_err(nixl)?;
        Ok(in_progress.then_some(req))
    }
}

impl RemoteWriter for NixlWriter {
    fn load_agent(&self, metadata: &[u8]) -> Result<String, WriteError> {
        let inner = self.cur()?;
        let name = inner.agent.load_remote_md(metadata).map_err(|e| WriteError::BadMetadata(format!("{e:?}")))?;
        inner.known.lock().unwrap().insert(name.clone());
        Ok(name)
    }

    fn knows(&self, agent: &str) -> bool {
        self.cur().is_ok_and(|i| i.known.lock().unwrap().contains(agent))
    }

    fn forget_agent(&self, agent: &str) -> Result<(), WriteError> {
        let inner = self.cur()?;
        inner.known.lock().unwrap().remove(agent);
        inner.agent.invalidate_remote_md(agent).map_err(nixl)
    }

    fn recycle(&self) -> Result<(), WriteError> {
        let mut slot = self.inner.lock().unwrap();
        if let Some(old) = slot.take() {
            if Arc::strong_count(&old) > 1 {
                *slot = Some(old);
                return Err(WriteError::Failed("NIXL agent in use; not recycled".into()));
            }
            // The old agent (and its rails, endpoints and registration) is gone before the new one,
            // with the same name, is created.
            drop(old);
        }
        *slot = Some(Arc::new(Inner::new(&self.name, &self.backend_name, self.region)?));
        Ok(())
    }

    fn write<'a>(
        &'a self,
        src: &'a [u8],
        agent: &'a str,
        remote_addr: u64,
        dev: u64,
        notif: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), WriteError>> {
        Box::pin(async move {
            let inner = self.cur()?;
            if !inner.known.lock().unwrap().contains(agent) {
                return Err(WriteError::UnknownAgent(agent.into()));
            }
            let Some(req) = self.post(&inner, src, agent, remote_addr, dev, &notif)? else {
                return Ok(());
            };
            // NIXL's progress thread drives the transfer; poll its status, yielding at first and
            // backing off to short sleeps so a long write does not pin a runtime thread.
            let mut spins = 0u32;
            let until = tokio::time::Instant::now() + self.deadline;
            loop {
                if tokio::time::Instant::now() >= until {
                    return Err(WriteError::Failed(format!(
                        "write of {} bytes to {agent} still in progress after {:?}",
                        src.len(),
                        self.deadline
                    )));
                }
                match inner.agent.get_xfer_status(&req).map_err(nixl)? {
                    XferStatus::Success => return Ok(()),
                    XferStatus::InProgress if spins < 64 => {
                        spins += 1;
                        tokio::task::yield_now().await;
                    }
                    XferStatus::InProgress => tokio::time::sleep(Duration::from_micros(20)).await,
                }
            }
        })
    }

    fn backend(&self) -> &str {
        &self.backend_name
    }
}
