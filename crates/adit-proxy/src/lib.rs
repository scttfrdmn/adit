// SPDX-License-Identifier: Apache-2.0
//! adit-proxy: fetch S3 ranges and RDMA-write them into a client's registered GPU buffer.

pub mod http;
pub mod loopback;
pub mod metrics;
pub mod nixl;
pub mod pipeline;
pub mod pool;
pub mod writer;
