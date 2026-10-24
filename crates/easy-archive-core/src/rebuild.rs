//! Exact single-part reconstruction from an intact-size regular source.
//! Only the target byte range is read; no claim about unrelated bytes is made.
use crate::{
    ByteRange, ErrorCode, PartDescriptor, Phase, Receipt, Result, SourceFile, TaskContext,
    VolumeError,
    error::io_error,
    manifest_input::ManifestInput,
    recovery::{CommitEvidence, RebuildEvidence, RecoverySource},
    restart::{RestartPermit, StartData, StoredOperation},
    secure_fs::{SafeDir, Snapshot, input_parent},
    space::{SpaceEstimate, estimate_space},
    stream::{BUFFER_BYTES, digest_string, hash_file, hash_range, read_chunk},
    transaction::{Artifact, Transaction, require_no_pending_except},
    validate_basename,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Seek, SeekFrom},
    path::PathBuf,
};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildPartRequest {
    pub original: PathBuf,
    pub manifest: PathBuf,
    pub part: u32,
    pub output_dir: Option<PathBuf>,
    pub basename: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RebuildPartPlan {
    pub original: SourceFile,
    pub manifest_path: PathBuf,
    pub target: PartDescriptor,
    pub range: ByteRange,
    pub output_path: PathBuf,
    pub space: SpaceEstimate,
    pub verification_scope: &'static str,
}
pub fn plan_rebuild_part(request: &RebuildPartRequest) -> Result<RebuildPartPlan> {
    plan_rebuild_internal(request, None)
}
pub(crate) fn plan_rebuild_internal(
    request: &RebuildPartRequest,
    permit: Option<&RestartPermit>,
) -> Result<RebuildPartPlan> {
    let original = SourceFile::inspect(&request.original)?;
    let manifest = ManifestInput::load(&request.manifest, permit)?;
    if original.size != manifest.manifest.original_size {
        return Err(VolumeError::new(ErrorCode::SourceSizeMismatch));
    }
    let target = manifest
        .manifest
        .parts
        .get(
            request
                .part
                .checked_sub(1)
                .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))? as usize,
        )
        .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))?
        .clone();
    let range = ByteRange::new(
        target.offset,
        target
            .offset
            .checked_add(target.size)
            .ok_or_else(|| VolumeError::new(ErrorCode::ManifestInvalid))?,
    )?;
    let name = request.basename.as_deref().unwrap_or(&target.filename);
    validate_basename(name)?;
    let directory = SafeDir::open(
        request
            .output_dir
            .as_deref()
            .unwrap_or_else(|| original.validated_path.parent().expect("source parent")),
    )?;
    require_no_pending_except(&directory, permit)?;
    directory.require_absent(name)?;
    // A pending source directory must not be silently repurposed as an original.
    require_no_pending_except(
        &SafeDir::open(original.validated_path.parent().expect("source parent"))?,
        permit,
    )?;
    let space = estimate_space(&directory.path, target.size, 1)?;
    let output_path = directory.path.join(name);
    Ok(RebuildPartPlan {
        original,
        manifest_path: manifest.source().path,
        target,
        range,
        output_path,
        space,
        verification_scope: "target_range_only",
    })
}
pub(crate) fn start_inputs(
    request: &RebuildPartRequest,
    permit: Option<&RestartPermit>,
) -> Result<Vec<RecoverySource>> {
    let (directory, name) = input_parent(&request.original)?;
    let source = RecoverySource {
        path: directory.path.join(&name),
        snapshot: Snapshot::of(&directory.open_read(&name)?)?,
    };
    let manifest = ManifestInput::load(&request.manifest, permit)?;
    Ok(vec![source, manifest.source()])
}
pub fn rebuild_part(
    request: &RebuildPartRequest,
    context: &mut TaskContext<'_>,
) -> Result<Receipt> {
    rebuild_part_internal(request, context, None)
}
pub(crate) fn rebuild_part_internal(
    request: &RebuildPartRequest,
    context: &mut TaskContext<'_>,
    permit: Option<&RestartPermit>,
) -> Result<Receipt> {
    context.cancellation.check()?;
    let planned = plan_rebuild_internal(request, permit)?;
    planned.space.require_sufficient()?;
    let manifest = ManifestInput::load(&request.manifest, permit)?;
    let (source_dir, source_name) = input_parent(&request.original)?;
    let mut source = source_dir.open_read(&source_name)?;
    let snapshot = Snapshot::of(&source)?;
    if snapshot.size != planned.original.size
        || manifest.manifest.original_size != snapshot.size
        || manifest.manifest.parts.get(request.part as usize - 1) != Some(&planned.target)
    {
        return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
    }
    let mut buffer = vec![0; BUFFER_BYTES];
    context.report(Phase::Verifying, 0, planned.target.size, Some(request.part))?;
    if hash_range(&mut source, planned.range, context, &mut buffer)? != planned.target.sha256 {
        return Err(VolumeError::new(ErrorCode::PartHashMismatch).for_part(request.part));
    }
    let target = SafeDir::open(planned.output_path.parent().expect("planned parent"))?;
    let output_name = planned
        .output_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("validated name")
        .to_owned();
    let original_guard = RecoverySource {
        path: source_dir.path.join(&source_name),
        snapshot: snapshot.clone(),
    };
    let manifest_guard = manifest.source();
    let start = StartData {
        operation: StoredOperation::RebuildPart(RebuildPartRequest {
            original: original_guard.path.clone(),
            manifest: manifest_guard.path.clone(),
            output_dir: Some(target.path.clone()),
            basename: Some(output_name.clone()),
            ..request.clone()
        }),
        inputs: vec![original_guard.clone(), manifest_guard.clone()],
        verified_sha256: Some(planned.target.sha256.clone()),
    };
    let mut transaction = if let Some(permit) = permit {
        Transaction::restart_writing(
            &target,
            std::slice::from_ref(&output_name),
            start,
            permit,
            context,
        )?
    } else {
        Transaction::begin(&target, std::slice::from_ref(&output_name), start, context)?
    };
    let result = (|| -> Result<Receipt> {
        context.checkpoint(crate::IoPoint::WritingIntentDurable)?;
        let stage_name = transaction.payload_name("payload-rebuilt");
        let mut output = transaction.create_payload(&stage_name)?;
        source
            .seek(SeekFrom::Start(planned.range.start))
            .map_err(io_error)?;
        let mut remaining = planned.target.size;
        let mut hash = Sha256::new();
        context.report(Phase::Writing, 0, planned.target.size, Some(request.part))?;
        while remaining > 0 {
            let limit = remaining.min(buffer.len() as u64) as usize;
            let count = read_chunk(&mut source, &mut buffer[..limit], context)?;
            if count == 0 {
                return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
            }
            context.write_payload(&mut output, &buffer[..count])?;
            hash.update(&buffer[..count]);
            remaining -= count as u64;
            context.report(
                Phase::Writing,
                planned.target.size - remaining,
                planned.target.size,
                Some(request.part),
            )?;
        }
        context.sync_payload(&output)?;
        if digest_string(hash) != planned.target.sha256 {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch));
        }
        context.report(Phase::Verifying, 0, planned.target.size, Some(request.part))?;
        if hash_file(&mut output, planned.target.size, context, &mut buffer)?
            != planned.target.sha256
        {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch));
        }
        let artifact = Artifact {
            stage_name,
            name: output_name,
            size: planned.target.size,
            sha256: planned.target.sha256.clone(),
            snapshot: Snapshot::of(&output)?,
        };
        drop(output);
        context.report(
            Phase::CheckingSources,
            0,
            planned.target.size,
            Some(request.part),
        )?;
        if hash_range(&mut source, planned.range, context, &mut buffer)? != planned.target.sha256 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
        let evidence = CommitEvidence {
            rescue: None,
            sources: vec![original_guard],
            source_size: planned.target.size,
            sha256: planned.target.sha256.clone(),
            rebuild: Some(RebuildEvidence {
                manifest: manifest_guard,
                part_index: request.part,
                range: planned.range,
            }),
        };
        transaction.commit(&[artifact], evidence, context, &mut buffer, &mut || {
            snapshot.check(&source)?;
            source_dir.check_location()?;
            snapshot.check(&source_dir.open_read(&source_name)?)?;
            manifest.check()
        })?;
        Ok(Receipt {
            source_size: planned.target.size,
            part_count: 1,
            sha256: planned.target.sha256,
            output_files: vec![planned.output_path],
            manifest_path: None,
            verification_level: "verified_against_available_sources".into(),
            verification_scope: "target_range_only",
            target_range: Some(planned.range),
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
