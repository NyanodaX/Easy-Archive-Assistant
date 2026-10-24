use easy_archive_core::{
    CancellationToken, ErrorCode, IoPoint, SplitPolicy, SplitRequest, TaskContext,
    recovery::{plan_recover_transaction, recover_transaction},
    restart::{plan_restart_task, restart_task},
    split_file,
    transaction::inspect_transactions,
    verify_group,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

fn request(input: &Path) -> SplitRequest {
    SplitRequest {
        input: input.to_owned(),
        output_dir: None,
        basename: None,
        policy: SplitPolicy::Hard { max_bytes: 2 },
        alignment: None,
        allow_any_file: false,
    }
}
fn sample(root: &Path) -> PathBuf {
    let path = root.join("sample.zip");
    fs::write(&path, b"hello").unwrap();
    fs::write(root.join("keep.txt"), b"KEEP").unwrap();
    path
}
fn inventory(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(inventory(&path));
        } else {
            files.push((path.clone(), fs::read(path).unwrap()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}
fn faulted(root: &Path, point: IoPoint) -> PathBuf {
    let input = sample(root);
    let mut fault = |current| {
        if current == point {
            Err(io::Error::from(io::ErrorKind::StorageFull))
        } else {
            Ok(())
        }
    };
    let error = split_file(
        &request(&input),
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InsufficientDiskSpace);
    assert!(error.recovery_task_id.is_some());
    assert_eq!(fs::read(&input).unwrap(), b"hello");
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"KEEP");
    inspect_transactions(root).unwrap().remove(0).directory
}

#[test]
fn early_startup_failure_and_cancellation_always_report_the_created_task_and_preserve_input() {
    for point in [
        IoPoint::StageDirectoryCreated,
        IoPoint::OwnerWrite,
        IoPoint::OwnerSync,
        IoPoint::StartWrite,
        IoPoint::StartSync,
        IoPoint::BeforeStartLink,
        IoPoint::AfterStartLink,
        IoPoint::JournalWrite,
        IoPoint::JournalSync,
    ] {
        for cancel_instead in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let input = sample(temp.path());
            let token = CancellationToken::default();
            let cancel = token.clone();
            let mut fault = |current| {
                if current == point {
                    let task = fs::read_dir(temp.path())
                        .unwrap()
                        .map(|e| e.unwrap().path())
                        .find(|p| {
                            p.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(".eaa-")
                        })
                        .unwrap();
                    fs::write(task.join("unknown.txt"), b"UNKNOWN").unwrap();
                    if cancel_instead {
                        cancel.cancel();
                    } else {
                        return Err(io::Error::from(io::ErrorKind::StorageFull));
                    }
                }
                Ok(())
            };
            let error = split_file(
                &request(&input),
                &mut TaskContext {
                    cancellation: token,
                    io_fault: Some(&mut fault),
                    ..TaskContext::default()
                },
            )
            .unwrap_err();
            assert_eq!(
                error.code,
                if cancel_instead {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::InsufficientDiskSpace
                }
            );
            let tasks = inspect_transactions(temp.path()).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].task_id, error.recovery_task_id);
            let task = &tasks[0].directory;
            assert_eq!(fs::read(task.join("unknown.txt")).unwrap(), b"UNKNOWN");
            assert_eq!(fs::read(&input).unwrap(), b"hello");
            assert_eq!(fs::read(temp.path().join("keep.txt")).unwrap(), b"KEEP");
            assert!(!temp.path().join("sample.zip.001").exists());
            let before = inventory(temp.path());
            let result = plan_restart_task(task, &mut TaskContext::default());
            assert_eq!(before, inventory(temp.path()));
            let viable = matches!(
                point,
                IoPoint::StartSync
                    | IoPoint::BeforeStartLink
                    | IoPoint::AfterStartLink
                    | IoPoint::JournalWrite
                    | IoPoint::JournalSync
            );
            assert_eq!(result.is_ok(), viable, "unexpected restart for {point:?}");
            if viable {
                restart_task(task, &mut TaskContext::default()).unwrap();
                assert_eq!(fs::read(task.join("unknown.txt")).unwrap(), b"UNKNOWN");
            }
        }
    }
}

#[test]
fn torn_start_draft_is_preserved_and_cannot_bypass_start_validation() {
    let temp = tempfile::tempdir().unwrap();
    let task = faulted(temp.path(), IoPoint::BeforeStartLink);
    fs::write(task.join("start-draft.json"), b"{\"schema_version\":1").unwrap();
    let before = inventory(temp.path());
    assert!(restart_task(&task, &mut TaskContext::default()).is_err());
    assert_eq!(before, inventory(temp.path()));
}

#[test]
fn plan_write_and_flush_failures_can_restart_without_replacing_old_drafts() {
    for point in [
        IoPoint::RecoveryPlanWrite,
        IoPoint::RecoveryPlanSync,
        IoPoint::BeforeRecoveryPlanLink,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let task = faulted(temp.path(), point);
        let before = inventory(&task);
        assert!(!task.join("recovery.json").exists());
        restart_task(&task, &mut TaskContext::default()).unwrap();
        for (path, bytes) in before {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn marker_write_and_flush_failures_keep_all_payloads_and_recover() {
    for point in [
        IoPoint::CommitMarkerWrite,
        IoPoint::CommitMarkerSync,
        IoPoint::AfterCommitMarker,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let task = faulted(temp.path(), point);
        assert!(task.join("payload-00001").exists());
        let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(receipt.staged_payload_links_scheduled_for_removal, 0);
        assert!(
            task.join("payload-00001").exists(),
            "recovery removed persisted staging"
        );
        verify_group(
            receipt.manifest_path.as_ref().unwrap(),
            None,
            &mut TaskContext::default(),
        )
        .unwrap();
    }
}

#[test]
fn journal_failure_after_a_link_is_published_does_not_require_a_published_event() {
    let temp = tempfile::tempdir().unwrap();
    let input = sample(temp.path());
    let mut writes = 0;
    let mut fault = |point| {
        if point == IoPoint::JournalWrite {
            writes += 1;
            if writes == 4 {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
        }
        Ok(())
    };
    split_file(
        &request(&input),
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert!(temp.path().join("sample.zip.001").exists());
    assert!(!temp.path().join("sample.zip.afparts.json").exists());
    let task = inspect_transactions(temp.path())
        .unwrap()
        .remove(0)
        .directory;
    recover_transaction(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(fs::read(input).unwrap(), b"hello");
}

#[cfg(windows)]
#[test]
fn live_cleanup_removes_only_this_attempt_and_completed_recovery_reads_real_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let task = faulted(temp.path(), IoPoint::PayloadSync);
    let old = inventory(&task);
    let mut fault = |point| {
        if point == IoPoint::BeforeCommitMarker {
            fs::write(task.join("payload-99999"), b"UNKNOWN").unwrap();
        }
        Ok(())
    };
    let receipt = restart_task(
        &task,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap();
    assert_eq!(receipt.staged_payload_links_scheduled_for_removal, 4);
    for (path, bytes) in old {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(fs::read(task.join("payload-99999")).unwrap(), b"UNKNOWN");
    let plan: serde_json::Value =
        serde_json::from_slice(&fs::read(task.join("recovery.json")).unwrap()).unwrap();
    for artifact in plan["artifacts"].as_array().unwrap() {
        assert!(!task.join(artifact["stage_name"].as_str().unwrap()).exists());
    }
    let before = inventory(temp.path());
    plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
    recover_transaction(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(before, inventory(temp.path()));
    assert_eq!(fs::read(temp.path().join("sample.zip")).unwrap(), b"hello");
}

#[cfg(windows)]
#[test]
fn cleanup_failure_is_nonfatal_and_mixed_retention_reverifies_without_deletion() {
    for point in [IoPoint::BeforeCleanupLink, IoPoint::AfterCleanupLink] {
        let temp = tempfile::tempdir().unwrap();
        let input = sample(temp.path());
        let mut fault = |current| {
            if current == point {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        };
        let receipt = split_file(
            &request(&input),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            receipt.staged_payload_links_scheduled_for_removal,
            if point == IoPoint::BeforeCleanupLink {
                0
            } else {
                1
            }
        );
        let before = inventory(temp.path());
        recover_transaction(&receipt.staging_directory, &mut TaskContext::default()).unwrap();
        assert_eq!(before, inventory(temp.path()));
    }
}

#[cfg(windows)]
#[test]
#[ignore = "explicitly launched by lifecycle process-kill test"]
fn lifecycle_crash_worker() {
    let root = PathBuf::from(std::env::var_os("EAA_LIFECYCLE_ROOT").unwrap());
    let name = std::env::var("EAA_LIFECYCLE_POINT").unwrap();
    let target = if name == "draft" {
        IoPoint::BeforeStartLink
    } else {
        IoPoint::AfterCleanupLink
    };
    let mut fault = |point| {
        if point == target {
            fs::write(root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Ok(())
    };
    split_file(
        &request(&root.join("sample.zip")),
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap();
}

#[cfg(windows)]
#[test]
fn killed_process_before_start_publication_or_during_cleanup_preserves_recoverability() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for point in ["draft", "cleanup"] {
        let temp = tempfile::tempdir().unwrap();
        sample(temp.path());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "lifecycle_crash_worker"])
            .env("EAA_LIFECYCLE_ROOT", temp.path())
            .env("EAA_LIFECYCLE_POINT", point)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !temp.path().join("ready").exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                panic!("worker exited early");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let reached = temp.path().join("ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(reached);
        let task = inspect_transactions(temp.path())
            .unwrap()
            .remove(0)
            .directory;
        let receipt = if point == "draft" {
            restart_task(&task, &mut TaskContext::default()).unwrap()
        } else {
            recover_transaction(&task, &mut TaskContext::default()).unwrap()
        };
        assert_eq!(
            receipt
                .output_files
                .iter()
                .flat_map(|p| fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            b"hello"
        );
        assert_eq!(fs::read(temp.path().join("keep.txt")).unwrap(), b"KEEP");
    }
}
