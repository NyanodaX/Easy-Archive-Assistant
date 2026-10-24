use easy_archive_core::{
    ErrorCode, IoPoint, MergeRequest, SplitPolicy, SplitRequest, TaskContext, merge_file,
    recovery::{plan_recover_transaction, recover_transaction},
    space::SpaceEstimate,
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
    fs::write(root.join("unrelated.txt"), b"KEEP").unwrap();
    path
}
fn failed_split(root: &Path, at: IoPoint) -> PathBuf {
    let path = sample(root);
    let mut fault = |point| {
        if point == at {
            Err(io::Error::from(io::ErrorKind::StorageFull))
        } else {
            Ok(())
        }
    };
    let error = split_file(
        &request(&path),
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InsufficientDiskSpace);
    assert!(error.recovery_task_id.is_some());
    assert_eq!(fs::read(path).unwrap(), b"hello");
    assert_eq!(fs::read(root.join("unrelated.txt")).unwrap(), b"KEEP");
    inspect_transactions(root).unwrap().remove(0).directory
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

#[test]
fn failing_read_write_and_flush_preserve_inputs_and_incomplete_staging() {
    for point in [IoPoint::Read, IoPoint::PayloadWrite, IoPoint::PayloadSync] {
        let temp = tempfile::tempdir().unwrap();
        let task = failed_split(temp.path(), point);
        assert!(!temp.path().join("sample.zip.001").exists());
        assert!(!task.join("recovery.json").exists());
        let before = inventory(temp.path());
        assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
        assert_eq!(before, inventory(temp.path()));
    }
}

#[test]
fn publication_boundary_failures_resume_idempotently_after_full_validation() {
    for point in [
        IoPoint::PlanDurable,
        IoPoint::BeforePublish,
        IoPoint::AfterPublish,
        IoPoint::BeforeCommitMarker,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let task = failed_split(temp.path(), point);
        assert!(task.join("recovery.json").is_file());
        let before = inventory(temp.path());
        let plan = plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(
            plan.verification_level,
            "verified_against_available_sources"
        );
        assert_eq!(before, inventory(temp.path()), "dry-run performed a write");
        let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(receipt.transaction_id, plan.transaction_id);
        let assembled: Vec<_> = receipt
            .output_files
            .iter()
            .flat_map(|p| fs::read(p).unwrap())
            .collect();
        assert_eq!(assembled, b"hello");
        let after = inventory(temp.path());
        recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(after, inventory(temp.path()), "repeat recovery wrote files");
        assert_eq!(
            inspect_transactions(temp.path()).unwrap()[0].status,
            "committed"
        );
        verify_group(
            receipt.manifest_path.as_ref().unwrap(),
            None,
            &mut TaskContext::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(temp.path().join("unrelated.txt")).unwrap(),
            b"KEEP"
        );
    }
}

#[test]
fn recovery_refuses_unknown_same_bytes_file_instead_of_claiming_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let task = failed_split(temp.path(), IoPoint::PlanDurable);
    fs::write(temp.path().join("sample.zip.002"), b"ll").unwrap();
    let before = inventory(temp.path());
    let error = recover_transaction(&task, &mut TaskContext::default()).unwrap_err();
    assert_eq!(error.code, ErrorCode::OutputExists);
    assert_eq!(before, inventory(temp.path()));
    assert!(!temp.path().join("sample.zip.001").exists());
}

#[test]
fn recovery_refuses_changed_sources_and_changed_staged_payloads() {
    for change_source in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let task = failed_split(temp.path(), IoPoint::PlanDurable);
        let path = if change_source {
            temp.path().join("sample.zip")
        } else {
            task.join("payload-00001")
        };
        fs::write(
            &path,
            if change_source {
                &b"HELLO"[..]
            } else {
                &b"XX"[..]
            },
        )
        .unwrap();
        let before = inventory(temp.path());
        assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
        assert_eq!(before, inventory(temp.path()));
        assert!(!temp.path().join("sample.zip.001").exists());
    }
}

#[test]
fn live_transaction_lock_prevents_a_second_publisher() {
    let temp = tempfile::tempdir().unwrap();
    let input = sample(temp.path());
    let mut checked = false;
    let mut fault = |point| {
        if point == IoPoint::PlanDurable {
            let task = inspect_transactions(temp.path())
                .unwrap()
                .remove(0)
                .directory;
            assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
            assert!(!temp.path().join("sample.zip.001").exists());
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
fn matching_recorded_part_hashes_do_not_replace_comparison_with_actual_sources() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let task = failed_split(temp.path(), IoPoint::PlanDurable);
    let path = task.join("recovery.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(task.join("payload-00001"), b"HE").unwrap();
    let changed_hash = format!("{:x}", Sha256::digest(b"HE"));
    plan["artifacts"][0]["sha256"] = changed_hash.clone().into();
    plan["artifacts"][0]["snapshot"]["modified"] = serde_json::to_value(
        fs::metadata(task.join("payload-00001"))
            .unwrap()
            .modified()
            .unwrap(),
    )
    .unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(task.join("payload-manifest")).unwrap()).unwrap();
    manifest["parts"][0]["sha256"] = changed_hash.into();
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(task.join("payload-manifest"), &bytes).unwrap();
    plan["artifacts"][3]["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
    plan["artifacts"][3]["size"] = (bytes.len() as u64).into();
    plan["artifacts"][3]["snapshot"]["size"] = (bytes.len() as u64).into();
    plan["artifacts"][3]["snapshot"]["modified"] = serde_json::to_value(
        fs::metadata(task.join("payload-manifest"))
            .unwrap()
            .modified()
            .unwrap(),
    )
    .unwrap();
    fs::write(path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let before = inventory(temp.path());
    assert_eq!(
        recover_transaction(&task, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::SourceHashMismatch
    );
    assert_eq!(before, inventory(temp.path()));
}

#[test]
fn corrupt_records_and_unsafe_names_never_write_or_escape_task_directory() {
    for mutation in ["truncated", "duplicate", "traversal", "count", "identity"] {
        let temp = tempfile::tempdir().unwrap();
        let task = failed_split(temp.path(), IoPoint::PlanDurable);
        let path = task.join("recovery.json");
        let mut plan: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match mutation {
            "traversal" => plan["artifacts"][0]["name"] = "../escape.zip".into(),
            "count" => plan["artifacts"] = serde_json::json!([]),
            "identity" => plan["target_identity"] = serde_json::json!([0, 0]),
            "duplicate" => plan["artifacts"][1]["name"] = plan["artifacts"][0]["name"].clone(),
            _ => (),
        }
        fs::write(
            &path,
            if mutation == "truncated" {
                b"{".to_vec()
            } else {
                serde_json::to_vec(&plan).unwrap()
            },
        )
        .unwrap();
        let before = inventory(temp.path());
        assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
        assert_eq!(before, inventory(temp.path()));
    }
}

#[test]
fn legacy_tasks_are_reported_but_never_reinterpreted_for_writes() {
    let temp = tempfile::tempdir().unwrap();
    let task = temp
        .path()
        .join(".archiveflow-00000000000000000000000000000001");
    fs::create_dir(&task).unwrap();
    fs::write(task.join("owner.json"), b"{\"schema_version\":1}").unwrap();
    fs::write(task.join("unknown.txt"), b"KEEP").unwrap();
    let before = inventory(temp.path());
    let tasks = inspect_transactions(temp.path()).unwrap();
    assert_eq!(tasks[0].status, "needs_recovery");
    assert!(!tasks[0].recovery_plan_present);
    assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
    assert_eq!(before, inventory(temp.path()));
}

#[test]
fn recovery_of_a_merge_rechecks_each_source_volume() {
    let temp = tempfile::tempdir().unwrap();
    let input = sample(temp.path());
    let split = split_file(&request(&input), &mut TaskContext::default()).unwrap();
    let request = MergeRequest {
        part_paths: Vec::new(),
        input: split.output_files[0].clone(),
        output_dir: None,
        basename: Some("merged.zip".to_owned()),
        expected_sha256: None,
    };
    let mut fault = |point| {
        if point == IoPoint::AfterPublish {
            Err(io::Error::from(io::ErrorKind::Other))
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
    let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
    assert_eq!(receipt.part_count, 3);
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"hello");
}

#[test]
fn a_commit_marker_never_hides_a_missing_published_file() {
    let temp = tempfile::tempdir().unwrap();
    let input = sample(temp.path());
    let receipt = split_file(&request(&input), &mut TaskContext::default()).unwrap();
    // Remove only this test's known output to simulate a user moving a volume.
    fs::remove_file(&receipt.output_files[0]).unwrap();
    let error =
        recover_transaction(&receipt.staging_directory, &mut TaskContext::default()).unwrap_err();
    assert_eq!(error.code, ErrorCode::PartMissing);
}

#[test]
fn advisory_space_check_reports_shortfall_without_overflow_or_writes() {
    let estimate = SpaceEstimate {
        estimated_payload_bytes: 5,
        overhead_estimate_bytes: 1,
        required_space_bytes: 6,
        available_space_bytes: Some(5),
        free_space_checked: true,
        sufficient: Some(false),
    };
    assert_eq!(
        estimate.require_sufficient().unwrap_err().code,
        ErrorCode::InsufficientDiskSpace
    );
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        easy_archive_core::space::estimate_space(temp.path(), u64::MAX, 1)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "child process used only by killed_process_recovers..."]
fn crash_worker() {
    let root = PathBuf::from(std::env::var_os("EAA_TEST_CRASH_ROOT").expect("worker root"));
    let point_name = std::env::var("EAA_TEST_CRASH_POINT").unwrap();
    let point = match point_name.as_str() {
        "plan" => IoPoint::PlanDurable,
        "marker" => IoPoint::BeforeCommitMarker,
        _ => IoPoint::AfterPublish,
    };
    let input = root.join("sample.zip");
    let mut fault = |current| {
        if current == point {
            fs::write(root.join("worker-ready"), b"ready").unwrap();
            // The parent terminates us at this precise durable boundary.
            loop {
                std::thread::sleep(Duration::from_millis(50));
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
    .unwrap();
}

#[test]
fn killed_process_recovers_from_plan_link_and_pre_marker_boundaries() {
    for point in ["plan", "link", "marker"] {
        let temp = tempfile::tempdir().unwrap();
        sample(temp.path());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "crash_worker"])
            .env("EAA_TEST_CRASH_ROOT", temp.path())
            .env("EAA_TEST_CRASH_POINT", point)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !temp.path().join("worker-ready").exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                panic!("worker exited before fault boundary");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let reached = temp.path().join("worker-ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(reached, "worker did not reach {point}");
        let task = inspect_transactions(temp.path())
            .unwrap()
            .remove(0)
            .directory;
        let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(
            receipt
                .output_files
                .iter()
                .flat_map(|p| fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            b"hello"
        );
        assert_eq!(fs::read(temp.path().join("sample.zip")).unwrap(), b"hello");
        assert_eq!(
            fs::read(temp.path().join("unrelated.txt")).unwrap(),
            b"KEEP"
        );
    }
}
