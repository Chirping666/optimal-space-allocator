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

#[inline]
pub(crate) const fn align_up(v: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (v + align - 1) & !(align - 1)
}
