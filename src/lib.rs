#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

mod allocator;
mod block;
mod lock;

pub use allocator::Allocator;

#[cfg(test)]
mod tests;
