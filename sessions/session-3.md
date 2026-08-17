# Session 3 — 2026-08-17

Started with no `CHECKLIST.md` — the previous session's was completed and
deleted — so the session opened by reviewing the caller-provided-buffer API
introduced in `2e80854` and writing a fresh checklist from what that review
found. Three of the five critical items were confirmed by experiment before
being written down, rather than reasoned about on paper.

## Log

- **Add session 3 checklist**: Reviewed the new buffer API and recorded five
  critical items, five design items, and four testing items. Confirmed the
  lifetime escape, the misaligned header write, and the compaction panic with
  throwaway probe tests first, so each entry describes a reproduced failure
  rather than a suspicion.

- **Tie `Allocator` to its buffer's lifetime**: `new(&mut [u8])` stored a
  `*mut [u8]` in a struct with no lifetime parameter, so the borrow ended at
  the constructor call. Safe code could build an allocator from a stack
  buffer, let the buffer die, and keep allocating into the dead frame — the
  probe returned a live pointer into freed stack. Added a `'buf` parameter and
  a `PhantomData<&'buf mut [u8]>` field, with a `compile_fail` doctest pinning
  the escape as a borrow-check error. All 22 tests pass.

- **Make `from_ptr` unsafe**: It was a safe function accepting an arbitrary
  `*mut [u8]`, so safe code could point an allocator at a dangling region. It
  also took a `length` separate from the slice pointer's own metadata, and a
  length exceeding the real region would be written past. Made it `unsafe`
  with a documented contract and took the length from the pointer, removing
  the mismatch entirely. All 22 tests pass.

- **Align the usable region for `BlockHeader`**: `get`/`set` do
  `ptr::read`/`ptr::write` of a `BlockHeader` at `base + off`, but a caller's
  `&mut [u8]` carries no alignment guarantee — a buffer starting at an address
  ≡ 2 mod 8 had every header accessed misaligned, which is UB. Since every
  block offset is a multiple of the header's alignment, aligning the base once
  in the constructor fixes all of them. This also folded in the redundant-state
  design item: the fix needs one adjusted `base` plus one `length`, which is
  the deduplication that item asked for. Added alignment `debug_assert!`s to
  both header accessors. All 22 tests pass.

- **Skip compaction moves that would widen a block**: A block's footprint
  includes alignment padding measured from its own offset, so moving it left
  can make it *wider* and push its end past where it previously finished. The
  overlap `debug_assert!` fired on a legitimate compaction (two 8-aligned
  blocks then a 256-aligned one, free the first two, compact), and in release
  the running `target` cursor advanced by a body length computed for an offset
  the block was never placed at. Now both candidate extents are computed and
  the block moves only when its extent genuinely shrinks. All 22 tests pass.

- **Reject requests larger than the buffer up front**: `body_len` adds
  `size + padding` and `fit_gap` adds `HEADER`, all unchecked. A `Layout`
  bounds its size, but `realloc`'s `new_size` is a bare `usize` — a huge value
  wrapped and compared as fitting, which would hand back a block far smaller
  than requested. Such a request can never be satisfied anyway, so refusing it
  in `alloc` and `realloc` both fixes the wrap and keeps the remaining
  arithmetic clear of its limits. All 22 tests pass.

- **Name the best-fit candidate**: `alloc` tracked its best candidate as
  `Option<(usize, usize, usize)>` with a comment naming the fields, and one of
  those fields was never read. Introduced a `Placement` struct and let
  `fit_gap` (now `waste_in_gap`) return only the waste it is actually
  consulted for. Its overlap `debug_assert!` turned out to restate
  `needed <= gap`, so it could never fire and was deleted. Folding the waste
  comparison into let-chains cleared both `collapsible_if` warnings. All 22
  tests pass.

- **Document the `optimize_space` callback contract**: The `relocate` closure
  runs with the spin lock held, so touching the allocator from it deadlocks —
  now stated alongside the existing safety requirement. Also corrected the
  claim that `relocate` fires when a user pointer changes: it fires when a
  *block* moves, which for a heavily aligned block can leave the user pointer
  exactly where it was.

- **Add regression tests**: Four tests — `unaligned_buffer_is_usable`,
  `optimize_space_with_mixed_alignments`, `realloc_beyond_buffer_returns_null`,
  and `from_ptr_length_comes_from_metadata`. The first three were each checked
  against the code as it stood before its fix and each fails there: the
  alignment assert, the overlap assert, and an overflow inside `body_len`
  respectively. Added a `repr(align(16))` test buffer so the tests that derive
  offsets from a buffer address are unaffected by the constructor's new
  padding. 26 tests pass in debug, 23 in release (three are
  `debug_assertions`-only), and clippy is clean.

## Notes for next session

- `cargo miri test` is still unavailable on this toolchain (stable only), so
  the alignment fix is verified by `debug_assert!`s in `get`/`set` rather than
  by a UB checker. Worth re-running under miri if a nightly toolchain becomes
  available — the unsafe surface has not had a real UB check yet.
- `#[repr(C)]` on `Allocator` was added in session 2 so `data` would sit at
  offset 0 for tests that assumed it. No test depends on the struct layout any
  more, so the attribute is now unmotivated and could be dropped.
- The spin lock has no poisoning and no backoff beyond `spin_loop`. Fine for
  short critical sections, but `optimize_space` holds it across an unbounded
  user callback.
