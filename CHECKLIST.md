# Code Review Checklist

## Critical Issues

- [x] **Make spin lock panic-safe**: Replace manual `acquire()`/`release()` with a RAII `LockGuard` that releases in `Drop`, so a panic between acquire and release (e.g. from `debug_assert!` in `dealloc`) does not permanently deadlock all threads

## Design Issues

- [ ] **Guard `align_up` against `align == 0`**: `align_up(v, 0)` wraps via `0 - 1 = usize::MAX` and silently returns 0; add a `debug_assert!(align.is_power_of_two())` precondition
- [ ] **Add `Send` safety comment**: `Send` is auto-derived; add an explicit comment next to the `Sync` impl explaining why `Send` is also safe
- [ ] **Handle `realloc` with `new_size == 0`**: Behaviour is implementation-defined per `GlobalAlloc` docs; decide and document the policy (treat as dealloc, or return a minimal block)
- [ ] **Make `Allocator` `#[repr(C)]`**: Tests (`free_reclaims_full_space`, `alloc_exactly_fills_buffer`) rely on `data` being at offset 0 in the struct; without `#[repr(C)]` the compiler may reorder fields

## Testing

- [ ] **Add panic-safety test for lock**: Verify that after a panicking `dealloc` (invalid pointer in debug mode), subsequent operations on the allocator do not deadlock
- [ ] **Add `realloc` edge-case tests**: realloc to zero size, realloc of an invalid pointer, realloc that exactly fills the remaining gap
