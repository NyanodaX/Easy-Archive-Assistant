//! Capability directories anchor IO; no ambient path is used for publication.
use crate::error::io_error;
use crate::{
    ErrorCode, Result, VolumeError,
    paths::{checked_path, validate_basename},
};
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, File, Metadata, OpenOptions},
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

pub(crate) struct SafeDir {
    pub dir: Dir,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    modified: SystemTime,
}

pub(crate) fn reject_link(meta: &Metadata) -> Result<()> {
    #[cfg(windows)]
    let reparse = {
        use cap_std::fs::MetadataExt as _;
        meta.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if meta.is_symlink() || reparse {
        Err(VolumeError::new(ErrorCode::UnsafePath))
    } else {
        Ok(())
    }
}

impl Snapshot {
    pub fn of(file: &File) -> Result<Self> {
        let meta = file.metadata().map_err(io_error)?;
        reject_link(&meta)?;
        if !meta.is_file() {
            return Err(VolumeError::new(ErrorCode::InputNotRegularFile));
        }
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: meta.modified().map_err(io_error)?.into_std(),
        })
    }
    pub fn check(&self, file: &File) -> Result<()> {
        if *self != Self::of(file)? {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        Ok(())
    }
}

impl SafeDir {
    pub fn identity(&self) -> Result<(u64, u64)> {
        let meta = self.dir.dir_metadata().map_err(io_error)?;
        Ok((meta.dev(), meta.ino()))
    }
    pub fn open(path: &Path) -> Result<Self> {
        let path = checked_path(path, true)?;
        let root = path
            .ancestors()
            .last()
            .ok_or_else(|| VolumeError::new(ErrorCode::UnsafePath))?;
        let mut dir = Dir::open_ambient_dir(root, ambient_authority()).map_err(io_error)?;
        for component in path.components() {
            if let Component::Normal(name) = component {
                dir = dir.open_dir_nofollow(name).map_err(io_error)?;
                reject_link(&dir.dir_metadata().map_err(io_error)?)?;
            }
        }
        Ok(Self { dir, path })
    }

    pub fn check_location(&self) -> Result<()> {
        let current = Self::open(&self.path)?;
        let old = self.dir.dir_metadata().map_err(io_error)?;
        let new = current.dir.dir_metadata().map_err(io_error)?;
        if old.dev() != new.dev() || old.ino() != new.ino() {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        Ok(())
    }

    pub fn open_read(&self, name: &str) -> Result<File> {
        validate_basename(name)?;
        let metadata = self.dir.symlink_metadata(name).map_err(io_error)?;
        reject_link(&metadata)?;
        if !metadata.is_file() {
            return Err(VolumeError::new(ErrorCode::InputNotRegularFile));
        }
        let mut opts = OpenOptions::new();
        opts.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            // If a regular file is raced into a FIFO after the metadata check,
            // do not block in open before the handle's file type can be checked.
            opts.nonblock(true);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            // Refuse concurrent writers/deletion for the lifetime of the handle.
            opts.share_mode(1);
        }
        let file = self.dir.open_with(name, &opts).map_err(io_error)?;
        Snapshot::of(&file)?;
        Ok(file)
    }

    pub fn create_new(&self, name: &str) -> Result<File> {
        validate_basename(name)?;
        let mut opts = OpenOptions::new();
        opts.read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            opts.share_mode(1);
        }
        self.dir.open_with(name, &opts).map_err(io_error)
    }

    /// Delete access is requested only for files known to this live transaction.
    /// Failure retains staging; pathname-based deletion is never a fallback.
    pub fn open_for_publication(&self, name: &str, live_owned: bool) -> Result<(File, bool)> {
        #[cfg(windows)]
        if live_owned {
            use cap_std::fs::OpenOptionsExt;
            validate_basename(name)?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .follow(FollowSymlinks::No)
                .access_mode(easy_archive_windows::READ_DELETE_ACCESS)
                .share_mode(1);
            if let Ok(file) = self.dir.open_with(name, &options) {
                Snapshot::of(&file)?;
                return Ok((file, true));
            }
        }
        #[cfg(not(windows))]
        let _ = live_owned;
        Ok((self.open_read(name)?, false))
    }

    pub fn require_absent(&self, name: &str) -> Result<()> {
        validate_basename(name)?;
        match self.dir.symlink_metadata(name) {
            Ok(_) => Err(VolumeError::new(ErrorCode::OutputExists)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(e)),
        }
    }

    pub fn sync(&self) -> Result<()> {
        // Windows directory FlushFileBuffers is not generally supported by a
        // read-only directory handle. File flushes still run; do not claim
        // full power-loss durability on Windows.
        #[cfg(not(windows))]
        self.dir
            .try_clone()
            .map_err(io_error)?
            .into_std_file()
            .sync_all()
            .map_err(io_error)?;
        Ok(())
    }
}

pub(crate) fn input_parent(path: &Path) -> Result<(SafeDir, String)> {
    let absolute = checked_path(path, false)?;
    let name = absolute
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| VolumeError::new(ErrorCode::UnsafePath))?
        .to_owned();
    Ok((
        SafeDir::open(
            absolute
                .parent()
                .ok_or_else(|| VolumeError::new(ErrorCode::UnsafePath))?,
        )?,
        name,
    ))
}
