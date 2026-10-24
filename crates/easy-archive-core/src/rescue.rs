//! Cross-layout repair from complete, actually verified byte streams.
use crate::{
    ByteRange, ErrorCode, GroupInspection, Manifest, PartDescriptor, Phase, Receipt, Result,
    TaskContext, VerificationReport, VolumeError,
    error::io_error,
    group::{Group, load_group_with_permit, validate_expected_hash},
    manifest_input::ManifestInput,
    recovery::{CommitEvidence, RecoverySource, RescueEvidence},
    restart::{RestartPermit, StartData, StoredOperation},
    secure_fs::{SafeDir, Snapshot},
    space::{SpaceEstimate, estimate_space},
    stream::{BUFFER_BYTES, digest_string, hash_file, hash_range_with_whole, read_chunk},
    transaction::{Artifact, Transaction, require_no_pending_except},
    validate_basename,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// No public constructor or deserialization: names and records cannot create this model.
pub struct VerifiedSourceSet {
    group: Group,
    verification: VerificationReport,
    // Windows read sharing pins every source against concurrent write/delete.
    _part_handles: Vec<cap_std::fs::File>,
}
impl VerifiedSourceSet {
    pub fn inspection(&self) -> &GroupInspection {
        &self.group.info
    }
    pub fn verification(&self) -> &VerificationReport {
        &self.verification
    }
}
pub fn verify_source_set(
    path: &Path,
    expected: Option<&str>,
    context: &mut TaskContext<'_>,
) -> Result<VerifiedSourceSet> {
    let group = load_group_with_permit(path, None)?;
    if group.manifest.is_none() {
        return Err(VolumeError::new(ErrorCode::ManifestRequired));
    }
    let handles = group
        .info
        .parts
        .iter()
        .map(|part| group.open_part(part))
        .collect::<Result<Vec<_>>>()?;
    let verification = group.verify(expected, context, &mut vec![0; BUFFER_BYTES])?;
    Ok(VerifiedSourceSet {
        group,
        verification,
        _part_handles: handles,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueRequest {
    pub target_manifest: PathBuf,
    pub source_manifests: Vec<PathBuf>,
    pub part: u32,
    pub output_dir: Option<PathBuf>,
    pub basename: Option<String>,
    pub expected_sha256: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSpan {
    pub source_index: usize,
    pub part_index: u32,
    pub source_offset: u64,
    pub target_offset: u64,
    pub length: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct RescuePartPlan {
    pub target_manifest: PathBuf,
    pub target: PartDescriptor,
    pub range: ByteRange,
    pub source_groups: Vec<GroupInspection>,
    pub spans: Vec<ReadSpan>,
    pub output_path: PathBuf,
    pub space: SpaceEstimate,
    pub has_external_digest: bool,
    pub content_read: bool,
    pub writes_performed: bool,
}
/// Intersect one complete layout with the exact requested target interval.
pub(crate) fn read_spans(
    manifest: &Manifest,
    range: ByteRange,
    source_index: usize,
) -> Result<Vec<ReadSpan>> {
    let mut spans = Vec::new();
    let mut cursor = range.start;
    for part in &manifest.parts {
        let start = part.offset.max(range.start);
        let end = (part.offset + part.size).min(range.end_exclusive);
        if start < end {
            if start != cursor {
                return Err(VolumeError::new(ErrorCode::MissingCoverage));
            }
            spans.push(ReadSpan {
                source_index,
                part_index: part.index,
                source_offset: start - part.offset,
                target_offset: start - range.start,
                length: end - start,
            });
            cursor = end;
        }
    }
    if cursor != range.end_exclusive {
        return Err(VolumeError::new(ErrorCode::MissingCoverage));
    }
    Ok(spans)
}
struct Prepared {
    target: ManifestInput,
    groups: Vec<Group>,
    plan: RescuePartPlan,
}
fn prepare(request: &RescueRequest, permit: Option<&RestartPermit>) -> Result<Prepared> {
    if request.source_manifests.is_empty()
        || request.source_manifests.len() > crate::coverage::MAX_COVERAGE_GROUPS
    {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let target = ManifestInput::load(&request.target_manifest, permit)?;
    if let Some(hash) = &request.expected_sha256 {
        validate_expected_hash(hash)?;
        if hash != &target.manifest.original_sha256 {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
    }
    let part = request
        .part
        .checked_sub(1)
        .and_then(|i| target.manifest.parts.get(i as usize))
        .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))?
        .clone();
    let range = ByteRange::new(part.offset, part.offset + part.size)?;
    let mut groups = Vec::new();
    let mut paths = std::collections::BTreeSet::new();
    let mut count = 0;
    for path in &request.source_manifests {
        let group = load_group_with_permit(path, permit).map_err(|error| {
            if error.code == ErrorCode::PartMissing {
                let mut failure = VolumeError::new(ErrorCode::MissingCoverage);
                if let Ok(report) = crate::coverage::coverage_internal(
                    &crate::CoverageRequest {
                        target_manifest: request.target_manifest.clone(),
                        source_manifests: request.source_manifests.clone(),
                        part: Some(request.part),
                        metadata_only: true,
                    },
                    &mut TaskContext::default(),
                    permit,
                ) {
                    failure.coverage_details =
                        Some(Box::new(crate::error::CoverageFailureDetails {
                            missing_ranges: report.missing_ranges,
                            pending_ranges: report.pending_ranges,
                        }));
                }
                failure
            } else {
                error
            }
        })?;
        let manifest = group
            .manifest
            .as_ref()
            .ok_or_else(|| VolumeError::new(ErrorCode::ManifestRequired))?;
        if manifest.original_size != target.manifest.original_size
            || manifest.original_sha256 != target.manifest.original_sha256
        {
            return Err(VolumeError::new(ErrorCode::SourceIdentityMismatch));
        }
        let key = group
            .start_inputs()
            .last()
            .expect("manifest guard")
            .path
            .clone();
        #[cfg(windows)]
        let key = key.to_string_lossy().to_lowercase();
        if !paths.insert(key) {
            return Err(VolumeError::new(ErrorCode::InvalidArgument));
        }
        count += group.info.parts.len();
        if count > crate::MAX_PARTS {
            return Err(VolumeError::new(ErrorCode::LimitExceeded));
        }
        groups.push(group);
    }
    let directory = SafeDir::open(
        request
            .output_dir
            .as_deref()
            .unwrap_or(&target.directory.path),
    )?;
    let name = request.basename.as_deref().unwrap_or(&part.filename);
    validate_basename(name)?;
    directory.require_absent(name)?;
    require_no_pending_except(&directory, permit)?;
    let plan = RescuePartPlan {
        target_manifest: target.source().path,
        target: part.clone(),
        range,
        source_groups: groups.iter().map(|g| g.info.clone()).collect(),
        spans: read_spans(groups[0].manifest.as_ref().expect("manifest"), range, 0)?,
        output_path: directory.path.join(name),
        space: estimate_space(&directory.path, part.size, 1)?,
        has_external_digest: request.expected_sha256.is_some(),
        content_read: false,
        writes_performed: false,
    };
    Ok(Prepared {
        target,
        groups,
        plan,
    })
}
pub fn plan_rescue(request: &RescueRequest) -> Result<RescuePartPlan> {
    plan_rescue_internal(request, None)
}
pub(crate) fn plan_rescue_internal(
    request: &RescueRequest,
    permit: Option<&RestartPermit>,
) -> Result<RescuePartPlan> {
    Ok(prepare(request, permit)?.plan)
}
pub(crate) fn start_inputs(
    request: &RescueRequest,
    permit: Option<&RestartPermit>,
) -> Result<Vec<RecoverySource>> {
    let prepared = prepare(request, permit)?;
    let mut inputs: Vec<_> = prepared
        .groups
        .iter()
        .flat_map(Group::start_inputs)
        .collect();
    inputs.push(prepared.target.source());
    Ok(inputs)
}
fn hash_target(
    group: &Group,
    range: ByteRange,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
) -> Result<String> {
    let mut hash = Sha256::new();
    for span in read_spans(group.manifest.as_ref().expect("manifest"), range, 0)? {
        let part = &group.info.parts[span.part_index as usize - 1];
        let mut file = group.open_part(part)?;
        hash_range_with_whole(
            &mut file,
            ByteRange::new(span.source_offset, span.source_offset + span.length)?,
            context,
            buffer,
            Some(&mut hash),
        )?;
        part.snapshot
            .as_ref()
            .expect("source snapshot")
            .check(&file)?;
    }
    Ok(digest_string(hash))
}
pub fn rescue_part(request: &RescueRequest, context: &mut TaskContext<'_>) -> Result<Receipt> {
    rescue_part_internal(request, context, None)
}
pub(crate) fn rescue_part_internal(
    request: &RescueRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&RestartPermit>,
) -> Result<Receipt> {
    context.cancellation.check()?;
    let prepared = prepare(request, permit)?;
    prepared.plan.space.require_sufficient()?;
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut verified = Vec::with_capacity(prepared.groups.len());
    for group in prepared.groups {
        let handles = group
            .info
            .parts
            .iter()
            .map(|part| group.open_part(part))
            .collect::<Result<Vec<_>>>()?;
        let verification =
            group.verify(request.expected_sha256.as_deref(), context, &mut buffer)?;
        if hash_target(&group, prepared.plan.range, context, &mut buffer)?
            != prepared.plan.target.sha256
        {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch).for_part(request.part));
        }
        verified.push(VerifiedSourceSet {
            group,
            verification,
            _part_handles: handles,
        });
    }
    let directory = SafeDir::open(prepared.plan.output_path.parent().expect("planned parent"))?;
    let name = prepared
        .plan
        .output_path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("planned name")
        .to_owned();
    let mut guards: Vec<_> = verified
        .iter()
        .flat_map(|s| s.group.start_inputs())
        .collect();
    guards.push(prepared.target.source());
    let normalized = RescueRequest {
        target_manifest: prepared.target.source().path,
        source_manifests: verified
            .iter()
            .map(|s| {
                s.group
                    .start_inputs()
                    .last()
                    .expect("manifest")
                    .path
                    .clone()
            })
            .collect(),
        output_dir: Some(directory.path.clone()),
        basename: Some(name.clone()),
        ..request.clone()
    };
    let start = StartData {
        operation: StoredOperation::Rescue(normalized),
        inputs: guards,
        verified_sha256: Some(prepared.plan.target.sha256.clone()),
    };
    let mut transaction = if let Some(permit) = permit {
        Transaction::restart_writing(
            &directory,
            std::slice::from_ref(&name),
            start,
            permit,
            context,
        )?
    } else {
        Transaction::begin(&directory, std::slice::from_ref(&name), start, context)?
    };
    let result = (|| -> Result<Receipt> {
        context.checkpoint(crate::IoPoint::WritingIntentDurable)?;
        let stage_name = transaction.payload_name("payload-rescued");
        let mut output = transaction.create_payload(&stage_name)?;
        let mut copied = Sha256::new();
        let mut completed = 0;
        context.report(
            Phase::Writing,
            0,
            prepared.plan.target.size,
            Some(request.part),
        )?;
        for span in &prepared.plan.spans {
            let group = &verified[span.source_index].group;
            let part = &group.info.parts[span.part_index as usize - 1];
            let mut input = group.open_part(part)?;
            input
                .seek(SeekFrom::Start(span.source_offset))
                .map_err(io_error)?;
            let mut remaining = span.length;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = read_chunk(&mut input, &mut buffer[..limit], context)?;
                if count == 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                context.write_payload(&mut output, &buffer[..count])?;
                copied.update(&buffer[..count]);
                remaining -= count as u64;
                completed += count as u64;
                context.report(
                    Phase::Writing,
                    completed,
                    prepared.plan.target.size,
                    Some(request.part),
                )?;
            }
            part.snapshot
                .as_ref()
                .expect("source snapshot")
                .check(&input)?;
        }
        context.sync_payload(&output)?;
        if completed != prepared.plan.target.size
            || digest_string(copied) != prepared.plan.target.sha256
        {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch));
        }
        context.report(
            Phase::Verifying,
            0,
            prepared.plan.target.size,
            Some(request.part),
        )?;
        if hash_file(&mut output, prepared.plan.target.size, context, &mut buffer)?
            != prepared.plan.target.sha256
        {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch));
        }
        let artifact = Artifact {
            stage_name,
            name,
            size: prepared.plan.target.size,
            sha256: prepared.plan.target.sha256.clone(),
            snapshot: Snapshot::of(&output)?,
        };
        drop(output);
        context.report(
            Phase::CheckingSources,
            0,
            prepared.plan.target.size,
            Some(request.part),
        )?;
        for set in &verified {
            if set
                .group
                .verify(request.expected_sha256.as_deref(), context, &mut buffer)?
                .sha256
                != set.verification.sha256
                || hash_target(&set.group, prepared.plan.range, context, &mut buffer)?
                    != prepared.plan.target.sha256
            {
                return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
            }
        }
        let sources = verified
            .iter()
            .flat_map(|set| {
                set.group
                    .start_inputs()
                    .into_iter()
                    .take(set.group.info.part_count as usize)
            })
            .collect();
        let evidence = CommitEvidence {
            sources,
            source_size: prepared.plan.target.size,
            sha256: prepared.plan.target.sha256.clone(),
            rebuild: None,
            rescue: Some(RescueEvidence {
                target_manifest: prepared.target.source(),
                source_manifests: verified
                    .iter()
                    .map(|s| s.group.start_inputs().last().expect("manifest").clone())
                    .collect(),
                part_index: request.part,
                range: prepared.plan.range,
            }),
        };
        transaction.commit(&[artifact], evidence, context, &mut buffer, &mut || {
            prepared.target.check()?;
            for set in &verified {
                set.group.check_metadata()?;
            }
            Ok(())
        })?;
        Ok(Receipt {
            source_size: prepared.plan.target.size,
            part_count: 1,
            sha256: prepared.plan.target.sha256,
            output_files: vec![prepared.plan.output_path],
            manifest_path: None,
            verification_level: if request.expected_sha256.is_some() {
                "verified_against_original"
            } else {
                "verified_against_available_sources"
            }
            .into(),
            verification_scope: "target_range_only",
            target_range: Some(prepared.plan.range),
            provenance_authenticated: false,
            transaction_id: transaction.id.clone(),
            staging_directory: transaction.stage.path.clone(),
            staging_retained: true,
            staged_payload_links_scheduled_for_removal: transaction
                .staged_links_scheduled_for_removal,
        })
    })();
    result.map_err(|error| transaction.fail(error))
}
