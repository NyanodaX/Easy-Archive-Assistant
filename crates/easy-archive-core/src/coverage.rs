//! Read-only interval diagnostics. A report is never a reusable write permit.
//! Per-part hash matches from incomplete groups do not prove whole-stream identity.
use crate::{
    ByteRange, ErrorCode, Phase, Result, TaskContext, VolumeError,
    error::io_error,
    manifest_input::ManifestInput,
    secure_fs::Snapshot,
    stream::{BUFFER_BYTES, digest_string, hash_file_with_whole, read_chunk},
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Seek, SeekFrom},
    path::PathBuf,
};

pub const MAX_COVERAGE_GROUPS: usize = 8;
pub const MAX_COVERAGE_PARTS: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageRequest {
    pub target_manifest: PathBuf,
    pub source_manifests: Vec<PathBuf>,
    pub part: Option<u32>,
    pub metadata_only: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartAvailability {
    VerifiedPartHash,
    Pending,
    Missing,
    SizeMismatch,
    HashMismatch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Verified,
    Incomplete,
    Unchecked,
    HashMismatch,
}
#[derive(Debug, Serialize)]
pub struct CoveragePart {
    pub part_index: u32,
    pub range: ByteRange,
    pub status: PartAvailability,
    pub actual_size: Option<u64>,
}
#[derive(Debug, Serialize)]
pub struct CoverageSource {
    pub manifest_path: PathBuf,
    pub whole_stream_state: StreamState,
    pub parts: Vec<CoveragePart>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Covered,
    Pending,
    Missing,
    Conflict,
}
#[derive(Debug, Serialize)]
pub struct CoverageReference {
    pub source_index: usize,
    pub part_index: u32,
    pub whole_stream_verified: bool,
}
#[derive(Debug, Serialize)]
pub struct CoverageSegment {
    pub range: ByteRange,
    pub status: CoverageState,
    pub sources: Vec<CoverageReference>,
}
#[derive(Debug, Serialize)]
pub struct CoverageReport {
    pub original_size: u64,
    pub original_sha256: String,
    pub requested_range: ByteRange,
    pub sources: Vec<CoverageSource>,
    pub segments: Vec<CoverageSegment>,
    pub covered_bytes: u64,
    pub missing_ranges: Vec<ByteRange>,
    pub pending_ranges: Vec<ByteRange>,
    pub conflict_ranges: Vec<ByteRange>,
    pub has_integrity_errors: bool,
    pub complete_coverage: bool,
    pub provenance_authenticated: bool,
    pub metadata_only: bool,
    pub content_read: bool,
    pub writes_performed: bool,
}
struct Candidate {
    group: usize,
    part: usize,
    range: ByteRange,
    snapshot: Snapshot,
    checked: bool,
}
fn part_range(part: &crate::PartDescriptor) -> ByteRange {
    // Manifest validation already checked every addition and contiguous boundary.
    ByteRange {
        start: part.offset,
        end_exclusive: part.offset + part.size,
    }
}
fn append_range(ranges: &mut Vec<ByteRange>, range: ByteRange) {
    if let Some(last) = ranges.last_mut()
        && last.end_exclusive == range.start
    {
        last.end_exclusive = range.end_exclusive;
        return;
    }
    ranges.push(range);
}
fn read_exact(
    file: &mut cap_std::fs::File,
    mut bytes: &mut [u8],
    context: &mut TaskContext<'_>,
) -> Result<()> {
    while !bytes.is_empty() {
        let count = read_chunk(file, bytes, context)?;
        if count == 0 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        bytes = &mut bytes[count..];
    }
    Ok(())
}
fn overlap_equal(
    a: &Candidate,
    b: &Candidate,
    range: ByteRange,
    groups: &[ManifestInput],
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
) -> Result<bool> {
    let ga = &groups[a.group];
    let gb = &groups[b.group];
    let mut fa = ga
        .directory
        .open_read(&ga.manifest.parts[a.part].filename)?;
    let mut fb = gb
        .directory
        .open_read(&gb.manifest.parts[b.part].filename)?;
    a.snapshot.check(&fa)?;
    b.snapshot.check(&fb)?;
    fa.seek(SeekFrom::Start(range.start - a.range.start))
        .map_err(io_error)?;
    fb.seek(SeekFrom::Start(range.start - b.range.start))
        .map_err(io_error)?;
    let (left, right) = buffer.split_at_mut(buffer.len() / 2);
    let mut remaining = range.checked_len()?;
    let mut equal = true;
    while remaining > 0 {
        let len = remaining.min(left.len() as u64) as usize;
        read_exact(&mut fa, &mut left[..len], context)?;
        read_exact(&mut fb, &mut right[..len], context)?;
        if left[..len] != right[..len] {
            equal = false;
            break;
        }
        remaining -= len as u64;
    }
    a.snapshot.check(&fa)?;
    b.snapshot.check(&fb)?;
    Ok(equal)
}

pub fn coverage(
    request: &CoverageRequest,
    context: &mut TaskContext<'_>,
) -> Result<CoverageReport> {
    coverage_internal(request, context, None)
}
pub(crate) fn coverage_internal(
    request: &CoverageRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&crate::restart::RestartPermit>,
) -> Result<CoverageReport> {
    context.cancellation.check()?;
    if request.source_manifests.is_empty() || request.source_manifests.len() > MAX_COVERAGE_GROUPS {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let target = ManifestInput::load(&request.target_manifest, permit)?;
    let requested_range = if let Some(index) = request.part {
        let part = index
            .checked_sub(1)
            .and_then(|i| target.manifest.parts.get(i as usize))
            .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))?;
        part_range(part)
    } else {
        ByteRange {
            start: 0,
            end_exclusive: target.manifest.original_size,
        }
    };
    let mut groups = Vec::with_capacity(request.source_manifests.len());
    let mut total_parts = 0;
    let mut paths = BTreeSet::new();
    for path in &request.source_manifests {
        let input = ManifestInput::load(path, permit)?;
        let key = input.source().path;
        #[cfg(windows)]
        let key = key.to_string_lossy().to_lowercase();
        if !paths.insert(key) {
            return Err(VolumeError::new(ErrorCode::InvalidArgument));
        }
        if input.manifest.original_size != target.manifest.original_size
            || input.manifest.original_sha256 != target.manifest.original_sha256
        {
            return Err(VolumeError::new(ErrorCode::SourceIdentityMismatch));
        }
        total_parts += input.manifest.parts.len();
        if total_parts > MAX_COVERAGE_PARTS {
            return Err(VolumeError::new(ErrorCode::LimitExceeded));
        }
        groups.push(input);
    }
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut candidates = Vec::new();
    let mut sources = Vec::new();
    let mut has_integrity_errors = false;
    let mut content_read = false;
    for (group_index, group) in groups.iter().enumerate() {
        let mut parts = Vec::with_capacity(group.manifest.parts.len());
        let mut whole = Sha256::default();
        let mut all_verified = !request.metadata_only;
        let mut completed = 0;
        for (part_index, part) in group.manifest.parts.iter().enumerate() {
            context.cancellation.check()?;
            let mut file = match group.directory.open_read(&part.filename) {
                Ok(file) => file,
                Err(error) if error.code == ErrorCode::InputNotFound => {
                    all_verified = false;
                    parts.push(CoveragePart {
                        part_index: part.index,
                        range: part_range(part),
                        status: PartAvailability::Missing,
                        actual_size: None,
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let snapshot = Snapshot::of(&file)?;
            let status = if snapshot.size != part.size {
                has_integrity_errors = true;
                PartAvailability::SizeMismatch
            } else if request.metadata_only {
                PartAvailability::Pending
            } else {
                context.report(
                    Phase::Verifying,
                    completed,
                    group.manifest.original_size,
                    Some(part.index),
                )?;
                content_read = true;
                let hash = hash_file_with_whole(
                    &mut file,
                    part.size,
                    context,
                    &mut buffer,
                    Some(&mut whole),
                )?;
                snapshot.check(&file)?;
                completed += part.size;
                if hash == part.sha256 {
                    PartAvailability::VerifiedPartHash
                } else {
                    has_integrity_errors = true;
                    PartAvailability::HashMismatch
                }
            };
            if status != PartAvailability::VerifiedPartHash {
                all_verified = false;
            }
            if matches!(
                status,
                PartAvailability::VerifiedPartHash | PartAvailability::Pending
            ) {
                candidates.push(Candidate {
                    group: group_index,
                    part: part_index,
                    range: part_range(part),
                    snapshot,
                    checked: status == PartAvailability::VerifiedPartHash,
                });
            }
            parts.push(CoveragePart {
                part_index: part.index,
                range: part_range(part),
                status,
                actual_size: Some(file.metadata().map_err(io_error)?.len()),
            });
        }
        let whole_stream_state = if all_verified {
            if digest_string(whole) == group.manifest.original_sha256 {
                StreamState::Verified
            } else {
                has_integrity_errors = true;
                StreamState::HashMismatch
            }
        } else if request.metadata_only {
            StreamState::Unchecked
        } else {
            StreamState::Incomplete
        };
        sources.push(CoverageSource {
            manifest_path: group.source().path,
            whole_stream_state,
            parts,
        });
    }
    // Sweep bounded endpoints instead of repeatedly scanning every part.
    let mut events: BTreeMap<u64, Vec<(usize, bool)>> = BTreeMap::new();
    events.entry(requested_range.start).or_default();
    events.entry(requested_range.end_exclusive).or_default();
    for (index, candidate) in candidates.iter().enumerate() {
        let start = candidate.range.start.max(requested_range.start);
        let end = candidate
            .range
            .end_exclusive
            .min(requested_range.end_exclusive);
        if start < end {
            events.entry(start).or_default().push((index, true));
            events.entry(end).or_default().push((index, false));
        }
    }
    let events: Vec<_> = events.into_iter().collect();
    let mut active = BTreeSet::new();
    let mut segments = Vec::new();
    let mut missing_ranges = Vec::new();
    let mut pending_ranges = Vec::new();
    let mut conflict_ranges = Vec::new();
    let mut covered_bytes = 0;
    for pair in events.windows(2) {
        context.cancellation.check()?;
        for &(index, enter) in &pair[0].1 {
            if enter {
                active.insert(index);
            } else {
                active.remove(&index);
            }
        }
        let range = ByteRange {
            start: pair[0].0,
            end_exclusive: pair[1].0,
        };
        let eligible = active
            .iter()
            .any(|&i| sources[candidates[i].group].whole_stream_state == StreamState::Verified);
        let checked: Vec<_> = active
            .iter()
            .filter(|&&i| candidates[i].checked)
            .copied()
            .collect();
        let mut conflict = false;
        if let Some((&first, rest)) = checked.split_first() {
            for &other in rest {
                if !overlap_equal(
                    &candidates[first],
                    &candidates[other],
                    range,
                    &groups,
                    context,
                    &mut buffer,
                )? {
                    conflict = true;
                    break;
                }
            }
        }
        let status = if conflict {
            has_integrity_errors = true;
            CoverageState::Conflict
        } else if eligible {
            CoverageState::Covered
        } else if active.is_empty() {
            CoverageState::Missing
        } else {
            CoverageState::Pending
        };
        match status {
            CoverageState::Covered => covered_bytes += range.checked_len()?,
            CoverageState::Missing => append_range(&mut missing_ranges, range),
            CoverageState::Pending => append_range(&mut pending_ranges, range),
            CoverageState::Conflict => append_range(&mut conflict_ranges, range),
        }
        let refs = active
            .iter()
            .map(|&i| {
                let c = &candidates[i];
                CoverageReference {
                    source_index: c.group,
                    part_index: groups[c.group].manifest.parts[c.part].index,
                    whole_stream_verified: sources[c.group].whole_stream_state
                        == StreamState::Verified,
                }
            })
            .collect();
        segments.push(CoverageSegment {
            range,
            status,
            sources: refs,
        });
    }
    // Recheck identities after hashing/comparing. The report authorizes no later writes.
    target.check()?;
    for group in &groups {
        group.check()?;
    }
    for candidate in &candidates {
        context.cancellation.check()?;
        let group = &groups[candidate.group];
        candidate.snapshot.check(
            &group
                .directory
                .open_read(&group.manifest.parts[candidate.part].filename)?,
        )?;
    }
    let complete_coverage = !has_integrity_errors
        && !request.metadata_only
        && missing_ranges.is_empty()
        && pending_ranges.is_empty()
        && conflict_ranges.is_empty()
        && (requested_range.checked_len()? > 0
            || sources
                .iter()
                .any(|s| s.whole_stream_state == StreamState::Verified));
    Ok(CoverageReport {
        original_size: target.manifest.original_size,
        original_sha256: target.manifest.original_sha256.clone(),
        requested_range,
        sources,
        segments,
        covered_bytes,
        missing_ranges,
        pending_ranges,
        conflict_ranges,
        has_integrity_errors,
        complete_coverage,
        provenance_authenticated: false,
        metadata_only: request.metadata_only,
        content_read,
        writes_performed: false,
    })
}
