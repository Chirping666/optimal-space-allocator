use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::AtomicBool;

use crate::block::{align_up, body_len, BlockHeader, HEADER, NONE};
use crate::lock::LockGuard;

/// Best-fit allocator over a caller-provided byte buffer.
///
/// Only *allocated* blocks carry inline [`BlockHeader`]s, kept sorted by
/// offset. Free space is implicit: every gap between allocated blocks.
/// Freeing a block simply unlinks it — no coalescing required.
///
/// Thread safety is provided by an internal spin lock that serialises all
/// operations on the allocator.
///
/// The `'buf` parameter ties the allocator to the buffer it was built from,
/// so it can never outlive that buffer. Installing one as a
/// `#[global_allocator]` therefore requires an `Allocator<'static>`, backed by
/// a `static` buffer.
#[repr(C)]
pub struct Allocator<'buf> {
    /// Start of the usable region, aligned for [`BlockHeader`].
    base: *mut u8,
    /// Bytes usable from `base`.
    length: usize,
    /// Offset of the first allocated block (sorted by position), or [`NONE`].
    head: UnsafeCell<usize>,
    /// Spin lock protecting the buffer and `head`.
    lock: AtomicBool,
    /// Borrows `'buf` mutably: the buffer is exclusively ours for that long.
    buffer: PhantomData<&'buf mut [u8]>,
}

// SAFETY: All mutable access to the buffer and head is guarded by the `lock`
// spin lock, ensuring mutual exclusion across threads. The raw pointer `data`
// is only dereferenced under the lock.
unsafe impl Sync for Allocator<'_> {}
unsafe impl Send for Allocator<'_> {}

impl<'buf> Allocator<'buf> {
    /// Build an allocator over `data`.
    ///
    /// The allocator borrows `data` for as long as it lives, so it cannot
    /// outlive the buffer it hands out pointers into:
    ///
    /// ```compile_fail
    /// use optimal_space_allocator::Allocator;
    /// let allocator = {
    ///     let mut buffer = [0u8; 1024];
    ///     Allocator::new(&mut buffer)
    /// };
    /// ```
    pub fn new(data: &'buf mut [u8]) -> Self {
        Self::over(data.as_mut_ptr(), data.len())
    }

    /// Build an allocator over a raw buffer, for callers that cannot produce
    /// a `&mut [u8]`. Prefer [`Allocator::new`] where one is available.
    ///
    /// The usable length is taken from `data`'s own slice metadata, so it can
    /// never disagree with the region actually pointed to.
    ///
    /// # Safety
    ///
    /// - `data` must point to `data.len()` bytes of writable memory that stays
    ///   valid for all of `'buf`.
    /// - Nothing else may read or write that memory while the allocator lives;
    ///   the allocator assumes exclusive access to it.
    pub unsafe fn from_ptr(data: *mut [u8]) -> Self {
        Self::over(data as *mut u8, data.len())
    }

    /// Claim the largest `BlockHeader`-aligned region inside `[data, data + length)`.
    ///
    /// Every block offset is a multiple of `align_of::<BlockHeader>()` — both
    /// `HEADER` and every `body_len` are — so aligning the base once is what
    /// keeps every inline header aligned. A buffer too short to contain even
    /// the padding yields a zero-length allocator, which simply never fits
    /// anything.
    fn over(data: *mut u8, length: usize) -> Self {
        let padding = align_up(data as usize, align_of::<BlockHeader>()) - data as usize;
        let (base, length) = match length.checked_sub(padding) {
            // SAFETY: padding <= length, so `data + padding` lands inside the
            // buffer or one byte past its end.
            Some(usable) => (unsafe { data.add(padding) }, usable),
            None => (data, 0),
        };
        debug_assert!(base as usize % align_of::<BlockHeader>() == 0 || length == 0);
        Self {
            base,
            length,
            head: UnsafeCell::new(NONE),
            lock: AtomicBool::new(false),
            buffer: PhantomData,
        }
    }

    fn lock(&self) -> LockGuard<'_> {
        LockGuard::acquire(&self.lock)
    }

    fn buf(&self) -> *mut u8 {
        self.base
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
            debug_assert!(p.is_aligned(), "header read at misaligned offset {off}");
            ptr::read(p)
        }
    }

    unsafe fn set(&self, off: usize, h: BlockHeader) {
        // SAFETY: caller guarantees `off` is a valid header offset within the buffer
        unsafe {
            let p = self.buf().add(off) as *mut BlockHeader;
            debug_assert!(p.is_aligned(), "header write at misaligned offset {off}");
            ptr::write(p, h);
        }
    }

    /// Try to fit `size` bytes at `align` into the gap `[gap_start, gap_end)`.
    /// Returns `(body_len, waste)` on success.
    fn fit_gap(
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
        let body = body_len(self.buf() as usize, gap_start, size, align);
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
        let base_addr = self.buf() as usize;
        let mut target: usize = 0;
        let mut prev = NONE;
        // SAFETY: spin lock held — exclusive access
        let mut cur = unsafe { self.head() };

        while cur != NONE {
            // SAFETY: cur is a valid block offset in the allocated list
            let hdr = unsafe { self.get(cur) };
            let end_if_kept = cur + HEADER + body_len(base_addr, cur, hdr.size, hdr.align);
            let end_if_moved = target + HEADER + body_len(base_addr, target, hdr.size, hdr.align);

            // A block's alignment padding depends on where it sits, so moving
            // one left can make it *wider* and push its end past where it used
            // to finish — over the next block. Only move when the extent
            // genuinely shrinks; otherwise leave the block alone.
            let moving = target < cur && end_if_moved <= end_if_kept;

            if moving {
                let old_user = align_up(base_addr + cur + HEADER, hdr.align) as *mut u8;
                let new_user = align_up(base_addr + target + HEADER, hdr.align) as *mut u8;
                debug_assert!(new_user <= old_user, "compaction must never move a block right");

                // SAFETY: both lie inside the buffer and ptr::copy handles overlap
                unsafe { ptr::copy(old_user, new_user, hdr.size) };

                // SAFETY: target is a valid offset for a header within the buffer
                unsafe { self.set(target, hdr) };

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
            }

            prev = if moving { target } else { cur };
            target = if moving { end_if_moved } else { end_if_kept };
            cur = hdr.next;
        }
    }
}

unsafe impl GlobalAlloc for Allocator<'_> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _guard = self.lock();
        let size = layout.size();
        let align = layout.align();
        let base = self.buf();
        let len = self.length;

        // A block costs a header plus padding on top of `size`, so anything
        // larger than the whole buffer is hopeless. Rejecting it here also
        // keeps `size + padding` in `body_len` well clear of wrapping.
        if size > len {
            return ptr::null_mut();
        }

        let mut best: Option<(usize, usize, usize)> = None; // (gap_start, prev, body_len)
        let mut best_waste = usize::MAX;

        let mut prev = NONE;
        let mut gap_start: usize = 0;
        // SAFETY: spin lock held — exclusive access
        let mut cur = unsafe { self.head() };

        while cur != NONE {
            if let Some((body, waste)) = self.fit_gap(gap_start, cur, size, align) {
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

        if let Some((body, waste)) = self.fit_gap(gap_start, len, size, align) {
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

        // `new_size` is an arbitrary `usize` here, unlike a `Layout`'s size.
        // A request larger than the buffer can never be satisfied, and letting
        // it reach `body_len` risks wrapping into a small block. The original
        // allocation stays untouched, as a failed realloc must leave it.
        if new_size > self.length {
            return ptr::null_mut();
        }

        {
            let _guard = self.lock();
            let base = self.buf();
            let base_addr = base as usize;
            let len = self.length;
            let target = ptr as usize;

            // Walk the list to find the block matching `ptr`.
            let mut cur = unsafe { self.head() };
            while cur != NONE {
                // SAFETY: cur is a valid block offset
                let hdr = unsafe { self.get(cur) };
                if align_up(base_addr + cur + HEADER, hdr.align) == target {
                    // Found the block. Determine the gap end (next block or buffer end).
                    let gap_end = if hdr.next == NONE { len } else { hdr.next };
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
        let base = self.buf() as usize;
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
