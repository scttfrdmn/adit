// SPDX-License-Identifier: Apache-2.0
//! The proxy's receive pages: one page-aligned DRAM region, registered with NIXL once, carved
//! into fixed-size slots. A slot holds one chunk from S3 until its RDMA write completes.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const PAGE: usize = 4096;

pub struct Pool {
    ptr: *mut u8,
    layout: Layout,
    slot_size: usize,
    free: Mutex<Vec<usize>>,
    permits: Arc<Semaphore>,
}

// SAFETY: slots are handed out exclusively (free list under a mutex, gated by the semaphore), so
// no two holders alias the same bytes.
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Pool {
    pub fn new(slots: usize, slot_size: usize) -> Self {
        assert!(slots > 0 && slot_size > 0);
        let slot_size = slot_size.div_ceil(PAGE) * PAGE;
        let layout = Layout::from_size_align(slots * slot_size, PAGE).expect("pool layout");
        // SAFETY: non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "pool allocation of {} bytes failed", layout.size());
        Self {
            ptr,
            layout,
            slot_size,
            free: Mutex::new((0..slots).rev().collect()),
            permits: Arc::new(Semaphore::new(slots)),
        }
    }

    pub fn slots(&self) -> usize {
        self.layout.size() / self.slot_size
    }

    pub fn slot_size(&self) -> usize {
        self.slot_size
    }

    /// Base address and length of the whole region, for registration.
    pub fn region(&self) -> (*const u8, usize) {
        (self.ptr, self.layout.size())
    }

    pub async fn acquire(&self) -> Slot<'_> {
        let permit = self.permits.clone().acquire_owned().await.expect("pool semaphore closed");
        let index = self.free.lock().unwrap().pop().expect("permit without a free slot");
        Slot { pool: self, index, _permit: permit }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// Exclusive use of one slot; returns it to the pool on drop.
pub struct Slot<'p> {
    pool: &'p Pool,
    index: usize,
    _permit: OwnedSemaphorePermit,
}

impl Deref for Slot<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: slot `index` lies within the allocation and is held exclusively.
        unsafe { std::slice::from_raw_parts(self.pool.ptr.add(self.index * self.pool.slot_size), self.pool.slot_size) }
    }
}

impl DerefMut for Slot<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above.
        unsafe {
            std::slice::from_raw_parts_mut(self.pool.ptr.add(self.index * self.pool.slot_size), self.pool.slot_size)
        }
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.pool.free.lock().unwrap().push(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slots_are_page_aligned_and_disjoint() {
        let p = Pool::new(3, 5000);
        assert_eq!(p.slot_size(), 8192);
        let a = p.acquire().await;
        let b = p.acquire().await;
        let (pa, pb) = (a.as_ptr() as usize, b.as_ptr() as usize);
        assert_eq!(pa % PAGE, 0);
        assert_eq!(pb % PAGE, 0);
        assert!(pa.abs_diff(pb) >= 8192);
    }

    #[tokio::test]
    async fn acquire_waits_for_a_free_slot() {
        let p = Arc::new(Pool::new(1, 4096));
        let held = p.acquire().await;
        let p2 = p.clone();
        let waiter = tokio::spawn(async move {
            let _s = p2.acquire().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(held);
        waiter.await.unwrap();
    }
}
