use crate::{
    ErrorCode, MAX_MANIFEST_BYTES, Manifest, PartDescriptor, Phase, Result, SCHEMA_VERSION,
    SourceFile, SplitPlan, SplitPolicy, TaskContext, VolumeError, plan_split, validate_basename,
};
use crate::{
    group::{GroupInspection, load_selected_with_permit, validate_expected_hash},
    paths::plan_output_paths,
    recovery::{CommitEvidence, RecoverySource},
    restart::{RestartPermit, StartData, StoredOperation},
    secure_fs::{SafeDir, Snapshot, input_parent},
    space::{SpaceEstimate, estimate_space},
    stream::{BUFFER_BYTES, digest_string, hash_file, hash_file_with_whole, read_chunk},
    transaction::{Artifact, Transaction, require_no_pending_except},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SplitRequest {
    pub input: PathBuf,
    pub output_dir: Option<PathBuf>,
    pub basename: Option<String>,
    pub policy: SplitPolicy,
    pub alignment: Option<u64>,
    pub allow_any_file: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitFilePlan {
    pub source: SourceFile,
    pub plan: SplitPlan,
    pub output_files: Vec<PathBuf>,
    pub manifest_path: PathBuf,
    pub space: SpaceEstimate,
}

pub fn plan_split_file(request: &SplitRequest) -> Result<SplitFilePlan> {
    plan_split_internal(request, None)
}

pub(crate) fn plan_split_internal(
    request: &SplitRequest,
    permit: Option<&RestartPermit>,
) -> Result<SplitFilePlan> {
    let source = SourceFile::inspect(&request.input)?;
    source.require_splittable(request.allow_any_file)?;
    let plan = plan_split(source.size, request.policy, request.alignment)?;
    let directory = request
        .output_dir
        .as_deref()
        .unwrap_or_else(|| source.validated_path.parent().expect("validated file"));
    let basename = request.basename.as_deref().unwrap_or_else(|| {
        source
            .validated_path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("validated filename")
    });
    let (output_files, manifest_path) = plan_output_paths(directory, basename, plan.part_count)?;
    // Conservative bound for pretty JSON, hashes, numeric fields and timestamp.
    // Reject an oversized manifest before writing the potentially large payload.
    let manifest_bound = 2048
        + source.validated_path.file_name().expect("filename").len()
        + output_files
            .iter()
            .map(|p| p.file_name().expect("filename").len() + 256)
            .sum::<usize>();
    if manifest_bound > MAX_MANIFEST_BYTES {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    require_no_pending_except(&SafeDir::open(directory)?, permit)?;
    let space = estimate_space(directory, source.size, plan.part_count + 1)?;
    Ok(SplitFilePlan {
        source,
        plan,
        output_files,
        manifest_path,
        space,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    pub input: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub part_paths: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub basename: Option<String>,
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeFilePlan {
    pub group: GroupInspection,
    pub output_path: PathBuf,
    pub space: SpaceEstimate,
}

pub fn plan_merge_file(request: &MergeRequest) -> Result<MergeFilePlan> {
    plan_merge_internal(request, None)
}

pub(crate) fn plan_merge_internal(
    request: &MergeRequest,
    permit: Option<&RestartPermit>,
) -> Result<MergeFilePlan> {
    if let Some(hash) = &request.expected_sha256 {
        validate_expected_hash(hash)?;
    }
    if !request.part_paths.is_empty() && request.output_dir.is_none() {
        return Err(VolumeError::new(ErrorCode::OutputDirectoryRequired));
    }
    let group = load_selected_with_permit(&request.input, &request.part_paths, permit)?;
    let name = request
        .basename
        .as_deref()
        .unwrap_or(&group.info.original_filename);
    validate_basename(name)?;
    let output = SafeDir::open(
        request
            .output_dir
            .as_deref()
            .unwrap_or(&group.directory.path),
    )?;
    output.require_absent(name)?;
    require_no_pending_except(&output, permit)?;
    let output_path = output.path.join(name);
    let space = estimate_space(&output.path, group.info.source_size, 1)?;
    Ok(MergeFilePlan {
        group: group.info,
        output_path,
        space,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub source_size: u64,
    pub part_count: u32,
    pub sha256: String,
    pub output_files: Vec<PathBuf>,
    pub manifest_path: Option<PathBuf>,
    pub verification_level: String,
    pub verification_scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_range: Option<crate::ByteRange>,
    pub provenance_authenticated: bool,
    pub transaction_id: String,
    pub staging_directory: PathBuf,
    pub staging_retained: bool,
    /// Verified staging links marked for deletion and closed by this invocation.
    /// Other readers or OS behavior may delay actual space reclamation.
    pub staged_payload_links_scheduled_for_removal: u32,
}

fn filename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .expect("validated filename")
        .to_owned()
}

pub fn split_file(request: &SplitRequest, context: &mut TaskContext<'_>) -> Result<Receipt> {
    split_file_internal(request, context, None)
}

pub(crate) fn split_file_internal(
    request: &SplitRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&RestartPermit>,
) -> Result<Receipt> {
    context.cancellation.check()?;
    let prepared = plan_split_internal(request, permit)?;
    prepared.space.require_sufficient()?;
    let (input_dir, input_name) = input_parent(&request.input)?;
    let mut input = input_dir.open_read(&input_name)?;
    let input_snapshot = Snapshot::of(&input)?;
    if input_snapshot.size != prepared.source.size {
        return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
    }
    let output_dir = SafeDir::open(prepared.manifest_path.parent().expect("planned parent"))?;
    let mut names: Vec<String> = prepared.output_files.iter().map(|p| filename(p)).collect();
    names.push(filename(&prepared.manifest_path));
    let normalized = SplitRequest {
        input: prepared.source.validated_path.clone(),
        output_dir: Some(output_dir.path.clone()),
        basename: Some(
            request
                .basename
                .clone()
                .unwrap_or_else(|| input_name.clone()),
        ),
        ..request.clone()
    };
    let start = StartData {
        operation: StoredOperation::Split(normalized),
        inputs: vec![RecoverySource {
            path: prepared.source.validated_path.clone(),
            snapshot: input_snapshot.clone(),
        }],
        verified_sha256: None,
    };
    let mut transaction = if let Some(permit) = permit {
        Transaction::restart_writing(&output_dir, &names, start, permit, context)?
    } else {
        Transaction::begin(&output_dir, &names, start, context)?
    };
    let result = (|| -> Result<Receipt> {
        context.checkpoint(crate::IoPoint::WritingIntentDurable)?;
        let mut buffer = vec![0; BUFFER_BYTES];
        let mut original = Sha256::new();
        let mut assembled = Sha256::new();
        let mut completed = 0_u64;
        let mut parts = Vec::with_capacity(prepared.plan.parts.len());
        let mut artifacts = Vec::with_capacity(names.len());
        for part in &prepared.plan.parts {
            context.report(
                Phase::Writing,
                completed,
                prepared.source.size,
                Some(part.index),
            )?;
            let stage_name = transaction.payload_name(&format!("payload-{:05}", part.index));
            let mut output = transaction.create_payload(&stage_name)?;
            let mut part_hasher = Sha256::new();
            let mut remaining = part.size;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = read_chunk(&mut input, &mut buffer[..limit], context)?;
                if count == 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                context.cancellation.check()?;
                context.write_payload(&mut output, &buffer[..count])?;
                original.update(&buffer[..count]);
                part_hasher.update(&buffer[..count]);
                remaining -= count as u64;
                completed += count as u64;
                context.report(
                    Phase::Writing,
                    completed,
                    prepared.source.size,
                    Some(part.index),
                )?;
            }
            context.sync_payload(&output)?;
            let expected = digest_string(part_hasher);
            context.report(
                Phase::Verifying,
                part.range.start,
                prepared.source.size,
                Some(part.index),
            )?;
            let actual = hash_file_with_whole(
                &mut output,
                part.size,
                context,
                &mut buffer,
                Some(&mut assembled),
            )?;
            if actual != expected {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch));
            }
            let name = names[part.index as usize - 1].clone();
            parts.push(PartDescriptor {
                index: part.index,
                filename: name.clone(),
                offset: part.range.start,
                size: part.size,
                sha256: actual.clone(),
            });
            artifacts.push(Artifact {
                stage_name,
                name,
                size: part.size,
                sha256: actual,
                snapshot: Snapshot::of(&output)?,
            });
        }
        let hash = digest_string(original);
        if digest_string(assembled) != hash {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        context.report(Phase::CheckingSources, 0, prepared.source.size, None)?;
        input_snapshot.check(&input)?;
        if hash_file(&mut input, input_snapshot.size, context, &mut buffer)? != hash {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        input_snapshot.check(&input)?;
        input_dir.check_location()?;
        input_snapshot.check(&input_dir.open_read(&input_name)?)?;
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            tool: crate::PRODUCT_NAME_EN.to_owned(),
            original_filename: input_name.clone(),
            original_size: prepared.source.size,
            original_sha256: hash.clone(),
            split_mode: if matches!(request.policy, SplitPolicy::Hard { .. }) {
                "hard"
            } else {
                "balanced"
            }
            .to_owned(),
            part_count: prepared.plan.part_count,
            parts,
            created_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .map_err(|_| VolumeError::new(ErrorCode::IoError))?,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        manifest.validate()?;
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| VolumeError::new(ErrorCode::ManifestInvalid))?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(VolumeError::new(ErrorCode::ManifestInvalid));
        }
        let stage_name = transaction.payload_name("payload-manifest");
        let mut output = transaction.create_payload(&stage_name)?;
        context.write_payload(&mut output, &bytes)?;
        context.sync_payload(&output)?;
        let manifest_hash = format!("{:x}", Sha256::digest(&bytes));
        if hash_file(&mut output, bytes.len() as u64, context, &mut buffer)? != manifest_hash {
            return Err(VolumeError::new(ErrorCode::ManifestInvalid));
        }
        artifacts.push(Artifact {
            stage_name,
            name: names.last().expect("manifest name").clone(),
            size: bytes.len() as u64,
            sha256: manifest_hash,
            snapshot: Snapshot::of(&output)?,
        });
        drop(output);
        // Manifest is deliberately the last artifact, never a group-atomic claim.
        let evidence = CommitEvidence {
            rescue: None,
            rebuild: None,
            sources: vec![RecoverySource {
                path: prepared.source.validated_path.clone(),
                snapshot: input_snapshot.clone(),
            }],
            source_size: prepared.source.size,
            sha256: hash.clone(),
        };
        transaction.commit(&artifacts, evidence, context, &mut buffer, &mut || {
            input_snapshot.check(&input)?;
            input_dir.check_location()?;
            input_snapshot.check(&input_dir.open_read(&input_name)?)
        })?;
        Ok(Receipt {
            source_size: prepared.source.size,
            part_count: prepared.plan.part_count,
            sha256: hash,
            output_files: prepared.output_files,
            manifest_path: Some(prepared.manifest_path),
            verification_level: "verified_against_original".to_owned(),
            verification_scope: "full_stream",
            target_range: None,
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

pub fn merge_file(request: &MergeRequest, context: &mut TaskContext<'_>) -> Result<Receipt> {
    merge_file_internal(request, context, None)
}

pub(crate) fn merge_file_internal(
    request: &MergeRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&RestartPermit>,
) -> Result<Receipt> {
    context.cancellation.check()?;
    let planned = plan_merge_internal(request, permit)?;
    planned.space.require_sufficient()?;
    let group = load_selected_with_permit(&request.input, &request.part_paths, permit)?;
    let mut buffer = vec![0; BUFFER_BYTES];
    let verified = group.verify(request.expected_sha256.as_deref(), context, &mut buffer)?;
    let output_dir = SafeDir::open(planned.output_path.parent().expect("planned parent"))?;
    let output_name = filename(&planned.output_path);
    let (selected_dir, selected_name) = input_parent(&request.input)?;
    let normalized = MergeRequest {
        input: selected_dir.path.join(selected_name),
        part_paths: group.selected_paths(),
        output_dir: Some(output_dir.path.clone()),
        basename: Some(output_name.clone()),
        expected_sha256: request.expected_sha256.clone(),
    };
    let start = StartData {
        operation: StoredOperation::Merge(normalized),
        inputs: group.start_inputs(),
        verified_sha256: Some(verified.sha256.clone()),
    };
    let mut transaction = if let Some(permit) = permit {
        Transaction::restart_writing(
            &output_dir,
            std::slice::from_ref(&output_name),
            start,
            permit,
            context,
        )?
    } else {
        Transaction::begin(
            &output_dir,
            std::slice::from_ref(&output_name),
            start,
            context,
        )?
    };
    let result = (|| -> Result<Receipt> {
        context.checkpoint(crate::IoPoint::WritingIntentDurable)?;
        let stage_name = transaction.payload_name("payload-merged");
        let mut output = transaction.create_payload(&stage_name)?;
        let mut copied = Sha256::new();
        let mut completed = 0_u64;
        for part in &group.info.parts {
            let mut input = group.open_part(part)?;
            let mut part_hash = Sha256::new();
            let mut remaining = part.size;
            context.report(
                Phase::Writing,
                completed,
                group.info.source_size,
                Some(part.index),
            )?;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = read_chunk(&mut input, &mut buffer[..limit], context)?;
                if count == 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                context.cancellation.check()?;
                context.write_payload(&mut output, &buffer[..count])?;
                copied.update(&buffer[..count]);
                part_hash.update(&buffer[..count]);
                completed += count as u64;
                remaining -= count as u64;
                context.report(
                    Phase::Writing,
                    completed,
                    group.info.source_size,
                    Some(part.index),
                )?;
            }
            if group.manifest.as_ref().is_some_and(|m| {
                digest_string(part_hash) != m.parts[part.index as usize - 1].sha256
            }) {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch));
            }
            part.snapshot
                .as_ref()
                .expect("loaded group")
                .check(&input)?;
        }
        context.sync_payload(&output)?;
        if digest_string(copied) != verified.sha256 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        context.report(Phase::Verifying, 0, group.info.source_size, None)?;
        if hash_file(&mut output, group.info.source_size, context, &mut buffer)? != verified.sha256
        {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        let artifact = Artifact {
            stage_name,
            name: output_name,
            size: group.info.source_size,
            sha256: verified.sha256.clone(),
            snapshot: Snapshot::of(&output)?,
        };
        drop(output);
        context.report(Phase::CheckingSources, 0, group.info.source_size, None)?;
        if group
            .verify(request.expected_sha256.as_deref(), context, &mut buffer)?
            .sha256
            != verified.sha256
        {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        let evidence = CommitEvidence {
            rescue: None,
            rebuild: None,
            sources: group
                .info
                .parts
                .iter()
                .map(|part| RecoverySource {
                    path: part.path.clone(),
                    snapshot: part.snapshot.clone().expect("loaded group"),
                })
                .collect(),
            source_size: group.info.source_size,
            sha256: verified.sha256.clone(),
        };
        transaction.commit(&[artifact], evidence, context, &mut buffer, &mut || {
            group.check_metadata()
        })?;
        Ok(Receipt {
            source_size: group.info.source_size,
            part_count: group.info.part_count,
            sha256: verified.sha256,
            output_files: vec![planned.output_path],
            manifest_path: None,
            verification_level: verified.verification_level,
            verification_scope: "full_stream",
            target_range: None,
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
