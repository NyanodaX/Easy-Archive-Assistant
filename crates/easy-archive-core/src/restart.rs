//! Restart incomplete writing with a fresh namespace in the same locked task.
//! Previous attempts and unknown files are retained; nothing is overwritten.
use crate::{
    ErrorCode, MAX_PARTS, MergeRequest, Receipt, Result, SplitRequest, TaskContext, VolumeError,
    engine::{merge_file_internal, plan_merge_internal, plan_split_internal, split_file_internal},
    error::io_error,
    group::load_selected_with_permit,
    rebuild::{RebuildPartRequest, plan_rebuild_internal, rebuild_part_internal},
    recovery::{RecoverySource, read_bounded},
    rescue::{RescueRequest, plan_rescue_internal, rescue_part_internal},
    reshard::{ReshardRequest, plan_reshard_internal, reshard_file_internal},
    secure_fs::{SafeDir, Snapshot, input_parent},
    space::SpaceEstimate,
    transaction::{TaskInspection, task_id},
    validate_basename,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_START_BYTES: usize = 16 * 1024 * 1024;
fn invalid() -> VolumeError {
    VolumeError::new(ErrorCode::TransactionNeedsRecovery)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "operation",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum StoredOperation {
    Split(SplitRequest),
    Merge(MergeRequest),
    Reshard(ReshardRequest),
    #[serde(rename = "rebuild-part")]
    RebuildPart(RebuildPartRequest),
    Rescue(RescueRequest),
}
impl StoredOperation {
    pub fn name(&self) -> &str {
        match self {
            Self::Split(_) => "split",
            Self::Merge(_) => "merge",
            Self::Reshard(_) => "reshard",
            Self::RebuildPart(_) => "rebuild-part",
            Self::Rescue(_) => "rescue",
        }
    }
}

pub(crate) struct StartData {
    pub operation: StoredOperation,
    pub inputs: Vec<RecoverySource>,
    pub verified_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StartRecord {
    schema_version: u32,
    tool: String,
    task_id: String,
    target_identity: (u64, u64),
    stage_identity: (u64, u64),
    operation: StoredOperation,
    inputs: Vec<RecoverySource>,
    names: Vec<String>,
    verified_sha256: Option<String>,
}

#[derive(Serialize)]
struct StartView<'a> {
    schema_version: u32,
    tool: &'a str,
    task_id: &'a str,
    target_identity: (u64, u64),
    stage_identity: (u64, u64),
    operation: &'a StoredOperation,
    inputs: &'a [RecoverySource],
    names: &'a [String],
    verified_sha256: Option<&'a str>,
}
struct SizeCounter {
    bytes: usize,
    exceeded: bool,
}
impl Write for SizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_START_BYTES - self.bytes {
            self.exceeded = true;
            return Err(std::io::Error::other("bounded record"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Bound serialization before creating any task directory or allocating its JSON.
pub(crate) fn check_start_size(names: &[String], data: &StartData) -> Result<()> {
    let mut counter = SizeCounter {
        bytes: 0,
        exceeded: false,
    };
    let view = StartView {
        schema_version: 1,
        tool: crate::PRODUCT_NAME_EN,
        task_id: "00000000000000000000000000000000",
        target_identity: (u64::MAX, u64::MAX),
        stage_identity: (u64::MAX, u64::MAX),
        operation: &data.operation,
        inputs: &data.inputs,
        names,
        verified_sha256: data.verified_sha256.as_deref(),
    };
    serde_json::to_writer(&mut counter, &view).map_err(|_| {
        VolumeError::new(if counter.exceeded {
            ErrorCode::LimitExceeded
        } else {
            ErrorCode::UnsafePath
        })
    })
}

pub(crate) fn write_start(
    target: &SafeDir,
    stage: &SafeDir,
    id: &str,
    names: &[String],
    data: StartData,
    context: &mut TaskContext<'_>,
) -> Result<()> {
    let record = StartRecord {
        schema_version: 1,
        tool: crate::PRODUCT_NAME_EN.to_owned(),
        task_id: id.to_owned(),
        target_identity: target.identity()?,
        stage_identity: stage.identity()?,
        operation: data.operation,
        inputs: data.inputs,
        names: names.to_vec(),
        verified_sha256: data.verified_sha256,
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| VolumeError::new(ErrorCode::UnsafePath))?;
    if bytes.len() > MAX_START_BYTES {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let mut draft = stage.create_new("start-draft.json")?;
    context.checkpoint(crate::IoPoint::StartWrite)?;
    draft.write_all(&bytes).map_err(io_error)?;
    context.checkpoint(crate::IoPoint::StartSync)?;
    draft.sync_all().map_err(io_error)?;
    drop(draft);
    context.checkpoint(crate::IoPoint::BeforeStartLink)?;
    stage
        .dir
        .hard_link("start-draft.json", &stage.dir, "start.json")
        .map_err(io_error)?;
    stage.sync()?;
    context.checkpoint(crate::IoPoint::AfterStartLink)
}

struct HeldInput {
    directory: SafeDir,
    name: String,
    file: cap_std::fs::File,
    snapshot: Snapshot,
}

pub(crate) struct RestartPermit {
    pub stage: SafeDir,
    pub id: String,
    record: StartRecord,
    inputs: Vec<HeldInput>,
    _lock: std::fs::File,
}
impl RestartPermit {
    fn check_inputs(&self) -> Result<()> {
        for input in &self.inputs {
            input.directory.check_location()?;
            input.snapshot.check(&input.file)?;
            input
                .snapshot
                .check(&input.directory.open_read(&input.name)?)?;
        }
        Ok(())
    }
    pub fn validate_target(&self, target: &SafeDir) -> Result<()> {
        self.stage.check_location()?;
        target.check_location()?;
        if target.identity()? != self.record.target_identity
            || self.stage.identity()? != self.record.stage_identity
        {
            return Err(invalid());
        }
        self.stage
            .require_absent("recovery.json")
            .map_err(|_| invalid())?;
        self.stage
            .require_absent("COMMITTED.json")
            .map_err(|_| invalid())?;
        for name in &self.record.names {
            target.require_absent(name)?;
        }
        self.check_inputs()
    }
    pub fn allows_task(&self, target: &SafeDir, task: &TaskInspection) -> Result<bool> {
        if task.task_id.as_deref() != Some(&self.id) || task.directory != self.stage.path {
            return Ok(false);
        }
        self.validate_target(target)?;
        Ok(true)
    }
    pub fn check_expected(
        &self,
        target: &SafeDir,
        names: &[String],
        data: &StartData,
    ) -> Result<()> {
        self.validate_target(target)?;
        if self.record.names != names
            || self.record.inputs != data.inputs
            || self.record.operation != data.operation
            || self.record.verified_sha256 != data.verified_sha256
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct RestartPlan {
    pub transaction_id: String,
    pub staging_directory: PathBuf,
    pub operation: String,
    pub source_size: u64,
    pub part_count: u32,
    pub output_files: Vec<PathBuf>,
    pub space: SpaceEstimate,
    pub previous_attempts_retained: bool,
    pub content_read: bool,
    pub writes_performed: bool,
}

fn prepare(path: &Path, context: &mut TaskContext<'_>) -> Result<(RestartPermit, RestartPlan)> {
    context.cancellation.check()?;
    let stage = SafeDir::open(path)?;
    let name = stage
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?;
    let id = task_id(name).ok_or_else(invalid)?.to_owned();
    if !name.starts_with(".eaa-")
        || id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let target = SafeDir::open(stage.path.parent().ok_or_else(invalid)?)?;
    let lock = crate::space::lock(stage.open_read("lock")?)?;
    let owner: serde_json::Value =
        serde_json::from_slice(&read_bounded(&stage, "owner.json", 4096)?)
            .map_err(|_| invalid())?;
    if owner["schema_version"] != 3
        || owner["tool"] != crate::PRODUCT_NAME_EN
        || owner["task_id"] != id
    {
        return Err(invalid());
    }
    // A crash after writing a complete draft but before exposing its link can
    // still preserve every option. Accept only a fully parsed and revalidated
    // draft when the final record is absent; never bypass an invalid final one.
    let start_bytes = match read_bounded(&stage, "start.json", MAX_START_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.code == ErrorCode::InputNotFound => {
            read_bounded(&stage, "start-draft.json", MAX_START_BYTES)?
        }
        Err(error) => return Err(error),
    };
    let record: StartRecord = serde_json::from_slice(&start_bytes).map_err(|_| invalid())?;
    if record.schema_version != 1
        || record.tool != crate::PRODUCT_NAME_EN
        || record.task_id != id
        || owner["operation"] != record.operation.name()
        || record.inputs.is_empty()
        || record.inputs.len() > MAX_PARTS + crate::coverage::MAX_COVERAGE_GROUPS + 1
        || record.names.is_empty()
        || record.names.len() > MAX_PARTS + 1
    {
        return Err(invalid());
    }
    let mut names = HashSet::new();
    match &record.operation {
        StoredOperation::Split(_)
            if record.inputs.len() != 1 || record.verified_sha256.is_some() =>
        {
            return Err(invalid());
        }
        StoredOperation::Merge(_)
        | StoredOperation::Reshard(_)
        | StoredOperation::RebuildPart(_)
        | StoredOperation::Rescue(_)
            if !record.verified_sha256.as_ref().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) =>
        {
            return Err(invalid());
        }
        _ => (),
    }
    for name in &record.names {
        validate_basename(name)?;
        if !names.insert(name.to_lowercase()) {
            return Err(invalid());
        }
    }
    let mut inputs = Vec::with_capacity(record.inputs.len());
    for source in &record.inputs {
        context.cancellation.check()?;
        if !source.path.is_absolute() {
            return Err(invalid());
        }
        let (directory, name) = input_parent(&source.path)?;
        let file = directory.open_read(&name)?;
        source.snapshot.check(&file)?;
        inputs.push(HeldInput {
            directory,
            name,
            file,
            snapshot: source.snapshot.clone(),
        });
    }
    let permit = RestartPermit {
        stage,
        id,
        record,
        inputs,
        _lock: lock,
    };
    permit.validate_target(&target)?;
    let (source_size, part_count, output_files, space, current_inputs) =
        match &permit.record.operation {
            StoredOperation::Split(request) => {
                let plan = plan_split_internal(request, Some(&permit))?;
                let (input_dir, input_name) = input_parent(&request.input)?;
                let snapshot = Snapshot::of(&input_dir.open_read(&input_name)?)?;
                let current = vec![RecoverySource {
                    path: input_dir.path.join(input_name),
                    snapshot,
                }];
                let mut outputs = plan.output_files;
                outputs.push(plan.manifest_path);
                (
                    plan.source.size,
                    plan.plan.part_count,
                    outputs,
                    plan.space,
                    current,
                )
            }
            StoredOperation::Rescue(request) => {
                let plan = plan_rescue_internal(request, Some(&permit))?;
                (
                    plan.target.size,
                    1,
                    vec![plan.output_path],
                    plan.space,
                    crate::rescue::start_inputs(request, Some(&permit))?,
                )
            }
            StoredOperation::RebuildPart(request) => {
                let plan = plan_rebuild_internal(request, Some(&permit))?;
                (
                    plan.target.size,
                    1,
                    vec![plan.output_path],
                    plan.space,
                    crate::rebuild::start_inputs(request, Some(&permit))?,
                )
            }
            StoredOperation::Reshard(request) => {
                let plan = plan_reshard_internal(request, Some(&permit))?;
                let group =
                    load_selected_with_permit(&request.input, &request.part_paths, Some(&permit))?;
                let mut outputs = plan.output_files;
                outputs.push(plan.manifest_path);
                (
                    plan.group.source_size,
                    plan.plan.part_count,
                    outputs,
                    plan.space,
                    group.start_inputs(),
                )
            }
            StoredOperation::Merge(request) => {
                let plan = plan_merge_internal(request, Some(&permit))?;
                let group =
                    load_selected_with_permit(&request.input, &request.part_paths, Some(&permit))?;
                (
                    plan.group.source_size,
                    plan.group.part_count,
                    vec![plan.output_path],
                    plan.space,
                    group.start_inputs(),
                )
            }
        };
    let actual_names: Vec<_> = output_files
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .expect("validated name")
                .to_owned()
        })
        .collect();
    if actual_names != permit.record.names || current_inputs != permit.record.inputs {
        return Err(invalid());
    }
    for output in &output_files {
        if output.parent() != Some(target.path.as_path()) {
            return Err(invalid());
        }
    }
    let plan = RestartPlan {
        transaction_id: permit.id.clone(),
        staging_directory: permit.stage.path.clone(),
        operation: permit.record.operation.name().to_owned(),
        source_size,
        part_count,
        output_files,
        space,
        previous_attempts_retained: true,
        content_read: false,
        writes_performed: false,
    };
    Ok((permit, plan))
}

/// Metadata-only plan. Original snapshots must still match; no writes occur.
pub fn plan_restart_task(path: &Path, context: &mut TaskContext<'_>) -> Result<RestartPlan> {
    Ok(prepare(path, context)?.1)
}

/// Full new copy into exclusive attempt files; never append to a partial file.
pub fn restart_task(path: &Path, context: &mut TaskContext<'_>) -> Result<Receipt> {
    let (permit, plan) = prepare(path, context)?;
    plan.space.require_sufficient()?;
    match &permit.record.operation {
        StoredOperation::Split(request) => split_file_internal(request, context, Some(&permit)),
        StoredOperation::Merge(request) => merge_file_internal(request, context, Some(&permit)),
        StoredOperation::Reshard(request) => reshard_file_internal(request, context, Some(&permit)),
        StoredOperation::RebuildPart(request) => {
            rebuild_part_internal(request, context, Some(&permit))
        }
        StoredOperation::Rescue(request) => rescue_part_internal(request, context, Some(&permit)),
    }
}

pub(crate) fn matches_payload(name: &str, expected: &str) -> bool {
    if name == expected {
        return true;
    }
    let Some(rest) = name.strip_prefix("attempt-") else {
        return false;
    };
    let Some((id, suffix)) = rest.split_once('-') else {
        return false;
    };
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && suffix == expected
}
