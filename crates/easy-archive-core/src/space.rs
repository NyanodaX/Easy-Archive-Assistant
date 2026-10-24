//! Advisory filesystem query. Space is never reserved; IO errors remain decisive.
use crate::{ErrorCode, Result, VolumeError, secure_fs::SafeDir};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct SpaceEstimate {
    pub estimated_payload_bytes: u64,
    pub overhead_estimate_bytes: u64,
    pub required_space_bytes: u64,
    pub available_space_bytes: Option<u64>,
    pub free_space_checked: bool,
    pub sufficient: Option<bool>,
}

impl SpaceEstimate {
    pub fn require_sufficient(&self) -> Result<()> {
        if self.sufficient == Some(false) {
            Err(VolumeError::new(ErrorCode::InsufficientDiskSpace))
        } else {
            Ok(())
        }
    }
}

pub fn estimate_space(directory: &Path, payload: u64, files: u32) -> Result<SpaceEstimate> {
    let safe = SafeDir::open(directory)?;
    safe.check_location()?;
    // Allow for manifest, bounded recovery plan, journal and small-file allocation.
    // Filesystem compression, quotas and concurrent writers can invalidate this estimate.
    let overhead = 1024 * 1024 + u64::from(files) * 8192;
    let required = payload
        .checked_add(overhead)
        .ok_or_else(|| VolumeError::new(ErrorCode::LimitExceeded))?;
    let available = fs4::available_space(&safe.path).ok();
    safe.check_location()?;
    Ok(SpaceEstimate {
        estimated_payload_bytes: payload,
        overhead_estimate_bytes: overhead,
        required_space_bytes: required,
        available_space_bytes: available,
        free_space_checked: available.is_some(),
        sufficient: available.map(|n| n >= required),
    })
}

pub(crate) fn lock(file: cap_std::fs::File) -> Result<std::fs::File> {
    let file = file.into_std();
    fs4::FileExt::try_lock(&file)
        .map_err(|_| VolumeError::new(ErrorCode::TransactionNeedsRecovery))?;
    Ok(file)
}
