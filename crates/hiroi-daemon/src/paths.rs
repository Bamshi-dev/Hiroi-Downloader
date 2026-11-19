//! Where the service keeps its socket, lock, and database.
//!
//! The runtime directory (socket and lock) follows the XDG base
//! directory specification: `$XDG_RUNTIME_DIR/hiroi`, which the system
//! creates per user and clears at logout. Without it we fall back to
//! `/tmp/hiroi-<uid>`, which we create ourselves and refuse to use when
//! another user owns it.

use std::io;
use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};

pub const SOCKET_NAME: &str = "hiroid.sock";
pub const LOCK_NAME: &str = "hiroid.lock";
pub const DATABASE_NAME: &str = "hiroi.db";

pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("hiroi"),
        None => std::env::temp_dir().join(format!("hiroi-{}", user_id())),
    }
}

pub fn default_socket() -> PathBuf {
    runtime_dir().join(SOCKET_NAME)
}

/// `~/.local/share/hiroi/hiroi.db` on Linux.
pub fn default_database() -> PathBuf {
    match ProjectDirs::from("", "", "hiroi") {
        Some(dirs) => dirs.data_dir().join(DATABASE_NAME),
        None => PathBuf::from(DATABASE_NAME),
    }
}

/// The user's download folder, or the current directory.
pub fn default_download_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Creates `dir` with mode 0700, or checks that an existing one belongs
/// to us and is not open to others. A directory prepared by another
/// user could hold a socket or symlink that impersonates the service.
pub fn prepare_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::symlink_metadata(dir)?;
        if !metadata.is_dir() || metadata.uid() != user_id() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a directory owned by you", dir.display()),
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn user_id() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
pub(crate) fn user_id() -> u32 {
    0
}
