#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

const NONE: usize = usize::MAX;
const HEADER: usize = size_of::<[usize; 2]>();

/// Best-fit allocator over a fixed `[u8; N]` buffer.
///
/// Free blocks are linked via inline `[length, next]` headers.
/// Allocation always picks the tightest-fitting free block.
pub struct Allocator<const N: usize> {
    data: UnsafeCell<[u8; N]>,
    /// Byte offset of the first free block's header, or [`NONE`].
    free: UnsafeCell<usize>,
    init: UnsafeCell<bool>,
}

// SAFETY: GlobalAlloc contract requires callers to synchronise.
unsafe impl<const N: usize> Sync for Allocator<N> {}

impl<const N: usize> Allocator<N> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0; N]),
            free: UnsafeCell::new(NONE),
            init: UnsafeCell::new(false),
        }
    }

    unsafe fn buf(&self) -> *mut u8 {
        (*self.data.get()).as_mut_ptr()
    }

    unsafe fn head(&self) -> usize {
        *self.free.get()
    }

    unsafe fn set_head(&self, v: usize) {
        *self.free.get() = v;
    }

    /// Read the `[length, next]` header at byte offset `off`.
    unsafe fn get(&self, off: usize) -> [usize; 2] {
        ptr::read(self.buf().add(off) as *const [usize; 2])
    }

    /// Write a `[length, next]` header at byte offset `off`.
    unsafe fn set(&self, off: usize, h: [usize; 2]) {
        ptr::write(self.buf().add(off) as *mut [usize; 2], h);
    }

    unsafe fn ensure_init(&self) {
        if *self.init.get() {
            return;
        }
        *self.init.get() = true;
        assert!(N >= HEADER, "buffer too small");
        self.set(0, [N - HEADER, NONE]);
        self.set_head(0);
    }

    /// Point `prev`'s next (or the list head) at `replacement`.
    unsafe fn relink(&self, prev: usize, replacement: usize) {
        if prev == NONE {
            self.set_head(replacement);
        } else {
            let [len, _] = self.get(prev);
            self.set(prev, [len, replacement]);
        }
    }

    /// Remove the free block at `off` from the list.
    unsafe fn remove_free(&self, off: usize) {
        let mut prev = NONE;
        let mut cur = self.head();
        while cur != NONE {
            let [_, next] = self.get(cur);
            if cur == off {
                self.relink(prev, next);
                return;
            }
            prev = cur;
            cur = next;
        }
    }

    /// Return a freed block to the list, coalescing with adjacent neighbours.
    unsafe fn insert_and_coalesce(&self, off: usize, len: usize) {
        let end = off + HEADER + len;
        let (mut before, mut after) = (NONE, NONE);

        let mut cur = self.head();
        while cur != NONE {
            let [cl, next] = self.get(cur);
            if cur + HEADER + cl == off {
                before = cur;
            }
            if end == cur {
                after = cur;
            }
            cur = next;
        }

        match (before != NONE, after != NONE) {
            (true, true) => {
                let [bl, _] = self.get(before);
                let [al, _] = self.get(after);
                self.remove_free(after);
                let [_, bn] = self.get(before);
                self.set(before, [bl + HEADER + len + HEADER + al, bn]);
            }
            (true, false) => {
                let [bl, bn] = self.get(before);
                self.set(before, [bl + HEADER + len, bn]);
            }
            (false, true) => {
                let [al, _] = self.get(after);
                self.remove_free(after);
                self.set(off, [len + HEADER + al, self.head()]);
                self.set_head(off);
            }
            (false, false) => {
                self.set(off, [len, self.head()]);
                self.set_head(off);
            }
        }
    }
}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

unsafe impl<const N: usize> GlobalAlloc for Allocator<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.ensure_init();

        let size = layout.size();
        let align = layout.align();
        let base = self.buf();

        // ---- best-fit scan ----
        let (mut best, mut best_prev, mut best_waste) = (NONE, NONE, usize::MAX);
        let mut prev = NONE;
        let mut cur = self.head();

        while cur != NONE {
            let [len, next] = self.get(cur);
            let raw = base.add(cur + HEADER) as usize;
            let total = align_up(size + (align_up(raw, align) - raw), size_of::<usize>());

            if len >= total {
                let waste = len - total;
                if waste < best_waste {
                    (best, best_prev, best_waste) = (cur, prev, waste);
                    if waste == 0 {
                        break;
                    }
                }
            }

            prev = cur;
            cur = next;
        }

        if best == NONE {
            return ptr::null_mut();
        }

        // ---- carve out the chosen block ----
        let [len, next] = self.get(best);
        let raw = base.add(best + HEADER) as usize;
        let padding = align_up(raw, align) - raw;
        let total = align_up(size + padding, size_of::<usize>());
        let remainder = len - total;

        let (body_len, successor) = if remainder >= HEADER + size_of::<usize>() {
            let split = best + HEADER + total;
            self.set(split, [remainder - HEADER, next]);
            (total, split)
        } else {
            (len, next) // keep the full block to avoid tiny fragments
        };

        self.relink(best_prev, successor);
        self.set(best, [body_len, 0]);

        // Back-pointer for O(1) dealloc (lands in header's `next` slot or padding).
        let user_ptr = align_up(raw, align) as *mut u8;
        ptr::write((user_ptr as *mut usize).sub(1), best);

        user_ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let off = ptr::read((ptr as *const usize).sub(1));
        let [len, _] = self.get(off);
        self.insert_and_coalesce(off, len);
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
    fn best_fit_prefers_tighter_block() {
        let a = Allocator::<8192>::new();
        unsafe {
            // Create holes of different sizes by allocating separators.
            let sep = lay(64, 8);
            let small = lay(100, 8);
            let big = lay(400, 8);

            let s1 = a.alloc(small); // 100-byte block
            let g1 = a.alloc(sep);   // separator
            let s2 = a.alloc(big);   // 400-byte block
            let g2 = a.alloc(sep);   // separator

            a.dealloc(s1, small); // free 100-byte hole
            a.dealloc(s2, big);   // free 400-byte hole

            // Ask for 80 bytes — best fit should pick the 100-byte hole.
            let req = lay(80, 8);
            let p = a.alloc(req);
            assert!(!p.is_null());
            // p should land in s1's region (lower address, smaller hole).
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
                assert_eq!(
                    p as usize % align,
                    0,
                    "pointer not {align}-byte aligned"
                );
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
    fn coalescing_reclaims_full_space() {
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            a.dealloc(p2, l);
            a.dealloc(p1, l);
            a.dealloc(p3, l);

            // After full coalescing the entire buffer should be available again.
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
