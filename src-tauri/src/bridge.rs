//! The webview has no shell, filesystem, network, or arbitrary-command plugin.
//! Writes require a single-use server-side plan token and re-planning.
use core::{CancellationToken, ErrorCode, TaskContext, VolumeError};
use easy_archive_core as core;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};

type ApiResult = Result<Value, Value>;
fn failure(code: &str, en: &str, zh: &str) -> Value {
    json!({"code":code,"message":{"en":en,"zh_cn":zh},"suggestion":{"en":"Review the selected files and plan again.","zh_cn":"请检查所选文件并重新预览计划。"}})
}
fn invalid() -> VolumeError {
    VolumeError::new(ErrorCode::InvalidArgument)
}
fn encode<T: Serialize>(data: T) -> core::Result<Value> {
    serde_json::to_value(data).map_err(|_| invalid())
}
/// IPC byte values must never round through JavaScript's 53-bit Number.
pub fn exact_integers(value: &mut Value) {
    match value {
        Value::Number(n) if n.is_u64() || n.is_i64() => *value = Value::String(n.to_string()),
        Value::Object(map) => map.values_mut().for_each(exact_integers),
        Value::Array(items) => items.iter_mut().for_each(exact_integers),
        _ => {}
    }
}
fn error_value(error: VolumeError) -> Value {
    let mut value = serde_json::to_value(error).expect("fixed error schema");
    exact_integers(&mut value);
    value
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiRequest {
    pub operation: String,
    pub input: String,
    #[serde(default)]
    pub part_paths: Vec<String>,
    #[serde(default)]
    pub output_dir: String,
    #[serde(default)]
    pub basename: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub policy: String,
    #[serde(default)]
    pub tolerance: String,
    #[serde(default)]
    pub alignment: String,
    #[serde(default)]
    pub manifest: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub part: String,
    #[serde(default)]
    pub expected_sha256: String,
    #[serde(default)]
    pub metadata_only: bool,
}
fn optional(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

#[derive(Clone)]
enum Operation {
    Split(core::SplitRequest),
    Merge(core::MergeRequest),
    Reshard(core::ReshardRequest),
    Rebuild(core::RebuildPartRequest),
    Rescue(core::RescueRequest),
    Coverage(core::CoverageRequest),
    Verify(PathBuf, Vec<PathBuf>, Option<String>),
    Transactions(PathBuf),
    Recover(PathBuf),
    Restart(PathBuf),
}
impl UiRequest {
    fn resolve(&self) -> core::Result<Operation> {
        if serde_json::to_vec(self).map_err(|_| invalid())?.len() > 4 * 1024 * 1024
            || self.sources.len() > 8
            || self.part_paths.len() > core::MAX_PARTS
            || self.input.is_empty()
        {
            return Err(invalid());
        }
        let input = PathBuf::from(&self.input);
        let part_paths = self.part_paths.iter().map(PathBuf::from).collect();
        let output_dir = optional(&self.output_dir).map(PathBuf::from);
        let basename = optional(&self.basename);
        let expected_sha256 = optional(&self.expected_sha256);
        if let Some(hash) = &expected_sha256 {
            core::group::validate_expected_hash(hash)?;
        }
        let policy = || -> core::Result<core::SplitPolicy> {
            let bytes = core::parse_size(&self.size)?;
            match self.policy.as_str() {
                "hard" => Ok(core::SplitPolicy::Hard { max_bytes: bytes }),
                "balanced" => {
                    let tolerance = self.tolerance.parse::<u32>().map_err(|_| invalid())?;
                    if tolerance > 100 {
                        return Err(invalid());
                    }
                    Ok(core::SplitPolicy::Balanced {
                        target_bytes: bytes,
                        tolerance_ppm: tolerance * 10_000,
                        hard_max_bytes: None,
                    })
                }
                _ => Err(invalid()),
            }
        };
        let alignment = || {
            optional(&self.alignment)
                .map(|v| core::parse_size(&v))
                .transpose()
        };
        let part = || -> core::Result<u32> {
            let value = self.part.parse::<u32>().map_err(|_| invalid())?;
            if value == 0 || value > 10_000 {
                Err(invalid())
            } else {
                Ok(value)
            }
        };
        Ok(match self.operation.as_str() {
            "split" => Operation::Split(core::SplitRequest {
                input,
                output_dir,
                basename,
                policy: policy()?,
                alignment: alignment()?,
                allow_any_file: false,
            }),
            "merge" => Operation::Merge(core::MergeRequest {
                part_paths,
                input,
                output_dir,
                basename,
                expected_sha256,
            }),
            "reshard" => Operation::Reshard(core::ReshardRequest {
                part_paths,
                input,
                output_dir,
                basename: basename.ok_or_else(invalid)?,
                policy: policy()?,
                alignment: alignment()?,
                expected_sha256,
            }),
            "rebuild" => Operation::Rebuild(core::RebuildPartRequest {
                original: input,
                manifest: self.manifest.clone().into(),
                part: part()?,
                output_dir,
                basename,
            }),
            "rescue" => Operation::Rescue(core::RescueRequest {
                target_manifest: input,
                source_manifests: self.sources.iter().map(PathBuf::from).collect(),
                part: part()?,
                output_dir,
                basename,
                expected_sha256,
            }),
            "coverage" => Operation::Coverage(core::CoverageRequest {
                target_manifest: input,
                source_manifests: self.sources.iter().map(PathBuf::from).collect(),
                part: if self.part.is_empty() {
                    None
                } else {
                    Some(part()?)
                },
                metadata_only: self.metadata_only,
            }),
            "verify" => Operation::Verify(input, part_paths, expected_sha256),
            "transactions" => Operation::Transactions(input),
            "recover" => Operation::Recover(input),
            "restart" => Operation::Restart(input),
            _ => return Err(invalid()),
        })
    }
}
impl Operation {
    fn plan(&self, ctx: &mut TaskContext<'_>) -> core::Result<Value> {
        ctx.cancellation.check()?;
        match self {
            Self::Split(r) => encode(core::plan_split_file(r)?),
            Self::Merge(r) => encode(core::plan_merge_file(r)?),
            Self::Reshard(r) => encode(core::plan_reshard_file(r)?),
            Self::Rebuild(r) => encode(core::plan_rebuild_part(r)?),
            Self::Rescue(r) => encode(core::plan_rescue(r)?),
            Self::Coverage(r) => {
                let mut preview = r.clone();
                preview.metadata_only = true;
                encode(core::coverage(&preview, ctx)?)
            }
            Self::Verify(path, parts, _) => encode(core::inspect_selected_group(path, parts)?),
            Self::Transactions(path) => encode(core::transaction::inspect_transactions(path)?),
            Self::Recover(path) => encode(core::recovery::plan_recover_transaction(path, ctx)?),
            Self::Restart(path) => encode(core::restart::plan_restart_task(path, ctx)?),
        }
    }
    fn execute(&self, ctx: &mut TaskContext<'_>) -> core::Result<Value> {
        match self {
            Self::Split(r) => encode(core::split_file(r, ctx)?),
            Self::Merge(r) => encode(core::merge_file(r, ctx)?),
            Self::Reshard(r) => encode(core::reshard_file(r, ctx)?),
            Self::Rebuild(r) => encode(core::rebuild_part(r, ctx)?),
            Self::Rescue(r) => encode(core::rescue_part(r, ctx)?),
            Self::Coverage(r) => encode(core::coverage(r, ctx)?),
            Self::Verify(path, parts, hash) => encode(core::verify_selected_group(
                path,
                parts,
                hash.as_deref(),
                ctx,
            )?),
            Self::Transactions(path) => encode(core::transaction::inspect_transactions(path)?),
            Self::Recover(path) => encode(core::recovery::recover_transaction(path, ctx)?),
            Self::Restart(path) => encode(core::restart::restart_task(path, ctx)?),
        }
    }
}
fn stable_plan(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("space");
            map.values_mut().for_each(stable_plan);
        }
        Value::Array(items) => items.iter_mut().for_each(stable_plan),
        _ => {}
    }
}
struct Pending {
    token: String,
    operation: Operation,
    preview: Value,
}
struct Active {
    id: String,
    cancel: CancellationToken,
}
#[derive(Default)]
struct Inner {
    pending: Option<Pending>,
    active: Option<Active>,
    closing: bool,
}
#[derive(Default, Clone)]
pub struct Session(Arc<Mutex<Inner>>);
impl Session {
    pub fn request_close(&self) -> bool {
        let mut inner = self.0.lock().expect("session mutex");
        if let Some(active) = &inner.active {
            active.cancel.cancel();
            inner.closing = true;
            true
        } else {
            false
        }
    }
    fn reserve(&self, id: &str) -> Result<JobGuard, Value> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(error_value(invalid()));
        }
        let mut inner = self.0.lock().expect("session mutex");
        if inner.active.is_some() || inner.closing {
            return Err(failure(
                "TASK_BUSY",
                "A task is still running.",
                "仍有任务正在运行。",
            ));
        }
        let cancel = CancellationToken::default();
        inner.active = Some(Active {
            id: id.to_owned(),
            cancel: cancel.clone(),
        });
        Ok(JobGuard {
            session: self.clone(),
            cancel,
            app: None,
        })
    }
}
struct JobGuard {
    session: Session,
    cancel: CancellationToken,
    app: Option<tauri::AppHandle>,
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        let closing = {
            let mut inner = self.session.0.lock().expect("session mutex");
            inner.active = None;
            inner.closing
        };
        if closing && let Some(app) = &self.app {
            app.exit(0);
        }
    }
}
async fn worker(
    app: tauri::AppHandle,
    job_id: String,
    request: Option<UiRequest>,
    token: Option<String>,
) -> ApiResult {
    let session = app.state::<Session>().inner().clone();
    let mut guard = session.reserve(&job_id)?;
    guard.app = Some(app.clone());
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut callback = |progress: core::Progress| {
            if last.elapsed() >= Duration::from_millis(80)
                || progress.completed_bytes == progress.total_bytes
            {
                let mut data = json!({"jobId": job_id, "progress": progress});
                exact_integers(&mut data);
                let _ = app.emit("archive-progress", data);
                last = Instant::now();
            }
        };
        let mut ctx = TaskContext {
            cancellation: _guard.cancel.clone(),
            progress: Some(&mut callback),
            io_fault: None,
        };
        let result = if let Some(request) = request {
            // A failed/new preview invalidates every older write authorization.
            session.0.lock().expect("session mutex").pending = None;
            let operation = request.resolve().map_err(error_value)?;
            let preview = operation.plan(&mut ctx).map_err(error_value)?;
            let token = uuid::Uuid::new_v4().to_string();
            session.0.lock().expect("session mutex").pending = Some(Pending {
                token: token.clone(),
                operation,
                preview: preview.clone(),
            });
            json!({"token":token,"preview":preview})
        } else {
            let pending = session
                .0
                .lock()
                .expect("session mutex")
                .pending
                .take()
                .ok_or_else(|| {
                    failure(
                        "PLAN_REQUIRED",
                        "Preview a new plan before starting.",
                        "开始前请预览新计划。",
                    )
                })?;
            if token.as_deref() != Some(pending.token.as_str()) {
                return Err(failure(
                    "PLAN_REQUIRED",
                    "The plan token expired.",
                    "计划凭据已失效。",
                ));
            }
            let mut current = pending.operation.plan(&mut ctx).map_err(error_value)?;
            let mut previous = pending.preview;
            stable_plan(&mut current);
            stable_plan(&mut previous);
            if current != previous {
                return Err(error_value(VolumeError::new(
                    ErrorCode::InputChangedDuringOperation,
                )));
            }
            pending.operation.execute(&mut ctx).map_err(error_value)?
        };
        let mut result = result;
        exact_integers(&mut result);
        Ok(result)
    })
    .await
    .map_err(|_| {
        failure(
            "TASK_FAILED",
            "The worker stopped unexpectedly; review retained transactions.",
            "工作线程意外停止；请检查保留的事务。",
        )
    })?
}
#[tauri::command]
pub async fn plan_operation(
    app: tauri::AppHandle,
    request: UiRequest,
    job_id: String,
) -> ApiResult {
    worker(app, job_id, Some(request), None).await
}
#[tauri::command]
pub async fn execute_plan(app: tauri::AppHandle, token: String, job_id: String) -> ApiResult {
    worker(app, job_id, None, Some(token)).await
}
#[tauri::command]
pub fn cancel_task(session: tauri::State<'_, Session>, job_id: String) -> bool {
    let inner = session.0.lock().expect("session mutex");
    if let Some(active) = &inner.active
        && active.id == job_id
    {
        active.cancel.cancel();
        true
    } else {
        false
    }
}
#[tauri::command]
pub async fn choose_paths() -> Result<Vec<String>, Value> {
    tauri::async_runtime::spawn_blocking(|| {
        let paths = rfd::FileDialog::new().pick_files().unwrap_or_default();
        if paths.len() > core::MAX_PARTS + 1 {
            return Err(error_value(VolumeError::new(ErrorCode::LimitExceeded)));
        }
        let paths: Vec<String> = paths
            .into_iter()
            .map(|p| {
                p.into_os_string()
                    .into_string()
                    .map_err(|_| error_value(invalid()))
            })
            .collect::<Result<_, _>>()?;
        if serde_json::to_vec(&paths)
            .map_err(|_| error_value(invalid()))?
            .len()
            > 4 * 1024 * 1024
        {
            return Err(error_value(VolumeError::new(ErrorCode::LimitExceeded)));
        }
        Ok(paths)
    })
    .await
    .map_err(|_| error_value(invalid()))?
}
#[tauri::command]
pub async fn choose_path(kind: String) -> Result<Option<String>, Value> {
    tauri::async_runtime::spawn_blocking(move || {
        let dialog = rfd::FileDialog::new();
        let path = match kind.as_str() {
            "directory" => dialog.pick_folder(),
            "archive" => dialog.add_filter("ZIP / 7z", &["zip", "7z"]).pick_file(),
            "manifest" => dialog.add_filter("Volume manifest", &["json"]).pick_file(),
            "file" => dialog.pick_file(),
            _ => return Err(error_value(invalid())),
        };
        path.map(|p| {
            p.into_os_string()
                .into_string()
                .map_err(|_| error_value(invalid()))
        })
        .transpose()
    })
    .await
    .map_err(|_| error_value(invalid()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipc_preserves_u64_offsets_and_error_ranges() {
        let mut value =
            json!({"offset":u64::MAX,"range":{"start":9_007_199_254_740_993_u64},"flag":false});
        exact_integers(&mut value);
        assert_eq!(value["offset"], u64::MAX.to_string());
        assert_eq!(value["range"]["start"], "9007199254740993");
        assert_eq!(value["flag"], false);
    }
    #[test]
    fn reservation_serializes_jobs_and_cancellation_targets_only_current_job() {
        let session = Session::default();
        let id = uuid::Uuid::new_v4().to_string();
        let guard = session.reserve(&id).unwrap();
        assert!(session.reserve(&id).is_err());
        assert!(session.request_close());
        assert_eq!(guard.cancel.check().unwrap_err().code, ErrorCode::Cancelled);
        drop(guard);
        assert!(session.reserve(&id).is_err());
    }
    #[test]
    fn preview_cannot_silently_change_offsets_or_outputs() {
        let mut old = json!({"output_path":"A","range":{"start":1,"end_exclusive":2},"space":{"available":10}});
        let mut new = old.clone();
        new["space"]["available"] = json!(9);
        stable_plan(&mut old);
        stable_plan(&mut new);
        assert_eq!(old, new);
        new["range"]["end_exclusive"] = json!(3);
        assert_ne!(old, new);
    }
}
