// SPDX-License-Identifier: Apache-2.0
//! [`RemoteWriter`] on NIXL: one agent, one backend (LIBFABRIC on EFA), the slot pool registered
//! once as DRAM, and one WRITE transfer request per chunk with a notification attached.

use std::collections::HashSet;
use std::sync::Mutex;
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

pub struct NixlWriter {
    agent: Agent,
    backend: Backend,
    backend_name: String,
    _registration: RegistrationHandle,
    known: Mutex<HashSet<String>>,
}

impl NixlWriter {
    /// Create agent `name` with backend `backend_name` (e.g. `LIBFABRIC`) and register `pool`.
    /// The pool must outlive the writer.
    pub fn new(name: &str, backend_name: &str, pool: &Pool) -> Result<Self, WriteError> {
        let cfg = AgentConfig { thread_sync: ThreadSync::Rw, ..AgentConfig::default() };
        let agent = Agent::new_configured(name, &cfg).map_err(nixl)?;
        let (_mems, params) = agent.get_plugin_params(backend_name).map_err(nixl)?;
        let backend = agent.create_backend(backend_name, &params).map_err(nixl)?;
        let (ptr, len) = pool.region();
        let mut opt = OptArgs::new().map_err(nixl)?;
        opt.add_backend(&backend).map_err(nixl)?;
        let registration = agent.register_memory(&PoolRegion { ptr: ptr as usize, len }, Some(&opt)).map_err(nixl)?;
        Ok(Self {
            agent,
            backend,
            backend_name: backend_name.into(),
            _registration: registration,
            known: Mutex::default(),
        })
    }

    /// This proxy's own metadata (for diagnostics; clients do not need it for push).
    pub fn local_metadata(&self) -> Result<Vec<u8>, WriteError> {
        self.agent.get_local_md().map_err(nixl)
    }

    fn post(
        &self,
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
        opt.add_backend(&self.backend).map_err(nixl)?;
        opt.set_notification_message(notif).map_err(nixl)?;
        opt.set_has_notification(true).map_err(nixl)?;
        let req = self.agent.create_xfer_req(XferOp::Write, &local, &remote, agent, Some(&opt)).map_err(nixl)?;
        let in_progress = self.agent.post_xfer_req(&req, Some(&opt)).map_err(nixl)?;
        Ok(in_progress.then_some(req))
    }
}

impl RemoteWriter for NixlWriter {
    fn load_agent(&self, metadata: &[u8]) -> Result<String, WriteError> {
        let name = self.agent.load_remote_md(metadata).map_err(|e| WriteError::BadMetadata(format!("{e:?}")))?;
        self.known.lock().unwrap().insert(name.clone());
        Ok(name)
    }

    fn knows(&self, agent: &str) -> bool {
        self.known.lock().unwrap().contains(agent)
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
            if !self.knows(agent) {
                return Err(WriteError::UnknownAgent(agent.into()));
            }
            let Some(req) = self.post(src, agent, remote_addr, dev, &notif)? else {
                return Ok(());
            };
            // NIXL's progress thread drives the transfer; poll its status, yielding at first and
            // backing off to short sleeps so a long write does not pin a runtime thread.
            let mut spins = 0u32;
            loop {
                match self.agent.get_xfer_status(&req).map_err(nixl)? {
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
