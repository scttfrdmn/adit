// SPDX-License-Identifier: Apache-2.0
//! The S3 fetch engine shared by adit (and later lith): ranged GETs against presigned URLs into
//! caller-supplied, pre-registered buffers, with per-range completion.
//!
//! This crate knows nothing about NIXL or GPUs. It splits a byte range into chunks, fetches each
//! chunk into the buffer it is given, and reports what landed.

pub mod plan;
pub mod source;

#[cfg(feature = "crt")]
pub mod crt;

#[cfg(feature = "mock")]
pub mod mock;

pub use plan::{Chunk, chunks, stripe};
pub use source::{FetchError, FetchFailure, FetchStats, RangeSource, Retrying};
