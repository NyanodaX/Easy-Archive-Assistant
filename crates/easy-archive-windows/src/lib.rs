//! Minimal Windows handle operation. No pathname lookup or handle ownership transfer.
//! The core remains unsafe-free; this adapter isolates the single Win32 FFI call.
#![cfg(windows)]
use std::{
    io,
    os::windows::io::{AsHandle, AsRawHandle},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
};

pub const READ_DELETE_ACCESS: u32 = 0x8000_0000 | 0x0001_0000;

/// Mark the opened directory entry for deletion when its handles close.
/// The caller must establish ownership and identity first and retain outputs.
/// Windows requires DELETE access. Failure never falls back to pathname deletion.
#[allow(unsafe_code)]
pub fn mark_delete_on_close(file: &impl AsHandle) -> io::Result<()> {
    let borrowed = file.as_handle();
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the borrowed handle stays live for this synchronous call. `info`
    // has the exact Win32 layout and length for FileDispositionInfo. The API
    // neither retains its buffer nor transfers/closes the borrowed handle.
    let ok = unsafe {
        SetFileInformationByHandle(
            borrowed.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::windows::fs::OpenOptionsExt};

    #[test]
    fn deletes_the_opened_staging_link_and_preserves_published_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("stage");
        let published = temp.path().join("published");
        fs::write(&staged, b"verified bytes").unwrap();
        fs::hard_link(&staged, &published).unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .access_mode(READ_DELETE_ACCESS)
            .share_mode(1)
            .open(&staged)
            .unwrap();
        assert!(fs::write(&staged, b"wrong").is_err());
        assert!(fs::rename(&staged, temp.path().join("other")).is_err());
        mark_delete_on_close(&file).unwrap();
        drop(file);
        assert!(!staged.exists());
        assert_eq!(fs::read(published).unwrap(), b"verified bytes");
    }

    #[test]
    fn read_only_handle_cannot_authorize_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keep");
        fs::write(&path, b"KEEP").unwrap();
        let file = fs::File::open(&path).unwrap();
        assert!(mark_delete_on_close(&file).is_err());
        drop(file);
        assert_eq!(fs::read(path).unwrap(), b"KEEP");
    }
}
