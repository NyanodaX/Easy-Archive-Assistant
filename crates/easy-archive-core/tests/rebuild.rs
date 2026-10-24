use easy_archive_core::{
    CancellationToken, ErrorCode, IoPoint, Phase, Progress, RebuildPartRequest, SplitPolicy,
    SplitRequest, TaskContext, parse_manifest, plan_rebuild_part, rebuild_part,
    recovery::{plan_recover_transaction, recover_transaction},
    restart::{plan_restart_task, restart_task},
    split_file,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

fn setup(root: &Path, bytes: &[u8]) -> RebuildPartRequest {
    let original = root.join("原件.zip");
    fs::write(&original, bytes).unwrap();
    let split = split_file(
        &SplitRequest {
            input: original.clone(),
            output_dir: None,
            basename: None,
            policy: SplitPolicy::Hard { max_bytes: 4 },
            alignment: None,
            allow_any_file: false,
        },
        &mut TaskContext::default(),
    )
    .unwrap();
    let output = root.join("重建");
    fs::create_dir(&output).unwrap();
    RebuildPartRequest {
        original,
        manifest: split.manifest_path.unwrap(),
        part: 1,
        output_dir: Some(output),
        basename: None,
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
fn unchanged(before: &[(PathBuf, Vec<u8>)]) {
    for (path, bytes) in before {
        assert_eq!(&fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn rebuilds_only_the_exact_empty_first_middle_or_last_range_without_touching_old_files() {
    for bytes in [b"".as_slice(), b"a", b"abcdefghijklmn", b"abcdefghijk"] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), bytes);
        let manifest = parse_manifest(&fs::read(&req.manifest).unwrap()).unwrap();
        for part in &manifest.parts {
            req.part = part.index;
            let before = inventory(temp.path());
            let plan = plan_rebuild_part(&req).unwrap();
            assert_eq!(plan.space.estimated_payload_bytes, part.size);
            assert_eq!(plan.verification_scope, "target_range_only");
            assert_eq!(inventory(temp.path()), before);
            let receipt = rebuild_part(&req, &mut TaskContext::default()).unwrap();
            assert_eq!(
                fs::read(&receipt.output_files[0]).unwrap(),
                bytes[part.offset as usize..(part.offset + part.size) as usize]
            );
            assert_eq!(receipt.sha256, part.sha256);
            assert_eq!(receipt.part_count, 1);
            assert_eq!(
                receipt.verification_level,
                "verified_against_available_sources"
            );
            assert!(receipt.manifest_path.is_none());
            unchanged(&before);
            let recovered =
                recover_transaction(&receipt.staging_directory, &mut TaskContext::default())
                    .unwrap();
            assert_eq!(recovered.part_count, 1);
            assert_eq!(recovered.verification_scope, "target_range_only");
            assert_eq!(recovered.target_range, Some(plan.range));
            assert!(recovered.manifest_path.is_none());
        }
    }
}

#[test]
fn does_not_require_other_parts_or_hash_unrelated_original_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let mut req = setup(temp.path(), b"abcdefghijklmn");
    req.part = 3;
    let mut manifest = parse_manifest(&fs::read(&req.manifest).unwrap()).unwrap();
    // Inconsistent historical whole/other-part digests cannot authorize a whole-file claim.
    manifest.original_sha256 = "0".repeat(64);
    manifest.parts[0].sha256 = "1".repeat(64);
    fs::write(&req.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
    for part in &manifest.parts {
        fs::remove_file(temp.path().join(&part.filename)).unwrap();
    }
    let receipt = rebuild_part(&req, &mut TaskContext::default()).unwrap();
    let part = &manifest.parts[2];
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"ijk");
    assert_eq!(receipt.source_size, part.size);
    assert_eq!(
        receipt.verification_level,
        "verified_against_available_sources"
    );
}

#[test]
fn invalid_part_size_hash_manifest_and_conflicts_refuse_before_staging() {
    for case in [
        "zero", "index", "size", "hash", "manifest", "existing", "unsafe",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), b"abcdefghijklmn");
        let expected = match case {
            "zero" => {
                req.part = 0;
                ErrorCode::InvalidArgument
            }
            "index" => {
                req.part = 10001;
                ErrorCode::InvalidArgument
            }
            "size" => {
                fs::write(&req.original, b"bad").unwrap();
                ErrorCode::SourceSizeMismatch
            }
            "hash" => {
                fs::write(&req.original, b"BAD!efghijklmn").unwrap();
                ErrorCode::PartHashMismatch
            }
            "manifest" => {
                fs::write(&req.manifest, b"{}").unwrap();
                ErrorCode::ManifestInvalid
            }
            "existing" => {
                fs::write(
                    req.output_dir.as_ref().unwrap().join("原件.zip.001"),
                    b"KEEP",
                )
                .unwrap();
                ErrorCode::OutputExists
            }
            _ => {
                req.basename = Some("../escape".into());
                ErrorCode::UnsafePath
            }
        };
        let before = inventory(temp.path());
        assert_eq!(
            rebuild_part(&req, &mut TaskContext::default())
                .unwrap_err()
                .code,
            expected,
            "{case}"
        );
        assert_eq!(inventory(temp.path()), before);
    }
}

#[test]
fn cancellation_at_every_phase_preserves_original_manifest_and_user_files() {
    for (phase, after_copy) in [
        (Phase::Verifying, false),
        (Phase::Writing, false),
        (Phase::Verifying, true),
        (Phase::CheckingSources, true),
        (Phase::Publishing, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmn");
        let before = inventory(temp.path());
        let token = CancellationToken::default();
        let cancel = token.clone();
        let mut writing = false;
        let mut progress = |p: Progress| {
            writing |= p.phase == Phase::Writing;
            if p.phase == phase && (!after_copy || writing) {
                cancel.cancel();
            }
        };
        assert_eq!(
            rebuild_part(
                &req,
                &mut TaskContext {
                    cancellation: token,
                    progress: Some(&mut progress),
                    ..TaskContext::default()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::Cancelled
        );
        unchanged(&before);
        assert!(
            !req.output_dir
                .as_ref()
                .unwrap()
                .join("原件.zip.001")
                .exists()
        );
    }
}

#[test]
fn disk_full_restarts_with_range_and_manifest_guards_and_preserves_old_attempts() {
    for point in [IoPoint::PayloadWrite, IoPoint::PayloadSync] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), b"abcdefghijklmn");
        req.part = 3;
        let mut fault = |p| {
            if p == point {
                return Err(io::ErrorKind::StorageFull.into());
            }
            Ok(())
        };
        let error = rebuild_part(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InsufficientDiskSpace);
        let task = req
            .output_dir
            .as_ref()
            .unwrap()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        fs::write(task.join("unknown.txt"), b"KEEP").unwrap();
        let before = inventory(temp.path());
        let plan = plan_restart_task(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(plan.operation, "rebuild-part");
        assert_eq!(plan.source_size, 3);
        assert_eq!(inventory(temp.path()), before);
        let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"ijk");
        unchanged(&before);
    }
}

#[test]
fn rebuild_range_publication_recovers_and_is_idempotent_after_cleanup() {
    for point in [
        IoPoint::PlanDurable,
        IoPoint::AfterPublish,
        IoPoint::BeforeCommitMarker,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), b"abcdefghijklmn");
        req.part = 3;
        let mut fault = |p| {
            if p == point {
                return Err(io::ErrorKind::Other.into());
            }
            Ok(())
        };
        let error = rebuild_part(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        let task = req
            .output_dir
            .as_ref()
            .unwrap()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        let before = inventory(temp.path());
        plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(inventory(temp.path()), before);
        let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"ijk");
        unchanged(&before);
        let before = inventory(temp.path());
        recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(inventory(temp.path()), before);
    }
}

#[test]
fn recovery_rejects_a_forged_offset_even_when_other_source_bytes_have_the_same_hash() {
    let temp = tempfile::tempdir().unwrap();
    let mut req = setup(temp.path(), b"abcabcabc");
    req.part = 3;
    let mut fault = |p| {
        if p == IoPoint::PlanDurable {
            return Err(io::ErrorKind::Other.into());
        }
        Ok(())
    };
    let error = rebuild_part(
        &req,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    let task = req
        .output_dir
        .as_ref()
        .unwrap()
        .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
    let path = task.join("recovery.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    plan["rebuild"]["range"] = serde_json::json!({"start":0,"end_exclusive":3});
    fs::write(path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let before = inventory(temp.path());
    assert_eq!(
        recover_transaction(&task, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn original_or_manifest_changes_refuse_restart_without_creating_new_attempts() {
    for change_manifest in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmn");
        let mut fault = |p| {
            if p == IoPoint::PayloadWrite {
                return Err(io::ErrorKind::StorageFull.into());
            }
            Ok(())
        };
        let error = rebuild_part(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        let task = req
            .output_dir
            .as_ref()
            .unwrap()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        if change_manifest {
            fs::write(&req.manifest, b"{}").unwrap();
        } else {
            fs::write(&req.original, b"BAD").unwrap();
        }
        let before = inventory(temp.path());
        assert!(restart_task(&task, &mut TaskContext::default()).is_err());
        assert_eq!(inventory(temp.path()), before);
    }
}

#[test]
#[ignore = "controlled child launched by killed_rebuild_copy_or_publication_recovers"]
fn rebuild_crash_worker() {
    use std::time::Duration;
    let root = PathBuf::from(std::env::var_os("EAA_REBUILD_ROOT").expect("test root"));
    let request: RebuildPartRequest =
        serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
    let point = std::env::var("EAA_REBUILD_POINT").unwrap();
    let mut fault = |p| {
        if (point == "copy" && p == IoPoint::PayloadSync)
            || (point == "publish" && p == IoPoint::PlanDurable)
        {
            fs::write(root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(())
    };
    rebuild_part(
        &request,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap();
}

#[test]
fn killed_rebuild_copy_or_publication_recovers() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for point in ["copy", "publish"] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), b"abcdefghijklmn");
        req.part = 3;
        fs::write(
            temp.path().join("request.json"),
            serde_json::to_vec(&req).unwrap(),
        )
        .unwrap();
        let before = inventory(temp.path());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "rebuild_crash_worker"])
            .env("EAA_REBUILD_ROOT", temp.path())
            .env("EAA_REBUILD_POINT", point)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !temp.path().join("ready").exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                panic!("worker exited before checkpoint");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = temp.path().join("ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready);
        let task =
            easy_archive_core::transaction::inspect_transactions(req.output_dir.as_ref().unwrap())
                .unwrap()
                .remove(0)
                .directory;
        let receipt = if point == "copy" {
            restart_task(&task, &mut TaskContext::default()).unwrap()
        } else {
            recover_transaction(&task, &mut TaskContext::default()).unwrap()
        };
        assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"ijk");
        unchanged(&before);
    }
}

#[test]
#[ignore = "manual large-offset acceptance: >4 GiB logical original, only 17 target bytes read per pass"]
fn large_original_offset_reads_only_target_range() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("large.zip");
    let offset = 1_u64 << 32;
    let bytes = [0_u8; 17];
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&original)
        .unwrap()
        .set_len(offset + 17)
        .unwrap();
    let manifest = easy_archive_core::Manifest {
        schema_version: 1,
        tool: "test".into(),
        original_filename: "large.zip".into(),
        original_size: offset + 17,
        original_sha256: "0".repeat(64),
        split_mode: "hard".into(),
        part_count: 2,
        parts: vec![
            easy_archive_core::PartDescriptor {
                index: 1,
                filename: "large.zip.001".into(),
                offset: 0,
                size: offset,
                sha256: "0".repeat(64),
            },
            easy_archive_core::PartDescriptor {
                index: 2,
                filename: "large.zip.002".into(),
                offset,
                size: 17,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            },
        ],
        created_at: "2026-10-24T00:00:00Z".into(),
        app_version: "test".into(),
    };
    let path = temp.path().join("large.zip.afparts.json");
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let req = RebuildPartRequest {
        original,
        manifest: path,
        part: 2,
        output_dir: None,
        basename: None,
    };
    let plan = plan_rebuild_part(&req).unwrap();
    assert_eq!(plan.range.start, offset);
    assert_eq!(plan.space.estimated_payload_bytes, 17);
    let mut reads = 0;
    let mut fault = |p| {
        if p == IoPoint::Read {
            reads += 1;
        }
        Ok(())
    };
    let receipt = rebuild_part(
        &req,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap();
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), bytes);
    assert!(reads <= 16, "unexpected full-file reading");
    assert_eq!(
        receipt.verification_level,
        "verified_against_available_sources"
    );
}
