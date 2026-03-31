# Session 2 — 2026-03-31

## Log

- **Make spin lock panic-safe**: Replaced manual `acquire()`/`release()` with a RAII `LockGuard` struct that acquires the `AtomicBool` spin lock on construction and releases it in `Drop`. All call sites in `alloc`, `dealloc`, `realloc`, and `optimize_space` now use `let _guard = self.lock()` instead of paired acquire/release calls. In `realloc`, the guard is explicitly dropped before calling `alloc`/`dealloc` to avoid deadlock on the fallback path. If a panic occurs mid-operation (e.g. from `debug_assert!`), the guard's destructor still runs, preventing permanent deadlock. All 17 tests pass.

- **Guard `align_up` against `align == 0`**: Added `debug_assert!(align.is_power_of_two())` precondition to `align_up`. This catches `align == 0` (which would cause `0 - 1 = usize::MAX` and silently return 0) in debug builds. All 17 tests pass.

