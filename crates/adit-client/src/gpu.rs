// SPDX-License-Identifier: Apache-2.0
//! GPU side: device buffers, pinned staging for the copy paths, and the per-part SHA-256 kernel.
//!
//! The CUDA driver and NVRTC are loaded at runtime (`cudarc` dynamic loading), so this compiles
//! and links on machines without CUDA; calling into it there fails with a driver error.

use std::sync::Arc;

use cudarc::driver::{CudaContext, CudaFunction, CudaSlice, CudaStream, DevicePtr, LaunchConfig, PushKernelArg};

use crate::oracle::Digest32;

const KERNEL: &str = include_str!("kernels/sha256_parts.cu");

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("CUDA driver: {0}")]
    Driver(#[from] cudarc::driver::DriverError),
    #[error("NVRTC: {0}")]
    Compile(#[from] cudarc::nvrtc::CompileError),
    #[error("{0}")]
    Other(String),
}

pub struct Gpu {
    ctx: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    sha: CudaFunction,
    sha_segs: CudaFunction,
}

/// A device allocation. `addr()` is what gets registered with NIXL and sent to proxies.
pub struct GpuBuffer {
    slice: CudaSlice<u8>,
    stream: Arc<CudaStream>,
}

impl GpuBuffer {
    pub fn addr(&self) -> u64 {
        let (p, _sync) = self.slice.device_ptr(&self.stream);
        p
    }

    pub fn len(&self) -> usize {
        self.slice.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slice.len() == 0
    }

    pub fn ordinal(&self) -> usize {
        self.slice.ordinal()
    }

    pub fn slice_mut(&mut self) -> &mut CudaSlice<u8> {
        &mut self.slice
    }
}

impl Gpu {
    pub fn new(ordinal: usize) -> Result<Self, GpuError> {
        let ctx = CudaContext::new(ordinal)?;
        let stream = ctx.default_stream();
        let ptx = cudarc::nvrtc::compile_ptx(KERNEL)?;
        let module = ctx.load_module(ptx)?;
        let sha = module.load_function("sha256_parts")?;
        let sha_segs = module.load_function("sha256_segs")?;
        Ok(Self { ctx, stream, sha, sha_segs })
    }

    pub fn name(&self) -> String {
        self.ctx.name().unwrap_or_default()
    }

    pub fn ordinal(&self) -> usize {
        self.ctx.ordinal()
    }

    /// Allocate `len` bytes of device memory, zeroed (so a missing write shows as a mismatch, not
    /// as stale bytes from a previous run).
    pub fn alloc(&self, len: usize) -> Result<GpuBuffer, GpuError> {
        Ok(GpuBuffer { slice: self.stream.alloc_zeros::<u8>(len)?, stream: self.stream.clone() })
    }

    /// Zero a buffer between runs.
    pub fn zero(&self, buf: &mut GpuBuffer) -> Result<(), GpuError> {
        self.stream.memset_zeros(&mut buf.slice)?;
        self.stream.synchronize()?;
        Ok(())
    }

    /// Pinned host staging buffer for the copy paths (A, B).
    pub fn alloc_pinned(&self, len: usize) -> Result<cudarc::driver::PinnedHostSlice<u8>, GpuError> {
        // SAFETY: contents are written before they are read.
        Ok(unsafe { self.ctx.alloc_pinned::<u8>(len) }?)
    }

    /// Copy `src` (pinned) into `dst` at byte offset `off`, then wait.
    pub fn upload(
        &self,
        src: &cudarc::driver::PinnedHostSlice<u8>,
        n: usize,
        dst: &mut GpuBuffer,
        off: usize,
    ) -> Result<(), GpuError> {
        let host = src.as_slice()?;
        let mut view = dst.slice.slice_mut(off..off + n);
        self.stream.memcpy_htod(&host[..n], &mut view)?;
        self.stream.synchronize()?;
        Ok(())
    }

    /// SHA-256 of many objects' parts in one launch: `objects` are `(addr, len, part)`; the result
    /// is each object's part digests, in order. A wave of 80 one-GiB objects is 10,240 parts, which
    /// fills the GPU where one launch per object (128 threads) leaves it idle.
    pub fn sha256_parts_many(&self, objects: &[(u64, u64, u64)]) -> Result<Vec<Vec<Digest32>>, GpuError> {
        let (mut starts, mut lens, mut counts) = (Vec::new(), Vec::new(), Vec::new());
        for &(addr, len, part) in objects {
            if part == 0 {
                return Err(GpuError::Other("part size 0".into()));
            }
            let n = len.div_ceil(part);
            for k in 0..n {
                starts.push(addr + k * part);
                lens.push(part.min(len - k * part));
            }
            counts.push(n as usize);
        }
        let nparts = starts.len() as u64;
        if nparts == 0 {
            return Ok(counts.iter().map(|_| vec![]).collect());
        }
        let d_starts = self.stream.clone_htod(&starts)?;
        let d_lens = self.stream.clone_htod(&lens)?;
        let mut out = self.stream.alloc_zeros::<u8>(nparts as usize * 32)?;
        let threads = 128u32;
        let cfg = LaunchConfig {
            grid_dim: ((nparts as u32).div_ceil(threads), 1, 1),
            block_dim: (threads, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut b = self.stream.launch_builder(&self.sha_segs);
        b.arg(&d_starts).arg(&d_lens).arg(&nparts).arg(&mut out);
        // SAFETY: thread i reads lens[i] bytes at starts[i] (caller's contract: device memory in
        // this context) and writes 32 bytes at out[32 i], which has room for nparts digests.
        unsafe { b.launch(cfg) }?;
        let bytes = self.stream.clone_dtoh(&out)?;
        let all: Vec<Digest32> = bytes.as_chunks::<32>().0.to_vec();
        let mut res = Vec::with_capacity(counts.len());
        let mut at = 0;
        for n in counts {
            res.push(all[at..at + n].to_vec());
            at += n;
        }
        Ok(res)
    }

    /// SHA-256 of every `part`-byte part of the `len` bytes at device address `addr`, computed on
    /// the GPU. `addr` may be any device pointer in this context (ours, cupy's, KvikIO's).
    pub fn sha256_parts(&self, addr: u64, len: u64, part: u64) -> Result<Vec<Digest32>, GpuError> {
        if part == 0 {
            return Err(GpuError::Other("part size 0".into()));
        }
        let nparts = len.div_ceil(part);
        if nparts == 0 {
            return Ok(vec![]);
        }
        let mut out = self.stream.alloc_zeros::<u8>(nparts as usize * 32)?;
        let threads = 128u32;
        let cfg = LaunchConfig {
            grid_dim: ((nparts as u32).div_ceil(threads), 1, 1),
            block_dim: (threads, 1, 1),
            shared_mem_bytes: 0,
        };
        let mut b = self.stream.launch_builder(&self.sha);
        b.arg(&addr).arg(&len).arg(&part).arg(&nparts).arg(&mut out);
        // SAFETY: the kernel reads `len` bytes at `addr` (caller's contract) and writes
        // `32 * nparts` bytes into `out`, which is that size.
        unsafe { b.launch(cfg) }?;
        let bytes = self.stream.clone_dtoh(&out)?;
        Ok(bytes.as_chunks::<32>().0.to_vec())
    }
}
