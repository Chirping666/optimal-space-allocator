# Code Review Checklist

## Critical Issues

- [x] **Fix `dealloc` block matching**: Use stored `align` from block header (`self.get(cur)[1]`) instead of `layout.align()` to find the correct block (line 198)
- [x] **Remove `#![allow(unsafe_op_in_unsafe_fn)]`**: Wrap each unsafe operation in an explicit `unsafe {}` block with a comment stating the invariant relied upon
- [x] **Fix unsound `Sync` impl**: Either add proper synchronization (spin lock or atomic guard) or document that this allocator is single-threaded only and remove `Sync`
- [x] **Audit `body_len` for correctness**: Ensure padding + rounding cannot cause overlap between adjacent blocks; add debug assertions

## Design Issues

- [x] **Make `dealloc` detect invalid frees**: Add `debug_assert!` or return an error indicator when the target pointer is not found in the allocated list
- [x] **Introduce `BlockHeader` struct**: Replace raw `[usize; 3]` with a named struct to prevent index mixups and improve readability
- [x] **Override `realloc`**: Implement in-place realloc that expands into adjacent free gaps before falling back to alloc+copy+free
- [x] **Harden `optimize_space`**: Assert that the new compacted position plus `HEADER + new_body` does not exceed the old block's start offset

## Testing

- [x] **Add concurrency test**: Test concurrent alloc/dealloc to validate thread safety (or confirm single-threaded constraint)
- [x] **Make `free_reclaims_full_space` portable**: Remove assumptions about pointer width; compute expected sizes from `HEADER` and alignment dynamically
- [x] **Add edge-case tests**: Zero-size alloc, align > size, alloc that exactly fills the buffer, dealloc of invalid pointer
