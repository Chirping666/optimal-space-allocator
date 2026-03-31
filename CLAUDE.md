# CLAUDE.md

## Active Task

Follow the checklist in `CHECKLIST.md`. Work through items in order (Critical > Design > Testing). Check off each item (`- [x]`) immediately after completing it. Do not batch completions.

## Code Style

- **Ergonomic**: Prefer named types over tuples and raw arrays. Use builder patterns or descriptive constructors over positional arguments.
- **Expressive**: Code should read like intent. Choose names that describe *what* and *why*, not *how*. Avoid abbreviations except universally understood ones (`ptr`, `len`, `buf`).
- **Concise**: No boilerplate for its own sake. If a comment restates what the code already says, delete it. Prefer single clear expressions over multi-step decompositions that add no clarity.
- **Safe by default**: Minimize `unsafe` surface area. Every `unsafe` block must have a `// SAFETY:` comment explaining the exact invariant being upheld. Never blanket-allow unsafe lints.
- **Correct first**: Prioritize correctness over cleverness or performance. Add `debug_assert!` for invariants that are non-obvious. If an optimization makes reasoning harder, it needs a proof comment.

## Rust Conventions

- Use `#![deny(unsafe_op_in_unsafe_fn)]` — unsafe operations must be explicitly opted into even inside unsafe functions.
- Prefer `struct` with named fields over tuple types or fixed-size arrays for data with semantic meaning.
- Keep `unsafe` blocks as small as possible — one operation per block when feasible.
- Use `const` where possible. Prefer `const fn` for anything computable at compile time.
- Write tests that assert failure modes, not just happy paths.

## Architecture Principles

- This is a `no_std` fixed-buffer allocator. Do not introduce heap allocation or `std` dependencies.
- Thread safety must be explicit: either provide real synchronization or clearly document and enforce single-threaded usage at the type level.
- The allocator's linked list is the core data structure. Any changes to block layout must be reflected in `alloc`, `dealloc`, `optimize_space`, and all helpers — audit all call sites.
- Public API surface should be minimal. Internal helpers stay private.

## Testing

- Run tests with `cargo test`.
- All new functionality requires corresponding tests.
- Use `cargo miri test` when available to detect undefined behavior in unsafe code.
- Tests should be self-contained and not depend on platform-specific pointer sizes or buffer addresses.

## Commit Practices

- One checklist item per commit when possible.
- Commit message format: short imperative summary, then blank line, then explain *why* the change matters.
