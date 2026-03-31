# Session 1 — 2026-03-31

## Log

- **Fix `dealloc` block matching**: Changed `dealloc` to use the stored `align` from the block header (`self.get(cur)[1]`) instead of `layout.align()`. This ensures the correct block is found even if the caller-provided layout differs. All tests pass.

- **Remove `#![allow(unsafe_op_in_unsafe_fn)]`**: Replaced with `#![deny(unsafe_op_in_unsafe_fn)]`. Wrapped every unsafe operation inside unsafe fns with explicit `unsafe {}` blocks and added `// SAFETY:` comments explaining the invariant relied upon. All tests pass.

- **Fix unsound `Sync` impl**: Added an `AtomicBool` spin lock to the `Allocator` struct. `acquire()`/`release()` guard all mutable access in `alloc`, `dealloc`, and `optimize_space`. The `Sync` impl is now sound — mutual exclusion is enforced at the type level. All tests pass.

- **Audit `body_len` for correctness**: Added `debug_assert!` in `body_len` verifying alignment doesn't wrap and body >= size. Added assertion in `fit_gap` that a fitted block doesn't exceed the gap boundary. All existing tests pass with assertions enabled.

- **Make `dealloc` detect invalid frees**: Added `debug_assert!(false, ...)` at the end of `dealloc` that fires when a pointer is not found in the allocated list. This catches double-frees and frees of invalid pointers in debug builds.

- **Introduce `BlockHeader` struct**: Replaced raw `[usize; 3]` with a named `#[repr(C)] struct BlockHeader { size, align, next }` throughout. `get`/`set` now operate on `BlockHeader`. All destructuring uses named fields, eliminating index mixup risk. All tests pass.

- **Override `realloc`**: Implemented `GlobalAlloc::realloc` that checks if the gap after the current block can accommodate the new size. If so, updates the header in-place. Otherwise falls back to alloc+copy+dealloc. Added tests for in-place grow, fallback copy, and shrink. All 12 tests pass.

- **Harden `optimize_space`**: Added `debug_assert!` verifying that each compacted block at `target + HEADER + new_body` does not exceed the old block's start offset `cur`. Prevents silent data corruption from compaction overlap. All 12 tests pass.

- **Add concurrency test**: Added `concurrent_alloc_dealloc` test that spawns 4 threads each doing 100 alloc/dealloc cycles on a shared `static` allocator behind a `Barrier`. Validates the spin lock provides correct mutual exclusion. All 13 tests pass.

- **Make `free_reclaims_full_space` portable**: Replaced hardcoded `4096 - HEADER` with dynamic computation of max allocatable size from the buffer's actual base address, HEADER size, and alignment padding. No pointer-width assumptions remain.

- **Add edge-case tests**: Added 4 tests — `zero_size_alloc` (size=0, align=1), `align_greater_than_size` (align=256, size=8), `alloc_exactly_fills_buffer` (fills to capacity and verifies next alloc returns null), `dealloc_invalid_pointer_panics` (debug-only panic on invalid free). All 17 tests pass.
