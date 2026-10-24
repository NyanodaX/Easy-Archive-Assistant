use crate::{
    ErrorCode, MAX_MANIFEST_BYTES, MAX_PARTS, Manifest, Phase, Result, TaskContext, VolumeError,
    parse_manifest,
};
use crate::{
    error::io_error,
    secure_fs::{SafeDir, Snapshot, input_parent},
    stream::{digest_string, read_chunk},
    transaction::MAX_DIRECTORY_ENTRIES,
};
use cap_std::fs::File;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize)]
pub struct GroupPart {
    pub index: u32,
    pub filename: String,
    /// Actual, validated location; manifests still contain basic filenames only.
    pub path: PathBuf,
    pub size: u64,
    #[serde(skip)]
    directory_index: Option<usize>,
    #[serde(skip)]
    pub(crate) snapshot: Option<Snapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupInspection {
    pub directory: PathBuf,
    pub original_filename: String,
    pub source_size: u64,
    pub part_count: u32,
    pub has_manifest: bool,
    pub explicit_selection: bool,
    pub parts: Vec<GroupPart>,
}

pub(crate) struct Group {
    pub directory: SafeDir,
    part_directories: Vec<SafeDir>,
    pub info: GroupInspection,
    pub manifest: Option<Manifest>,
    manifest_input: Option<(String, File, Snapshot)>,
}

fn same_name(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    {
        a.to_lowercase() == b.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn missing(error: VolumeError) -> VolumeError {
    if error.code == ErrorCode::InputNotFound {
        VolumeError::new(ErrorCode::PartMissing)
    } else {
        error
    }
}

pub(crate) fn load_group(path: &Path) -> Result<Group> {
    load_group_with_permit(path, None)
}

pub(crate) fn load_group_with_permit(
    path: &Path,
    permit: Option<&crate::restart::RestartPermit>,
) -> Result<Group> {
    load_selected_with_permit(path, &[], permit)
}

pub const MAX_SOURCE_DIRECTORIES: usize = 64;
pub const MAX_SELECTED_PATH_BYTES: usize = 4 * 1024 * 1024;

fn volume_index(base: &str, name: &str, explicit: bool) -> Result<Option<u32>> {
    let Some((part_base, suffix)) = name.rsplit_once('.') else {
        return if explicit {
            Err(VolumeError::new(ErrorCode::UnsupportedVolumeFormat))
        } else {
            Ok(None)
        };
    };
    if !same_name(part_base, base) {
        return if explicit {
            Err(VolumeError::new(ErrorCode::PartGroupMismatch))
        } else {
            Ok(None)
        };
    }
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return if explicit {
            Err(VolumeError::new(ErrorCode::UnsupportedVolumeFormat))
        } else {
            Ok(None)
        };
    }
    let index: u32 = suffix
        .parse()
        .map_err(|_| VolumeError::new(ErrorCode::LimitExceeded))?;
    if index == 0 || index as usize > MAX_PARTS {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    if suffix != format!("{index:03}") {
        return Err(VolumeError::new(ErrorCode::PartDuplicate));
    }
    Ok(Some(index))
}

/// Explicit paths never trigger adjacent-volume discovery. Each source directory
/// is independently anchored and checked, including removable-drive locations.
pub(crate) fn load_selected_with_permit(
    path: &Path,
    selected_paths: &[PathBuf],
    permit: Option<&crate::restart::RestartPermit>,
) -> Result<Group> {
    if selected_paths.len() > MAX_PARTS
        || selected_paths
            .iter()
            .try_fold(0usize, |total, p| total.checked_add(p.as_os_str().len()))
            .is_none_or(|n| n > MAX_SELECTED_PATH_BYTES)
    {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let (directory, selected) = input_parent(path)?;
    // A known unfinished publication must not be treated as a complete,
    // unmanifested source group merely by choosing another output directory.
    crate::transaction::require_no_pending_except(&directory, permit)?;
    let (base, explicit_manifest) = if let Some(base) = selected.strip_suffix(".afparts.json") {
        (base.to_owned(), true)
    } else {
        let (base, suffix) = selected
            .rsplit_once('.')
            .ok_or_else(|| VolumeError::new(ErrorCode::UnsupportedVolumeFormat))?;
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(VolumeError::new(ErrorCode::UnsupportedVolumeFormat));
        }
        (base.to_owned(), false)
    };
    crate::validate_basename(&base)?;
    let manifest_name = if explicit_manifest {
        selected.clone()
    } else {
        format!("{base}.afparts.json")
    };
    let manifest_present = if !selected_paths.is_empty() && !explicit_manifest {
        false
    } else {
        match directory.dir.symlink_metadata(&manifest_name) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_error(e)),
        }
    };
    let mut manifest_input = None;
    let manifest = if manifest_present {
        let mut file = directory.open_read(&manifest_name)?;
        let snapshot = Snapshot::of(&file)?;
        if snapshot.size > MAX_MANIFEST_BYTES as u64 {
            return Err(VolumeError::new(ErrorCode::ManifestInvalid));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        snapshot.check(&file)?;
        let manifest = parse_manifest(&bytes)?;
        // The selected group and manifest must identify the same naming scheme.
        if !same_name(&manifest.parts[0].filename, &format!("{base}.001")) {
            return Err(VolumeError::new(ErrorCode::ManifestInvalid));
        }
        manifest_input = Some((manifest_name, file, snapshot));
        Some(manifest)
    } else {
        None
    };
    if explicit_manifest && manifest.is_none() {
        return Err(VolumeError::new(ErrorCode::ManifestInvalid));
    }
    let mut discovered = BTreeMap::new();
    let mut part_directories: Vec<SafeDir> = Vec::new();
    let mut directory_indices = BTreeMap::new();
    if selected_paths.is_empty() {
        for (i, entry) in directory.dir.entries().map_err(io_error)?.enumerate() {
            if i >= MAX_DIRECTORY_ENTRIES {
                return Err(VolumeError::new(ErrorCode::LimitExceeded));
            }
            let name = entry.map_err(io_error)?.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(index) = volume_index(&base, name, false)?
                && discovered.insert(index, (name.to_owned(), None)).is_some()
            {
                return Err(VolumeError::new(ErrorCode::PartDuplicate));
            }
        }
    } else {
        for source in selected_paths {
            let (source_directory, name) = input_parent(source)?;
            let index = volume_index(&base, &name, true)?.expect("explicit volume");
            let directory_index =
                if let Some(existing) = directory_indices.get(&source_directory.path) {
                    *existing
                } else {
                    if part_directories.len() == MAX_SOURCE_DIRECTORIES {
                        return Err(VolumeError::new(ErrorCode::LimitExceeded));
                    }
                    crate::transaction::require_no_pending_except(&source_directory, permit)?;
                    let next = part_directories.len();
                    directory_indices.insert(source_directory.path.clone(), next);
                    part_directories.push(source_directory);
                    next
                };
            if discovered
                .insert(index, (name, Some(directory_index)))
                .is_some()
            {
                return Err(VolumeError::new(ErrorCode::PartDuplicate).for_part(index));
            }
        }
        if !explicit_manifest
            && !discovered.values().any(|(name, dir)| {
                same_name(
                    &part_directories[dir.expect("selected directory")]
                        .path
                        .join(name)
                        .to_string_lossy(),
                    &directory.path.join(&selected).to_string_lossy(),
                )
            })
        {
            return Err(VolumeError::new(ErrorCode::InvalidArgument));
        }
    }
    let count = manifest
        .as_ref()
        .map(|m| m.part_count as usize)
        .unwrap_or(discovered.len());
    if count == 0 {
        return Err(VolumeError::new(ErrorCode::PartMissing));
    }
    if discovered.len() > count {
        return Err(VolumeError::new(ErrorCode::PartDuplicate));
    }
    let mut total = 0_u64;
    let mut parts = Vec::with_capacity(count);
    for i in 1..=count as u32 {
        let (filename, directory_index) = discovered
            .get(&i)
            .ok_or_else(|| VolumeError::new(ErrorCode::PartMissing).for_part(i))?
            .clone();
        let source_directory = directory_index
            .map(|index| &part_directories[index])
            .unwrap_or(&directory);
        let file = source_directory
            .open_read(&filename)
            .map_err(|error| missing(error).for_part(i))?;
        let snapshot = Snapshot::of(&file)?;
        if let Some(m) = &manifest {
            let part = &m.parts[i as usize - 1];
            if !same_name(&part.filename, &filename) || part.size != snapshot.size {
                return Err(VolumeError::new(ErrorCode::PartMissing).for_part(i));
            }
        }
        if snapshot.size == 0 && count != 1 {
            return Err(VolumeError::new(ErrorCode::PartMissing));
        }
        total = total
            .checked_add(snapshot.size)
            .ok_or_else(|| VolumeError::new(ErrorCode::LimitExceeded))?;
        parts.push(GroupPart {
            index: i,
            path: source_directory.path.join(&filename),
            filename,
            directory_index,
            size: snapshot.size,
            snapshot: Some(snapshot),
        });
    }
    let original_filename = manifest
        .as_ref()
        .map(|m| m.original_filename.clone())
        .unwrap_or(base);
    let info = GroupInspection {
        directory: directory.path.clone(),
        original_filename,
        source_size: total,
        part_count: count as u32,
        has_manifest: manifest.is_some(),
        explicit_selection: !selected_paths.is_empty(),
        parts,
    };
    Ok(Group {
        directory,
        part_directories,
        info,
        manifest,
        manifest_input,
    })
}

pub fn inspect_group(path: &Path) -> Result<GroupInspection> {
    Ok(load_group(path)?.info)
}

pub fn inspect_selected_group(path: &Path, selected_paths: &[PathBuf]) -> Result<GroupInspection> {
    Ok(load_selected_with_permit(path, selected_paths, None)?.info)
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub source_size: u64,
    pub part_count: u32,
    pub sha256: String,
    pub verification_level: String,
    pub has_manifest: bool,
    pub has_external_digest: bool,
    pub provenance_authenticated: bool,
}

pub fn validate_expected_hash(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(VolumeError::new(ErrorCode::InvalidArgument));
    }
    Ok(())
}

impl Group {
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        if self.info.explicit_selection {
            self.info
                .parts
                .iter()
                .map(|part| part.path.clone())
                .collect()
        } else {
            Vec::new()
        }
    }
    pub(crate) fn start_inputs(&self) -> Vec<crate::recovery::RecoverySource> {
        let mut inputs: Vec<_> = self
            .info
            .parts
            .iter()
            .map(|part| crate::recovery::RecoverySource {
                path: part.path.clone(),
                snapshot: part.snapshot.clone().expect("loaded group"),
            })
            .collect();
        if let Some((name, _, snapshot)) = &self.manifest_input {
            inputs.push(crate::recovery::RecoverySource {
                path: self.directory.path.join(name),
                snapshot: snapshot.clone(),
            });
        }
        inputs
    }
    pub fn open_part(&self, part: &GroupPart) -> Result<File> {
        let directory = part
            .directory_index
            .map(|i| &self.part_directories[i])
            .unwrap_or(&self.directory);
        directory.check_location()?;
        let file = directory.open_read(&part.filename).map_err(missing)?;
        part.snapshot
            .as_ref()
            .expect("loaded group part")
            .check(&file)?;
        Ok(file)
    }

    pub fn check_metadata(&self) -> Result<()> {
        self.directory.check_location()?;
        for directory in &self.part_directories {
            directory.check_location()?;
        }
        if let Some((name, file, snapshot)) = &self.manifest_input {
            snapshot.check(file)?;
            snapshot.check(&self.directory.open_read(name)?)?;
        }
        for part in &self.info.parts {
            self.open_part(part)?;
        }
        Ok(())
    }

    pub fn verify(
        &self,
        expected: Option<&str>,
        context: &mut TaskContext<'_>,
        buffer: &mut [u8],
    ) -> Result<VerificationReport> {
        if let Some(expected) = expected {
            validate_expected_hash(expected)?;
        }
        let mut whole = Sha256::new();
        let mut completed = 0;
        for part in &self.info.parts {
            context.report(
                Phase::Verifying,
                completed,
                self.info.source_size,
                Some(part.index),
            )?;
            let mut file = self.open_part(part)?;
            let mut part_hash = Sha256::new();
            let mut remaining = part.size;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = read_chunk(&mut file, &mut buffer[..limit], context)?;
                if count == 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                part_hash.update(&buffer[..count]);
                whole.update(&buffer[..count]);
                remaining -= count as u64;
                completed += count as u64;
                context.report(
                    Phase::Verifying,
                    completed,
                    self.info.source_size,
                    Some(part.index),
                )?;
            }
            let mut extra = [0];
            if read_chunk(&mut file, &mut extra, context)? != 0 {
                return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
            }
            part.snapshot
                .as_ref()
                .expect("loaded group part")
                .check(&file)?;
            if self.manifest.as_ref().is_some_and(|manifest| {
                digest_string(part_hash) != manifest.parts[part.index as usize - 1].sha256
            }) {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch).for_part(part.index));
            }
        }
        let hash = digest_string(whole);
        if self
            .manifest
            .as_ref()
            .is_some_and(|m| m.original_sha256 != hash)
            || expected.is_some_and(|v| v != hash)
        {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        self.check_metadata()?;
        Ok(VerificationReport {
            source_size: self.info.source_size,
            part_count: self.info.part_count,
            sha256: hash,
            verification_level: if expected.is_some() {
                "verified_against_original"
            } else {
                "verified_against_available_sources"
            }
            .to_owned(),
            has_manifest: self.manifest.is_some(),
            has_external_digest: expected.is_some(),
            provenance_authenticated: false,
        })
    }
}

pub fn verify_group(
    path: &Path,
    expected_sha256: Option<&str>,
    context: &mut TaskContext<'_>,
) -> Result<VerificationReport> {
    verify_selected_group(path, &[], expected_sha256, context)
}

pub fn verify_selected_group(
    path: &Path,
    selected_paths: &[PathBuf],
    expected_sha256: Option<&str>,
    context: &mut TaskContext<'_>,
) -> Result<VerificationReport> {
    let group = load_selected_with_permit(path, selected_paths, None)?;
    let mut buffer = vec![0; crate::stream::BUFFER_BYTES];
    group.verify(expected_sha256, context, &mut buffer)
}
