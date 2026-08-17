# Checklist — Session 3

Review of the caller-provided-buffer API introduced in `2e80854`. Items are
ordered Critical > Design > Testing.

## Critical

- [x] **`Allocator::new` discards the buffer's lifetime.** `new(&mut [u8])`
  stores a `*mut [u8]` in a struct with no lifetime parameter, so the borrow
  ends at the end of the constructor call. Entirely in safe code, an allocator
  can outlive its buffer and then write into a dead stack frame (verified: an
  `Allocator` built inside a block and used after it returns a live pointer
  into freed memory). Give `Allocator` a `'buf` lifetime parameter tied to the
  buffer.

- [x] **`from_ptr` is a safe function over a raw pointer.** It accepts an
  arbitrary `*mut [u8]` plus an independent `length`, so safe code can hand it
  a null/dangling pointer or a length that exceeds the pointed-to region. Make
  it `unsafe` with a documented contract, and take the length from the slice
  pointer's own metadata so the two can never disagree.

- [ ] **Block headers can be written at misaligned addresses.** `set`/`get`
  do `ptr::write`/`ptr::read` of a `BlockHeader` at `base + off`, but nothing
  constrains the caller's `[u8]` buffer to `align_of::<BlockHeader>()`
  (verified: a buffer starting at an address ≡ 2 mod 8 gets its first header
  written misaligned). Align the usable region up in the constructor and shrink
  the length accordingly.

- [ ] **`optimize_space` mishandles blocks whose alignment padding grows when
  they move left.** A block's footprint is `HEADER + align_up(size + padding,
  usize)`, and `padding` depends on the block's offset, so a compacted block
  can need a *larger* footprint at its new home. The `debug_assert!` guarding
  overlap fires on a legitimate compaction (verified: two 8-aligned blocks
  followed by a 256-aligned block, free the first two, compact → panics). In
  release the block is recorded at a `target` computed from the wrong offset,
  desynchronising the running `target` cursor from the block's real position.
  Skip the move when it would not actually shrink the block's extent.

- [ ] **Unchecked arithmetic on the request path can wrap.** `body_len` adds
  `size + padding` and `align_up` adds `align - 1` with no overflow check, and
  `fit_gap` then computes `HEADER + body`. `realloc`'s `new_size` is an
  arbitrary `usize` from the caller, so a wrapped `needed` can compare as
  fitting and hand back a block far smaller than requested. Reject sizes larger
  than the whole buffer before any of this arithmetic runs.

## Design

- [ ] **`data: *mut [u8]` and `length` are redundant state.** The slice pointer
  already carries its length; storing a second copy invites the two to
  disagree. Keep one base pointer and one length, both established in the
  constructor.

- [ ] **`alloc`'s best-fit candidate is an anonymous 3-tuple.** `Option<(usize,
  usize, usize)>` with a trailing comment naming the fields is exactly what
  CLAUDE.md's "named types over tuples" rule exists to prevent. Introduce a
  named struct.

- [ ] **`optimize_space`'s re-entrancy hazard is undocumented.** The `relocate`
  callback runs with the spin lock held, so calling any allocator method from
  it deadlocks. Say so in the safety docs.

- [ ] **Simplify the `target` cursor update.** `target = (if target < cur {
  target } else { cur }) + HEADER + new_body` re-derives the block's position
  after the fact instead of naming it.

- [ ] **Clear the two `clippy::collapsible_if` warnings in `alloc`.**

## Testing

- [ ] Test that an unaligned caller buffer still produces correctly aligned
  headers and usable allocations.

- [ ] Test compaction with mixed alignments, covering the case where a block
  cannot profitably move left.

- [ ] Test that `realloc` with a `new_size` larger than the buffer returns null
  and leaves the original allocation intact.

- [ ] Test `from_ptr` through its new unsafe contract.
