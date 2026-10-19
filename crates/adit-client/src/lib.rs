// SPDX-License-Identifier: Apache-2.0
//! adit-client: register a GPU buffer, have adit proxies RDMA-write objects into it, know exactly
//! what landed, and check it on the GPU against the oracle.

pub mod client;
pub mod completion;
pub mod ffi;
pub mod gpu;
pub mod nixl;
pub mod oracle;

pub use client::{Client, Config, GetError, Report};
pub mod counters;
pub mod csvrow;
