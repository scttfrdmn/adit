// SPDX-License-Identifier: Apache-2.0
//! Hardware tests: need a CUDA GPU. Run on instances with `make test-hw`.

use adit_client::gpu::Gpu;
use adit_client::oracle::sha256_parts_cpu;

#[test]
#[ignore = "needs a CUDA GPU"]
fn gpu_part_digests_equal_cpu_at_every_padding_edge() {
    let gpu = Gpu::new(0).expect("CUDA device 0");
    for &(len, part) in &[
        (1usize, 64usize),
        (55, 64),
        (56, 64),
        (63, 64),
        (64, 64),
        (65, 64),
        (119, 100),
        (120, 100),
        (1000, 4096),
        (8 << 20, 1 << 20),
        ((8 << 20) + 13, 1 << 20),
    ] {
        let host: Vec<u8> = (0..len).map(|i| (i as u32).wrapping_mul(2654435761).rotate_right(13) as u8).collect();
        let pinned = gpu.alloc_pinned(len).unwrap();
        // SAFETY: pinned buffer of `len` bytes, written before upload.
        unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), pinned.as_ptr().unwrap() as *mut u8, len) };
        let mut dev = gpu.alloc(len).unwrap();
        gpu.upload(&pinned, len, &mut dev, 0).unwrap();
        let got = gpu.sha256_parts(dev.addr(), len as u64, part as u64).unwrap();
        assert_eq!(got, sha256_parts_cpu(&host, part), "len={len} part={part}");
    }
}

#[test]
#[ignore = "needs a CUDA GPU"]
fn batched_digests_equal_per_object_digests() {
    let gpu = Gpu::new(0).expect("CUDA device 0");
    let sizes: [usize; 4] = [(3 << 20) + 5, 1 << 20, 7, (2 << 20) + 4096];
    let total: usize = sizes.iter().map(|s| s.div_ceil(4096) * 4096).sum();
    let host: Vec<u8> = (0..total).map(|i| (i as u32).wrapping_mul(2654435761).rotate_right(7) as u8).collect();
    let pinned = gpu.alloc_pinned(total).unwrap();
    // SAFETY: pinned buffer of `total` bytes, written before upload.
    unsafe { std::ptr::copy_nonoverlapping(host.as_ptr(), pinned.as_ptr().unwrap() as *mut u8, total) };
    let mut dev = gpu.alloc(total).unwrap();
    gpu.upload(&pinned, total, &mut dev, 0).unwrap();
    let mut spans = vec![];
    let mut off = 0u64;
    for &n in &sizes {
        spans.push((dev.addr() + off, n as u64, 1 << 20));
        off += (n as u64).div_ceil(4096) * 4096;
    }
    let many = gpu.sha256_parts_many(&spans).unwrap();
    for ((addr, len, part), got) in spans.iter().zip(many) {
        assert_eq!(got, gpu.sha256_parts(*addr, *len, *part).unwrap());
        let start = (addr - dev.addr()) as usize;
        assert_eq!(got, sha256_parts_cpu(&host[start..start + *len as usize], *part as usize));
    }
}
