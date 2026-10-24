use crate::{ErrorCode, Result, TaskContext, VolumeError};
use crate::{error::io_error, secure_fs::Snapshot};
use cap_std::fs::File;
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

pub const BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn digest_string(hasher: Sha256) -> String {
    format!("{:x}", hasher.finalize())
}

pub(crate) fn read_chunk(
    file: &mut File,
    bytes: &mut [u8],
    context: &mut TaskContext<'_>,
) -> Result<usize> {
    loop {
        context.checkpoint(crate::IoPoint::Read)?;
        match file.read(bytes) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            value => return value.map_err(io_error),
        }
    }
}

pub(crate) fn hash_file(
    file: &mut File,
    expected_size: u64,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
) -> Result<String> {
    hash_file_with_whole(file, expected_size, context, buffer, None)
}

/// Hash only this half-open interval; never read unrelated archive bytes.
pub(crate) fn hash_range(
    file: &mut File,
    range: crate::ByteRange,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
) -> Result<String> {
    hash_range_with_whole(file, range, context, buffer, None)
}

pub(crate) fn hash_range_with_whole(
    file: &mut File,
    range: crate::ByteRange,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
    mut whole: Option<&mut Sha256>,
) -> Result<String> {
    let before = Snapshot::of(file)?;
    let mut remaining = range.checked_len()?;
    if range.end_exclusive > before.size {
        return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
    }
    file.seek(SeekFrom::Start(range.start)).map_err(io_error)?;
    let mut hasher = Sha256::new();
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let count = read_chunk(file, &mut buffer[..limit], context)?;
        if count == 0 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        hasher.update(&buffer[..count]);
        if let Some(whole) = &mut whole {
            whole.update(&buffer[..count]);
        }
        remaining -= count as u64;
    }
    context.cancellation.check()?;
    before.check(file)?;
    Ok(digest_string(hasher))
}

pub(crate) fn hash_file_with_whole(
    file: &mut File,
    expected_size: u64,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
    mut whole: Option<&mut Sha256>,
) -> Result<String> {
    let before = Snapshot::of(file)?;
    if before.size != expected_size {
        return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut remaining = expected_size;
    let mut hasher = Sha256::new();
    while remaining > 0 {
        let len = remaining.min(buffer.len() as u64) as usize;
        let n = read_chunk(file, &mut buffer[..len], context)?;
        if n == 0 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        hasher.update(&buffer[..n]);
        if let Some(whole) = &mut whole {
            whole.update(&buffer[..n]);
        }
        remaining -= n as u64;
    }
    let mut extra = [0];
    if read_chunk(file, &mut extra, context)? != 0 {
        return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
    }
    before.check(file)?;
    Ok(digest_string(hasher))
}
