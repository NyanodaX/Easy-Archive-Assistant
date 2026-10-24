use crate::{
    ErrorCode, MAX_MANIFEST_BYTES, MAX_PARTS, Result, SCHEMA_VERSION, VolumeError,
    validate_basename,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartDescriptor {
    pub index: u32,
    pub filename: String,
    pub offset: u64,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub tool: String,
    pub original_filename: String,
    pub original_size: u64,
    pub original_sha256: String,
    pub split_mode: String,
    pub part_count: u32,
    pub parts: Vec<PartDescriptor>,
    pub created_at: String,
    pub app_version: String,
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl Manifest {
    /// Structural validation only. Does not read parts or authenticate hashes.
    pub fn validate(&self) -> Result<()> {
        let invalid = || VolumeError::new(ErrorCode::ManifestInvalid);
        if self.schema_version != SCHEMA_VERSION
            || self.tool.is_empty()
            || self.tool.len() > 64
            || self.tool.chars().any(char::is_control)
            || self.app_version.is_empty()
            || self.app_version.len() > 64
            || self.app_version.chars().any(char::is_control)
            || !valid_hash(&self.original_sha256)
            || !matches!(self.split_mode.as_str(), "hard" | "balanced")
            || self.parts.is_empty()
            || self.parts.len() > MAX_PARTS
            || self.part_count as usize != self.parts.len()
            || OffsetDateTime::parse(&self.created_at, &Rfc3339).is_err()
        {
            return Err(invalid());
        }
        validate_basename(&self.original_filename).map_err(|_| invalid())?;
        let mut offset = 0_u64;
        let mut filenames = HashSet::new();
        let mut common_prefix: Option<&str> = None;
        for (i, part) in self.parts.iter().enumerate() {
            validate_basename(&part.filename).map_err(|_| invalid())?;
            let suffix = format!(".{:03}", i + 1);
            let prefix = part
                .filename
                .strip_suffix(&suffix)
                .filter(|v| !v.is_empty())
                .ok_or_else(invalid)?;
            if common_prefix.is_some_and(|p| p != prefix) {
                return Err(invalid());
            }
            common_prefix = Some(prefix);
            if part.index as usize != i + 1
                || part.offset != offset
                || !valid_hash(&part.sha256)
                || !filenames.insert(part.filename.to_lowercase())
                || (part.size == 0 && !(self.original_size == 0 && self.parts.len() == 1))
            {
                return Err(invalid());
            }
            offset = offset.checked_add(part.size).ok_or_else(invalid)?;
        }
        if offset != self.original_size {
            return Err(invalid());
        }
        Ok(())
    }
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(VolumeError::new(ErrorCode::ManifestInvalid));
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|_| VolumeError::new(ErrorCode::ManifestInvalid))?;
    manifest.validate()?;
    Ok(manifest)
}
