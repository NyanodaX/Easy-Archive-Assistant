//! Re-layout a verified logical byte stream directly into new volumes.
//! No complete archive is materialized between input and output volumes.
use crate::{
    ErrorCode, Manifest, PartDescriptor, Phase, Receipt, Result, SplitPlan, SplitPolicy,
    TaskContext, VolumeError,
    group::{Group, GroupInspection, load_selected_with_permit, validate_expected_hash},
    paths::plan_output_paths,
    recovery::{CommitEvidence, RecoverySource},
    restart::{RestartPermit, StartData, StoredOperation},
    secure_fs::{SafeDir, Snapshot, input_parent},
    space::{SpaceEstimate, estimate_space},
    stream::{BUFFER_BYTES, digest_string, hash_file, hash_file_with_whole, read_chunk},
    transaction::{Artifact, Transaction, require_no_pending_except},
};
use cap_std::fs::File;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReshardRequest {
    pub input: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub part_paths: Vec<PathBuf>,
    pub output_dir: Option<PathBuf>,
    /// Required even in a separate directory, to make the new group explicit.
    pub basename: String,
    pub policy: SplitPolicy,
    pub alignment: Option<u64>,
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReshardFilePlan {
    pub group: GroupInspection,
    pub plan: SplitPlan,
    pub output_files: Vec<PathBuf>,
    pub manifest_path: PathBuf,
    pub space: SpaceEstimate,
    pub creates_complete_intermediate: bool,
}

/// Metadata-only planning. Execution verifies every source byte before writing.
pub fn plan_reshard_file(request: &ReshardRequest) -> Result<ReshardFilePlan> {
    plan_reshard_internal(request, None)
}

pub(crate) fn plan_reshard_internal(
    request: &ReshardRequest,
    permit: Option<&RestartPermit>,
) -> Result<ReshardFilePlan> {
    if let Some(hash) = &request.expected_sha256 {
        validate_expected_hash(hash)?;
    }
    if !request.part_paths.is_empty() && request.output_dir.is_none() {
        return Err(VolumeError::new(ErrorCode::OutputDirectoryRequired));
    }
    let group = load_selected_with_permit(&request.input, &request.part_paths, permit)?;
    if group.manifest.is_none() {
        return Err(VolumeError::new(ErrorCode::ManifestRequired));
    }
    let plan = crate::plan_split(group.info.source_size, request.policy, request.alignment)?;
    let directory = request
        .output_dir
        .as_deref()
        .unwrap_or(&group.directory.path);
    let (output_files, manifest_path) =
        plan_output_paths(directory, &request.basename, plan.part_count)?;
    let bound = 2048
        + group.info.original_filename.len()
        + output_files
            .iter()
            .map(|p| p.file_name().expect("planned filename").len() + 256)
            .sum::<usize>();
    if bound > crate::MAX_MANIFEST_BYTES {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    require_no_pending_except(&SafeDir::open(directory)?, permit)?;
    let space = estimate_space(directory, group.info.source_size, plan.part_count + 1)?;
    Ok(ReshardFilePlan {
        group: group.info,
        plan,
        output_files,
        manifest_path,
        space,
        creates_complete_intermediate: false,
    })
}

/// A bounded cursor across source parts. One source handle is open at a time;
/// target cuts may fall anywhere, including inside or exactly between parts.
struct Cursor<'a> {
    group: &'a Group,
    index: usize,
    file: Option<File>,
    remaining: u64,
    part_hash: Sha256,
    whole_hash: Sha256,
}
impl<'a> Cursor<'a> {
    fn new(group: &'a Group) -> Self {
        Self {
            group,
            index: 0,
            file: None,
            remaining: 0,
            part_hash: Sha256::new(),
            whole_hash: Sha256::new(),
        }
    }

    fn advance(&mut self, context: &mut TaskContext<'_>) -> Result<bool> {
        while self.remaining == 0 {
            if let Some(mut file) = self.file.take() {
                let part = &self.group.info.parts[self.index];
                let mut extra = [0];
                if read_chunk(&mut file, &mut extra, context)? != 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                part.snapshot
                    .as_ref()
                    .expect("loaded source")
                    .check(&file)?;
                let hash = digest_string(std::mem::take(&mut self.part_hash));
                if hash
                    != self
                        .group
                        .manifest
                        .as_ref()
                        .expect("required manifest")
                        .parts[self.index]
                        .sha256
                {
                    return Err(VolumeError::new(ErrorCode::PartHashMismatch).for_part(part.index));
                }
                self.index += 1;
            }
            let Some(part) = self.group.info.parts.get(self.index) else {
                return Ok(false);
            };
            self.file = Some(self.group.open_part(part)?);
            self.remaining = part.size;
        }
        Ok(true)
    }

    fn read(&mut self, buffer: &mut [u8], context: &mut TaskContext<'_>) -> Result<usize> {
        if !self.advance(context)? {
            return Ok(0);
        }
        let limit = self.remaining.min(buffer.len() as u64) as usize;
        let count = read_chunk(
            self.file.as_mut().expect("opened source"),
            &mut buffer[..limit],
            context,
        )?;
        if count == 0 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        self.part_hash.update(&buffer[..count]);
        self.whole_hash.update(&buffer[..count]);
        self.remaining -= count as u64;
        Ok(count)
    }

    fn finish(mut self, context: &mut TaskContext<'_>) -> Result<String> {
        if self.advance(context)? {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        Ok(digest_string(self.whole_hash))
    }
}

pub fn reshard_file(request: &ReshardRequest, context: &mut TaskContext<'_>) -> Result<Receipt> {
    reshard_file_internal(request, context, None)
}

pub(crate) fn reshard_file_internal(
    request: &ReshardRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&RestartPermit>,
) -> Result<Receipt> {
    context.cancellation.check()?;
    let planned = plan_reshard_internal(request, permit)?;
    planned.space.require_sufficient()?;
    let group = load_selected_with_permit(&request.input, &request.part_paths, permit)?;
    let mut buffer = vec![0; BUFFER_BYTES];
    let verified = group.verify(request.expected_sha256.as_deref(), context, &mut buffer)?;
    let target = SafeDir::open(planned.manifest_path.parent().expect("planned parent"))?;
    let names: Vec<String> = planned
        .output_files
        .iter()
        .chain(std::iter::once(&planned.manifest_path))
        .map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .expect("validated filename")
                .to_owned()
        })
        .collect();
    let (selected, name) = input_parent(&request.input)?;
    let start = StartData {
        operation: StoredOperation::Reshard(ReshardRequest {
            input: selected.path.join(name),
            part_paths: group.selected_paths(),
            output_dir: Some(target.path.clone()),
            ..request.clone()
        }),
        inputs: group.start_inputs(),
        verified_sha256: Some(verified.sha256.clone()),
    };
    let mut transaction = if let Some(permit) = permit {
        Transaction::restart_writing(&target, &names, start, permit, context)?
    } else {
        Transaction::begin(&target, &names, start, context)?
    };
    let result = (|| -> Result<Receipt> {
        context.checkpoint(crate::IoPoint::WritingIntentDurable)?;
        let mut cursor = Cursor::new(&group);
        let mut assembled = Sha256::new();
        let mut artifacts = Vec::with_capacity(names.len());
        let mut parts = Vec::with_capacity(planned.plan.parts.len());
        let mut completed = 0;
        for part in &planned.plan.parts {
            context.report(
                Phase::Writing,
                completed,
                group.info.source_size,
                Some(part.index),
            )?;
            let stage_name = transaction.payload_name(&format!("payload-{:05}", part.index));
            let mut output = transaction.create_payload(&stage_name)?;
            let mut part_hash = Sha256::new();
            let mut remaining = part.size;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let count = cursor.read(&mut buffer[..limit], context)?;
                if count == 0 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
                context.write_payload(&mut output, &buffer[..count])?;
                part_hash.update(&buffer[..count]);
                remaining -= count as u64;
                completed += count as u64;
                context.report(
                    Phase::Writing,
                    completed,
                    group.info.source_size,
                    Some(part.index),
                )?;
            }
            context.sync_payload(&output)?;
            context.report(
                Phase::Verifying,
                part.range.start,
                group.info.source_size,
                Some(part.index),
            )?;
            let actual = hash_file_with_whole(
                &mut output,
                part.size,
                context,
                &mut buffer,
                Some(&mut assembled),
            )?;
            if actual != digest_string(part_hash) {
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
        if cursor.finish(context)? != verified.sha256 || digest_string(assembled) != verified.sha256
        {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        context.report(Phase::CheckingSources, 0, group.info.source_size, None)?;
        if group
            .verify(request.expected_sha256.as_deref(), context, &mut buffer)?
            .sha256
            != verified.sha256
        {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        let manifest = Manifest {
            schema_version: crate::SCHEMA_VERSION,
            tool: crate::PRODUCT_NAME_EN.to_owned(),
            original_filename: group.info.original_filename.clone(),
            original_size: group.info.source_size,
            original_sha256: verified.sha256.clone(),
            split_mode: if matches!(request.policy, SplitPolicy::Hard { .. }) {
                "hard"
            } else {
                "balanced"
            }
            .to_owned(),
            part_count: planned.plan.part_count,
            parts,
            created_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .map_err(|_| VolumeError::new(ErrorCode::IoError))?,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        manifest.validate()?;
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| VolumeError::new(ErrorCode::ManifestInvalid))?;
        if bytes.len() > crate::MAX_MANIFEST_BYTES {
            return Err(VolumeError::new(ErrorCode::LimitExceeded));
        }
        let stage_name = transaction.payload_name("payload-manifest");
        let mut output = transaction.create_payload(&stage_name)?;
        context.write_payload(&mut output, &bytes)?;
        context.sync_payload(&output)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if hash_file(&mut output, bytes.len() as u64, context, &mut buffer)? != sha256 {
            return Err(VolumeError::new(ErrorCode::ManifestInvalid));
        }
        artifacts.push(Artifact {
            stage_name,
            name: names.last().expect("manifest").clone(),
            size: bytes.len() as u64,
            sha256,
            snapshot: Snapshot::of(&output)?,
        });
        drop(output);
        let evidence = CommitEvidence {
            rescue: None,
            rebuild: None,
            sources: group
                .info
                .parts
                .iter()
                .map(|part| RecoverySource {
                    path: part.path.clone(),
                    snapshot: part.snapshot.clone().expect("loaded source"),
                })
                .collect(),
            source_size: group.info.source_size,
            sha256: verified.sha256.clone(),
        };
        transaction.commit(&artifacts, evidence, context, &mut buffer, &mut || {
            group.check_metadata()
        })?;
        Ok(Receipt {
            source_size: group.info.source_size,
            part_count: planned.plan.part_count,
            sha256: verified.sha256,
            output_files: planned.output_files,
            manifest_path: Some(planned.manifest_path),
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
