// SPDX-License-Identifier: Apache-2.0
//! Where landed chunks go: a one-sided write into a remote agent's registered memory, carrying a
//! completion notification.

use futures::future::BoxFuture;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum WriteError {
    #[error("unknown remote agent {0:?}; register it first")]
    UnknownAgent(String),
    #[error("bad agent metadata: {0}")]
    BadMetadata(String),
    #[error("write failed: {0}")]
    Failed(String),
}

pub trait RemoteWriter: Send + Sync + 'static {
    /// Load a remote agent's serialized metadata; returns the agent's name.
    fn load_agent(&self, metadata: &[u8]) -> Result<String, WriteError>;

    fn knows(&self, agent: &str) -> bool;

    /// Write `src` (which lies inside this proxy's registered pool) to `agent`'s memory at
    /// `remote_addr` on device `dev`, delivering `notif` to the agent once the bytes are there.
    fn write<'a>(
        &'a self,
        src: &'a [u8],
        agent: &'a str,
        remote_addr: u64,
        dev: u64,
        notif: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), WriteError>>;

    /// Backend name for `/metrics` and results rows.
    fn backend(&self) -> &str;
}
