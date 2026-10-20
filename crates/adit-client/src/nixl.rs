// SPDX-License-Identifier: Apache-2.0
//! The client's NIXL agent: registers the destination (VRAM on the GPU host; DRAM is allowed for
//! diagnostics), serializes metadata for proxies, and is the notification source.

use std::sync::Arc;

use nixl_sys::{
    Agent, AgentConfig, Backend, MemType, MemoryRegion, NixlDescriptor, NotificationMap, OptArgs, RegistrationHandle,
    ThreadSync, XferDescList, XferOp, XferStatus,
};

use crate::completion::NotifSource;

#[derive(Debug)]
struct Region {
    addr: u64,
    len: usize,
    dev: u64,
    mem: MemType,
}

impl MemoryRegion for Region {
    unsafe fn as_ptr(&self) -> *const u8 {
        self.addr as *const u8
    }
    fn size(&self) -> usize {
        self.len
    }
}

impl NixlDescriptor for Region {
    fn mem_type(&self) -> MemType {
        self.mem
    }
    fn device_id(&self) -> u64 {
        self.dev
    }
}

pub struct ClientAgent {
    agent: Agent,
    _backend: Backend,
    _registrations: Vec<RegistrationHandle>,
    name: String,
}

#[derive(Debug, thiserror::Error)]
#[error("nixl: {0}")]
pub struct NixlFailure(pub String);

fn nixl(e: nixl_sys::NixlError) -> NixlFailure {
    NixlFailure(format!("{e:?}"))
}

impl ClientAgent {
    /// Create agent `name` on `backend` and register `len` bytes at `addr` (device `dev`).
    pub fn new(
        name: &str,
        backend: &str,
        addr: u64,
        len: usize,
        dev: u64,
        vram: bool,
    ) -> Result<Arc<Self>, NixlFailure> {
        Self::with_progress_delay(name, backend, addr, len, dev, vram, 0)
    }

    /// As [`ClientAgent::new`], with NIXL's progress-thread delay between polls. NIXL's default of 0
    /// busy-polls for the agent's lifetime; #17 measures whether that is the client's core.
    pub fn with_progress_delay(
        name: &str,
        backend: &str,
        addr: u64,
        len: usize,
        dev: u64,
        vram: bool,
        progress_delay_us: u64,
    ) -> Result<Arc<Self>, NixlFailure> {
        Self::with_regions(name, backend, &[(addr, len, dev)], vram, progress_delay_us)
    }

    /// One agent registering several regions `(addr, len, dev)`, e.g. one buffer per GPU of a
    /// multi-GPU client (G2). Proxies see them all in one metadata blob.
    pub fn with_regions(
        name: &str,
        backend: &str,
        regions: &[(u64, usize, u64)],
        vram: bool,
        progress_delay_us: u64,
    ) -> Result<Arc<Self>, NixlFailure> {
        if nixl_sys::is_stub() {
            return Err(NixlFailure("built against the NIXL stub API".into()));
        }
        if regions.is_empty() {
            return Err(NixlFailure("no regions to register".into()));
        }
        let cfg =
            AgentConfig { thread_sync: ThreadSync::Rw, pthr_delay_us: progress_delay_us, ..AgentConfig::default() };
        let agent = Agent::new_configured(name, &cfg).map_err(nixl)?;
        let (_mems, params) = agent.get_plugin_params(backend).map_err(nixl)?;
        let b = agent.create_backend(backend, &params).map_err(nixl)?;
        let mut opt = OptArgs::new().map_err(nixl)?;
        opt.add_backend(&b).map_err(nixl)?;
        let mem = if vram { MemType::Vram } else { MemType::Dram };
        let registrations = regions
            .iter()
            .map(|&(addr, len, dev)| agent.register_memory(&Region { addr, len, dev, mem }, Some(&opt)).map_err(nixl))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(Self { agent, _backend: b, _registrations: registrations, name: name.into() }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Load a peer's metadata (for client-initiated READ); returns its agent name.
    pub fn load_remote(&self, md: &[u8]) -> Result<String, NixlFailure> {
        self.agent.load_remote_md(md).map_err(nixl)
    }

    /// Client-initiated pull: READ `len` bytes at `remote_addr` (DRAM on `remote`) into this
    /// agent's registered VRAM at `local_addr`, blocking until done. The fallback the design
    /// names for EFA generations without hardware RDMA write (#20).
    pub fn read(
        &self,
        local_addr: u64,
        dev: u64,
        remote: &str,
        remote_addr: u64,
        len: usize,
        deadline: std::time::Duration,
    ) -> Result<(), NixlFailure> {
        let mut local = XferDescList::new(MemType::Vram).map_err(nixl)?;
        local.add_desc(local_addr as usize, len, dev);
        let mut far = XferDescList::new(MemType::Dram).map_err(nixl)?;
        far.add_desc(remote_addr as usize, len, 0);
        let mut opt = OptArgs::new().map_err(nixl)?;
        opt.add_backend(&self._backend).map_err(nixl)?;
        let req = self.agent.create_xfer_req(XferOp::Read, &local, &far, remote, Some(&opt)).map_err(nixl)?;
        if !self.agent.post_xfer_req(&req, Some(&opt)).map_err(nixl)? {
            return Ok(());
        }
        let until = std::time::Instant::now() + deadline;
        loop {
            match self.agent.get_xfer_status(&req).map_err(nixl)? {
                XferStatus::Success => return Ok(()),
                XferStatus::InProgress if std::time::Instant::now() < until => std::thread::yield_now(),
                XferStatus::InProgress => {
                    return Err(NixlFailure(format!("read of {len} bytes still in progress after {deadline:?}")));
                }
            }
        }
    }

    /// Serialized metadata (connection info plus registered memory) for proxies.
    pub fn metadata(&self) -> Result<Vec<u8>, NixlFailure> {
        self.agent.get_local_md().map_err(nixl)
    }
}

impl NotifSource for ClientAgent {
    fn poll(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut map = NotificationMap::new().map_err(|e| format!("{e:?}"))?;
        self.agent.get_notifications(&mut map, None).map_err(|e| format!("{e:?}"))?;
        let all = map.take_notifs().map_err(|e| format!("{e:?}"))?;
        Ok(all.into_values().flatten().map(String::into_bytes).collect())
    }
}
