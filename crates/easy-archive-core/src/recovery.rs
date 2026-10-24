//! Explicit recovery of complete, verified staging. No cleanup or deletion.
//! Logs and recorded digests are untrusted data, never provenance credentials.
use crate::{
    ByteRange, ErrorCode, MAX_PARTS, Receipt, Result, TaskContext, VolumeError,
    error::io_error,
    parse_manifest,
    secure_fs::{SafeDir, Snapshot, input_parent},
    stream::{BUFFER_BYTES, digest_string, hash_file, hash_file_with_whole, hash_range},
    transaction::{Artifact, Transaction, task_id},
    validate_basename,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub(crate) const MAX_RECOVERY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoverySource {
    pub path: PathBuf,
    pub snapshot: Snapshot,
}

pub(crate) struct CommitEvidence {
    pub sources: Vec<RecoverySource>,
    pub source_size: u64,
    pub sha256: String,
    pub rebuild: Option<RebuildEvidence>,
    pub rescue: Option<RescueEvidence>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RescueEvidence {
    pub target_manifest: RecoverySource,
    pub source_manifests: Vec<RecoverySource>,
    pub part_index: u32,
    pub range: ByteRange,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RebuildEvidence {
    pub manifest: RecoverySource,
    pub part_index: u32,
    pub range: ByteRange,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryPlan {
    schema_version: u32,
    pub task_id: String,
    pub operation: String,
    target_identity: (u64, u64),
    stage_identity: (u64, u64),
    source_size: u64,
    sha256: String,
    sources: Vec<RecoverySource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rebuild: Option<RebuildEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rescue: Option<RescueEvidence>,
    pub artifacts: Vec<Artifact>,
}

fn invalid() -> VolumeError {
    VolumeError::new(ErrorCode::TransactionNeedsRecovery)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RecoveryPlan {
    pub(crate) fn new(
        target: &SafeDir,
        stage: &SafeDir,
        id: &str,
        operation: &str,
        artifacts: &[Artifact],
        evidence: CommitEvidence,
    ) -> Result<Self> {
        Ok(Self {
            schema_version: if evidence.rescue.is_some() {
                4
            } else if evidence.rebuild.is_some() {
                3
            } else {
                2
            },
            task_id: id.to_owned(),
            operation: operation.to_owned(),
            target_identity: target.identity()?,
            stage_identity: stage.identity()?,
            source_size: evidence.source_size,
            sha256: evidence.sha256,
            sources: evidence.sources,
            rebuild: evidence.rebuild,
            rescue: evidence.rescue,
            artifacts: artifacts.to_vec(),
        })
    }

    fn validate(&self, target: &SafeDir, stage: &SafeDir, id: &str) -> Result<()> {
        let range_operation = matches!(self.operation.as_str(), "rebuild-part" | "rescue");
        if !((self.schema_version == 2
            && self.rebuild.is_none()
            && self.rescue.is_none()
            && !range_operation)
            || (self.schema_version == 3
                && self.rebuild.is_some()
                && self.rescue.is_none()
                && self.operation == "rebuild-part")
            || (self.schema_version == 4
                && self.rescue.is_some()
                && self.rebuild.is_none()
                && self.operation == "rescue"))
            || self.task_id != id
            || !valid_hash(&self.sha256)
            || self.target_identity != target.identity()?
            || self.stage_identity != stage.identity()?
            || self.sources.is_empty()
            || self.sources.len() > MAX_PARTS
            || self.artifacts.is_empty()
            || self.artifacts.len() > MAX_PARTS + 1
            || !matches!(
                self.operation.as_str(),
                "split" | "merge" | "reshard" | "rebuild-part" | "rescue"
            )
        {
            return Err(invalid());
        }
        let total = self.sources.iter().try_fold(0_u64, |sum, source| {
            if !source.path.is_absolute() {
                return Err(invalid());
            }
            sum.checked_add(source.snapshot.size).ok_or_else(invalid)
        })?;
        if !range_operation && total != self.source_size {
            return Err(invalid());
        }
        let mut names = HashSet::new();
        let mut stages = HashSet::new();
        for artifact in &self.artifacts {
            validate_basename(&artifact.name)?;
            validate_basename(&artifact.stage_name)?;
            if !valid_hash(&artifact.sha256)
                || artifact.size != artifact.snapshot.size
                || !names.insert(artifact.name.to_lowercase())
                || !stages.insert(&artifact.stage_name)
            {
                return Err(invalid());
            }
        }
        if let Some(rescue) = &self.rescue {
            if self.artifacts.len() != 1
                || rescue.part_index == 0
                || rescue.part_index as usize > MAX_PARTS
                || rescue.source_manifests.is_empty()
                || rescue.source_manifests.len() > crate::coverage::MAX_COVERAGE_GROUPS
                || !rescue.target_manifest.path.is_absolute()
                || rescue
                    .source_manifests
                    .iter()
                    .any(|s| !s.path.is_absolute())
                || rescue.range.checked_len().map_err(|_| invalid())? != self.source_size
                || !crate::restart::matches_payload(
                    &self.artifacts[0].stage_name,
                    "payload-rescued",
                )
                || self.artifacts[0].size != self.source_size
                || self.artifacts[0].sha256 != self.sha256
            {
                return Err(invalid());
            }
        } else if let Some(rebuild) = &self.rebuild {
            if self.sources.len() != 1
                || self.artifacts.len() != 1
                || rebuild.part_index == 0
                || rebuild.part_index as usize > MAX_PARTS
                || !rebuild.manifest.path.is_absolute()
                || rebuild.range.checked_len().map_err(|_| invalid())? != self.source_size
                || rebuild.range.end_exclusive > self.sources[0].snapshot.size
                || !crate::restart::matches_payload(
                    &self.artifacts[0].stage_name,
                    "payload-rebuilt",
                )
                || self.artifacts[0].size != self.source_size
                || self.artifacts[0].sha256 != self.sha256
            {
                return Err(invalid());
            }
        } else if self.operation != "merge" {
            if (self.operation == "split" && self.sources.len() != 1) || self.artifacts.len() < 2 {
                return Err(invalid());
            }
            let payload = &self.artifacts[..self.artifacts.len() - 1];
            let mut total = 0_u64;
            for (i, artifact) in payload.iter().enumerate() {
                if !crate::restart::matches_payload(
                    &artifact.stage_name,
                    &format!("payload-{:05}", i + 1),
                ) {
                    return Err(invalid());
                }
                total = total.checked_add(artifact.size).ok_or_else(invalid)?;
            }
            let last = self.artifacts.last().expect("nonempty");
            if total != self.source_size
                || !crate::restart::matches_payload(&last.stage_name, "payload-manifest")
                || !last.name.ends_with(".afparts.json")
            {
                return Err(invalid());
            }
        } else if self.artifacts.len() != 1
            || !crate::restart::matches_payload(&self.artifacts[0].stage_name, "payload-merged")
            || self.artifacts[0].size != self.source_size
            || self.artifacts[0].sha256 != self.sha256
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn receipt(&self, target: &SafeDir, stage: &SafeDir) -> Receipt {
        let mut outputs: Vec<_> = self
            .artifacts
            .iter()
            .map(|a| target.path.join(&a.name))
            .collect();
        let manifest = if matches!(self.operation.as_str(), "split" | "reshard") {
            outputs.pop()
        } else {
            None
        };
        Receipt {
            source_size: self.source_size,
            part_count: if matches!(self.operation.as_str(), "rebuild-part" | "rescue") {
                1
            } else if self.operation != "merge" {
                outputs.len() as u32
            } else {
                self.sources.len() as u32
            },
            sha256: self.sha256.clone(),
            output_files: outputs,
            manifest_path: manifest,
            verification_level: "verified_against_available_sources".to_owned(),
            verification_scope: if self.rebuild.is_some() || self.rescue.is_some() {
                "target_range_only"
            } else {
                "full_stream"
            },
            target_range: self
                .rebuild
                .as_ref()
                .map(|r| r.range)
                .or_else(|| self.rescue.as_ref().map(|r| r.range)),
            provenance_authenticated: false,
            transaction_id: self.task_id.clone(),
            staging_directory: stage.path.clone(),
            staging_retained: true,
            staged_payload_links_scheduled_for_removal: 0,
        }
    }
}

struct HeldSource {
    directory: SafeDir,
    name: String,
    file: cap_std::fs::File,
    snapshot: Snapshot,
}
fn check_sources(sources: &[HeldSource]) -> Result<()> {
    for source in sources {
        source.directory.check_location()?;
        source.snapshot.check(&source.file)?;
        source
            .snapshot
            .check(&source.directory.open_read(&source.name)?)?;
    }
    Ok(())
}

fn manifest_guard(source: &RecoverySource) -> Result<(HeldSource, crate::Manifest)> {
    let (directory, name) = input_parent(&source.path)?;
    let mut file = directory.open_read(&name)?;
    source.snapshot.check(&file)?;
    let manifest = parse_manifest(&read_bounded_file(&mut file, crate::MAX_MANIFEST_BYTES)?)?;
    Ok((
        HeldSource {
            directory,
            name,
            file,
            snapshot: source.snapshot.clone(),
        },
        manifest,
    ))
}

fn rescue_sources(
    plan: &RecoveryPlan,
    context: &mut TaskContext<'_>,
    buffer: &mut [u8],
) -> Result<Vec<HeldSource>> {
    let evidence = plan.rescue.as_ref().expect("rescue evidence");
    let (target_guard, target) = manifest_guard(&evidence.target_manifest)?;
    let part = target
        .parts
        .get(evidence.part_index as usize - 1)
        .ok_or_else(invalid)?;
    if part.offset != evidence.range.start
        || part.size != plan.source_size
        || part.sha256 != plan.sha256
    {
        return Err(invalid());
    }
    let mut held = Vec::with_capacity(plan.sources.len() + evidence.source_manifests.len() + 1);
    held.push(target_guard);
    let mut source_cursor = 0;
    for guard in &evidence.source_manifests {
        context.cancellation.check()?;
        let (manifest_guard, manifest) = manifest_guard(guard)?;
        if manifest.original_size != target.original_size
            || manifest.original_sha256 != target.original_sha256
        {
            return Err(invalid());
        }
        let spans = crate::rescue::read_spans(&manifest, evidence.range, 0)?;
        let mut span_cursor = 0;
        let mut whole = Sha256::default();
        let mut range_hash = Sha256::default();
        for part in &manifest.parts {
            let source = plan.sources.get(source_cursor).ok_or_else(invalid)?;
            source_cursor += 1;
            if source.path != manifest_guard.directory.path.join(&part.filename)
                || source.snapshot.size != part.size
            {
                return Err(invalid());
            }
            let (directory, name) = input_parent(&source.path)?;
            let mut file = directory.open_read(&name)?;
            source.snapshot.check(&file)?;
            if hash_file_with_whole(&mut file, part.size, context, buffer, Some(&mut whole))?
                != part.sha256
            {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch).for_part(part.index));
            }
            if let Some(span) = spans
                .get(span_cursor)
                .filter(|s| s.part_index == part.index)
            {
                crate::stream::hash_range_with_whole(
                    &mut file,
                    ByteRange::new(span.source_offset, span.source_offset + span.length)?,
                    context,
                    buffer,
                    Some(&mut range_hash),
                )?;
                span_cursor += 1;
            }
            source.snapshot.check(&file)?;
            held.push(HeldSource {
                directory,
                name,
                file,
                snapshot: source.snapshot.clone(),
            });
        }
        if digest_string(whole) != target.original_sha256
            || digest_string(range_hash) != plan.sha256
            || span_cursor != spans.len()
        {
            return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
        }
        held.push(manifest_guard);
    }
    if source_cursor != plan.sources.len() {
        return Err(invalid());
    }
    check_sources(&held)?;
    Ok(held)
}

pub(crate) fn read_bounded(stage: &SafeDir, name: &str, limit: usize) -> Result<Vec<u8>> {
    let mut file = stage.open_read(name)?;
    read_bounded_file(&mut file, limit)
}

/// Read the pinned handle itself; never reopen a name to read different bytes.
pub(crate) fn read_bounded_file(file: &mut cap_std::fs::File, limit: usize) -> Result<Vec<u8>> {
    let snapshot = Snapshot::of(file)?;
    if snapshot.size > limit as u64 {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    snapshot.check(file)?;
    if bytes.len() > limit {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    Ok(bytes)
}

struct Prepared {
    target: SafeDir,
    stage: SafeDir,
    plan: RecoveryPlan,
    sources: Vec<HeldSource>,
    lock: std::fs::File,
}

fn prepare(path: &Path, context: &mut TaskContext<'_>, buffer: &mut [u8]) -> Result<Prepared> {
    context.cancellation.check()?;
    let stage = SafeDir::open(path)?;
    let name = stage
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let id = task_id(name).ok_or_else(invalid)?;
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let target = SafeDir::open(stage.path.parent().ok_or_else(invalid)?)?;
    // The live writer holds this OS lock. A terminated process releases it.
    let lock = crate::space::lock(stage.open_read("lock")?)?;
    let owner: serde_json::Value =
        serde_json::from_slice(&read_bounded(&stage, "owner.json", 4096)?)
            .map_err(|_| invalid())?;
    if (owner["schema_version"] != 2 && owner["schema_version"] != 3)
        || owner["task_id"] != id
        || owner["tool"] != crate::PRODUCT_NAME_EN
    {
        return Err(invalid());
    }
    let plan: RecoveryPlan =
        serde_json::from_slice(&read_bounded(&stage, "recovery.json", MAX_RECOVERY_BYTES)?)
            .map_err(|_| invalid())?;
    plan.validate(&target, &stage, id)?;
    if owner["operation"] != plan.operation {
        return Err(invalid());
    }
    let committed = match stage.open_read("COMMITTED.json") {
        Ok(_) => {
            let marker: serde_json::Value =
                serde_json::from_slice(&read_bounded(&stage, "COMMITTED.json", 4096)?)
                    .map_err(|_| invalid())?;
            if marker["schema_version"] != 1
                || marker["task_id"] != id
                || marker["status"] != "committed"
            {
                return Err(invalid());
            }
            true
        }
        Err(error) if error.code == ErrorCode::InputNotFound => false,
        Err(error) => return Err(error),
    };
    let mut sources = Vec::with_capacity(plan.sources.len());
    let mut whole = Sha256::default();
    if plan.rescue.is_none() {
        for source in &plan.sources {
            let (directory, name) = input_parent(&source.path)?;
            let mut file = directory.open_read(&name)?;
            source.snapshot.check(&file)?;
            if let Some(rebuild) = &plan.rebuild {
                if hash_range(&mut file, rebuild.range, context, buffer)? != plan.sha256 {
                    return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
                }
            } else {
                hash_file_with_whole(
                    &mut file,
                    source.snapshot.size,
                    context,
                    buffer,
                    Some(&mut whole),
                )?;
            }
            sources.push(HeldSource {
                directory,
                name,
                file,
                snapshot: source.snapshot.clone(),
            });
        }
        if plan.rebuild.is_none() && digest_string(whole) != plan.sha256 {
            return Err(VolumeError::new(ErrorCode::InputChangedDuringOperation));
        }
    } else {
        sources = rescue_sources(&plan, context, buffer)?;
    }
    if let Some(rebuild) = &plan.rebuild {
        let (directory, name) = input_parent(&rebuild.manifest.path)?;
        let mut file = directory.open_read(&name)?;
        rebuild.manifest.snapshot.check(&file)?;
        let manifest = parse_manifest(&read_bounded_file(&mut file, crate::MAX_MANIFEST_BYTES)?)?;
        let part = manifest
            .parts
            .get(rebuild.part_index as usize - 1)
            .ok_or_else(invalid)?;
        if manifest.original_size != plan.sources[0].snapshot.size
            || part.offset != rebuild.range.start
            || part.size != plan.source_size
            || part.sha256 != plan.sha256
        {
            return Err(invalid());
        }
        sources.push(HeldSource {
            directory,
            name,
            file,
            snapshot: rebuild.manifest.snapshot.clone(),
        });
    }
    // Check every staged payload and every existing destination BEFORE any writes.
    let mut staged_whole = Sha256::default();
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        // A completed live transaction may already have removed its staging
        // link. Verify the actual published handle instead; the marker alone
        // never substitutes for source, identity or content verification.
        let mut file = match stage.open_read(&artifact.stage_name) {
            Ok(file) => file,
            Err(error) if committed && error.code == ErrorCode::InputNotFound => {
                target.open_read(&artifact.name).map_err(|error| {
                    if error.code == ErrorCode::InputNotFound {
                        VolumeError::new(ErrorCode::PartMissing)
                    } else {
                        error
                    }
                })?
            }
            Err(error) => return Err(error),
        };
        artifact.snapshot.check(&file)?;
        let aggregate = if matches!(plan.operation.as_str(), "merge" | "rebuild-part" | "rescue")
            || index + 1 < plan.artifacts.len()
        {
            Some(&mut staged_whole)
        } else {
            None
        };
        if hash_file_with_whole(&mut file, artifact.size, context, buffer, aggregate)?
            != artifact.sha256
        {
            return Err(VolumeError::new(ErrorCode::PartHashMismatch));
        }
        match target.open_read(&artifact.name) {
            Ok(mut existing) => {
                if artifact.snapshot.check(&existing).is_err()
                    || hash_file(&mut existing, artifact.size, context, buffer)? != artifact.sha256
                {
                    return Err(VolumeError::new(ErrorCode::OutputExists));
                }
            }
            Err(error) if error.code == ErrorCode::InputNotFound => {
                if committed {
                    return Err(VolumeError::new(ErrorCode::PartMissing));
                }
            }
            Err(error) => return Err(error),
        }
    }
    if digest_string(staged_whole) != plan.sha256 {
        return Err(VolumeError::new(ErrorCode::SourceHashMismatch));
    }
    if matches!(plan.operation.as_str(), "split" | "reshard") {
        let artifact = plan.artifacts.last().expect("validated manifest");
        let (manifest_dir, manifest_name) = match stage.open_read(&artifact.stage_name) {
            Ok(_) => (&stage, artifact.stage_name.as_str()),
            Err(error) if committed && error.code == ErrorCode::InputNotFound => {
                (&target, artifact.name.as_str())
            }
            Err(error) => return Err(error),
        };
        let manifest = parse_manifest(&read_bounded(
            manifest_dir,
            manifest_name,
            crate::MAX_MANIFEST_BYTES,
        )?)?;
        if manifest.original_size != plan.source_size
            || manifest.original_sha256 != plan.sha256
            || manifest.parts.len() + 1 != plan.artifacts.len()
        {
            return Err(invalid());
        }
        for (part, artifact) in manifest.parts.iter().zip(&plan.artifacts) {
            if part.filename != artifact.name
                || part.size != artifact.size
                || part.sha256 != artifact.sha256
            {
                return Err(invalid());
            }
        }
    }
    check_sources(&sources)?;
    stage.check_location()?;
    target.check_location()?;
    Ok(Prepared {
        target,
        stage,
        plan,
        sources,
        lock,
    })
}

/// Read-only full verification; a later execution repeats all checks.
pub fn plan_recover_transaction(path: &Path, context: &mut TaskContext<'_>) -> Result<Receipt> {
    let mut buffer = vec![0; BUFFER_BYTES];
    let prepared = prepare(path, context, &mut buffer)?;
    Ok(prepared.plan.receipt(&prepared.target, &prepared.stage))
}

/// Resume publication only. Incomplete writing and legacy M1 tasks remain untouched.
pub fn recover_transaction(path: &Path, context: &mut TaskContext<'_>) -> Result<Receipt> {
    let mut buffer = vec![0; BUFFER_BYTES];
    let prepared = prepare(path, context, &mut buffer)?;
    let receipt = prepared.plan.receipt(&prepared.target, &prepared.stage);
    match prepared.stage.open_read("COMMITTED.json") {
        Ok(_) => {
            let value: serde_json::Value =
                serde_json::from_slice(&read_bounded(&prepared.stage, "COMMITTED.json", 4096)?)
                    .map_err(|_| invalid())?;
            if value["schema_version"] == 1
                && value["task_id"] == prepared.plan.task_id
                && value["status"] == "committed"
            {
                for artifact in &prepared.plan.artifacts {
                    let output = prepared.target.open_read(&artifact.name).map_err(|error| {
                        if error.code == ErrorCode::InputNotFound {
                            VolumeError::new(ErrorCode::PartMissing)
                        } else {
                            error
                        }
                    })?;
                    artifact.snapshot.check(&output)?;
                }
                return Ok(receipt);
            }
            return Err(VolumeError::new(ErrorCode::OutputExists));
        }
        Err(error) if error.code == ErrorCode::InputNotFound => (),
        Err(error) => return Err(error),
    }
    let mut transaction = Transaction::resume(
        &prepared.target,
        prepared.stage,
        &prepared.plan,
        prepared.lock,
    )?;
    transaction
        .publish(
            &prepared.plan.artifacts,
            context,
            &mut buffer,
            &mut || check_sources(&prepared.sources),
            true,
        )
        .map_err(|error| transaction.fail(error))?;
    Ok(receipt)
}
