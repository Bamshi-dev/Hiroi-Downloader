use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{EngineError, Result};

/// The `.hiroi-part` file a download writes into. It is locked for as long
/// as this value lives, so two processes can never write the same download.
#[derive(Debug)]
pub(crate) struct OutputFile {
    file: Arc<File>,
    path: PathBuf,
}

impl OutputFile {
    /// Opens or creates the part file. `fresh` discards existing content,
    /// which only happens after the lock is ours.
    pub fn open(path: &Path, fresh: bool) -> Result<Self> {
        let io_error = |e| EngineError::io(path, e);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(EngineError::FileLocked(path.into())),
            // Some network file systems have no locks; carry on without.
            Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(TryLockError::Error(e)) => return Err(io_error(e)),
        }
        if fresh {
            file.set_len(0).map_err(io_error)?;
        }
        Ok(Self {
            file: Arc::new(file),
            path: path.to_path_buf(),
        })
    }

    /// Checks free space, then reserves `size` bytes, so a full disk is
    /// reported up front instead of halfway through.
    pub fn reserve(&self, size: u64, preallocate: bool) -> Result<()> {
        let io_error = |e| EngineError::io(&self.path, e);
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let needed = size.saturating_sub(allocated_bytes(&self.file).map_err(io_error)?);
        if let Some(available) = available_space(dir).map_err(io_error)?
            && needed > available
        {
            return Err(EngineError::InsufficientSpace { needed, available });
        }
        if preallocate {
            allocate(&self.file, size).map_err(io_error)
        } else if self.file.metadata().map_err(io_error)?.len() < size {
            self.file.set_len(size).map_err(io_error)
        } else {
            Ok(())
        }
    }

    /// Writes `buffer` at `offset` on a blocking thread and hands the buffer
    /// back for reuse.
    /// Starts writing `buffer` at `offset` on a blocking thread, and hands
    /// back the handle of the write, which returns the buffer.
    pub fn start_write(
        &self,
        buffer: Vec<u8>,
        offset: u64,
    ) -> tokio::task::JoinHandle<io::Result<Vec<u8>>> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || write_all_at(&file, &buffer, offset).map(|()| buffer))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn write_at(&self, buffer: Vec<u8>, offset: u64) -> Result<Vec<u8>> {
        let file = Arc::clone(&self.file);
        let written = tokio::task::spawn_blocking(move || {
            write_all_at(&file, &buffer, offset).map(|()| buffer)
        })
        .await
        .map_err(|e| EngineError::io(&self.path, io::Error::other(e)))?;
        written.map_err(|e| EngineError::io(&self.path, e))
    }

    /// Flushes data to the disk, so a checkpoint never claims bytes a power
    /// cut could still lose.
    pub async fn sync(&self) -> Result<()> {
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || file.sync_data())
            .await
            .map_err(|e| EngineError::io(&self.path, io::Error::other(e)))?
            .map_err(|e| EngineError::io(&self.path, e))
    }
}

/// Moves the finished part file to its real name. Without `overwrite` this
/// never replaces a file that appeared in the meantime.
pub(crate) fn finalize(part: &Path, target: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        return std::fs::rename(part, target).map_err(|e| EngineError::io(target, e));
    }
    rename_no_replace(part, target)
}

#[cfg(target_os = "linux")]
fn rename_no_replace(part: &Path, target: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;

    match renameat_with(CWD, part, CWD, target, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::EXIST) => Err(EngineError::AlreadyExists(target.into())),
        // File systems without RENAME_NOREPLACE (some FUSE mounts, old kernels).
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => rename_checked(part, target),
        Err(errno) => Err(EngineError::io(target, errno.into())),
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(part: &Path, target: &Path) -> Result<()> {
    rename_checked(part, target)
}

fn rename_checked(part: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        return Err(EngineError::AlreadyExists(target.into()));
    }
    std::fs::rename(part, target).map_err(|e| EngineError::io(target, e))
}

#[cfg(unix)]
fn write_all_at(file: &File, buffer: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buffer, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let written = file.seek_write(buffer, offset)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buffer = &buffer[written..];
        offset += written as u64;
    }
    Ok(())
}

/// Bytes actually reserved on disk (not the apparent, possibly sparse, size).
#[cfg(unix)]
fn allocated_bytes(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.blocks() * 512)
}

#[cfg(not(unix))]
fn allocated_bytes(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

#[cfg(unix)]
fn available_space(dir: &Path) -> io::Result<Option<u64>> {
    let stats = rustix::fs::statvfs(dir)?;
    Ok(Some(stats.f_bavail.saturating_mul(stats.f_frsize)))
}

#[cfg(not(unix))]
fn available_space(_dir: &Path) -> io::Result<Option<u64>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn allocate(file: &File, size: u64) -> io::Result<()> {
    use rustix::fs::{FallocateFlags, fallocate};
    use rustix::io::Errno;

    match fallocate(file, FallocateFlags::empty(), 0, size) {
        Ok(()) => Ok(()),
        Err(Errno::OPNOTSUPP | Errno::NOSYS) => extend(file, size),
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn allocate(file: &File, size: u64) -> io::Result<()> {
    extend(file, size)
}

fn extend(file: &File, size: u64) -> io::Result<()> {
    if file.metadata()?.len() < size {
        file.set_len(size)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_open_is_refused_while_locked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.hiroi-part");
        let _first = OutputFile::open(&path, true).unwrap();
        assert!(matches!(
            OutputFile::open(&path, true),
            Err(EngineError::FileLocked(_))
        ));
    }

    #[test]
    fn finalize_never_replaces_without_permission() {
        let dir = tempfile::tempdir().unwrap();
        let (part, target) = (dir.path().join("a.part"), dir.path().join("a"));
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();
        assert!(matches!(
            finalize(&part, &target, false),
            Err(EngineError::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        finalize(&part, &target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[tokio::test]
    async fn reserves_and_writes_at_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.hiroi-part");
        let file = OutputFile::open(&path, true).unwrap();
        file.reserve(8, true).unwrap();
        file.write_at(b"5678".to_vec(), 4).await.unwrap();
        file.write_at(b"1234".to_vec(), 0).await.unwrap();
        file.sync().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"12345678");
    }

    #[test]
    fn refuses_impossible_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let file = OutputFile::open(&dir.path().join("c.hiroi-part"), true).unwrap();
        let result = file.reserve(u64::MAX / 2, false);
        assert!(matches!(result, Err(EngineError::InsufficientSpace { .. })));
    }
}
