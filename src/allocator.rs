use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::AtomicBool;

use crate::block::{align_up, body_len, BlockHeader, HEADER, NONE};
use crate::lock::LockGuard;

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
