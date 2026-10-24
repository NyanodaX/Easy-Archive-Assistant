use easy_archive_core::transaction::inspect_transactions;
use easy_archive_core::{
    CancellationToken, ErrorCode, MergeRequest, Phase, SplitPolicy, SplitRequest, TaskContext,
    inspect_group, merge_file, parse_manifest, plan_merge_file, plan_split_file, split_file,
    verify_group,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

fn split_request(input: &Path, cap: u64) -> SplitRequest {
    SplitRequest {
        input: input.to_owned(),
        output_dir: None,
        basename: None,
        policy: SplitPolicy::Hard { max_bytes: cap },
        alignment: None,
        allow_any_file: false,
    }
}
fn merge_request(input: &Path, basename: &str) -> MergeRequest {
    MergeRequest {
        part_paths: Vec::new(),
        input: input.to_owned(),
        output_dir: None,
        basename: Some(basename.to_owned()),
        expected_sha256: None,
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bytes(size: usize) -> Vec<u8> {
    (0..size)
        .map(|n| ((n.wrapping_mul(17) ^ (n >> 8)) % 256) as u8)
        .collect()
}

#[test]
fn round_trip_empty_one_target_boundaries_and_random_unequal_volumes() {
    for size in [0, 1, 63, 64, 65, 128, 129, 193, 205, 1009, 8193] {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("原始 加密数据.zip");
        let data = bytes(size);
        fs::write(&original, &data).unwrap();
        let split = split_file(&split_request(&original, 64), &mut TaskContext::default()).unwrap();
        let manifest_path = split.manifest_path.as_ref().unwrap();
        let manifest = parse_manifest(&fs::read(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.original_sha256, digest(&data));
        assert_eq!(split.sha256, digest(&data));
        assert_eq!(split.verification_level, "verified_against_original");
        for part in &manifest.parts {
            assert_eq!(
                digest(&fs::read(temp.path().join(&part.filename)).unwrap()),
                part.sha256
            );
        }
        let verified = verify_group(manifest_path, None, &mut TaskContext::default()).unwrap();
        assert_eq!(
            verified.verification_level,
            "verified_against_available_sources"
        );
        assert!(!verified.provenance_authenticated);
        let merged = merge_file(
            &merge_request(&split.output_files[0], "恢复.zip"),
            &mut TaskContext::default(),
        )
        .unwrap();
        assert_eq!(merged.sha256, digest(&data));
        assert_eq!(fs::read(&merged.output_files[0]).unwrap(), data);
        assert_eq!(fs::read(&original).unwrap(), data);
        assert!(
            inspect_transactions(temp.path())
                .unwrap()
                .iter()
                .all(|t| t.status == "committed")
        );
    }
}

#[test]
fn hard_balanced_and_alignment_outputs_match_plan_and_source() {
    for policy in [
        SplitPolicy::Hard { max_bytes: 100 },
        SplitPolicy::Balanced {
            target_bytes: 100,
            tolerance_ppm: 50_000,
            hard_max_bytes: None,
        },
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sample.7z");
        fs::write(&path, bytes(205)).unwrap();
        let mut request = split_request(&path, 100);
        request.policy = policy;
        request.alignment = Some(16);
        let plan = plan_split_file(&request).unwrap();
        let receipt = split_file(&request, &mut TaskContext::default()).unwrap();
        assert_eq!(receipt.output_files.len(), plan.plan.part_count as usize);
        for (output, part) in receipt.output_files.iter().zip(&plan.plan.parts) {
            assert_eq!(fs::metadata(output).unwrap().len(), part.size);
        }
    }
}

#[test]
fn unmanifested_nonuniform_parts_are_merged_with_finite_verification() {
    let temp = tempfile::tempdir().unwrap();
    for (i, data) in [b"abc".as_slice(), b"d", b"efghij"].iter().enumerate() {
        fs::write(temp.path().join(format!("sample.zip.{:03}", i + 1)), data).unwrap();
    }
    let request = merge_request(&temp.path().join("sample.zip.002"), "restored.zip");
    let plan = plan_merge_file(&request).unwrap();
    assert!(!plan.group.has_manifest);
    let receipt = merge_file(&request, &mut TaskContext::default()).unwrap();
    assert_eq!(
        receipt.verification_level,
        "verified_against_available_sources"
    );
    assert_eq!(fs::read(&receipt.output_files[0]).unwrap(), b"abcdefghij");
}

#[test]
fn trusted_external_digest_is_required_for_historical_original_claim() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.7z.001"), b"abc").unwrap();
    let path = temp.path().join("a.7z.001");
    let result = verify_group(&path, Some(&digest(b"abc")), &mut TaskContext::default()).unwrap();
    assert_eq!(result.verification_level, "verified_against_original");
    assert!(result.has_external_digest);
    assert_eq!(
        verify_group(&path, Some(&"0".repeat(64)), &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::SourceHashMismatch
    );
    let mut request = merge_request(&path, "restored.7z");
    request.expected_sha256 = Some(digest(b"abc"));
    assert_eq!(
        merge_file(&request, &mut TaskContext::default())
            .unwrap()
            .verification_level,
        "verified_against_original"
    );
}

#[test]
fn conflicts_never_overwrite_original_parts_or_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("a.zip");
    fs::write(&input, b"hello").unwrap();
    fs::write(temp.path().join("a.zip.afparts.json"), b"keep").unwrap();
    assert_eq!(
        split_file(&split_request(&input, 2), &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::OutputExists
    );
    assert_eq!(
        fs::read(temp.path().join("a.zip.afparts.json")).unwrap(),
        b"keep"
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    let mut request = split_request(&input, 2);
    request.basename = Some("new.zip".to_owned());
    let split = split_file(&request, &mut TaskContext::default()).unwrap();
    let merge = MergeRequest {
        part_paths: Vec::new(),
        input: split.output_files[0].clone(),
        output_dir: None,
        basename: None,
        expected_sha256: None,
    };
    assert_eq!(
        merge_file(&merge, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::OutputExists
    );
    assert_eq!(fs::read(input).unwrap(), b"hello");
}

fn make_group(root: &Path) -> PathBuf {
    let input = root.join("a.zip");
    fs::write(&input, bytes(205)).unwrap();
    split_file(&split_request(&input, 64), &mut TaskContext::default())
        .unwrap()
        .manifest_path
        .unwrap()
}

#[test]
fn missing_middle_or_last_manifest_volume_is_rejected() {
    for index in [2, 4] {
        let temp = tempfile::tempdir().unwrap();
        let manifest = make_group(temp.path());
        fs::remove_file(temp.path().join(format!("a.zip.{index:03}"))).unwrap();
        assert_eq!(
            verify_group(&manifest, None, &mut TaskContext::default())
                .unwrap_err()
                .code,
            ErrorCode::PartMissing
        );
        assert!(!temp.path().join("restored.zip").exists());
    }
}

#[test]
fn modified_volume_and_forged_whole_hash_fail_before_any_merge_output() {
    for tamper_part in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let manifest = make_group(temp.path());
        if tamper_part {
            fs::write(temp.path().join("a.zip.002"), vec![0; 51]).unwrap();
        } else {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
            value["original_sha256"] = serde_json::json!("0".repeat(64));
            fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let error = merge_file(
            &merge_request(&manifest, "restored.zip"),
            &mut TaskContext::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.code,
            if tamper_part {
                ErrorCode::PartHashMismatch
            } else {
                ErrorCode::SourceHashMismatch
            }
        );
        assert!(error.recovery_task_id.is_none());
        assert!(!temp.path().join("restored.zip").exists());
    }
}

#[test]
fn numbering_aliases_gaps_zero_and_extra_volumes_are_rejected() {
    for (names, expected) in [
        (vec!["a.zip.001", "a.zip.003"], ErrorCode::PartMissing),
        (vec!["a.zip.001", "a.zip.01"], ErrorCode::PartDuplicate),
        (vec!["a.zip.001", "a.zip.0001"], ErrorCode::PartDuplicate),
        (vec!["a.zip.001", "a.zip.000"], ErrorCode::LimitExceeded),
    ] {
        let temp = tempfile::tempdir().unwrap();
        for name in names {
            fs::write(temp.path().join(name), b"a").unwrap();
        }
        assert_eq!(
            inspect_group(&temp.path().join("a.zip.001"))
                .unwrap_err()
                .code,
            expected
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let manifest = make_group(temp.path());
    fs::write(temp.path().join("a.zip.005"), b"extra").unwrap();
    assert_eq!(
        inspect_group(&manifest).unwrap_err().code,
        ErrorCode::PartDuplicate
    );
}

#[test]
fn malformed_or_traversing_manifest_never_falls_back_to_unverified_merge() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("a.zip.001"), b"x").unwrap();
    fs::write(temp.path().join("a.zip.afparts.json"), b"{}").unwrap();
    assert_eq!(
        inspect_group(&temp.path().join("a.zip.001"))
            .unwrap_err()
            .code,
        ErrorCode::ManifestInvalid
    );
}

#[test]
fn cancellation_before_start_or_during_write_never_publishes_success() {
    for before_start in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("a.zip");
        fs::write(&input, b"hello").unwrap();
        let token = CancellationToken::default();
        if before_start {
            token.cancel();
        }
        let cancel = token.clone();
        let mut progress = move |p: easy_archive_core::Progress| {
            if p.phase == Phase::Writing && p.completed_bytes > 0 {
                cancel.cancel();
            }
        };
        let mut context = TaskContext {
            cancellation: token,
            progress: Some(&mut progress),
            ..TaskContext::default()
        };
        let error = split_file(&split_request(&input, 2), &mut context).unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(error.recovery_task_id.is_some(), !before_start);
        assert!(!temp.path().join("a.zip.001").exists());
        assert!(!temp.path().join("a.zip.afparts.json").exists());
        assert_eq!(fs::read(input).unwrap(), b"hello");
    }
}

#[test]
fn publication_race_keeps_user_file_and_reports_incomplete_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("a.zip");
    fs::write(&input, b"hello").unwrap();
    let conflict = temp.path().join("a.zip.002");
    let mut progress = |p: easy_archive_core::Progress| {
        if p.phase == Phase::Publishing && p.completed_bytes == 1 {
            fs::write(&conflict, b"keep").unwrap();
        }
    };
    let error = split_file(
        &split_request(&input, 2),
        &mut TaskContext {
            cancellation: CancellationToken::default(),
            progress: Some(&mut progress),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::OutputExists);
    assert!(error.recovery_task_id.is_some());
    assert_eq!(fs::read(conflict).unwrap(), b"keep");
    assert!(temp.path().join("a.zip.001").exists());
    assert!(!temp.path().join("a.zip.afparts.json").exists());
    let tasks = inspect_transactions(temp.path()).unwrap();
    assert_eq!(tasks[0].status, "needs_recovery");
    assert_eq!(
        verify_group(
            &temp.path().join("a.zip.001"),
            None,
            &mut TaskContext::default()
        )
        .unwrap_err()
        .code,
        ErrorCode::TransactionNeedsRecovery
    );
    let clean = tempfile::tempdir().unwrap();
    let mut merge = merge_request(&temp.path().join("a.zip.001"), "restored.zip");
    merge.output_dir = Some(clean.path().to_owned());
    assert_eq!(
        merge_file(&merge, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
    assert_eq!(fs::read_dir(clean.path()).unwrap().count(), 0);
    let mut retry = split_request(&input, 2);
    retry.basename = Some("new.zip".to_owned());
    assert_eq!(
        split_file(&retry, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
}

#[test]
fn streamed_chunks_cross_eight_mib_buffer_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("big.7z");
    let data = bytes(8 * 1024 * 1024 + 13);
    fs::write(&input, &data).unwrap();
    let split = split_file(
        &split_request(&input, 9 * 1024 * 1024),
        &mut TaskContext::default(),
    )
    .unwrap();
    let merge = merge_file(
        &merge_request(&split.output_files[0], "restored.7z"),
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(merge.sha256, digest(&data));
    assert_eq!(fs::read(&merge.output_files[0]).unwrap(), data);
}

#[test]
fn encrypted_zip_fixture_round_trips_without_a_password_or_format_parser() {
    let hex = include_str!("../../../tests/fixtures/cpython-encrypted.zip.hex").trim();
    let data: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("encrypted.zip");
    fs::write(&input, &data).unwrap();
    let split = split_file(&split_request(&input, 64), &mut TaskContext::default()).unwrap();
    let merged = merge_file(
        &merge_request(&split.output_files[0], "restored.zip"),
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(merged.sha256, digest(&data));
    assert_eq!(fs::read(&merged.output_files[0]).unwrap(), data);
}

#[test]
fn deterministic_random_lengths_round_trip_with_both_policies() {
    let mut seed = 12345_u64;
    for i in 0..12 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let data = bytes((seed % 3000) as usize);
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("random.7z");
        fs::write(&input, &data).unwrap();
        let mut request = split_request(&input, 257);
        if i % 2 == 0 {
            request.policy = SplitPolicy::Balanced {
                target_bytes: 257,
                tolerance_ppm: 50_000,
                hard_max_bytes: None,
            };
        }
        let split = split_file(&request, &mut TaskContext::default()).unwrap();
        let merged = merge_file(
            &merge_request(&split.output_files[0], "restored.7z"),
            &mut TaskContext::default(),
        )
        .unwrap();
        assert_eq!(merged.sha256, digest(&data));
        assert_eq!(fs::read(&merged.output_files[0]).unwrap(), data);
    }
}

#[test]
fn concurrent_input_writes_are_blocked_or_detected_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("a.zip");
    fs::write(&input, b"hello").unwrap();
    let mut attempted = false;
    let mut prevented = false;
    let mut progress = |p: easy_archive_core::Progress| {
        if !attempted && p.phase == Phase::Writing && p.completed_bytes > 0 {
            attempted = true;
            prevented = fs::write(&input, b"HELLO").is_err();
        }
    };
    let result = split_file(
        &split_request(&input, 2),
        &mut TaskContext {
            cancellation: CancellationToken::default(),
            progress: Some(&mut progress),
            ..TaskContext::default()
        },
    );
    assert!(attempted);
    if prevented {
        assert!(result.is_ok());
        assert_eq!(fs::read(input).unwrap(), b"hello");
    } else {
        assert_eq!(
            result.unwrap_err().code,
            ErrorCode::InputChangedDuringOperation
        );
        assert!(!temp.path().join("a.zip.afparts.json").exists());
    }
}

#[test]
fn cancellation_during_publication_leaves_no_group_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("a.zip");
    fs::write(&input, b"hello").unwrap();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let mut progress = move |p: easy_archive_core::Progress| {
        if p.phase == Phase::Publishing && p.completed_bytes == 1 {
            cancel.cancel();
        }
    };
    let error = split_file(
        &split_request(&input, 2),
        &mut TaskContext {
            cancellation: token,
            progress: Some(&mut progress),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert!(temp.path().join("a.zip.001").exists());
    assert!(!temp.path().join("a.zip.afparts.json").exists());
    assert_eq!(fs::read(input).unwrap(), b"hello");
}

#[cfg(windows)]
#[test]
fn windows_group_discovery_accepts_case_variations_in_selected_path() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("UPPER.ZIP");
    fs::write(&input, b"hello").unwrap();
    split_file(&split_request(&input, 2), &mut TaskContext::default()).unwrap();
    let report = verify_group(
        &temp.path().join("upper.zip.001"),
        None,
        &mut TaskContext::default(),
    )
    .unwrap();
    assert_eq!(report.sha256, digest(b"hello"));
}
