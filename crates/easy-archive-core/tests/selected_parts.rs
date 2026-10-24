use easy_archive_core::{
    CancellationToken, ErrorCode, IoPoint, MergeRequest, ReshardRequest, SplitPolicy, SplitRequest,
    TaskContext, inspect_selected_group, merge_file, plan_merge_file,
    recovery::{plan_recover_transaction, recover_transaction},
    reshard_file,
    restart::{plan_restart_task, restart_task},
    split_file,
    transaction::inspect_transactions,
    verify_group, verify_selected_group,
};
use std::{fs, io, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    manifest: PathBuf,
    parts: Vec<PathBuf>,
    output: PathBuf,
    data: Vec<u8>,
}
fn fixture(data: &[u8]) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let seed = root.join("seed");
    let output = root.join("output");
    fs::create_dir(&seed).unwrap();
    fs::create_dir(&output).unwrap();
    let input = seed.join("多盘 sample.zip");
    fs::write(&input, data).unwrap();
    let receipt = split_file(
        &SplitRequest {
            input,
            output_dir: None,
            basename: None,
            policy: SplitPolicy::Hard { max_bytes: 3 },
            alignment: None,
            allow_any_file: false,
        },
        &mut TaskContext::default(),
    )
    .unwrap();
    let mut parts = Vec::new();
    for (i, path) in receipt.output_files.iter().enumerate() {
        let directory = root.join(format!("USB_{i}"));
        fs::create_dir(&directory).unwrap();
        let selected = directory.join(path.file_name().unwrap());
        fs::copy(path, &selected).unwrap();
        parts.push(selected);
    }
    Fixture {
        _temp: temp,
        root,
        manifest: receipt.manifest_path.unwrap(),
        parts,
        output,
        data: data.to_vec(),
    }
}
fn request(f: &Fixture) -> MergeRequest {
    let mut paths = f.parts.clone();
    paths.reverse();
    MergeRequest {
        input: f.manifest.clone(),
        part_paths: paths,
        output_dir: Some(f.output.clone()),
        basename: Some("restored.zip".into()),
        expected_sha256: None,
    }
}
fn intact(f: &Fixture) {
    let mut collected = Vec::new();
    for part in &f.parts {
        collected.extend(fs::read(part).unwrap());
    }
    assert_eq!(collected, f.data);
}
fn task(f: &Fixture) -> PathBuf {
    inspect_transactions(&f.output)
        .unwrap()
        .pop()
        .unwrap()
        .directory
}

#[test]
fn explicit_selection_does_not_adopt_an_adjacent_manifest() {
    let f = fixture(b"abcdefghi");
    let adjacent = f.parts[0]
        .parent()
        .unwrap()
        .join("多盘 sample.zip.afparts.json");
    fs::write(&adjacent, b"not a manifest").unwrap();
    assert_eq!(
        easy_archive_core::inspect_group(&f.parts[0])
            .unwrap_err()
            .code,
        ErrorCode::ManifestInvalid
    );
    let inspection = inspect_selected_group(&f.parts[0], &f.parts).unwrap();
    assert!(!inspection.has_manifest);
    assert_eq!(inspection.parts.len(), 3);
    let verified =
        verify_selected_group(&f.parts[0], &f.parts, None, &mut TaskContext::default()).unwrap();
    assert_eq!(
        verified.verification_level,
        "verified_against_available_sources"
    );
    intact(&f);
}

#[test]
fn shuffled_files_from_separate_directories_merge_verify_and_reshard() {
    for bytes in [b"".as_slice(), b"a", b"abcdefghi"] {
        let f = fixture(bytes);
        let req = request(&f);
        let plan = plan_merge_file(&req).unwrap();
        assert!(plan.group.explicit_selection);
        assert_eq!(
            plan.group
                .parts
                .iter()
                .map(|p| p.path.clone())
                .collect::<Vec<_>>(),
            f.parts
        );
        assert!(fs::read_dir(&f.output).unwrap().next().is_none());
        let verification = verify_selected_group(
            &req.input,
            &req.part_paths,
            None,
            &mut TaskContext::default(),
        )
        .unwrap();
        let result = merge_file(&req, &mut TaskContext::default()).unwrap();
        assert_eq!(result.sha256, verification.sha256);
        assert_eq!(fs::read(&result.output_files[0]).unwrap(), bytes);
        intact(&f);
        let sharded = reshard_file(
            &ReshardRequest {
                input: req.input,
                part_paths: req.part_paths,
                output_dir: Some(f.output.clone()),
                basename: "new.zip".into(),
                policy: SplitPolicy::Hard { max_bytes: 2 },
                alignment: None,
                expected_sha256: Some(result.sha256.clone()),
            },
            &mut TaskContext::default(),
        )
        .unwrap();
        assert_eq!(
            verify_group(
                sharded.manifest_path.as_ref().unwrap(),
                None,
                &mut TaskContext::default()
            )
            .unwrap()
            .sha256,
            result.sha256
        );
        intact(&f);
    }
}

#[test]
fn explicit_selection_does_not_fill_missing_parts_from_manifest_directory() {
    let f = fixture(b"abcdefghi");
    let mut req = request(&f);
    req.part_paths.pop();
    assert_eq!(
        plan_merge_file(&req).unwrap_err().code,
        ErrorCode::PartMissing
    );
    assert!(fs::read_dir(&f.output).unwrap().next().is_none());
    intact(&f);
}

#[test]
fn rejects_duplicate_mixed_noncanonical_and_unsafe_selections() {
    let f = fixture(b"abcdefghi");
    let mut duplicate = request(&f);
    duplicate.part_paths.push(f.parts[1].clone());
    assert_eq!(
        plan_merge_file(&duplicate).unwrap_err().code,
        ErrorCode::PartDuplicate
    );
    let duplicate_path = f.root.join("USB_0").join(f.parts[1].file_name().unwrap());
    fs::copy(&f.parts[1], &duplicate_path).unwrap();
    duplicate.part_paths.pop();
    duplicate.part_paths.push(duplicate_path);
    assert_eq!(
        plan_merge_file(&duplicate).unwrap_err().code,
        ErrorCode::PartDuplicate
    );
    let other = f.root.join("USB_0/other.zip.003");
    fs::write(&other, b"ghi").unwrap();
    let mut mixed = request(&f);
    mixed.part_paths[0] = other;
    assert_eq!(
        plan_merge_file(&mixed).unwrap_err().code,
        ErrorCode::PartGroupMismatch
    );
    let malformed = f.root.join("USB_0/多盘 sample.zip.01");
    fs::write(&malformed, b"abc").unwrap();
    mixed = request(&f);
    mixed.part_paths[2] = malformed;
    assert_eq!(
        plan_merge_file(&mixed).unwrap_err().code,
        ErrorCode::PartDuplicate
    );
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&f.manifest).unwrap()).unwrap();
    manifest["parts"][0]["filename"] = serde_json::json!("../escape.001");
    fs::write(&f.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert_eq!(
        plan_merge_file(&request(&f)).unwrap_err().code,
        ErrorCode::ManifestInvalid
    );
    assert!(fs::read_dir(&f.output).unwrap().next().is_none());
    intact(&f);
}

#[test]
fn requires_output_and_limits_selected_paths_and_directories() {
    let f = fixture(b"abcdefghi");
    let mut req = request(&f);
    req.output_dir = None;
    assert_eq!(
        plan_merge_file(&req).unwrap_err().code,
        ErrorCode::OutputDirectoryRequired
    );
    req = request(&f);
    req.part_paths = vec![f.parts[0].clone(); 10_001];
    assert_eq!(
        plan_merge_file(&req).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    req = request(&f);
    req.part_paths = (1..=65)
        .map(|i| {
            let dir = f.root.join(format!("many-{i}"));
            fs::create_dir(&dir).unwrap();
            let file = dir.join(format!("多盘 sample.zip.{i:03}"));
            fs::write(&file, b"x").unwrap();
            file
        })
        .collect();
    assert_eq!(
        plan_merge_file(&req).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    assert!(fs::read_dir(&f.output).unwrap().next().is_none());
    intact(&f);
}

#[test]
fn without_manifest_continuity_cannot_prove_a_missing_final_part() {
    let f = fixture(b"abcdefghi");
    let mut req = request(&f);
    req.input = f.parts[0].clone();
    let report = inspect_selected_group(&req.input, &req.part_paths).unwrap();
    assert!(!report.has_manifest);
    let complete = verify_selected_group(
        &req.input,
        &req.part_paths,
        None,
        &mut TaskContext::default(),
    )
    .unwrap();
    req.part_paths.pop(); // removes .001 in reversed order, then select the first two explicitly
    req.part_paths = f.parts[..2].to_vec();
    let limited = verify_selected_group(
        &req.input,
        &req.part_paths,
        None,
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(
        limited.verification_level,
        "verified_against_available_sources"
    );
    assert!(!limited.has_manifest);
    assert!(!limited.has_external_digest);
    req.expected_sha256 = Some(complete.sha256);
    assert_eq!(
        merge_file(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::SourceHashMismatch
    );
    assert!(fs::read_dir(&f.output).unwrap().next().is_none());
    intact(&f);
}

#[test]
fn corrupt_or_unavailable_source_stops_before_publication() {
    let f = fixture(b"abcdefghi");
    fs::write(&f.parts[1], b"BAD").unwrap();
    assert_eq!(
        merge_file(&request(&f), &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::PartHashMismatch
    );
    assert!(fs::read_dir(&f.output).unwrap().next().is_none());
    fs::write(&f.parts[1], b"def").unwrap();
    let moved = f.parts[2].with_extension("unavailable");
    let mut disconnected = false;
    let mut fault = |point| {
        if point == IoPoint::PayloadWrite && !disconnected {
            fs::rename(&f.parts[2], &moved).unwrap();
            disconnected = true;
        }
        Ok(())
    };
    assert!(
        merge_file(
            &request(&f),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            }
        )
        .is_err()
    );
    assert!(!f.output.join("restored.zip").exists());
    assert_eq!(fs::read(moved).unwrap(), b"ghi");
}

#[test]
fn cancellation_storage_failure_and_changed_output_preserve_sources() {
    for point in [IoPoint::PayloadWrite, IoPoint::PayloadSync] {
        let f = fixture(b"abcdefghi");
        let token = CancellationToken::default();
        let cancel = token.clone();
        let mut fault = |current| {
            if current == point {
                cancel.cancel();
            }
            Ok(())
        };
        assert_eq!(
            merge_file(
                &request(&f),
                &mut TaskContext {
                    cancellation: token,
                    io_fault: Some(&mut fault),
                    ..TaskContext::default()
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::Cancelled
        );
        intact(&f);
        assert!(!f.output.join("restored.zip").exists());
    }
    let f = fixture(b"abcdefghi");
    let mut fault = |point| {
        if point == IoPoint::PayloadWrite {
            Err(io::Error::from(io::ErrorKind::StorageFull))
        } else {
            Ok(())
        }
    };
    assert_eq!(
        merge_file(
            &request(&f),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::InsufficientDiskSpace
    );
    intact(&f);
    let f = fixture(b"abcdefghi");
    let req = request(&f);
    plan_merge_file(&req).unwrap();
    fs::write(f.output.join("restored.zip"), b"KEEP").unwrap();
    assert_eq!(
        merge_file(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::OutputExists
    );
    assert_eq!(fs::read(f.output.join("restored.zip")).unwrap(), b"KEEP");
    intact(&f);
}

#[test]
fn selected_path_snapshots_survive_restart_and_recover_idempotently() {
    for reshard in [false, true] {
        let f = fixture(b"abcdefghi");
        let req = request(&f);
        let shard = ReshardRequest {
            input: req.input.clone(),
            part_paths: req.part_paths.clone(),
            output_dir: req.output_dir.clone(),
            basename: "new.zip".into(),
            policy: SplitPolicy::Hard { max_bytes: 2 },
            alignment: None,
            expected_sha256: None,
        };
        let mut fault = |point| {
            if point == IoPoint::PayloadSync {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        };
        let mut ctx = TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        };
        assert!(
            if reshard {
                reshard_file(&shard, &mut ctx)
            } else {
                merge_file(&req, &mut ctx)
            }
            .is_err()
        );
        let task = task(&f);
        plan_restart_task(&task, &mut TaskContext::default()).unwrap();
        let result = restart_task(&task, &mut TaskContext::default()).unwrap();
        let recovered = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(recovered.sha256, result.sha256);
        assert_eq!(
            recover_transaction(&task, &mut TaskContext::default())
                .unwrap()
                .sha256,
            result.sha256
        );
        intact(&f);
        if !reshard {
            assert_eq!(fs::read(&result.output_files[0]).unwrap(), f.data);
        }
    }
    let f = fixture(b"abcdefghi");
    let mut fault = |point| {
        if point == IoPoint::BeforePublish {
            Err(io::Error::from(io::ErrorKind::Other))
        } else {
            Ok(())
        }
    };
    assert!(
        merge_file(
            &request(&f),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            }
        )
        .is_err()
    );
    let task = task(&f);
    plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
    fs::write(&f.parts[2], b"BAD").unwrap();
    assert!(recover_transaction(&task, &mut TaskContext::default()).is_err());
    assert!(!f.output.join("restored.zip").exists());
}
