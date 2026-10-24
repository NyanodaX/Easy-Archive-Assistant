use easy_archive_core::{
    ByteRange, CoverageRequest, ErrorCode, IoPoint, Phase, Progress, Receipt, SplitPolicy,
    SplitRequest, TaskContext, coverage,
    coverage::{CoverageState, PartAvailability, StreamState},
    parse_manifest, split_file,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

fn group(root: &Path, name: &str, bytes: &[u8], size: u64) -> Receipt {
    let directory = root.join(name);
    fs::create_dir(&directory).unwrap();
    let input = directory.join("data.7z");
    fs::write(&input, bytes).unwrap();
    split_file(
        &SplitRequest {
            input,
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
fn request(target: &Receipt, source: &Receipt) -> CoverageRequest {
    CoverageRequest {
        target_manifest: target.manifest_path.clone().unwrap(),
        source_manifests: vec![source.manifest_path.clone().unwrap()],
        part: None,
        metadata_only: false,
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
fn read_manifest(receipt: &Receipt) -> easy_archive_core::Manifest {
    parse_manifest(&fs::read(receipt.manifest_path.as_ref().unwrap()).unwrap()).unwrap()
}
fn write_manifest(receipt: &Receipt, manifest: &easy_archive_core::Manifest) {
    fs::write(
        receipt.manifest_path.as_ref().unwrap(),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn target_range_crosses_two_other_volume_boundaries_without_any_writes() {
    let temp = tempfile::tempdir().unwrap();
    let bytes: Vec<_> = (0..37).collect();
    let target = group(temp.path(), "目标", &bytes, 10);
    let source = group(temp.path(), "备用", &bytes, 7);
    let mut req = request(&target, &source);
    req.part = Some(2);
    let before = inventory(temp.path());
    let report = coverage(&req, &mut TaskContext::default()).unwrap();
    assert_eq!(report.requested_range, ByteRange::new(10, 19).unwrap());
    assert!(report.complete_coverage);
    assert_eq!(report.covered_bytes, 9);
    assert!(!report.provenance_authenticated);
    assert_eq!(report.segments.len(), 2);
    assert_eq!(report.segments[0].range, ByteRange::new(10, 13).unwrap());
    assert_eq!(report.segments[1].range, ByteRange::new(13, 19).unwrap());
    assert_eq!(report.segments[0].sources[0].part_index, 2);
    assert_eq!(report.segments[1].sources[0].part_index, 3);
    assert!(
        report
            .segments
            .iter()
            .all(|s| s.status == CoverageState::Covered)
    );
    assert_eq!(report.sources[0].whole_stream_state, StreamState::Verified);
    assert!(!report.writes_performed);
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn arbitrary_missing_parts_report_exact_missing_intervals_and_pending_identity() {
    for seed in 0..9 {
        let temp = tempfile::tempdir().unwrap();
        let bytes: Vec<_> = (0..53).collect();
        let target = group(temp.path(), "target", &bytes, 11);
        let source = group(temp.path(), "source", &bytes, 7);
        let manifest = read_manifest(&source);
        let mut missing = vec![false; bytes.len()];
        for part in &manifest.parts {
            if (part.index + seed) % 3 == 0 || part.index == 1 {
                fs::remove_file(source.output_files[part.index as usize - 1].clone()).unwrap();
                missing[part.offset as usize..(part.offset + part.size) as usize].fill(true);
            }
        }
        let before = inventory(temp.path());
        let report = coverage(&request(&target, &source), &mut TaskContext::default()).unwrap();
        assert!(!report.complete_coverage);
        assert_eq!(report.covered_bytes, 0);
        assert!(!report.has_integrity_errors);
        let mut actual = vec![false; bytes.len()];
        for range in report.missing_ranges {
            actual[range.start as usize..range.end_exclusive as usize].fill(true);
        }
        assert_eq!(actual, missing);
        assert!(!report.pending_ranges.is_empty());
        assert_eq!(
            report.sources[0].whole_stream_state,
            StreamState::Incomplete
        );
        assert_eq!(inventory(temp.path()), before);
    }
}

#[test]
fn complete_backup_covers_a_missing_target_group_but_complementary_unverified_groups_stay_pending()
{
    let temp = tempfile::tempdir().unwrap();
    let bytes: Vec<_> = (0..21).collect();
    let target = group(temp.path(), "target", &bytes, 6);
    let a = group(temp.path(), "a", &bytes, 7);
    let b = group(temp.path(), "b", &bytes, 7);
    fs::remove_file(&target.output_files[1]).unwrap();
    let mut req = request(&target, &a);
    req.source_manifests.push(b.manifest_path.clone().unwrap());
    assert!(
        coverage(&req, &mut TaskContext::default())
            .unwrap()
            .complete_coverage
    );
    fs::remove_file(&a.output_files[0]).unwrap();
    fs::remove_file(&b.output_files[1]).unwrap();
    let report = coverage(&req, &mut TaskContext::default()).unwrap();
    assert!(!report.complete_coverage);
    assert!(report.missing_ranges.is_empty());
    assert!(!report.pending_ranges.is_empty());
    assert_eq!(report.covered_bytes, 0);
}

#[test]
fn metadata_only_never_marks_existing_ranges_verified_and_empty_sources_need_verification() {
    for bytes in [b"".as_slice(), b"abcdefghijk"] {
        let temp = tempfile::tempdir().unwrap();
        let target = group(temp.path(), "t", bytes, 4);
        let source = group(temp.path(), "s", bytes, 3);
        let mut req = request(&target, &source);
        req.metadata_only = true;
        let report = coverage(&req, &mut TaskContext::default()).unwrap();
        assert!(!report.complete_coverage);
        assert!(!report.content_read);
        assert_eq!(report.covered_bytes, 0);
        assert!(
            report.sources[0]
                .parts
                .iter()
                .all(|p| p.status == PartAvailability::Pending)
        );
        req.metadata_only = false;
        assert!(
            coverage(&req, &mut TaskContext::default())
                .unwrap()
                .complete_coverage
        );
        for path in &source.output_files {
            fs::remove_file(path).unwrap();
        }
        assert!(
            !coverage(&req, &mut TaskContext::default())
                .unwrap()
                .complete_coverage
        );
    }
}

#[test]
fn corrupt_parts_and_forged_whole_digests_remain_diagnostic_errors() {
    for case in ["size", "hash", "whole"] {
        let temp = tempfile::tempdir().unwrap();
        let target = group(temp.path(), "t", b"abcdefghijk", 4);
        let source = group(temp.path(), "s", b"abcdefghijk", 3);
        match case {
            "size" => fs::write(&source.output_files[0], b"bad length").unwrap(),
            "hash" => fs::write(&source.output_files[0], b"BAD").unwrap(),
            _ => {
                for receipt in [&target, &source] {
                    let mut manifest = read_manifest(receipt);
                    manifest.original_sha256 = "0".repeat(64);
                    write_manifest(receipt, &manifest);
                }
            }
        }
        let before = inventory(temp.path());
        let report = coverage(&request(&target, &source), &mut TaskContext::default()).unwrap();
        assert!(!report.complete_coverage);
        assert!(report.has_integrity_errors);
        assert_eq!(report.covered_bytes, 0);
        if case == "whole" {
            assert_eq!(
                report.sources[0].whole_stream_state,
                StreamState::HashMismatch
            );
        } else {
            assert_eq!(
                report.sources[0].parts[0].status,
                if case == "size" {
                    PartAvailability::SizeMismatch
                } else {
                    PartAvailability::HashMismatch
                }
            );
        }
        assert_eq!(inventory(temp.path()), before);
    }
}

#[test]
fn overlapping_bytes_from_a_forged_partial_group_are_compared_and_reported_as_conflict() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"abcdefghijklmnopqrst";
    let target = group(temp.path(), "t", bytes, 5);
    let good = group(temp.path(), "good", bytes, 5);
    let forged = group(temp.path(), "forged", bytes, 5);
    fs::write(&forged.output_files[0], b"BAD!!").unwrap();
    fs::remove_file(&forged.output_files[3]).unwrap();
    let mut manifest = read_manifest(&forged);
    manifest.parts[0].sha256 = format!("{:x}", Sha256::digest(b"BAD!!"));
    write_manifest(&forged, &manifest);
    let mut req = request(&target, &good);
    req.source_manifests.push(forged.manifest_path.unwrap());
    let before = inventory(temp.path());
    let report = coverage(&req, &mut TaskContext::default()).unwrap();
    assert!(!report.complete_coverage);
    assert!(report.has_integrity_errors);
    assert_eq!(report.conflict_ranges, vec![ByteRange::new(0, 5).unwrap()]);
    assert_eq!(report.covered_bytes, 15);
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn incompatible_manifests_unknown_pending_tasks_and_bad_arguments_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let target = group(temp.path(), "t", b"abcdefghijk", 4);
    let source = group(temp.path(), "s", b"DIFFERENT!!", 3);
    let req = request(&target, &source);
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::SourceIdentityMismatch
    );
    let mut req = request(&target, &target);
    req.part = Some(0);
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    req.part = None;
    req.source_manifests = vec![];
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    req.source_manifests = vec![target.manifest_path.clone().unwrap(); 9];
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    req.source_manifests = vec![target.manifest_path.clone().unwrap(); 2];
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    req.source_manifests.truncate(1);
    fs::create_dir(
        target.output_files[0]
            .parent()
            .unwrap()
            .join(".eaa-unknown"),
    )
    .unwrap();
    let before = inventory(temp.path());
    assert_eq!(
        coverage(&req, &mut TaskContext::default())
            .unwrap_err()
            .code,
        ErrorCode::TransactionNeedsRecovery
    );
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn cancellation_io_failure_and_source_changes_do_not_create_files_or_fake_coverage() {
    for case in ["cancel", "io", "change"] {
        let temp = tempfile::tempdir().unwrap();
        let target = group(temp.path(), "t", b"abcdefghijk", 4);
        let source = group(temp.path(), "s", b"abcdefghijk", 3);
        let before = inventory(temp.path());
        let token = easy_archive_core::CancellationToken::default();
        let cancel = token.clone();
        let mut changed = false;
        let mut progress = |p: Progress| {
            if case == "cancel" {
                cancel.cancel();
            }
            if case == "change"
                && !changed
                && p.phase == Phase::Verifying
                && p.part_index == Some(2)
            {
                changed = true;
                fs::write(&source.output_files[0], b"BAD").unwrap();
            }
        };
        let mut fault = |p| {
            if case == "io" && p == IoPoint::Read {
                return Err(io::ErrorKind::Other.into());
            }
            Ok(())
        };
        let error = coverage(
            &request(&target, &source),
            &mut TaskContext {
                cancellation: token,
                progress: Some(&mut progress),
                io_fault: Some(&mut fault),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code,
            match case {
                "cancel" => ErrorCode::Cancelled,
                "io" => ErrorCode::IoError,
                _ => ErrorCode::InputChangedDuringOperation,
            }
        );
        if case != "change" {
            assert_eq!(inventory(temp.path()), before);
        } else {
            assert_eq!(inventory(temp.path()).len(), before.len());
        }
    }
}

#[test]
fn huge_u64_missing_range_is_reported_without_allocating_payload_memory() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = easy_archive_core::Manifest {
        schema_version: 1,
        tool: "test".into(),
        original_filename: "huge.7z".into(),
        original_size: u64::MAX,
        original_sha256: "0".repeat(64),
        split_mode: "hard".into(),
        part_count: 1,
        parts: vec![easy_archive_core::PartDescriptor {
            index: 1,
            filename: "huge.7z.001".into(),
            offset: 0,
            size: u64::MAX,
            sha256: "1".repeat(64),
        }],
        created_at: "2026-10-24T00:00:00Z".into(),
        app_version: "test".into(),
    };
    let path = temp.path().join("huge.7z.afparts.json");
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let req = CoverageRequest {
        target_manifest: path.clone(),
        source_manifests: vec![path],
        part: None,
        metadata_only: false,
    };
    let report = coverage(&req, &mut TaskContext::default()).unwrap();
    assert!(!report.complete_coverage);
    assert_eq!(
        report.missing_ranges,
        vec![ByteRange::new(0, u64::MAX).unwrap()]
    );
    assert!(!report.content_read);
}

#[test]
fn total_source_part_limit_is_checked_before_opening_any_payload() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = vec![0; 10_000];
    let target = group(temp.path(), "t", &bytes, 10_000);
    let mut req = request(&target, &target);
    req.source_manifests.clear();
    let mut manifest = read_manifest(&target);
    manifest.part_count = 10_000;
    manifest.parts = (0..10_000)
        .map(|i| easy_archive_core::PartDescriptor {
            index: i + 1,
            filename: format!("data.7z.{:03}", i + 1),
            offset: i as u64,
            size: 1,
            sha256: "0".repeat(64),
        })
        .collect();
    for i in 0..3 {
        let path = temp.path().join(format!("source{i}.json"));
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        req.source_manifests.push(path);
    }
    let mut reads = 0;
    let mut fault = |p| {
        if p == IoPoint::Read {
            reads += 1;
        }
        Ok(())
    };
    assert_eq!(
        coverage(
            &req,
            &mut TaskContext {
                io_fault: Some(&mut fault),
                ..TaskContext::default()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::LimitExceeded
    );
    assert_eq!(reads, 0);
}
