use easy_archive_core::{
    CancellationToken, ErrorCode, IoPoint, MergeRequest, Phase, SplitPolicy, SplitRequest,
    TaskContext, merge_file, parse_manifest,
    recovery::recover_transaction,
    restart::{plan_restart_task, restart_task},
    split_file,
    transaction::inspect_transactions,
    verify_group,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn request(path: &Path) -> SplitRequest {
    SplitRequest {
        input: path.to_owned(),
        output_dir: None,
        basename: None,
        policy: SplitPolicy::Hard { max_bytes: 2 },
        alignment: None,
        allow_any_file: false,
    }
}
fn inventory(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(inventory(&path));
        } else {
            out.push((path.clone(), fs::read(path).unwrap()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
fn partial_split(root: &Path) -> PathBuf {
    let input = root.join("sample.zip");
    fs::write(&input, b"hello").unwrap();
    let mut writes = 0;
    let mut fault = |point| {
        if point == IoPoint::PayloadWrite {
            writes += 1;
            if writes == 2 {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
        }
        Ok(())
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
    let task = inspect_transactions(root).unwrap().remove(0);
    assert!(task.restart_record_present);
    assert!(!task.recovery_plan_present);
    assert_eq!(
        fs::read(task.directory.join("payload-00001")).unwrap(),
        b"he"
    );
    task.directory
}

#[test]
fn restart_reuses_the_locked_task_with_new_attempt_files_and_no_old_file_changes() {
    let temp = tempfile::tempdir().unwrap();
    let task = partial_split(temp.path());
    fs::write(task.join("unknown-user.txt"), b"KEEP").unwrap();
    let before = inventory(temp.path());
    let plan = plan_restart_task(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(plan.operation, "split");
    assert!(!plan.content_read);
    assert!(!plan.writes_performed);
    assert_eq!(before, inventory(temp.path()));
    let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(receipt.transaction_id, plan.transaction_id);
    assert_eq!(inspect_transactions(temp.path()).unwrap().len(), 1);
    assert_eq!(
        receipt
            .output_files
            .iter()
            .flat_map(|p| fs::read(p).unwrap())
            .collect::<Vec<_>>(),
        b"hello"
    );
    for (path, bytes) in before {
        assert_eq!(fs::read(path).unwrap(), bytes, "old file changed");
    }
    assert!(fs::read_dir(&task).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("attempt-")
    }));
    verify_group(
        receipt.manifest_path.as_ref().unwrap(),
        None,
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(
        plan_restart_task(&task, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
}

#[test]
fn repeated_cancelled_restarts_keep_previous_attempts_and_can_finish() {
    let temp = tempfile::tempdir().unwrap();
    let task = partial_split(temp.path());
    for _ in 0..2 {
        let token = CancellationToken::default();
        let cancel = token.clone();
        let mut progress = move |p: easy_archive_core::Progress| {
            if p.phase == Phase::Writing && p.completed_bytes > 0 {
                cancel.cancel();
            }
        };
        let error = restart_task(
            &task,
            &mut TaskContext {
                cancellation: token,
                progress: Some(&mut progress),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert!(!temp.path().join("sample.zip.001").exists());
    }
    let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"he");
    assert_eq!(fs::read(temp.path().join("sample.zip")).unwrap(), b"hello");
}

#[test]
fn restarted_writing_can_switch_to_publication_recovery_after_another_failure() {
    let temp = tempfile::tempdir().unwrap();
    let task = partial_split(temp.path());
    let mut fault = |point| {
        if point == IoPoint::AfterPublish {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    };
    restart_task(
        &task,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert_eq!(
        plan_restart_task(&task, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
    let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(
        receipt
            .output_files
            .iter()
            .flat_map(|p| fs::read(p).unwrap())
            .collect::<Vec<_>>(),
        b"hello"
    );
    assert_eq!(fs::read(task.join("payload-00001")).unwrap(), b"he");
}

#[test]
fn restart_refuses_changed_sources_output_conflicts_other_tasks_and_unsafe_records() {
    for mutation in [
        "source",
        "output",
        "other-task",
        "record",
        "directory",
        "request",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let task = partial_split(temp.path());
        match mutation {
            "source" => fs::write(temp.path().join("sample.zip"), b"HELLO").unwrap(),
            "output" => fs::write(temp.path().join("sample.zip.001"), b"KEEP").unwrap(),
            "other-task" => {
                fs::create_dir(temp.path().join(".eaa-00000000000000000000000000000001")).unwrap()
            }
            _ => {
                let path = task.join("start.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                match mutation {
                    "record" => value["names"][0] = "../escape".into(),
                    "directory" => value["stage_identity"] = serde_json::json!([0, 0]),
                    _ => {
                        value["operation"]["request"]["input"] =
                            temp.path().join("different.zip").to_str().unwrap().into()
                    }
                }
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        let before = inventory(temp.path());
        assert!(
            restart_task(&task, &mut TaskContext::default()).is_err(),
            "accepted {mutation}"
        );
        assert_eq!(
            before,
            inventory(temp.path()),
            "wrote on {mutation} rejection"
        );
    }
}

#[test]
fn restart_of_merge_rechecks_all_parts_and_preserves_verification_level() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("sample.zip");
    fs::write(&input, b"hello").unwrap();
    let split = split_file(&request(&input), &mut TaskContext::default()).unwrap();
    let request = MergeRequest {
        part_paths: Vec::new(),
        input: split.output_files[0].clone(),
        output_dir: None,
        basename: Some("restored.zip".to_owned()),
        expected_sha256: None,
    };
    let mut fault = |point| {
        if point == IoPoint::PayloadSync {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        } else {
            Ok(())
        }
    };
    let error = merge_file(
        &request,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    let task = inspect_transactions(temp.path())
        .unwrap()
        .into_iter()
        .find(|t| t.task_id == error.recovery_task_id)
        .unwrap()
        .directory;
    let before = inventory(&task);
    let plan = plan_restart_task(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(plan.operation, "merge");
    let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(
        receipt.verification_level,
        "verified_against_available_sources"
    );
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"hello");
    for (path, bytes) in before {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn restart_keeps_empty_unicode_balanced_policy_and_separate_output_directory() {
    for bytes in [&b""[..], &b"hello"[..]] {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let input = temp.path().join("加密 样本.7z");
        fs::write(&input, bytes).unwrap();
        let mut req = request(&input);
        req.output_dir = Some(output.clone());
        req.basename = Some("new.7z".to_owned());
        req.policy = SplitPolicy::Balanced {
            target_bytes: 2,
            tolerance_ppm: 50_000,
            hard_max_bytes: None,
        };
        let mut fault = |point| {
            if point == IoPoint::PayloadSync {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        };
        split_file(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        let task = inspect_transactions(&output).unwrap().remove(0).directory;
        let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
        let manifest = parse_manifest(&fs::read(receipt.manifest_path.unwrap()).unwrap()).unwrap();
        assert_eq!(manifest.split_mode, "balanced");
        assert_eq!(manifest.original_filename, "加密 样本.7z");
        assert_eq!(
            receipt
                .output_files
                .iter()
                .flat_map(|p| fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            bytes
        );
    }
}

#[test]
fn live_transaction_cannot_be_restarted() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("sample.zip");
    fs::write(&input, b"hello").unwrap();
    let mut checked = false;
    let mut fault = |point| {
        if point == IoPoint::WritingIntentDurable {
            let task = inspect_transactions(temp.path())
                .unwrap()
                .remove(0)
                .directory;
            assert!(restart_task(&task, &mut TaskContext::default()).is_err());
            checked = true;
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
    .unwrap();
    assert!(checked);
}

#[test]
#[ignore = "worker explicitly launched by process-kill test"]
fn restart_crash_worker() {
    let root = PathBuf::from(std::env::var_os("EAA_RESTART_ROOT").unwrap());
    let point = std::env::var("EAA_RESTART_POINT").unwrap();
    let mut writes = 0;
    let mut fault = |current| {
        if current == IoPoint::PayloadSync {
            writes += 1;
        }
        if (point == "start" && current == IoPoint::WritingIntentDurable)
            || (point == "copy" && current == IoPoint::PayloadSync && writes == 1)
        {
            fs::write(root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(50));
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

#[test]
fn killed_process_before_and_during_copy_can_restart_without_hiding_the_task() {
    for point in ["start", "copy"] {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("sample.zip"), b"hello").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "restart_crash_worker"])
            .env("EAA_RESTART_ROOT", temp.path())
            .env("EAA_RESTART_POINT", point)
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
        let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(
            receipt
                .output_files
                .iter()
                .flat_map(|p| fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            b"hello"
        );
    }
}
