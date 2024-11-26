// TODO: revise examples in the doc
// #![doc = include_str!("../README.md")]

pub(crate) mod hp_impl;
mod strong;
mod utils;
mod weak;

pub use strong::*;
pub use weak::*;

pub fn num_garbages() -> usize {
    hp_impl::DEFAULT_DOMAIN.num_garbages()
}

pub fn set_counts_between_flush(counts: usize) {
    hp_impl::set_counts_between_flush(counts);
}
