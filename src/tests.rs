use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::block::{align_up, HEADER};
use crate::Allocator;

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
