#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

const NONE: usize = usize::MAX;
/// Per-block header: `[size, align, next]`.
const HEADER: usize = size_of::<[usize; 3]>();

/// Best-fit allocator over a fixed `[u8; N]` buffer.
///
/// Only *allocated* blocks carry inline `[size, align, next]` headers, kept
/// sorted by offset. Free space is implicit: every gap between allocated
/// blocks. Freeing a block simply unlinks it — no coalescing required.
pub struct Allocator<const N: usize> {
    data: UnsafeCell<[u8; N]>,
    /// Offset of the first allocated block (sorted by position), or [`NONE`].
    head: UnsafeCell<usize>,
}

// SAFETY: GlobalAlloc contract requires callers to synchronise.
unsafe impl<const N: usize> Sync for Allocator<N> {}

/// Compute how many body bytes a block at `off` actually occupies,
/// given the user's `size` and `align` and the buffer base address.
#[inline]
fn body_len(base: usize, off: usize, size: usize, align: usize) -> usize {
    let raw = base + off + HEADER;
    let padding = align_up(raw, align) - raw;
    align_up(size + padding, size_of::<usize>())
}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

impl<const N: usize> Allocator<N> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0; N]),
            head: UnsafeCell::new(NONE),
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

    unsafe fn get(&self, off: usize) -> [usize; 3] {
        ptr::read(self.buf().add(off) as *const [usize; 3])
    }

    unsafe fn set(&self, off: usize, h: [usize; 3]) {
        ptr::write(self.buf().add(off) as *mut [usize; 3], h);
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
        let body = body_len(self.buf() as usize, gap_start, size, align);
        let needed = HEADER + body;
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
        let base = self.buf();
        let base_addr = base as usize;
        let mut target: usize = 0; // next available offset
        let mut prev = NONE;
        let mut cur = self.head();

        while cur != NONE {
            let [size, align, next] = self.get(cur);
            let new_body = body_len(base_addr, target, size, align);

            if target < cur {
                let old_user = align_up(base_addr + cur + HEADER, align) as *mut u8;
                let new_user = align_up(base_addr + target + HEADER, align) as *mut u8;

                // Copy user data to new position (ptr::copy handles overlap).
                ptr::copy(old_user, new_user, size);

                self.set(target, [size, align, next]);

                if prev == NONE {
                    self.set_head(target);
                } else {
                    let [ps, pa, _] = self.get(prev);
                    self.set(prev, [ps, pa, target]);
                }

                relocate(old_user, new_user);
                prev = target;
            } else {
                prev = cur;
            }

            target = (if target < cur { target } else { cur }) + HEADER + new_body;
            cur = next;
        }
    }
}

unsafe impl<const N: usize> GlobalAlloc for Allocator<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();
        let base = self.buf();

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

            let [sz, al, next] = self.get(cur);
            prev = cur;
            gap_start = cur + HEADER + body_len(base as usize, cur, sz, al);
            cur = next;
        }

        if let Some((body, waste)) = self.fit_gap(gap_start, N, size, align) {
            if waste < best_waste {
                best = Some((gap_start, prev, body));
            }
        }

        let (gap, prev, _body) = match best {
            Some(b) => b,
            None => return ptr::null_mut(),
        };

        let next = if prev == NONE {
            let old = self.head();
            self.set_head(gap);
            old
        } else {
            let [ps, pa, old_next] = self.get(prev);
            self.set(prev, [ps, pa, gap]);
            old_next
        };

        self.set(gap, [size, align, next]);

        align_up(base.add(gap + HEADER) as usize, align) as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let base = self.buf() as usize;
        let target = ptr as usize;
        let align = layout.align();

        let mut prev = NONE;
        let mut cur = self.head();
        while cur != NONE {
            let [_, _, next] = self.get(cur);
            if align_up(base + cur + HEADER, align) == target {
                if prev == NONE {
                    self.set_head(next);
                } else {
                    let [ps, pa, _] = self.get(prev);
                    self.set(prev, [ps, pa, next]);
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
        let a = Allocator::<4096>::new();
        unsafe {
            let l = lay(64, 8);
            let p1 = a.alloc(l);
            let p2 = a.alloc(l);
            let p3 = a.alloc(l);

            a.dealloc(p2, l);
            a.dealloc(p1, l);
            a.dealloc(p3, l);

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
}
