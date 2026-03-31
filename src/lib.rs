#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

const NONE: usize = usize::MAX;
const HEADER: usize = size_of::<[usize; 2]>();

/// Best-fit allocator over a fixed `[u8; N]` buffer.
///
/// Only *allocated* blocks carry inline `[length, next]` headers, kept sorted
/// by offset. Free space is implicit: every gap between allocated blocks.
/// Freeing a block simply unlinks it — no coalescing required.
pub struct Allocator<const N: usize> {
    data: UnsafeCell<[u8; N]>,
    /// Offset of the first allocated block (sorted by position), or [`NONE`].
    head: UnsafeCell<usize>,
}

// SAFETY: GlobalAlloc contract requires callers to synchronise.
unsafe impl<const N: usize> Sync for Allocator<N> {}

impl<const N: usize> Allocator<N> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0; N]),
            head: UnsafeCell::new(NONE), // no allocations → entire buffer is free
        }
    }

    unsafe fn buf(&self) -> *mut u8 {
        (*self.data.get()).as_mut_ptr()
    }

    unsafe fn head(&self) -> usize {
        *self.head.get()
    }

    unsafe fn set_head(&self, v: usize) {
        *self.head.get() = v;
    }

    /// Read the `[length, next]` header at byte offset `off`.
    unsafe fn get(&self, off: usize) -> [usize; 2] {
        ptr::read(self.buf().add(off) as *const [usize; 2])
    }

    /// Write a `[length, next]` header at byte offset `off`.
    unsafe fn set(&self, off: usize, h: [usize; 2]) {
        ptr::write(self.buf().add(off) as *mut [usize; 2], h);
    }

    /// Try to fit an allocation of `size` bytes at `align` into a gap.
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
        let raw = self.buf().add(gap_start + HEADER) as usize;
        let padding = align_up(raw, align) - raw;
        let body = align_up(size + padding, size_of::<usize>());
        let needed = HEADER + body;
        (needed <= gap).then(|| (body, gap - needed))
    }
}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

unsafe impl<const N: usize> GlobalAlloc for Allocator<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        let base = self.buf();

        // Walk the sorted allocated list, evaluating every gap.
        let mut best: Option<(usize, usize, usize)> = None; // (gap_start, prev, body_len)
        let mut best_waste = usize::MAX;

        let mut prev = NONE;
        let mut gap_start: usize = 0;
        let mut cur = self.head();

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

            let [len, next] = self.get(cur);
            prev = cur;
            gap_start = cur + HEADER + len;
            cur = next;
        }

        // Final gap: [gap_start, N)
        if let Some((body, waste)) = self.fit_gap(gap_start, N, size, align) {
            if waste < best_waste {
                best = Some((gap_start, prev, body));
            }
        }

        let (gap, prev, body) = match best {
            Some(b) => b,
            None => return ptr::null_mut(),
        };

        // Insert into sorted allocated list.
        let next = if prev == NONE {
            let old = self.head();
            self.set_head(gap);
            old
        } else {
            let [pl, old_next] = self.get(prev);
            self.set(prev, [pl, gap]);
            old_next
        };

        self.set(gap, [body, next]);

        align_up(base.add(gap + HEADER) as usize, align) as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let base = self.buf() as usize;
        let target = ptr as usize;
        let align = layout.align();

        // Walk to find the block whose aligned user pointer matches `ptr`.
        let mut prev = NONE;
        let mut cur = self.head();
        while cur != NONE {
            let [_, next] = self.get(cur);
            if align_up(base + cur + HEADER, align) == target {
                if prev == NONE {
                    self.set_head(next);
                } else {
                    let [pl, _] = self.get(prev);
                    self.set(prev, [pl, next]);
                }
                return;
            }
            prev = cur;
            cur = next;
        }
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

            a.dealloc(s1, small); // ~100-byte gap
            a.dealloc(s2, big);   // ~400-byte gap

            // 80 bytes should land in the tighter ~100-byte gap.
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
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            a.dealloc(p2, l);
            a.dealloc(p1, l);
            a.dealloc(p3, l);

            // All blocks freed → entire buffer is one big gap again.
            let big = lay(4096 - HEADER, 8);
            let p = a.alloc(big);
            assert!(!p.is_null());
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
}
