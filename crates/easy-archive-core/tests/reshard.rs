use easy_archive_core::{
    CancellationToken, ErrorCode, IoPoint, Manifest, Phase, Progress, Receipt, ReshardRequest,
    SplitPolicy, SplitRequest, TaskContext, parse_manifest, plan_reshard_file,
    recovery::{plan_recover_transaction, recover_transaction},
    reshard_file,
    restart::{plan_restart_task, restart_task},
    split_file,
    transaction::inspect_transactions,
    verify_group,
};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn source(root: &Path, bytes: &[u8], size: u64) -> Receipt {
    let path = root.join("原始.7z");
    fs::write(&path, bytes).unwrap();
    split_file(
        &SplitRequest {
            input: path,
            output_dir: None,
            basename: None,
            policy: SplitPolicy::Hard { max_bytes: size },
            alignment: None,
            allow_any_file: false,
        },
        &mut TaskContext::default(),
    )
    .unwrap()
}
fn request(receipt: &Receipt, size: u64) -> ReshardRequest {
    ReshardRequest {
        part_paths: Vec::new(),
        input: receipt.manifest_path.clone().unwrap(),
        output_dir: None,
        basename: "新规格.7z".into(),
        policy: SplitPolicy::Hard { max_bytes: size },
        alignment: None,
        expected_sha256: None,
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
        assert_eq!(&fs::read(path).unwrap(), bytes, "old file changed");
    }
}
fn concatenated(receipt: &Receipt) -> Vec<u8> {
    receipt
        .output_files
        .iter()
        .flat_map(|p| fs::read(p).unwrap())
        .collect()
}

#[test]
fn direct_reshard_crosses_arbitrary_input_and_buffer_boundaries_and_preserves_old_group() {
    for (length, old_size, new_size) in [
        (0, 3, 5),
        (1, 3, 1),
        (19, 4, 3),
        (19, 3, 100),
        (19, 100, 3),
        (37, 8, 8),
        (
            8 * 1024 * 1024 + 23,
            3 * 1024 * 1024 + 7,
            5 * 1024 * 1024 + 11,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let bytes: Vec<_> = (0..length)
            .map(|i| ((i * 31 + i / 251) % 256) as u8)
            .collect();
        let original = source(temp.path(), &bytes, old_size);
        let before = inventory(temp.path());
        let mut req = request(&original, new_size);
        // Accept selecting any member, including a non-first part.
        req.input = original.output_files.last().unwrap().clone();
        let plan = plan_reshard_file(&req).unwrap();
        assert!(!plan.creates_complete_intermediate);
        assert_eq!(plan.space.estimated_payload_bytes, length);
        assert_eq!(inventory(temp.path()), before, "planning wrote files");
        let receipt = reshard_file(&req, &mut TaskContext::default()).unwrap();
        assert_eq!(concatenated(&receipt), bytes);
        assert_eq!(receipt.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            receipt.verification_level,
            "verified_against_available_sources"
        );
        let manifest =
            parse_manifest(&fs::read(receipt.manifest_path.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(manifest.original_filename, "原始.7z");
        assert_eq!(manifest.original_sha256, original.sha256);
        assert!(manifest.parts.iter().all(|p| p.size <= new_size));
        assert!(!temp.path().join("新规格.7z").exists());
        // Every staged payload is a new output volume or its manifest.
        for entry in fs::read_dir(&receipt.staging_directory).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("payload-") {
                assert!(
                    name == "payload-manifest"
                        || name
                            .strip_prefix("payload-")
                            .unwrap()
                            .bytes()
                            .all(|b| b.is_ascii_digit())
                );
            }
        }
        unchanged(&before);
        verify_group(
            receipt.manifest_path.as_ref().unwrap(),
            Some(&original.sha256),
            &mut TaskContext::default(),
        )
        .unwrap();
        // Completed recovery also works after live staging cleanup.
        let checked =
            plan_recover_transaction(&receipt.staging_directory, &mut TaskContext::default())
                .unwrap();
        assert_eq!(checked.part_count, receipt.part_count);
        assert_eq!(checked.manifest_path, receipt.manifest_path);
    }
}

#[test]
fn balanced_alignment_and_explicit_digest_keep_the_verification_level() {
    let temp = tempfile::tempdir().unwrap();
    let original = source(temp.path(), &[7; 107], 19);
    let out = temp.path().join("输出");
    fs::create_dir(&out).unwrap();
    let mut req = request(&original, 20);
    req.output_dir = Some(out);
    req.policy = SplitPolicy::Balanced {
        target_bytes: 20,
        tolerance_ppm: 100_000,
        hard_max_bytes: Some(22),
    };
    req.alignment = Some(2);
    req.expected_sha256 = Some(original.sha256.clone());
    let plan = plan_reshard_file(&req).unwrap();
    assert!(plan.plan.alignment_applied);
    let receipt = reshard_file(&req, &mut TaskContext::default()).unwrap();
    assert_eq!(receipt.part_count, 5);
    assert_eq!(receipt.verification_level, "verified_against_original");
    assert!(!receipt.provenance_authenticated);
    assert_eq!(concatenated(&receipt), [7; 107]);
}

#[test]
fn unmanifested_sources_are_rejected_even_with_an_external_digest() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("raw.zip.001");
    fs::write(&input, b"hello").unwrap();
    let req = ReshardRequest {
        part_paths: Vec::new(),
        input,
        output_dir: None,
        basename: "new.zip".into(),
        policy: SplitPolicy::Hard { max_bytes: 3 },
        alignment: None,
        expected_sha256: Some(format!("{:x}", Sha256::digest(b"hello"))),
    };
    let before = inventory(temp.path());
    assert_eq!(
        plan_reshard_file(&req).unwrap_err().code,
        ErrorCode::ManifestRequired
    );
    assert_eq!(
        reshard_file(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::ManifestRequired
    );
    assert_eq!(before, inventory(temp.path()));
}

#[test]
fn hash_mismatch_missing_parts_and_name_conflicts_stop_before_staging() {
    for case in [
        "hash", "external", "missing", "manifest", "existing", "same", "unsafe",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let original = source(temp.path(), b"abcdefghijk", 4);
        let mut req = request(&original, 3);
        let expected = match case {
            "hash" => {
                fs::write(&original.output_files[1], b"BAD!").unwrap();
                ErrorCode::PartHashMismatch
            }
            "external" => {
                req.expected_sha256 = Some("0".repeat(64));
                ErrorCode::SourceHashMismatch
            }
            "missing" => {
                fs::remove_file(&original.output_files[1]).unwrap();
                ErrorCode::PartMissing
            }
            "manifest" => {
                fs::write(&req.input, b"{}").unwrap();
                ErrorCode::ManifestInvalid
            }
            "existing" => {
                fs::write(temp.path().join("新规格.7z.002"), b"KEEP").unwrap();
                ErrorCode::OutputExists
            }
            "same" => {
                req.basename = "原始.7z".into();
                ErrorCode::OutputExists
            }
            _ => {
                req.basename = "../escape".into();
                ErrorCode::UnsafePath
            }
        };
        let before = inventory(temp.path());
        assert_eq!(
            reshard_file(&req, &mut TaskContext::default())
                .unwrap_err()
                .code,
            expected,
            "{case}"
        );
        assert_eq!(before, inventory(temp.path()));
    }
}

#[test]
fn cancelled_reshard_at_read_write_reread_and_publish_preserves_every_existing_file() {
    for (phase, after_writing) in [
        (Phase::Verifying, false),
        (Phase::Writing, false),
        (Phase::Verifying, true),
        (Phase::CheckingSources, true),
        (Phase::Publishing, true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let original = source(temp.path(), b"abcdefghijk", 4);
        let before = inventory(temp.path());
        let token = CancellationToken::default();
        let cancel = token.clone();
        let mut writing_seen = false;
        let mut progress = |p: Progress| {
            writing_seen |= p.phase == Phase::Writing;
            if p.phase == phase && (!after_writing || writing_seen) {
                cancel.cancel();
            }
        };
        let error = reshard_file(
            &request(&original, 3),
            &mut TaskContext {
                cancellation: token,
                progress: Some(&mut progress),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        unchanged(&before);
        assert!(!temp.path().join("新规格.7z.afparts.json").exists());
    }
}

#[test]
fn disk_full_during_payload_or_sync_can_restart_in_a_new_namespace() {
    for point in [IoPoint::PayloadWrite, IoPoint::PayloadSync] {
        let temp = tempfile::tempdir().unwrap();
        let original = source(temp.path(), b"abcdefghijk", 4);
        let mut count = 0;
        let mut fault = |p| {
            if p == point {
                count += 1;
                if count == 2 {
                    return Err(io::ErrorKind::StorageFull.into());
                }
            }
            Ok(())
        };
        let error = reshard_file(
            &request(&original, 3),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InsufficientDiskSpace);
        let task = temp
            .path()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        fs::write(task.join("unknown.txt"), b"KEEP").unwrap();
        let before = inventory(temp.path());
        let plan = plan_restart_task(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(plan.operation, "reshard");
        assert_eq!(inventory(temp.path()), before);
        let receipt = restart_task(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(concatenated(&receipt), b"abcdefghijk");
        unchanged(&before);
    }
}

#[test]
fn partial_reshard_publication_is_reverified_and_recovered_manifest_last() {
    for point in [
        IoPoint::PlanDurable,
        IoPoint::AfterPublish,
        IoPoint::BeforeCommitMarker,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let original = source(temp.path(), b"abcdefghijk", 4);
        let mut fault = |p| {
            if p == point {
                return Err(io::ErrorKind::Other.into());
            }
            Ok(())
        };
        let error = reshard_file(
            &request(&original, 3),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        let task = temp
            .path()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        if point != IoPoint::BeforeCommitMarker {
            assert!(!temp.path().join("新规格.7z.afparts.json").exists());
        }
        let before = inventory(temp.path());
        let plan = plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(inventory(temp.path()), before);
        let receipt = recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(receipt.manifest_path, plan.manifest_path);
        assert_eq!(concatenated(&receipt), b"abcdefghijk");
        unchanged(&before);
        let after = inventory(temp.path());
        recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(inventory(temp.path()), after);
    }
}

#[test]
fn changed_unopened_source_and_raced_destination_never_report_success() {
    for source_change in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let original = source(temp.path(), b"abcdefghijk", 4);
        let mut changed = false;
        let mut fault = |p| {
            if !changed
                && p == if source_change {
                    IoPoint::PayloadWrite
                } else {
                    IoPoint::BeforePublish
                }
            {
                changed = true;
                if source_change {
                    fs::write(&original.output_files[2], b"BAD").unwrap();
                } else {
                    fs::write(temp.path().join("新规格.7z.001"), b"KEEP").unwrap();
                }
            }
            Ok(())
        };
        let error = reshard_file(
            &request(&original, 3),
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        if source_change {
            assert!(matches!(
                error.code,
                ErrorCode::InputChangedDuringOperation | ErrorCode::PartHashMismatch
            ));
        } else {
            assert_eq!(error.code, ErrorCode::OutputExists);
            assert_eq!(
                fs::read(temp.path().join("新规格.7z.001")).unwrap(),
                b"KEEP"
            );
        }
        assert!(!temp.path().join("新规格.7z.afparts.json").exists());
    }
}

#[test]
#[ignore = "worker launched and terminated explicitly by killed_reshard_can_restart"]
fn reshard_crash_worker() {
    let root = PathBuf::from(std::env::var_os("EAA_RESHARD_ROOT").expect("test root"));
    let original: Receipt = {
        // Only construct the request from the pre-created group; do not write its sources.
        let mut fault = |p| {
            if p == IoPoint::PayloadSync {
                fs::write(root.join("ready"), b"ready").unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Ok(())
        };
        reshard_file(
            &ReshardRequest {
                part_paths: Vec::new(),
                input: std::env::var_os("EAA_RESHARD_INPUT")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| root.join("原始.7z.afparts.json")),
                output_dir: None,
                basename: std::env::var("EAA_RESHARD_BASENAME")
                    .unwrap_or_else(|_| "新规格.7z".into()),
                policy: SplitPolicy::Hard { max_bytes: 3 },
                alignment: None,
                expected_sha256: None,
            },
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap()
    };
    drop(original);
}

#[test]
fn killed_reshard_can_restart() {
    let temp = tempfile::tempdir().unwrap();
    source(temp.path(), b"abcdefghijk", 4);
    let before = inventory(temp.path());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "reshard_crash_worker"])
        .env("EAA_RESHARD_ROOT", temp.path())
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
    let ready = temp.path().join("ready").exists();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(ready);
    let task = inspect_transactions(temp.path())
        .unwrap()
        .into_iter()
        .find(|t| t.status == "needs_recovery")
        .unwrap();
    let receipt = restart_task(&task.directory, &mut TaskContext::default()).unwrap();
    assert_eq!(concatenated(&receipt), b"abcdefghijk");
    unchanged(&before);
}

#[test]
#[ignore = "manual 8 GiB disk/streaming acceptance: run this test explicitly in release mode"]
fn large_2gib_to_1point5gib() {
    use std::io::{Read, Write};
    let temp = tempfile::tempdir().unwrap();
    let old_size = 2_u64 * 1024 * 1024 * 1024;
    let mut buffer = vec![0_u8; 8 * 1024 * 1024];
    let mut whole = Sha256::new();
    let mut parts = Vec::new();
    for index in 1..=2 {
        buffer.fill(index as u8);
        let filename = format!("large.7z.{index:03}");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp.path().join(&filename))
            .unwrap();
        let mut hash = Sha256::new();
        for _ in 0..old_size / buffer.len() as u64 {
            file.write_all(&buffer).unwrap();
            hash.update(&buffer);
            whole.update(&buffer);
        }
        file.sync_all().unwrap();
        parts.push(easy_archive_core::PartDescriptor {
            index,
            filename,
            offset: (index as u64 - 1) * old_size,
            size: old_size,
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    let manifest = Manifest {
        schema_version: 1,
        tool: easy_archive_core::PRODUCT_NAME_EN.into(),
        original_filename: "large.7z".into(),
        original_size: old_size * 2,
        original_sha256: format!("{:x}", whole.finalize()),
        split_mode: "hard".into(),
        part_count: 2,
        parts,
        created_at: "2026-10-24T00:00:00Z".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
    };
    let path = temp.path().join("large.7z.afparts.json");
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let receipt = reshard_file(
        &ReshardRequest {
            part_paths: Vec::new(),
            input: path,
            output_dir: None,
            basename: "new-large.7z".into(),
            policy: SplitPolicy::Hard {
                max_bytes: 1536 * 1024 * 1024,
            },
            alignment: None,
            expected_sha256: Some(manifest.original_sha256.clone()),
        },
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(receipt.part_count, 3);
    assert_eq!(receipt.sha256, manifest.original_sha256);
    let mut offset = 0_u64;
    for path in &receipt.output_files {
        let mut file = fs::File::open(path).unwrap();
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            for (i, byte) in buffer[..count].iter().enumerate() {
                assert_eq!(*byte, if offset + (i as u64) < old_size { 1 } else { 2 });
            }
            offset += count as u64;
        }
    }
    assert_eq!(offset, old_size * 2);
    assert!(!temp.path().join("large.7z").exists());
    assert!(!temp.path().join("new-large.7z").exists());
}
