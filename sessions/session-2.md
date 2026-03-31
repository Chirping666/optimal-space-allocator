# Session 2 — 2026-03-31

## Log

- **Make spin lock panic-safe**: Replaced manual `acquire()`/`release()` with a RAII `LockGuard` struct that acquires the `AtomicBool` spin lock on construction and releases it in `Drop`. All call sites in `alloc`, `dealloc`, `realloc`, and `optimize_space` now use `let _guard = self.lock()` instead of paired acquire/release calls. In `realloc`, the guard is explicitly dropped before calling `alloc`/`dealloc` to avoid deadlock on the fallback path. If a panic occurs mid-operation (e.g. from `debug_assert!`), the guard's destructor still runs, preventing permanent deadlock. All 17 tests pass.

- **Guard `align_up` against `align == 0`**: Added `debug_assert!(align.is_power_of_two())` precondition to `align_up`. This catches `align == 0` (which would cause `0 - 1 = usize::MAX` and silently return 0) in debug builds. All 17 tests pass.

- **Add `Send` safety comment**: Added an explicit comment next to the `Sync` impl explaining why `Send` is also safe: all fields are `Send`, no thread-local state is referenced, and the spin lock ensures sound access from any thread.

- **Handle `realloc` with `new_size == 0`**: Added early return in `realloc` that treats `new_size == 0` as a dealloc, freeing the block and returning null. This makes the implementation-defined behaviour explicit and documented. All 17 tests pass.

- **Make `Allocator` `#[repr(C)]`**: Added `#[repr(C)]` to the `Allocator` struct so `data` is guaranteed at offset 0. Tests like `free_reclaims_full_space` and `alloc_exactly_fills_buffer` rely on this layout. Without it, the compiler could reorder fields. All 17 tests pass.

- **Add panic-safety test for lock**: Added `lock_released_after_panic` test that triggers a panicking `dealloc` (invalid pointer) inside `catch_unwind`, then verifies a subsequent `alloc` succeeds without deadlocking. Confirms the RAII `LockGuard` releases the lock on unwind. All 21 tests pass.

- **Add `realloc` edge-case tests**: Added 3 tests — `realloc_zero_size` (verifies dealloc semantics and space reclamation), `realloc_invalid_pointer` (returns null for unrecognised pointer), `realloc_fills_remaining_gap` (in-place grow to fill all trailing space). All 21 tests pass.

