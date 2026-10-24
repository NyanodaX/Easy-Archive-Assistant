//! Retained staging, durable recovery plan and no-replace publication.
//! Persisted recovery records never authorize deleting files. Live transactions
//! may remove verified staging links after durable publication on Windows.
use crate::{
    ErrorCode, Result, SCHEMA_VERSION, VolumeError,
    task::{Phase, TaskContext},
};
use crate::{
    error::io_error,
    recovery::{CommitEvidence, RecoveryPlan},
    restart::{RestartPermit, StartData, check_start_size, write_start},
    secure_fs::{SafeDir, Snapshot},
    stream::hash_file,
};
use cap_fs_ext::DirExt;
use cap_std::fs::{DirBuilder, File};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_DIRECTORY_ENTRIES: usize = 20_000;

#[derive(Debug, Serialize)]
pub struct TaskInspection {
    pub directory: PathBuf,
    pub task_id: Option<String>,
    pub status: String,
    /// Existence only; recover --dry-run validates its untrusted contents.
    pub recovery_plan_present: bool,
    /// Existence only; restart --dry-run validates it before use.
    pub restart_record_present: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn artifact_records_without_live_creation_tickets_cannot_authorize_cleanup() {
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        let target = SafeDir::open(temp.path()).unwrap();
        std::fs::write(temp.path().join("source.001"), b"KEEP").unwrap();
        let source = crate::recovery::RecoverySource {
            path: temp.path().join("source.001"),
            snapshot: Snapshot::of(&target.open_read("source.001").unwrap()).unwrap(),
        };
        let hash = format!("{:x}", Sha256::digest(b"KEEP"));
        let start = StartData {
            operation: crate::restart::StoredOperation::Merge(crate::MergeRequest {
                part_paths: Vec::new(),
                input: source.path.clone(),
                output_dir: Some(temp.path().to_owned()),
                basename: Some("output.zip".to_owned()),
                expected_sha256: None,
            }),
            inputs: vec![source.clone()],
            verified_sha256: Some(hash.clone()),
        };
        let mut context = TaskContext::default();
        let mut task =
            Transaction::begin(&target, &["output.zip".to_owned()], start, &mut context).unwrap();
        let mut file = task.stage.create_new("payload-merged").unwrap();
        file.write_all(b"KEEP").unwrap();
        file.sync_all().unwrap();
        let artifact = Artifact {
            stage_name: "payload-merged".to_owned(),
            name: "output.zip".to_owned(),
            size: 4,
            sha256: hash.clone(),
            snapshot: Snapshot::of(&file).unwrap(),
        };
        drop(file);
        let mut buffer = vec![0; 1024];
        task.commit(
            &[artifact],
            CommitEvidence {
                rescue: None,
                rebuild: None,
                sources: vec![source],
                source_size: 4,
                sha256: hash,
            },
            &mut context,
            &mut buffer,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(task.staged_links_scheduled_for_removal, 0);
        assert_eq!(
            std::fs::read(task.stage.path.join("payload-merged")).unwrap(),
            b"KEEP"
        );
    }
    #[test]
    fn oversized_start_record_is_rejected_before_creating_a_task_directory() {
        let temp = tempfile::tempdir().unwrap();
        let target = SafeDir::open(temp.path()).unwrap();
        let start = StartData {
            operation: crate::restart::StoredOperation::Split(crate::SplitRequest {
                input: PathBuf::from("x".repeat(17 * 1024 * 1024)),
                output_dir: None,
                basename: None,
                policy: crate::SplitPolicy::Hard { max_bytes: 1 },
                alignment: None,
                allow_any_file: true,
            }),
            inputs: vec![],
            verified_sha256: None,
        };
        let error = Transaction::begin(
            &target,
            &["output.001".to_owned()],
            start,
            &mut TaskContext::default(),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::LimitExceeded);
        assert!(error.recovery_task_id.is_none());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

pub fn inspect_transactions(path: &Path) -> Result<Vec<TaskInspection>> {
    scan_tasks(&SafeDir::open(path)?)
}

pub(crate) fn scan_tasks(directory: &SafeDir) -> Result<Vec<TaskInspection>> {
    let mut tasks = Vec::new();
    for (i, entry) in directory.dir.entries().map_err(io_error)?.enumerate() {
        if i >= MAX_DIRECTORY_ENTRIES {
            return Err(VolumeError::new(ErrorCode::LimitExceeded));
        }
        let name = entry.map_err(io_error)?.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = task_id(name) else {
            continue;
        };
        let valid_id = id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let committed = (|| -> Result<bool> {
            if !valid_id {
                return Ok(false);
            }
            let stage = directory.dir.open_dir_nofollow(name).map_err(io_error)?;
            let stage = SafeDir {
                dir: stage,
                path: directory.path.join(name),
            };
            let mut file = stage.open_read("COMMITTED.json")?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(io_error)?;
            if bytes.len() > 4096 {
                return Ok(false);
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| VolumeError::new(ErrorCode::ManifestInvalid))?;
            Ok(value["schema_version"] == SCHEMA_VERSION
                && value["task_id"] == id
                && value["status"] == "committed")
        })()
        .unwrap_or(false);
        tasks.push(TaskInspection {
            directory: directory.path.join(name),
            task_id: valid_id.then(|| id.to_owned()),
            status: if committed {
                "committed"
            } else {
                "needs_recovery"
            }
            .to_owned(),
            recovery_plan_present: (|| -> Result<bool> {
                let stage = SafeDir::open(&directory.path.join(name))?;
                Ok(stage.open_read("recovery.json").is_ok())
            })()
            .unwrap_or(false),
            restart_record_present: (|| -> Result<bool> {
                Ok(SafeDir::open(&directory.path.join(name))?
                    .open_read("start.json")
                    .is_ok()
                    || SafeDir::open(&directory.path.join(name))?
                        .open_read("start-draft.json")
                        .is_ok())
            })()
            .unwrap_or(false),
        });
    }
    tasks.sort_by(|a, b| a.directory.cmp(&b.directory));
    Ok(tasks)
}

pub(crate) fn require_no_pending(directory: &SafeDir) -> Result<()> {
    require_no_pending_except(directory, None)
}

pub(crate) fn require_no_pending_except(
    directory: &SafeDir,
    permit: Option<&RestartPermit>,
) -> Result<()> {
    for task in scan_tasks(directory)? {
        if task.status != "committed"
            && !permit
                .map(|p| p.allows_task(directory, &task))
                .transpose()?
                .unwrap_or(false)
        {
            return Err(VolumeError::new(ErrorCode::TransactionNeedsRecovery));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub stage_name: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub snapshot: Snapshot,
}

pub(crate) struct Transaction<'a> {
    target: &'a SafeDir,
    pub stage: SafeDir,
    pub id: String,
    journal: File,
    _lock: Option<std::fs::File>,
    operation: String,
    payload_prefix: String,
    pub staged_links_scheduled_for_removal: u32,
    /// Creation tickets exist only in memory; persisted records cannot populate them.
    created_payloads: BTreeMap<String, (u64, u64)>,
}

pub(crate) fn task_id(name: &str) -> Option<&str> {
    name.strip_prefix(".eaa-")
        .or_else(|| name.strip_prefix(".archiveflow-"))
}

impl<'a> Transaction<'a> {
    pub fn begin(
        target: &'a SafeDir,
        names: &[String],
        start: StartData,
        context: &mut TaskContext<'_>,
    ) -> Result<Self> {
        check_start_size(names, &start)?;
        let operation = start.operation.name().to_owned();
        require_no_pending(target)?;
        target.check_location()?;
        for name in names {
            target.require_absent(name)?;
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let name = format!(".eaa-{id}");
        let builder = DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use cap_std::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        target
            .dir
            .create_dir_with(&name, &builder)
            .map_err(io_error)?;
        let result = (|| -> Result<Self> {
            context.checkpoint(crate::IoPoint::StageDirectoryCreated)?;
            let stage_dir = target.dir.open_dir_nofollow(&name).map_err(io_error)?;
            let stage = SafeDir {
                dir: stage_dir,
                path: target.path.join(&name),
            };
            let lock = crate::space::lock(stage.create_new("lock")?)?;
            let mut owner = stage.create_new("owner.json")?;
            context.checkpoint(crate::IoPoint::OwnerWrite)?;
            owner.write_all(serde_json::to_string(&json!({"schema_version":3,"tool":crate::PRODUCT_NAME_EN,"task_id":id,"operation":operation})).expect("JSON").as_bytes()).map_err(io_error)?;
            context.checkpoint(crate::IoPoint::OwnerSync)?;
            owner.sync_all().map_err(io_error)?;
            write_start(target, &stage, &id, names, start, context)?;
            let journal = stage.create_new("journal.jsonl")?;
            let mut task = Self {
                target,
                stage,
                id: id.clone(),
                journal,
                _lock: Some(lock),
                operation: operation.to_owned(),
                payload_prefix: String::new(),
                staged_links_scheduled_for_removal: 0,
                created_payloads: BTreeMap::new(),
            };
            task.record_checked(
                json!({"phase":"writing","operation":operation,"planned_output":names}),
                context,
            )?;
            task.stage.sync()?;
            target.sync()?;
            Ok(task)
        })();
        result.map_err(|mut error| {
            error.recovery_task_id = Some(id);
            error
        })
    }

    pub fn restart_writing(
        target: &'a SafeDir,
        names: &[String],
        start: StartData,
        permit: &RestartPermit,
        context: &mut TaskContext<'_>,
    ) -> Result<Self> {
        require_no_pending_except(target, Some(permit))?;
        permit.check_expected(target, names, &start)?;
        let stage = SafeDir {
            dir: permit.stage.dir.try_clone().map_err(io_error)?,
            path: permit.stage.path.clone(),
        };
        let attempt = uuid::Uuid::new_v4().simple().to_string();
        let journal = stage.create_new(&format!("attempt-{attempt}.jsonl"))?;
        let mut task = Self {
            target,
            stage,
            id: permit.id.clone(),
            journal,
            _lock: None,
            operation: start.operation.name().to_owned(),
            payload_prefix: format!("attempt-{attempt}-"),
            staged_links_scheduled_for_removal: 0,
            created_payloads: BTreeMap::new(),
        };
        task.record_checked(
            json!({"phase":"restarting","attempt":attempt,"previous_attempts_retained":true}),
            context,
        )
        .map_err(|error| task.fail(error))?;
        Ok(task)
    }

    pub fn payload_name(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.payload_prefix)
    }

    pub fn create_payload(&mut self, name: &str) -> Result<File> {
        let file = self.stage.create_new(name)?;
        let identity = Snapshot::of(&file)?;
        self.created_payloads
            .insert(name.to_owned(), (identity.device, identity.inode));
        Ok(file)
    }

    pub fn record(&mut self, event: serde_json::Value) -> Result<()> {
        self.record_inner(event, None)
    }

    fn record_checked(
        &mut self,
        event: serde_json::Value,
        context: &mut TaskContext<'_>,
    ) -> Result<()> {
        self.record_inner(event, Some(context))
    }

    fn record_inner(
        &mut self,
        mut event: serde_json::Value,
        mut context: Option<&mut TaskContext<'_>>,
    ) -> Result<()> {
        event["schema_version"] = json!(SCHEMA_VERSION);
        event["task_id"] = json!(self.id);
        let mut line = serde_json::to_vec(&event).expect("JSON event");
        line.push(b'\n');
        if let Some(context) = context.as_deref_mut() {
            context.checkpoint(crate::IoPoint::JournalWrite)?;
        }
        self.journal.write_all(&line).map_err(io_error)?;
        if let Some(context) = context {
            context.checkpoint(crate::IoPoint::JournalSync)?;
        }
        self.journal.sync_all().map_err(io_error)
    }

    pub fn fail(&mut self, mut error: VolumeError) -> VolumeError {
        let _ = self.record(json!({"phase":"failed","error_code":error.code}));
        error.recovery_task_id = Some(self.id.clone());
        error
    }

    pub fn commit(
        &mut self,
        artifacts: &[Artifact],
        evidence: CommitEvidence,
        context: &mut TaskContext<'_>,
        buffer: &mut [u8],
        validate_sources: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        let plan = RecoveryPlan::new(
            self.target,
            &self.stage,
            &self.id,
            &self.operation,
            artifacts,
            evidence,
        )?;
        let bytes = serde_json::to_vec(&plan).expect("recovery JSON");
        if bytes.len() > crate::recovery::MAX_RECOVERY_BYTES {
            return Err(VolumeError::new(ErrorCode::LimitExceeded));
        }
        // Publish the plan only after a full flush; torn draft files stay inert.
        let draft_name = format!("recovery-draft-{}.json", uuid::Uuid::new_v4().simple());
        let mut draft = self.stage.create_new(&draft_name)?;
        context.checkpoint(crate::IoPoint::RecoveryPlanWrite)?;
        draft.write_all(&bytes).map_err(io_error)?;
        context.checkpoint(crate::IoPoint::RecoveryPlanSync)?;
        draft.sync_all().map_err(io_error)?;
        drop(draft);
        context.checkpoint(crate::IoPoint::BeforeRecoveryPlanLink)?;
        self.stage
            .dir
            .hard_link(&draft_name, &self.stage.dir, "recovery.json")
            .map_err(io_error)?;
        self.stage.sync()?;
        self.record_checked(json!({"phase":"ready_to_commit"}), context)?;
        context.checkpoint(crate::IoPoint::PlanDurable)?;
        self.publish(artifacts, context, buffer, validate_sources, false)
    }

    pub(crate) fn resume(
        target: &'a SafeDir,
        stage: SafeDir,
        plan: &RecoveryPlan,
        lock: std::fs::File,
    ) -> Result<Self> {
        let journal_name = format!("recovery-{}.jsonl", uuid::Uuid::new_v4().simple());
        let journal = stage.create_new(&journal_name)?;
        Ok(Self {
            target,
            stage,
            id: plan.task_id.clone(),
            journal,
            _lock: Some(lock),
            operation: plan.operation.clone(),
            payload_prefix: String::new(),
            staged_links_scheduled_for_removal: 0,
            created_payloads: BTreeMap::new(),
        })
    }

    pub(crate) fn publish(
        &mut self,
        artifacts: &[Artifact],
        context: &mut TaskContext<'_>,
        buffer: &mut [u8],
        validate_sources: &mut dyn FnMut() -> Result<()>,
        allow_published: bool,
    ) -> Result<()> {
        // Hold all published read handles through the marker. On Windows this
        // denies writes/deletion of earlier parts while later parts publish.
        let mut published_files = Vec::with_capacity(artifacts.len());
        let mut cleanup_handles = Vec::new();
        for (i, artifact) in artifacts.iter().enumerate() {
            context.report(Phase::Publishing, i as u64, artifacts.len() as u64, None)?;
            validate_sources()?;
            self.target.check_location()?;
            self.stage.check_location()?;
            let live_owned = !allow_published
                && self.created_payloads.get(&artifact.stage_name)
                    == Some(&(artifact.snapshot.device, artifact.snapshot.inode));
            let (mut staged, can_cleanup) = self
                .stage
                .open_for_publication(&artifact.stage_name, live_owned)?;
            artifact.snapshot.check(&staged)?;
            let actual = hash_file(&mut staged, artifact.size, context, buffer)?;
            if actual != artifact.sha256 {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch));
            }
            self.record_checked(json!({"phase":"publish_intent","output":artifact.name,"staged":artifact.stage_name,"size":artifact.size,"sha256":artifact.sha256,"device":artifact.snapshot.device,"inode":artifact.snapshot.inode}),context)?;
            context.cancellation.check()?;
            validate_sources()?;
            if allow_published {
                match self.target.open_read(&artifact.name) {
                    Ok(mut existing) => {
                        if artifact.snapshot.check(&existing).is_err()
                            || hash_file(&mut existing, artifact.size, context, buffer)?
                                != artifact.sha256
                        {
                            return Err(VolumeError::new(ErrorCode::OutputExists));
                        }
                        self.record_checked(
                            json!({"phase":"already_published","output":artifact.name}),
                            context,
                        )?;
                        published_files.push(existing);
                        continue;
                    }
                    Err(error) if error.code == ErrorCode::InputNotFound => (),
                    Err(error) => return Err(error),
                }
            }
            context.checkpoint(crate::IoPoint::BeforePublish)?;
            // hard_link fails if destination exists on both Windows and Unix.
            // The selected filesystem must support hard links; never fall back
            // to rename (may replace) or an unverified direct destination copy.
            self.stage
                .dir
                .hard_link(&artifact.stage_name, &self.target.dir, &artifact.name)
                .map_err(io_error)?;
            self.target.sync()?;
            // Also models a crash after link creation but before its journal event.
            context.checkpoint(crate::IoPoint::AfterPublish)?;
            let published = self.target.open_read(&artifact.name)?;
            artifact.snapshot.check(&published)?;
            self.record_checked(json!({"phase":"published","output":artifact.name}), context)?;
            published_files.push(published);
            if can_cleanup {
                cleanup_handles.push(staged);
            }
        }
        validate_sources()?;
        for (file, artifact) in published_files.iter_mut().zip(artifacts) {
            artifact.snapshot.check(file)?;
            if hash_file(file, artifact.size, context, buffer)? != artifact.sha256 {
                return Err(VolumeError::new(ErrorCode::PartHashMismatch));
            }
        }
        context.checkpoint(crate::IoPoint::BeforeCommitMarker)?;
        self.record_checked(json!({"phase":"committed"}), context)?;
        let marker_name = format!("commit-{}.json", uuid::Uuid::new_v4().simple());
        let mut marker = self.stage.create_new(&marker_name)?;
        context.checkpoint(crate::IoPoint::CommitMarkerWrite)?;
        marker.write_all(serde_json::to_string(&json!({"schema_version":SCHEMA_VERSION,"task_id":self.id,"status":"committed"})).expect("JSON").as_bytes()).map_err(io_error)?;
        context.checkpoint(crate::IoPoint::CommitMarkerSync)?;
        marker.sync_all().map_err(io_error)?;
        drop(marker);
        self.stage
            .dir
            .hard_link(&marker_name, &self.stage.dir, "COMMITTED.json")
            .map_err(io_error)?;
        self.stage.sync()?;
        context.checkpoint(crate::IoPoint::AfterCommitMarker)?;
        // Only handles from this live write are eligible. No file or directory
        // is selected for deletion from a persisted plan, glob, or scan.
        #[cfg(windows)]
        if !allow_published && self.stage.check_location().is_ok() {
            for file in cleanup_handles {
                if context
                    .checkpoint(crate::IoPoint::BeforeCleanupLink)
                    .is_err()
                {
                    break;
                }
                if easy_archive_windows::mark_delete_on_close(&file).is_ok() {
                    drop(file);
                    self.staged_links_scheduled_for_removal += 1;
                }
                if context
                    .checkpoint(crate::IoPoint::AfterCleanupLink)
                    .is_err()
                {
                    break;
                }
            }
        }
        Ok(())
    }
}
