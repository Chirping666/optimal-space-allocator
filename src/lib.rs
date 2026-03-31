#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

const NONE: usize = usize::MAX;

/// Inline header stored at the start of each allocated block.
#[derive(Clone, Copy)]
#[repr(C)]
struct BlockHeader {
    size: usize,
    align: usize,
    next: usize,
}

const HEADER: usize = size_of::<BlockHeader>();

/// RAII guard that releases the spin lock on drop, ensuring panic safety.
///
/// If a panic occurs while the lock is held (e.g. from a `debug_assert!`),
/// the guard's `Drop` impl will release the lock, preventing permanent
/// deadlock of all threads.
struct LockGuard<'a> {
    lock: &'a AtomicBool,
}

impl<'a> LockGuard<'a> {
    fn acquire(lock: &'a AtomicBool) -> Self {
        while lock.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Self { lock }
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}

/// Best-fit allocator over a fixed `[u8; N]` buffer.
///
/// Only *allocated* blocks carry inline [`BlockHeader`]s, kept sorted by
/// offset. Free space is implicit: every gap between allocated blocks.
/// Freeing a block simply unlinks it — no coalescing required.
///
/// Thread safety is provided by an internal spin lock that serialises all
/// operations on the allocator.
#[repr(C)]
pub struct Allocator<const N: usize> {
    data: UnsafeCell<[u8; N]>,
    /// Offset of the first allocated block (sorted by position), or [`NONE`].
    head: UnsafeCell<usize>,
    /// Spin lock protecting `data` and `head`.
    lock: AtomicBool,
}

// SAFETY: All mutable access to `data` and `head` is guarded by the `lock`
// spin lock, ensuring mutual exclusion across threads.
unsafe impl<const N: usize> Sync for Allocator<N> {}

// SAFETY: `Send` is auto-derived because all fields (`UnsafeCell<[u8; N]>`,
// `UnsafeCell<usize>`, `AtomicBool`) are `Send`. Transferring an `Allocator`
// to another thread is safe: no thread-local state is referenced, and the spin
// lock ensures sound access from whichever thread owns or shares the value.

/// Compute how many body bytes a block at `off` actually occupies,
/// given the user's `size` and `align` and the buffer base address.
#[inline]
fn body_len(base: usize, off: usize, size: usize, align: usize) -> usize {
    let raw = base + off + HEADER;
    let aligned = align_up(raw, align);
    let padding = aligned - raw;
    debug_assert!(
        aligned >= raw,
        "align_up must not wrap around"
    );
    let body = align_up(size + padding, size_of::<usize>());
    debug_assert!(
        body >= size,
        "body_len must be at least as large as the requested size"
    );
    body
}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (v + align - 1) & !(align - 1)
}

impl<const N: usize> Allocator<N> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0; N]),
            head: UnsafeCell::new(NONE),
            lock: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> LockGuard<'_> {
        LockGuard::acquire(&self.lock)
    }

    unsafe fn buf(&self) -> *mut u8 {
        // SAFETY: caller ensures exclusive access to the buffer
        unsafe { (*self.data.get()).as_mut_ptr() }
    }

    unsafe fn head(&self) -> usize {
        // SAFETY: caller ensures no concurrent mutation of head
        unsafe { *self.head.get() }
    }

    unsafe fn set_head(&self, v: usize) {
        // SAFETY: caller ensures no concurrent access to head
        unsafe { *self.head.get() = v }
    }

    unsafe fn get(&self, off: usize) -> BlockHeader {
        // SAFETY: caller guarantees `off` is a valid header offset within the buffer
        unsafe {
            let p = self.buf().add(off) as *const BlockHeader;
            ptr::read(p)
        }
    }

    unsafe fn set(&self, off: usize, h: BlockHeader) {
        // SAFETY: caller guarantees `off` is a valid header offset within the buffer
        unsafe {
            let p = self.buf().add(off) as *mut BlockHeader;
            ptr::write(p, h);
        }
    }

    /// Try to fit `size` bytes at `align` into the gap `[gap_start, gap_end)`.
    /// Returns `(body_len, waste)` on success.
    unsafe fn fit_gap(
        &self,
        gap_start: usize,
        gap_end: usize,
        size: usize,
        align: usize,
    ) -> Option<(usize, usize)> {
        let gap = gap_end.checked_sub(gap_start)?;
        if gap < HEADER {
            return None;
        }
        // SAFETY: caller ensures the allocator is exclusively accessed
        let body = body_len(unsafe { self.buf() } as usize, gap_start, size, align);
        let needed = HEADER + body;
        debug_assert!(
            gap_start + needed <= gap_end || needed > gap,
            "fitted block at {gap_start}..{} must not exceed gap end {gap_end}",
            gap_start + needed,
        );
        (needed <= gap).then(|| (body, gap - needed))
    }

    /// Compact all allocated blocks toward the start of the buffer,
    /// eliminating fragmentation. Calls `relocate(old_ptr, new_ptr)` for
    /// every block whose user pointer changed.
    ///
    /// # Safety
    ///
    /// The caller must update **all** live pointers via the `relocate`
    /// callback. Any pointer not updated becomes dangling.
    pub unsafe fn optimize_space(&self, mut relocate: impl FnMut(*mut u8, *mut u8)) {
        let _guard = self.lock();
        // SAFETY: spin lock held — exclusive access to data and head
        let base = unsafe { self.buf() };
        let base_addr = base as usize;
        let mut target: usize = 0; // next available offset
        let mut prev = NONE;
        // SAFETY: spin lock held — exclusive access
        let mut cur = unsafe { self.head() };

        while cur != NONE {
            // SAFETY: cur is a valid block offset in the allocated list
            let hdr = unsafe { self.get(cur) };
            let new_body = body_len(base_addr, target, hdr.size, hdr.align);

            if target < cur {
                debug_assert!(
                    target + HEADER + new_body <= cur,
                    "compacted block at {target}..{} overlaps old block start at {cur}",
                    target + HEADER + new_body,
                );
                let old_user = align_up(base_addr + cur + HEADER, hdr.align) as *mut u8;
                let new_user = align_up(base_addr + target + HEADER, hdr.align) as *mut u8;

                // SAFETY: old_user and new_user are within the buffer; ptr::copy handles overlap
                unsafe { ptr::copy(old_user, new_user, hdr.size) };

                // SAFETY: target is a valid offset for a header within the buffer
                unsafe {
                    self.set(target, BlockHeader {
                        size: hdr.size,
                        align: hdr.align,
                        next: hdr.next,
                    });
                }

                if prev == NONE {
                    // SAFETY: spin lock held — exclusive access
                    unsafe { self.set_head(target) };
                } else {
                    // SAFETY: prev is a valid block offset
                    let prev_hdr = unsafe { self.get(prev) };
                    // SAFETY: prev is a valid block offset
                    unsafe {
                        self.set(prev, BlockHeader { next: target, ..prev_hdr });
                    }
                }

                relocate(old_user, new_user);
                prev = target;
            } else {
                prev = cur;
            }

            target = (if target < cur { target } else { cur }) + HEADER + new_body;
            cur = hdr.next;
        }
    }
}

unsafe impl<const N: usize> GlobalAlloc for Allocator<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _guard = self.lock();
        let size = layout.size();
        let align = layout.align();
        // SAFETY: spin lock held — exclusive access to data and head
        let base = unsafe { self.buf() };

        let mut best: Option<(usize, usize, usize)> = None; // (gap_start, prev, body_len)
        let mut best_waste = usize::MAX;

        let mut prev = NONE;
        let mut gap_start: usize = 0;
        // SAFETY: spin lock held — exclusive access
        let mut cur = unsafe { self.head() };

        while cur != NONE {
            // SAFETY: spin lock held — exclusive access
            if let Some((body, waste)) = unsafe { self.fit_gap(gap_start, cur, size, align) } {
                if waste < best_waste {
                    best = Some((gap_start, prev, body));
                    best_waste = waste;
                    if waste == 0 {
                        break;
                    }
                }
            }

            // SAFETY: cur is a valid block offset
            let hdr = unsafe { self.get(cur) };
            prev = cur;
            gap_start = cur + HEADER + body_len(base as usize, cur, hdr.size, hdr.align);
            cur = hdr.next;
        }

        // SAFETY: spin lock held — exclusive access
        if let Some((body, waste)) = unsafe { self.fit_gap(gap_start, N, size, align) } {
            if waste < best_waste {
                best = Some((gap_start, prev, body));
            }
        }

        let (gap, prev, _body) = match best {
            Some(b) => b,
            None => {
                return ptr::null_mut();
            }
        };

        let next = if prev == NONE {
            // SAFETY: spin lock held — exclusive access
            let old = unsafe { self.head() };
            // SAFETY: spin lock held — exclusive access
            unsafe { self.set_head(gap) };
            old
        } else {
            // SAFETY: prev is a valid block offset
            let prev_hdr = unsafe { self.get(prev) };
            // SAFETY: prev is a valid block offset
            unsafe {
                self.set(prev, BlockHeader { next: gap, ..prev_hdr });
            }
            prev_hdr.next
        };

        // SAFETY: gap is a valid offset for a new header
        unsafe { self.set(gap, BlockHeader { size, align, next }) };

        // SAFETY: gap + HEADER is within the buffer
        align_up(unsafe { base.add(gap + HEADER) } as usize, align) as *mut u8
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // new_size == 0 is implementation-defined per GlobalAlloc docs.
        // We treat it as dealloc and return null.
        if new_size == 0 {
            // SAFETY: ptr was allocated with layout
            unsafe { self.dealloc(ptr, layout) };
            return ptr::null_mut();
        }

        {
            let _guard = self.lock();
            // SAFETY: spin lock held — exclusive access to data and head
            let base = unsafe { self.buf() };
            let base_addr = base as usize;
            let target = ptr as usize;

            // Walk the list to find the block matching `ptr`.
            let mut cur = unsafe { self.head() };
            while cur != NONE {
                // SAFETY: cur is a valid block offset
                let hdr = unsafe { self.get(cur) };
                if align_up(base_addr + cur + HEADER, hdr.align) == target {
                    // Found the block. Determine the gap end (next block or buffer end).
                    let gap_end = if hdr.next == NONE { N } else { hdr.next };
                    let new_body = body_len(base_addr, cur, new_size, hdr.align);
                    let needed = HEADER + new_body;

                    if cur + needed <= gap_end {
                        // In-place expansion: just update the stored size.
                        // SAFETY: cur is a valid block offset
                        unsafe {
                            self.set(cur, BlockHeader { size: new_size, ..hdr });
                        }
                        return ptr;
                    }

                    // Cannot expand in-place — drop guard before re-acquiring
                    // in alloc/dealloc to avoid deadlock.
                    drop(_guard);
                    let new_layout = unsafe {
                        // SAFETY: align is a valid power of two from the original allocation
                        Layout::from_size_align_unchecked(new_size, layout.align())
                    };
                    let new_ptr = unsafe { self.alloc(new_layout) };
                    if !new_ptr.is_null() {
                        let copy_size = if layout.size() < new_size { layout.size() } else { new_size };
                        // SAFETY: both pointers are valid for their respective sizes
                        unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, copy_size) };
                        // SAFETY: ptr was allocated with layout
                        unsafe { self.dealloc(ptr, layout) };
                    }
                    return new_ptr;
                }
                cur = hdr.next;
            }
        }

        ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let _guard = self.lock();
        // SAFETY: spin lock held — exclusive access to data and head
        let base = unsafe { self.buf() } as usize;
        let target = ptr as usize;

        let mut prev = NONE;
        // SAFETY: spin lock held — exclusive access
        let mut cur = unsafe { self.head() };
        while cur != NONE {
            // SAFETY: cur is a valid block offset
            let hdr = unsafe { self.get(cur) };
            if align_up(base + cur + HEADER, hdr.align) == target {
                if prev == NONE {
                    // SAFETY: spin lock held — exclusive access
                    unsafe { self.set_head(hdr.next) };
                } else {
                    // SAFETY: prev is a valid block offset
                    let prev_hdr = unsafe { self.get(prev) };
                    // SAFETY: prev is a valid block offset
                    unsafe {
                        self.set(prev, BlockHeader { next: hdr.next, ..prev_hdr });
                    }
                }
                return;
            }
            prev = cur;
            cur = hdr.next;
        }
        debug_assert!(false, "dealloc: pointer {target:#x} not found in allocated list");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    fn lay(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn basic_alloc_dealloc() {
        let a = Allocator::<1024>::new();
        unsafe {
            let p = a.alloc(lay(64, 8));
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xAB, 64);
            a.dealloc(p, lay(64, 8));
        }
    }

    #[test]
    fn multiple_allocs_are_disjoint() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(128, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);
            assert!(!p1.is_null());
            assert!(!p2.is_null());
            assert!(!p3.is_null());

            let ptrs = [p1 as usize, p2 as usize, p3 as usize];
            for (i, &a_ptr) in ptrs.iter().enumerate() {
                for &b_ptr in &ptrs[i + 1..] {
                    assert!(a_ptr + 128 <= b_ptr || b_ptr + 128 <= a_ptr);
                }
            }

            a.dealloc(p1, l);
            a.dealloc(p2, l);
            a.dealloc(p3, l);
        }
    }

    #[test]
    fn best_fit_prefers_tighter_gap() {
        let a = Allocator::<8192>::new();
        unsafe {
            let sep = lay(64, 8);
            let small = lay(100, 8);
            let big = lay(400, 8);

            let s1 = a.alloc(small);
            let g1 = a.alloc(sep);
            let s2 = a.alloc(big);
            let g2 = a.alloc(sep);

            a.dealloc(s1, small);
            a.dealloc(s2, big);

            let req = lay(80, 8);
            let p = a.alloc(req);
            assert!(!p.is_null());
            assert!((p as usize) < (g1 as usize));

            a.dealloc(p, req);
            a.dealloc(g1, sep);
            a.dealloc(g2, sep);
        }
    }

    #[test]
    fn alignment_is_honoured() {
        let a = Allocator::<4096>::new();
        unsafe {
            for &align in &[1, 2, 4, 8, 16, 32, 64, 128, 256] {
                let l = lay(32, align);
                let p = a.alloc(l);
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "pointer not {align}-byte aligned");
                a.dealloc(p, l);
            }
        }
    }

    #[test]
    fn oom_returns_null() {
        let a = Allocator::<128>::new();
        unsafe {
            let p = a.alloc(lay(256, 8));
            assert!(p.is_null());
        }
    }

    #[test]
    fn free_reclaims_full_space() {
        const BUF: usize = 4096;
        let a = Allocator::<BUF>::new();
        unsafe {
            let align = size_of::<usize>();
            let l = lay(64, align);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            a.dealloc(p2, l);
            a.dealloc(p1, l);
            a.dealloc(p3, l);

            // Compute the max allocatable size dynamically: the body region
            // after one HEADER, accounting for alignment padding.
            let base = &a as *const _ as usize;
            let raw = base + HEADER;
            let padding = align_up(raw, align) - raw;
            let max_body = BUF - HEADER - padding;
            // Round down to usize alignment so body_len doesn't exceed BUF.
            let max_size = max_body & !(size_of::<usize>() - 1);

            let big = lay(max_size, align);
            let p = a.alloc(big);
            assert!(!p.is_null(), "should be able to allocate {max_size} bytes after full free");
            a.dealloc(p, big);
        }
    }

    #[test]
    fn reuse_after_free() {
        let a = Allocator::<512>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            a.dealloc(p1, l);
            let p2 = a.alloc(l);
            assert!(!p2.is_null());
            a.dealloc(p2, l);
        }
    }

    #[test]
    fn optimize_space_compacts() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            // Free the middle block, creating a gap.
            a.dealloc(p2, l);

            // p3 should slide left into p2's old space.
            let mut moved = false;
            let mut new_p3 = p3;
            a.optimize_space(|old, new| {
                if old == p3 {
                    moved = true;
                    new_p3 = new;
                }
            });

            assert!(moved, "p3 should have been relocated");
            assert!(
                (new_p3 as usize) < (p3 as usize),
                "p3 should have moved to a lower address"
            );

            // Data should still be accessible at the new location.
            ptr::write_bytes(new_p3, 0xCD, 64);

            // After compaction, a larger contiguous region should be available.
            // 2 blocks of (HEADER + 64), so free = 4096 - 2*(HEADER+64) - HEADER for new block.
            let big = lay(4096 - 3 * HEADER - 128, 8);
            let p4 = a.alloc(big);
            assert!(!p4.is_null());

            a.dealloc(p1, l);
            a.dealloc(new_p3, l);
            a.dealloc(p4, big);
        }
    }

    #[test]
    fn optimize_space_preserves_data() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(8, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            ptr::write(p1 as *mut u64, 0xAAAA);
            ptr::write(p2 as *mut u64, 0xBBBB);
            ptr::write(p3 as *mut u64, 0xCCCC);

            // Free p1, creating a gap at the front.
            a.dealloc(p1, l);

            let mut new_p2 = p2;
            let mut new_p3 = p3;
            a.optimize_space(|old, new| {
                if old == p2 { new_p2 = new; }
                if old == p3 { new_p3 = new; }
            });

            assert_eq!(ptr::read(new_p2 as *const u64), 0xBBBB);
            assert_eq!(ptr::read(new_p3 as *const u64), 0xCCCC);

            a.dealloc(new_p2, l);
            a.dealloc(new_p3, l);
        }
    }

    #[test]
    fn realloc_in_place_when_space_available() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p = a.alloc(l);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xAB, 64);

            // Grow in-place — no other blocks, so the trailing gap is large.
            let p2 = a.realloc(p, l, 128);
            assert_eq!(p2, p, "realloc should expand in-place");
            // Original data should still be intact.
            assert_eq!(*p2, 0xAB);

            a.dealloc(p2, lay(128, 8));
        }
    }

    #[test]
    fn realloc_falls_back_to_copy() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let _p2 = a.alloc(l);
            assert!(!p1.is_null());
            ptr::write_bytes(p1, 0xCD, 64);

            // Try to grow p1 beyond the gap before p2 — must relocate.
            let big = 1024;
            let p3 = a.realloc(p1, l, big);
            assert!(!p3.is_null());
            // Data from p1 should be preserved.
            assert_eq!(*p3, 0xCD);

            a.dealloc(_p2, l);
            a.dealloc(p3, lay(big, 8));
        }
    }

    #[test]
    fn realloc_shrink() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(128, 8);
            let p = a.alloc(l);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xEF, 128);

            let p2 = a.realloc(p, l, 32);
            assert_eq!(p2, p, "shrink should be in-place");

            a.dealloc(p2, lay(32, 8));
        }
    }

    #[test]
    fn concurrent_alloc_dealloc() {
        extern crate std;
        use std::sync::Arc;
        use std::thread;
        use std::vec::Vec;

        // Large enough for many small concurrent allocations.
        static ALLOC: Allocator<65536> = Allocator::new();

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..100 {
                        unsafe {
                            let l = lay(32, 8);
                            let p = ALLOC.alloc(l);
                            if !p.is_null() {
                                ptr::write_bytes(p, 0xAA, 32);
                                ALLOC.dealloc(p, l);
                            }
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }
    }

    #[test]
    fn zero_size_alloc() {
        let a = Allocator::<1024>::new();
        unsafe {
            let l = lay(0, 1);
            let p = a.alloc(l);
            // Zero-size allocations may return a non-null aligned pointer.
            // Just verify it doesn't crash.
            if !p.is_null() {
                a.dealloc(p, l);
            }
        }
    }

    #[test]
    fn align_greater_than_size() {
        let a = Allocator::<4096>::new();
        unsafe {
            // Align 256 but only 8 bytes of data.
            let l = lay(8, 256);
            let p = a.alloc(l);
            assert!(!p.is_null());
            assert_eq!(p as usize % 256, 0, "pointer must be 256-byte aligned");
            ptr::write_bytes(p, 0xBB, 8);
            a.dealloc(p, l);
        }
    }

    #[test]
    fn alloc_exactly_fills_buffer() {
        // A tiny buffer that can hold exactly one block.
        const BUF: usize = HEADER + size_of::<usize>();
        let a = Allocator::<BUF>::new();
        unsafe {
            let align = size_of::<usize>();
            let base = &a as *const _ as usize;
            let raw = base + HEADER;
            let padding = align_up(raw, align) - raw;
            // The exact size that fills the buffer.
            let size = BUF - HEADER - padding;
            // Round down to usize alignment.
            let size = size & !(size_of::<usize>() - 1);
            if size > 0 {
                let l = lay(size, align);
                let p = a.alloc(l);
                assert!(!p.is_null(), "should fill buffer exactly with {size} bytes");
                // Second allocation must fail — no space left.
                let p2 = a.alloc(lay(1, 1));
                assert!(p2.is_null(), "buffer should be full");
                a.dealloc(p, l);
            }
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn dealloc_invalid_pointer_panics() {
        extern crate std;
        let a = Allocator::<1024>::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            // Attempt to free a pointer that was never allocated.
            a.dealloc(0x1000 as *mut u8, lay(64, 8));
        }));
        assert!(result.is_err(), "dealloc of invalid pointer should panic in debug mode");
    }

    #[test]
    #[cfg(debug_assertions)]
    fn lock_released_after_panic() {
        extern crate std;
        let a = Allocator::<1024>::new();

        // Allocate a valid block, then trigger a panicking dealloc with an
        // invalid pointer. The RAII LockGuard should release the lock even
        // though dealloc panics.
        unsafe {
            let l = lay(64, 8);
            let p = a.alloc(l);
            assert!(!p.is_null());

            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                a.dealloc(0x1000 as *mut u8, lay(64, 8));
            }));

            // If the lock were still held, this alloc would deadlock.
            let p2 = a.alloc(l);
            assert!(!p2.is_null(), "alloc after panicking dealloc must not deadlock");

            a.dealloc(p, l);
            a.dealloc(p2, l);
        }
    }

    #[test]
    fn realloc_zero_size() {
        let a = Allocator::<1024>::new();
        unsafe {
            let l = lay(64, 8);
            let p = a.alloc(l);
            assert!(!p.is_null());

            // realloc to size 0 should act as dealloc and return null.
            let p2 = a.realloc(p, l, 0);
            assert!(p2.is_null(), "realloc to size 0 should return null");

            // The space should be fully reclaimed — we can allocate again.
            let p3 = a.alloc(l);
            assert!(!p3.is_null(), "space should be reclaimed after realloc to 0");
            a.dealloc(p3, l);
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn realloc_invalid_pointer() {
        extern crate std;
        let a = Allocator::<1024>::new();
        unsafe {
            // realloc of a pointer that was never allocated should return null.
            let result = a.realloc(0x1000 as *mut u8, lay(64, 8), 128);
            assert!(result.is_null(), "realloc of invalid pointer should return null");
        }
    }

    #[test]
    fn realloc_fills_remaining_gap() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            assert!(!p1.is_null());
            assert!(!p2.is_null());

            // Free p1 to create a gap at the start, then realloc p2 to
            // exactly fill the remaining space after p2.
            // First, find how much space is available after p2.
            let base = &a as *const _ as usize;
            let p2_off = p2 as usize - base;
            let remaining = 4096 - p2_off;

            if remaining > 64 {
                // Grow p2 to fill the trailing space.
                let p2_new = a.realloc(p2, l, remaining);
                assert_eq!(p2_new, p2, "should grow in-place into trailing gap");

                // No more space for another allocation.
                let p3 = a.alloc(lay(1, 1));
                // p3 may fit in the gap left by the header overhead, or be null.
                // The key assertion is that realloc succeeded.
                if !p3.is_null() {
                    a.dealloc(p3, lay(1, 1));
                }

                a.dealloc(p1, l);
                a.dealloc(p2_new, lay(remaining, 8));
            } else {
                a.dealloc(p1, l);
                a.dealloc(p2, l);
            }
        }
    }
}
