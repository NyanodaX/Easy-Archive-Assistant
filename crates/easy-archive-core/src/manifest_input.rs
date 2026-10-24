//! A bounded, pinned manifest independent of whether its referenced parts exist.
use crate::{
    MAX_MANIFEST_BYTES, Manifest, Result, parse_manifest,
    recovery::{RecoverySource, read_bounded_file},
    restart::RestartPermit,
    secure_fs::{SafeDir, Snapshot, input_parent},
    transaction::require_no_pending_except,
};
use cap_std::fs::File;
use std::path::Path;

pub(crate) struct ManifestInput {
    pub directory: SafeDir,
    pub name: String,
    file: File,
    snapshot: Snapshot,
    pub manifest: Manifest,
}
impl ManifestInput {
    pub fn load(path: &Path, permit: Option<&RestartPermit>) -> Result<Self> {
        let (directory, name) = input_parent(path)?;
        require_no_pending_except(&directory, permit)?;
        let mut file = directory.open_read(&name)?;
        let snapshot = Snapshot::of(&file)?;
        let manifest = parse_manifest(&read_bounded_file(&mut file, MAX_MANIFEST_BYTES)?)?;
        let input = Self {
            directory,
            name,
            file,
            snapshot,
            manifest,
        };
        input.check()?;
        Ok(input)
    }
    pub fn check(&self) -> Result<()> {
        self.directory.check_location()?;
        self.snapshot.check(&self.file)?;
        self.snapshot.check(&self.directory.open_read(&self.name)?)
    }
    pub fn source(&self) -> RecoverySource {
        RecoverySource {
            path: self.directory.path.join(&self.name),
            snapshot: self.snapshot.clone(),
        }
    }
}
