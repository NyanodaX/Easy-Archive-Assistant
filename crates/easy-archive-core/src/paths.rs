use crate::error::io_error;
use crate::{ErrorCode, MAX_PARTS, Result, VolumeError};
use serde::Serialize;
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    ArchiveBytes,
    RawVolume,
    NativeZipVolume,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceFile {
    pub validated_path: PathBuf,
    pub size: u64,
    pub kind: InputKind,
}

pub fn validate_basename(name: &str) -> Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let numbered_device = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.encode_utf16().count() > 240
        || name.len() > 1024
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || numbered_device
    {
        return Err(VolumeError::new(ErrorCode::UnsafePath));
    }
    Ok(())
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Metadata checks are a read-only preflight, not a race-free write permit.
/// Future executors must revalidate handles and use exclusive output creation.
pub fn checked_path(path: &Path, directory: bool) -> Result<PathBuf> {
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(VolumeError::new(ErrorCode::UnsafePath)),
            Component::Normal(name) => validate_basename(
                name.to_str()
                    .ok_or_else(|| VolumeError::new(ErrorCode::UnsafePath))?,
            )?,
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                use std::path::Prefix;
                if !matches!(prefix.kind(), Prefix::Disk(_)) {
                    return Err(VolumeError::new(ErrorCode::UnsafePath));
                }
            }
            _ => (),
        }
    }
    let absolute = std::path::absolute(path).map_err(io_error)?;
    for ancestor in absolute.ancestors() {
        if is_link(&fs::symlink_metadata(ancestor).map_err(io_error)?) {
            return Err(VolumeError::new(ErrorCode::UnsafePath));
        }
    }
    let metadata = fs::symlink_metadata(&absolute).map_err(io_error)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(VolumeError::new(ErrorCode::InputNotRegularFile));
    }
    Ok(absolute)
}

pub fn classify_filename(name: &str) -> InputKind {
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit('.').next().unwrap_or("");
    if extension.len() >= 2
        && extension.starts_with('z')
        && extension[1..].bytes().all(|c| c.is_ascii_digit())
    {
        InputKind::NativeZipVolume
    } else if extension.len() >= 3 && extension.bytes().all(|c| c.is_ascii_digit()) {
        InputKind::RawVolume
    } else if lower.ends_with(".zip") || lower.ends_with(".7z") {
        InputKind::ArchiveBytes
    } else {
        InputKind::Other
    }
}

impl SourceFile {
    pub fn inspect(path: &Path) -> Result<Self> {
        let validated_path = checked_path(path, false)?;
        let kind = classify_filename(
            validated_path
                .file_name()
                .and_then(|v| v.to_str())
                .ok_or_else(|| VolumeError::new(ErrorCode::UnsafePath))?,
        );
        let size = fs::metadata(&validated_path).map_err(io_error)?.len();
        Ok(Self {
            validated_path,
            size,
            kind,
        })
    }

    pub fn require_splittable(&self, allow_any_file: bool) -> Result<()> {
        match self.kind {
            InputKind::ArchiveBytes => Ok(()),
            InputKind::Other if allow_any_file => Ok(()),
            _ => Err(VolumeError::new(ErrorCode::UnsupportedVolumeFormat)),
        }
    }
}

/// Checks every proposed name without creating files or directories.
pub fn plan_output_paths(
    directory: &Path,
    basename: &str,
    count: u32,
) -> Result<(Vec<PathBuf>, PathBuf)> {
    validate_basename(basename)?;
    if count == 0 || count as usize > MAX_PARTS {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let directory = checked_path(directory, true)?;
    let names: Vec<String> = (1..=count)
        .map(|index| format!("{basename}.{index:03}"))
        .collect();
    let manifest_name = format!("{basename}.afparts.json");
    for name in names.iter().chain(std::iter::once(&manifest_name)) {
        validate_basename(name)?;
    }
    let paths: Vec<PathBuf> = names.iter().map(|name| directory.join(name)).collect();
    let manifest = directory.join(manifest_name);
    for path in paths.iter().chain(std::iter::once(&manifest)) {
        match fs::symlink_metadata(path) {
            Ok(_) => return Err(VolumeError::new(ErrorCode::OutputExists)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok((paths, manifest))
}
