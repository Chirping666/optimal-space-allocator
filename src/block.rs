pub(crate) const NONE: usize = usize::MAX;

/// Inline header stored at the start of each allocated block.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct BlockHeader {
    pub(crate) size: usize,
    pub(crate) align: usize,
    pub(crate) next: usize,
}

pub(crate) const HEADER: usize = size_of::<BlockHeader>();

/// Compute how many body bytes a block at `off` actually occupies,
/// given the user's `size` and `align` and the buffer base address.
///
/// Infallible variant for blocks already vetted by [`checked_body_len`] when
/// they were placed. Re-running the same arithmetic at the same or a smaller
/// offset cannot wrap: every intermediate sum is monotonic in `off`.
#[inline]
pub(crate) fn body_len(base: usize, off: usize, size: usize, align: usize) -> usize {
    let raw = base + off + HEADER;
    let aligned = align_up(raw, align);
    let padding = aligned - raw;
    debug_assert!(aligned >= raw, "align_up must not wrap around");
    let body = align_up(size + padding, size_of::<usize>());
    debug_assert!(
        body >= size,
        "body_len must be at least as large as the requested size"
    );
    body
}

/// [`body_len`] for unvetted requests: `None` when any intermediate sum would
/// wrap, which a fit check must treat as "does not fit".
///
/// `align_up`'s `v + align - 1` wraps whenever `v` is within `align` of the
/// top of the address space — a real position for a `no_std` buffer on
/// kernel or embedded targets — and the wrapped result masquerades as a tiny
/// body that then compares as fitting.
#[inline]
pub(crate) fn checked_body_len(base: usize, off: usize, size: usize, align: usize) -> Option<usize> {
    let raw = base.checked_add(off)?.checked_add(HEADER)?;
    let padding = checked_align_up(raw, align)? - raw;
    checked_align_up(size.checked_add(padding)?, size_of::<usize>())
}

/// [`align_up`] that reports wrap-around instead of silently producing a
/// small result.
#[inline]
pub(crate) fn checked_align_up(v: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    Some(v.checked_add(align - 1)? & !(align - 1))
}

#[inline]
pub(crate) const fn align_up(v: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (v + align - 1) & !(align - 1)
}
