use easy_archive_core::{
    ByteRange, ErrorCode, IoPoint, RescueRequest, SplitPolicy, SplitRequest, TaskContext,
    parse_manifest, plan_rescue,
    recovery::{plan_recover_transaction, recover_transaction},
    rescue_part,
    restart::{plan_restart_task, restart_task},
    split_file, verify_group,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
fn group(root: &Path, name: &str, bytes: &[u8], cap: u64) -> easy_archive_core::Receipt {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    let input = directory.join("data.7z");
    fs::write(&input, bytes).unwrap();
    split_file(
        &SplitRequest {
            input,
            output_dir: None,
            basename: None,
            policy: SplitPolicy::Hard { max_bytes: cap },
            alignment: None,
            allow_any_file: false,
        },
        &mut TaskContext::default(),
    )
    .unwrap()
}
fn setup(root: &Path, bytes: &[u8]) -> RescueRequest {
    let a = group(root, "A", bytes, 10);
    let b = group(root, "B", bytes, 7);
    let part = if a.part_count > 1 { 2 } else { 1 };
    fs::remove_file(&a.output_files[part as usize - 1]).unwrap();
    RescueRequest {
        target_manifest: a.manifest_path.unwrap(),
        source_manifests: vec![b.manifest_path.unwrap()],
        part,
        output_dir: None,
        basename: None,
        expected_sha256: None,
    }
}
fn inventory(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(inventory(&p));
        } else {
            out.push((p.clone(), fs::read(p).unwrap()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
fn unchanged(old: &[(PathBuf, Vec<u8>)]) {
    for (p, b) in old {
        assert_eq!(&fs::read(p).unwrap(), b);
    }
}

#[test]
fn rescue_reads_two_source_spans_and_restores_missing_target_without_changing_old_files() {
    let temp = tempfile::tempdir().unwrap();
    let bytes: Vec<_> = (0..37).collect();
    let req = setup(temp.path(), &bytes);
    let before = inventory(temp.path());
    let plan = plan_rescue(&req).unwrap();
    assert_eq!(plan.range, ByteRange::new(10, 19).unwrap());
    assert_eq!(plan.spans.len(), 2);
    assert_eq!(
        (
            plan.spans[0].part_index,
            plan.spans[0].source_offset,
            plan.spans[0].length
        ),
        (2, 3, 3)
    );
    assert_eq!(
        (
            plan.spans[1].part_index,
            plan.spans[1].source_offset,
            plan.spans[1].length
        ),
        (3, 0, 6)
    );
    assert_eq!(plan.space.estimated_payload_bytes, 9);
    assert_eq!(inventory(temp.path()), before);
    let receipt = rescue_part(&req, &mut TaskContext::default()).unwrap();
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), bytes[10..19]);
    assert_eq!(receipt.verification_scope, "target_range_only");
    assert_eq!(
        receipt.verification_level,
        "verified_against_available_sources"
    );
    unchanged(&before);
    verify_group(&req.target_manifest, None, &mut TaskContext::default()).unwrap();
    let checked =
        plan_recover_transaction(&receipt.staging_directory, &mut TaskContext::default()).unwrap();
    assert_eq!(checked.target_range, Some(plan.range));
}
#[test]
fn empty_single_part_multiple_backups_and_external_original_digest_have_correct_levels() {
    for bytes in [b"".as_slice(), b"a", b"abcdefghijklmnopqrstuvw"] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), bytes);
        let c = group(temp.path(), "C", bytes, 4);
        req.source_manifests.push(c.manifest_path.unwrap());
        req.expected_sha256 = Some(
            parse_manifest(&fs::read(&req.target_manifest).unwrap())
                .unwrap()
                .original_sha256,
        );
        let plan = plan_rescue(&req).unwrap();
        let receipt = rescue_part(&req, &mut TaskContext::default()).unwrap();
        assert_eq!(
            fs::read(&receipt.output_files[0]).unwrap(),
            bytes[plan.range.start as usize..plan.range.end_exclusive as usize]
        );
        assert_eq!(receipt.verification_level, "verified_against_original");
        assert!(!receipt.provenance_authenticated);
        let recovered =
            recover_transaction(&receipt.staging_directory, &mut TaskContext::default()).unwrap();
        assert_eq!(
            recovered.verification_level,
            "verified_against_available_sources"
        );
    }
}
#[test]
fn missing_corrupt_incompatible_and_forged_sources_never_create_a_rescue_task() {
    for case in [
        "missing", "hash", "whole", "identity", "expected", "target", "existing",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
        let source = req.source_manifests[0].clone();
        let mut manifest = parse_manifest(&fs::read(&source).unwrap()).unwrap();
        let expected = match case {
            "missing" => {
                fs::remove_file(source.parent().unwrap().join(&manifest.parts[0].filename))
                    .unwrap();
                ErrorCode::MissingCoverage
            }
            "hash" => {
                let p = &manifest.parts[0];
                fs::write(
                    source.parent().unwrap().join(&p.filename),
                    vec![99; p.size as usize],
                )
                .unwrap();
                ErrorCode::PartHashMismatch
            }
            "whole" => {
                manifest.original_sha256 = "0".repeat(64);
                fs::write(&source, serde_json::to_vec(&manifest).unwrap()).unwrap();
                let mut target = parse_manifest(&fs::read(&req.target_manifest).unwrap()).unwrap();
                target.original_sha256 = "0".repeat(64);
                fs::write(&req.target_manifest, serde_json::to_vec(&target).unwrap()).unwrap();
                ErrorCode::SourceHashMismatch
            }
            "identity" => {
                manifest.original_sha256 = "0".repeat(64);
                fs::write(&source, serde_json::to_vec(&manifest).unwrap()).unwrap();
                ErrorCode::SourceIdentityMismatch
            }
            "expected" => {
                req.expected_sha256 = Some("0".repeat(64));
                ErrorCode::SourceHashMismatch
            }
            "target" => {
                let mut target = parse_manifest(&fs::read(&req.target_manifest).unwrap()).unwrap();
                target.parts[1].sha256 = "0".repeat(64);
                fs::write(&req.target_manifest, serde_json::to_vec(&target).unwrap()).unwrap();
                ErrorCode::PartHashMismatch
            }
            _ => {
                let p = parse_manifest(&fs::read(&req.target_manifest).unwrap())
                    .unwrap()
                    .parts[1]
                    .filename
                    .clone();
                fs::write(req.target_manifest.parent().unwrap().join(p), b"KEEP").unwrap();
                ErrorCode::OutputExists
            }
        };
        let before = inventory(temp.path());
        assert_eq!(
            rescue_part(&req, &mut TaskContext::default())
                .unwrap_err()
                .code,
            expected,
            "{case}"
        );
        assert_eq!(inventory(temp.path()), before);
    }
}
#[test]
fn interrupted_copy_restarts_and_complete_publication_recovers_with_unchanged_old_files() {
    for point in [
        IoPoint::PayloadSync,
        IoPoint::PlanDurable,
        IoPoint::AfterPublish,
        IoPoint::BeforeCommitMarker,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
        let mut fault = |p| {
            if p == point {
                return Err(io::ErrorKind::StorageFull.into());
            }
            Ok(())
        };
        let error = rescue_part(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InsufficientDiskSpace);
        let task = req
            .target_manifest
            .parent()
            .unwrap()
            .join(format!(".eaa-{}", error.recovery_task_id.unwrap()));
        fs::write(task.join("unknown.txt"), b"KEEP").unwrap();
        let before = inventory(temp.path());
        let receipt = if point == IoPoint::PayloadSync {
            let plan = plan_restart_task(&task, &mut TaskContext::default()).unwrap();
            assert_eq!(plan.operation, "rescue");
            assert_eq!(inventory(temp.path()), before);
            restart_task(&task, &mut TaskContext::default()).unwrap()
        } else {
            plan_recover_transaction(&task, &mut TaskContext::default()).unwrap();
            assert_eq!(inventory(temp.path()), before);
            recover_transaction(&task, &mut TaskContext::default()).unwrap()
        };
        assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"klmnopqrs");
        unchanged(&before);
        let after = inventory(temp.path());
        recover_transaction(&task, &mut TaskContext::default()).unwrap();
        assert_eq!(inventory(temp.path()), after);
    }
}

#[test]
fn cancellation_and_source_write_attempts_preserve_every_existing_input() {
    use easy_archive_core::{CancellationToken, Phase, Progress};
    for phase in [
        Phase::Verifying,
        Phase::Writing,
        Phase::CheckingSources,
        Phase::Publishing,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
        let before = inventory(temp.path());
        let token = CancellationToken::default();
        let cancel = token.clone();
        let mut progress = |p: Progress| {
            if p.phase == phase {
                cancel.cancel();
            }
        };
        let error = rescue_part(
            &req,
            &mut TaskContext {
                cancellation: token,
                progress: Some(&mut progress),
                ..TaskContext::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        unchanged(&before);
        assert!(!temp.path().join("A/data.7z.002").exists());
    }
    #[cfg(windows)]
    {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
        let before = inventory(temp.path());
        let mut tried = false;
        let mut progress = |p: Progress| {
            if p.phase == Phase::Writing && !tried {
                tried = true;
                assert!(fs::write(temp.path().join("B/data.7z.001"), b"BAD").is_err());
            }
        };
        rescue_part(
            &req,
            &mut TaskContext {
                progress: Some(&mut progress),
                ..TaskContext::default()
            },
        )
        .unwrap();
        assert!(tried);
        unchanged(&before);
    }
}

#[test]
fn missing_coverage_error_lists_byte_gaps_and_pending_ranges_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
    fs::remove_file(temp.path().join("B/data.7z.003")).unwrap();
    let before = inventory(temp.path());
    let error = plan_rescue(&req).unwrap_err();
    assert_eq!(error.code, ErrorCode::MissingCoverage);
    let details = error.coverage_details.unwrap();
    assert_eq!(
        details.missing_ranges,
        vec![ByteRange::new(13, 19).unwrap()]
    );
    assert_eq!(
        details.pending_ranges,
        vec![ByteRange::new(10, 13).unwrap()]
    );
    assert_eq!(inventory(temp.path()), before);
}

#[test]
#[ignore = "controlled worker launched by killed_rescue_restarts_or_recovers"]
fn rescue_crash_worker() {
    let root = PathBuf::from(std::env::var_os("EAA_RESCUE_ROOT").expect("test root"));
    let req: RescueRequest =
        serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
    let point = std::env::var("EAA_RESCUE_POINT").unwrap();
    let mut fault = |p| {
        if (point == "copy" && p == IoPoint::PayloadSync)
            || (point == "publish" && p == IoPoint::PlanDurable)
        {
            fs::write(root.join("ready"), b"ready").unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Ok(())
    };
    rescue_part(
        &req,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap();
}

#[test]
fn killed_rescue_restarts_or_recovers() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for point in ["copy", "publish"] {
        let temp = tempfile::tempdir().unwrap();
        let req = setup(temp.path(), b"abcdefghijklmnopqrstuvwxyz0123456789!");
        fs::write(
            temp.path().join("request.json"),
            serde_json::to_vec(&req).unwrap(),
        )
        .unwrap();
        let before = inventory(temp.path());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "rescue_crash_worker"])
            .env("EAA_RESCUE_ROOT", temp.path())
            .env("EAA_RESCUE_POINT", point)
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
        let task = easy_archive_core::transaction::inspect_transactions(
            req.target_manifest.parent().unwrap(),
        )
        .unwrap()
        .into_iter()
        .find(|t| t.status == "needs_recovery")
        .unwrap()
        .directory;
        let receipt = if point == "copy" {
            restart_task(&task, &mut TaskContext::default()).unwrap()
        } else {
            recover_transaction(&task, &mut TaskContext::default()).unwrap()
        };
        assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"klmnopqrs");
        unchanged(&before);
    }
}

#[test]
#[ignore = "manual M5 gate: 6 GiB backup and 2 GiB rescued output; owned fixtures need about 12 GiB"]
fn large_six_gib_cross_layout_rescue() {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let gib = 1_u64 << 30;
    let mut buffer = vec![0_u8; 8 * 1024 * 1024];
    let mut original_hash = Sha256::new();
    let mut a_hashes = [Sha256::new(), Sha256::new(), Sha256::new()];
    let mut a_first = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(a.join("data.7z.001"))
        .unwrap();
    let mut a_last = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(a.join("data.7z.003"))
        .unwrap();
    let mut b_parts = Vec::new();
    let mut offset = 0_u64;
    for index in 1..=4 {
        let filename = format!("data.7z.{index:03}");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(b.join(&filename))
            .unwrap();
        let mut part_hash = Sha256::new();
        let size = 3 * gib / 2;
        for _ in 0..size / buffer.len() as u64 {
            buffer.fill((offset / (gib / 2) + 1) as u8);
            file.write_all(&buffer).unwrap();
            part_hash.update(&buffer);
            original_hash.update(&buffer);
            let ai = (offset / (2 * gib)) as usize;
            a_hashes[ai].update(&buffer);
            if ai == 0 {
                a_first.write_all(&buffer).unwrap();
            } else if ai == 2 {
                a_last.write_all(&buffer).unwrap();
            }
            offset += buffer.len() as u64;
        }
        file.sync_all().unwrap();
        b_parts.push(easy_archive_core::PartDescriptor {
            index,
            filename,
            offset: (index as u64 - 1) * size,
            size,
            sha256: format!("{:x}", part_hash.finalize()),
        });
    }
    a_first.sync_all().unwrap();
    a_last.sync_all().unwrap();
    drop(a_first);
    drop(a_last);
    let hash = format!("{:x}", original_hash.finalize());
    let a_parts = a_hashes
        .into_iter()
        .enumerate()
        .map(|(i, h)| easy_archive_core::PartDescriptor {
            index: i as u32 + 1,
            filename: format!("data.7z.{:03}", i + 1),
            offset: i as u64 * 2 * gib,
            size: 2 * gib,
            sha256: format!("{:x}", h.finalize()),
        })
        .collect();
    let manifest = |parts: Vec<easy_archive_core::PartDescriptor>| easy_archive_core::Manifest {
        schema_version: 1,
        tool: "test".into(),
        original_filename: "data.7z".into(),
        original_size: 6 * gib,
        original_sha256: hash.clone(),
        split_mode: "hard".into(),
        part_count: parts.len() as u32,
        parts,
        created_at: "2026-10-24T00:00:00Z".into(),
        app_version: "test".into(),
    };
    let am = a.join("data.7z.afparts.json");
    let bm = b.join("data.7z.afparts.json");
    fs::write(&am, serde_json::to_vec(&manifest(a_parts)).unwrap()).unwrap();
    fs::write(&bm, serde_json::to_vec(&manifest(b_parts)).unwrap()).unwrap();
    let req = RescueRequest {
        target_manifest: am.clone(),
        source_manifests: vec![bm],
        part: 2,
        output_dir: None,
        basename: None,
        expected_sha256: Some(hash.clone()),
    };
    let plan = plan_rescue(&req).unwrap();
    assert_eq!(plan.spans.len(), 2);
    assert_eq!(
        (
            plan.spans[0].part_index,
            plan.spans[0].source_offset,
            plan.spans[0].length
        ),
        (2, gib / 2, gib)
    );
    assert_eq!(
        (
            plan.spans[1].part_index,
            plan.spans[1].source_offset,
            plan.spans[1].length
        ),
        (3, 0, gib)
    );
    let receipt = rescue_part(&req, &mut TaskContext::default()).unwrap();
    assert_eq!(receipt.source_size, 2 * gib);
    let mut output = fs::File::open(&receipt.output_files[0]).unwrap();
    let mut position = 2 * gib;
    loop {
        let count = output.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        for (i, value) in buffer[..count].iter().enumerate() {
            assert_eq!(*value, ((position + i as u64) / (gib / 2) + 1) as u8);
        }
        position += count as u64;
    }
    assert_eq!(position, 4 * gib);
    verify_group(&am, Some(&hash), &mut TaskContext::default()).unwrap();
    assert!(!a.join("data.7z").exists());
    assert!(!b.join("data.7z").exists());
}
