//! Everything that touches the local disk.

pub(crate) mod control;
pub(crate) mod file;
pub mod naming;
pub(crate) mod plan;

use std::io;
use std::path::Path;

/// Removes a temporary file, ignoring "not found".
pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
