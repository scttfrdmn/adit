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
