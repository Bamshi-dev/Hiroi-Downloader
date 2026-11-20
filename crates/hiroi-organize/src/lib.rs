//! Keeping downloads in order: which series a file belongs to, read
//! from its name, and names for groups of downloads the user did not
//! name.
//!
//! Everything here works on names only and never touches a file, so
//! deciding what to move stays with the caller.

mod group;
mod series;

pub use group::{Batch, group_name};
pub use series::{Episode, episode};

#[cfg(test)]
mod tests;
