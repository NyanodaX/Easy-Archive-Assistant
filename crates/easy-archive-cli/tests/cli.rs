use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_easy-archive-assistant"))
        .args(args)
        .output()
        .unwrap()
}
fn json_output(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value =
        serde_json::from_slice(&output.stdout).expect("stdout is exactly one JSON value");
    assert_eq!(parsed["schema_version"], 2);
    assert!(
        [
            "unverified",
            "verified_against_original",
            "verified_against_available_sources"
        ]
        .contains(&parsed["verification_level"].as_str().unwrap())
    );
    parsed
}
fn create_sample(path: &Path) {
    fs::write(path, [0_u8, 1, 2, 3, 255]).unwrap();
}

#[test]
fn rescue_cli_plans_without_writes_then_reconstructs_a_missing_cross_boundary_part() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.7z");
    create_sample(&path);
    let split = run(&["split", path.to_str().unwrap(), "--size", "2B", "--json"]);
    assert!(split.status.success());
    let hash = json_output(&split)["data"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let manifest = temp.path().join("sample.7z.afparts.json");
    assert!(
        run(&[
            "reshard",
            manifest.to_str().unwrap(),
            "--size",
            "3B",
            "--basename",
            "backup.7z",
            "--json"
        ])
        .status
        .success()
    );
    fs::remove_file(temp.path().join("sample.7z.002")).unwrap();
    let backup = temp.path().join("backup.7z.afparts.json");
    let args = [
        "rescue",
        "--target-manifest",
        manifest.to_str().unwrap(),
        "--source-manifest",
        backup.to_str().unwrap(),
        "--part",
        "2",
        "--json",
    ];
    let count = fs::read_dir(temp.path()).unwrap().count();
    let mut dry = args.to_vec();
    dry.push("--dry-run");
    let plan = json_output(&run(&dry));
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["data"]["plan"]["spans"].as_array().unwrap().len(), 2);
    assert_eq!(plan["data"]["content_read"], false);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), count);
    let mut actual = args.to_vec();
    actual.extend(["--expected-sha256", &hash]);
    let result = run(&actual);
    assert!(result.status.success());
    let receipt = json_output(&result);
    assert_eq!(receipt["verification_level"], "verified_against_original");
    assert_eq!(receipt["data"]["has_external_digest"], true);
    assert_eq!(
        receipt["data"]["target_range"],
        json!({"start":2,"end_exclusive":4})
    );
    assert_eq!(fs::read(temp.path().join("sample.7z.002")).unwrap(), [2, 3]);
    assert!(
        run(&[
            "verify",
            manifest.to_str().unwrap(),
            "--expected-sha256",
            &hash,
            "--json"
        ])
        .status
        .success()
    );
    let recovered = run(&[
        "recover",
        receipt["data"]["staging_directory"].as_str().unwrap(),
        "--json",
    ]);
    assert!(recovered.status.success());
    assert_eq!(
        json_output(&recovered)["data"]["verification_scope"],
        "target_range_only"
    );
}

#[test]
fn rescue_missing_bytes_return_structured_ranges_without_repair_output() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.7z");
    create_sample(&path);
    assert!(
        run(&["split", path.to_str().unwrap(), "--size", "2B", "--json"])
            .status
            .success()
    );
    let manifest = temp.path().join("sample.7z.afparts.json");
    fs::remove_file(temp.path().join("sample.7z.002")).unwrap();
    let count = fs::read_dir(temp.path()).unwrap().count();
    let result = run(&[
        "rescue",
        "--target-manifest",
        manifest.to_str().unwrap(),
        "--source-manifest",
        manifest.to_str().unwrap(),
        "--part",
        "2",
        "--json",
    ]);
    assert!(!result.status.success());
    let error = json_output(&result);
    assert_eq!(error["error"]["code"], "MISSING_COVERAGE");
    assert_eq!(
        error["error"]["coverage_details"]["missing_ranges"],
        json!([{"start":2,"end_exclusive":4}])
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), count);
}

#[test]
fn rebuild_cli_plans_and_reconstructs_only_a_named_part_with_limited_verification() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("sample.7z");
    create_sample(&original);
    assert!(
        run(&[
            "split",
            original.to_str().unwrap(),
            "--size",
            "2B",
            "--json"
        ])
        .status
        .success()
    );
    let manifest = temp.path().join("sample.7z.afparts.json");
    let args = [
        "rebuild-part",
        "--original",
        original.to_str().unwrap(),
        "--manifest",
        manifest.to_str().unwrap(),
        "--part",
        "2",
        "--basename",
        "rebuilt.7z.002",
        "--json",
    ];
    let count = fs::read_dir(temp.path()).unwrap().count();
    let mut plan_args = args.to_vec();
    plan_args.push("--dry-run");
    let plan = run(&plan_args);
    assert!(plan.status.success());
    let plan = json_output(&plan);
    assert_eq!(plan["status"], "planned");
    assert_eq!(
        plan["data"]["plan"]["range"],
        json!({"start":2,"end_exclusive":4})
    );
    assert_eq!(plan["data"]["plan"]["space"]["estimated_payload_bytes"], 2);
    assert_eq!(plan["data"]["content_read"], false);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), count);
    let result = run(&args);
    assert!(result.status.success());
    let value = json_output(&result);
    assert_eq!(value["operation"], "rebuild-part");
    assert_eq!(
        value["verification_level"],
        "verified_against_available_sources"
    );
    assert_eq!(value["data"]["verification_scope"], "target_range_only");
    assert_eq!(value["data"]["source_size"], 2);
    assert_eq!(
        fs::read(temp.path().join("rebuilt.7z.002")).unwrap(),
        [2, 3]
    );
    assert!(value["data"]["manifest_path"].is_null());
    let stage = value["data"]["staging_directory"].as_str().unwrap();
    let recovered = run(&["recover", stage, "--json"]);
    assert!(recovered.status.success());
    let recovered = json_output(&recovered);
    assert_eq!(recovered["data"]["part_count"], 1);
    assert_eq!(recovered["data"]["verification_scope"], "target_range_only");
    assert_eq!(
        recovered["data"]["target_range"],
        json!({"start":2,"end_exclusive":4})
    );
    let conflict = run(&args);
    assert_eq!(conflict.status.code(), Some(4));
    assert_eq!(json_output(&conflict)["error"]["code"], "OUTPUT_EXISTS");
}

#[test]
fn coverage_cli_is_read_only_and_its_human_map_is_bilingual() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("sample.7z");
    create_sample(&original);
    assert!(
        run(&[
            "split",
            original.to_str().unwrap(),
            "--size",
            "2B",
            "--json"
        ])
        .status
        .success()
    );
    let manifest = temp.path().join("sample.7z.afparts.json");
    let args = [
        "coverage",
        "--target-manifest",
        manifest.to_str().unwrap(),
        "--source-manifest",
        manifest.to_str().unwrap(),
        "--part",
        "2",
    ];
    let count = fs::read_dir(temp.path()).unwrap().count();
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let result = run(&json_args);
    assert!(result.status.success());
    let result = json_output(&result);
    assert_eq!(result["operation"], "coverage");
    assert_eq!(result["status"], "inspected");
    assert_eq!(result["verification_level"], "unverified");
    assert_eq!(result["data"]["covered_bytes"], 2);
    assert_eq!(result["data"]["complete_coverage"], true);
    assert_eq!(result["data"]["writes_performed"], false);
    let mut metadata = json_args.clone();
    metadata.push("--metadata-only");
    let pending = json_output(&run(&metadata));
    assert_eq!(pending["data"]["complete_coverage"], false);
    assert_eq!(pending["data"]["content_read"], false);
    for (lang, label) in [
        ("en", "covered"),
        ("zh-CN", "已覆盖"),
        ("both", "已覆盖 / covered"),
    ] {
        let mut human = args.to_vec();
        human.extend(["--lang", lang]);
        let result = run(&human);
        assert!(result.status.success());
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains(label));
        assert!(text.contains("[2, 4)"));
    }
    fs::remove_file(temp.path().join("sample.7z.002")).unwrap();
    let result = json_output(&run(&json_args));
    assert_eq!(result["data"]["complete_coverage"], false);
    assert_eq!(
        result["data"]["missing_ranges"],
        json!([{"start":2,"end_exclusive":4}])
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), count - 1);
}

#[test]
fn coverage_and_rebuild_missing_manifests_and_unsupported_bypasses_never_write() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.7z");
    create_sample(&path);
    for args in [
        vec![
            "coverage",
            "--target-manifest",
            path.to_str().unwrap(),
            "--source-manifest",
            path.to_str().unwrap(),
            "--json",
        ],
        vec![
            "rebuild-part",
            "--original",
            path.to_str().unwrap(),
            "--manifest",
            path.to_str().unwrap(),
            "--part",
            "1",
            "--json",
        ],
        vec![
            "rebuild-part",
            "--original",
            path.to_str().unwrap(),
            "--manifest",
            path.to_str().unwrap(),
            "--part",
            "1",
            "--force",
            "--json",
        ],
    ] {
        let result = run(&args);
        assert!(!result.status.success());
        let value = json_output(&result);
        assert_eq!(value["status"], "error");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}

#[test]
fn reshard_cli_plans_without_writes_then_creates_a_new_verified_group() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.7z");
    create_sample(&path);
    let split = run(&["split", path.to_str().unwrap(), "--size", "2B", "--json"]);
    assert!(split.status.success());
    let digest = json_output(&split)["data"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = temp.path().join("sample.7z.002");
    let count = fs::read_dir(temp.path()).unwrap().count();
    let args = [
        "reshard",
        input.to_str().unwrap(),
        "--size",
        "3B",
        "--basename",
        "new.7z",
        "--policy",
        "balanced",
        "--tolerance-percent",
        "0",
        "--align",
        "1B",
        "--json",
    ];
    let mut dry = args.to_vec();
    dry.push("--dry-run");
    let plan = run(&dry);
    assert!(plan.status.success());
    let plan = json_output(&plan);
    assert_eq!(plan["operation"], "reshard");
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["verification_level"], "unverified");
    assert_eq!(plan["data"]["writes_performed"], false);
    assert_eq!(plan["data"]["content_read"], false);
    assert_eq!(plan["data"]["creates_complete_intermediate"], false);
    assert_eq!(plan["data"]["part_count"], 2);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), count);
    let mut args = args.to_vec();
    args.extend(["--expected-sha256", &digest]);
    let actual = run(&args);
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stdout)
    );
    let actual = json_output(&actual);
    assert_eq!(actual["verification_level"], "verified_against_original");
    assert_eq!(actual["data"]["part_count"], 2);
    assert_eq!(actual["data"]["sha256"], digest);
    assert!(!temp.path().join("new.7z").exists());
    let manifest: Value =
        serde_json::from_slice(&fs::read(temp.path().join("new.7z.afparts.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["original_filename"], "sample.7z");
    let inspect = run(&["inspect", input.to_str().unwrap(), "--json"]);
    assert!(
        json_output(&inspect)["data"]["supported_operations"]
            .as_array()
            .unwrap()
            .contains(&json!("reshard"))
    );
}

#[test]
fn reshard_cli_requires_a_new_name_and_manifest_and_rejects_split_only_flags() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("raw.zip.001");
    fs::write(&path, b"hello").unwrap();
    for (extra, code) in [
        (vec![], "INVALID_ARGUMENT"),
        (vec!["--basename", "new.zip"], "MANIFEST_REQUIRED"),
        (
            vec!["--basename", "new.zip", "--allow-any-file"],
            "INVALID_ARGUMENT",
        ),
    ] {
        let mut args = vec!["reshard", path.to_str().unwrap(), "--size", "3B", "--json"];
        args.extend(extra);
        let result = run(&args);
        assert!(!result.status.success());
        assert_eq!(json_output(&result)["error"]["code"], code);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}

#[test]
fn dry_run_is_a_complete_read_only_plan_in_unicode_directory() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("样本.zip");
    create_sample(&path);
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--size",
        "2B",
        "--dry-run",
        "--json",
    ]);
    assert!(output.status.success());
    let value = json_output(&output);
    assert_eq!(value["operation"], "split");
    assert_eq!(value["status"], "planned");
    assert_eq!(value["data"]["part_count"], 3);
    assert_eq!(value["data"]["estimated_payload_bytes"], 5);
    assert_eq!(value["data"]["free_space_checked"], true);
    assert!(value["data"]["available_space_bytes"].as_u64().is_some());
    assert_eq!(value["data"]["writes_performed"], false);
    assert_eq!(value["planned_output"].as_array().unwrap().len(), 4);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(fs::read(path).unwrap(), [0, 1, 2, 3, 255]);
}

#[test]
fn actual_split_verify_and_merge_return_verified_json_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.7z");
    create_sample(&path);
    let output = run(&["split", path.to_str().unwrap(), "--size", "2B", "--json"]);
    assert!(output.status.success());
    let split = json_output(&output);
    assert_eq!(split["status"], "success");
    assert_eq!(split["verification_level"], "verified_against_original");
    #[cfg(windows)]
    assert_eq!(
        split["data"]["staged_payload_links_scheduled_for_removal"],
        4
    );
    let manifest = temp.path().join("sample.7z.afparts.json");
    let output = run(&["verify", manifest.to_str().unwrap(), "--json"]);
    assert!(output.status.success());
    assert_eq!(
        json_output(&output)["verification_level"],
        "verified_against_available_sources"
    );
    let output = run(&[
        "merge",
        manifest.to_str().unwrap(),
        "--basename",
        "restored.7z",
        "--expected-sha256",
        split["data"]["sha256"].as_str().unwrap(),
        "--json",
    ]);
    assert!(output.status.success());
    assert_eq!(
        json_output(&output)["verification_level"],
        "verified_against_original"
    );
    assert_eq!(
        fs::read(temp.path().join("restored.7z")).unwrap(),
        [0, 1, 2, 3, 255]
    );
    assert_eq!(fs::read(path).unwrap(), [0, 1, 2, 3, 255]);
}

#[test]
fn output_conflicts_leave_existing_files_intact_and_suggest_new_name() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.zip");
    create_sample(&path);
    fs::write(temp.path().join("sample.zip.001"), b"keep").unwrap();
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--size",
        "2",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json_output(&output)["error"]["code"], "OUTPUT_EXISTS");
    assert_eq!(
        fs::read(temp.path().join("sample.zip.001")).unwrap(),
        b"keep"
    );
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--basename",
        "new.zip",
        "--size",
        "2",
        "--dry-run",
        "--json",
    ]);
    assert!(output.status.success());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn inspect_never_claims_archive_or_hash_verification() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("not-a-real.zip");
    create_sample(&path);
    let output = run(&["inspect", path.to_str().unwrap(), "--json"]);
    assert!(output.status.success());
    let value = json_output(&output);
    assert_eq!(value["data"]["identification"], "filename_only");
    assert_eq!(value["data"]["content_read"], false);
    assert_eq!(value["data"]["supported_operations"], json!(["split"]));
}

#[test]
fn unknown_arguments_and_bad_units_have_private_bilingual_json_errors() {
    for args in [
        vec!["split", "PRIVATE-PATH.zip", "--unknown", "--json"],
        vec!["--lang", "bad", "--json"],
        vec!["--json"],
        vec!["reshard", "PRIVATE-PATH.zip", "--json"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        let value = json_output(&output);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(!value["error"]["message"]["en"].as_str().unwrap().is_empty());
        assert!(
            !value["error"]["message"]["zh_cn"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-PATH"));
    }
}

#[test]
fn human_help_and_errors_can_be_chinese_english_or_both() {
    for lang in ["en", "zh-CN", "both"] {
        let output = run(&["--lang", lang, "--help"]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.contains("离线"), lang != "en");
        assert_eq!(text.contains("Offline"), lang != "zh-CN");
        let output = run(&["--lang", lang, "unknown"]);
        assert!(output.stdout.is_empty());
        let text = String::from_utf8(output.stderr).unwrap();
        assert_eq!(text.contains("命令或选项无效"), lang != "en");
        assert_eq!(text.contains("Invalid command"), lang != "zh-CN");
    }
}

#[test]
fn json_help_and_version_are_also_json() {
    for arg in ["--help", "--version"] {
        let output = run(&["--json", arg]);
        assert!(output.status.success());
        assert_eq!(json_output(&output)["status"], "info");
    }
}

#[test]
fn recover_dry_run_and_actual_resume_keep_the_json_contract() {
    use easy_archive_core::{IoPoint, SplitPolicy, SplitRequest, TaskContext, split_file};
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("sample.zip");
    create_sample(&input);
    let request = SplitRequest {
        input,
        output_dir: None,
        basename: None,
        policy: SplitPolicy::Hard { max_bytes: 2 },
        alignment: None,
        allow_any_file: false,
    };
    let mut fault = |point| {
        if point == IoPoint::AfterPublish {
            Err(std::io::Error::other("injected"))
        } else {
            Ok(())
        }
    };
    let error = split_file(
        &request,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    let task = easy_archive_core::transaction::inspect_transactions(temp.path())
        .unwrap()
        .into_iter()
        .find(|t| t.task_id == error.recovery_task_id)
        .unwrap()
        .directory;
    let before_count = fs::read_dir(&task).unwrap().count();
    let dry = run(&["recover", task.to_str().unwrap(), "--dry-run", "--json"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stdout)
    );
    let value = json_output(&dry);
    assert_eq!(value["operation"], "recover");
    assert_eq!(value["status"], "planned");
    assert_eq!(value["verification_level"], "unverified");
    assert_eq!(value["data"]["content_read"], true);
    assert_eq!(value["data"]["writes_performed"], false);
    assert_eq!(fs::read_dir(&task).unwrap().count(), before_count);
    let actual = run(&["recover", task.to_str().unwrap(), "--json"]);
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stdout)
    );
    let value = json_output(&actual);
    assert_eq!(value["status"], "success");
    assert_eq!(
        value["verification_level"],
        "verified_against_available_sources"
    );
    assert!(temp.path().join("sample.zip.afparts.json").exists());
}

fn unfinished_copy(root: &Path) -> std::path::PathBuf {
    use easy_archive_core::{IoPoint, SplitPolicy, SplitRequest, TaskContext, split_file};
    let input = root.join("sample.zip");
    create_sample(&input);
    let request = SplitRequest {
        input,
        output_dir: None,
        basename: None,
        policy: SplitPolicy::Hard { max_bytes: 2 },
        alignment: None,
        allow_any_file: false,
    };
    let mut writes = 0;
    let mut fault = |point| {
        if point == IoPoint::PayloadWrite {
            writes += 1;
            if writes == 2 {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
        }
        Ok(())
    };
    split_file(
        &request,
        &mut TaskContext {
            io_fault: Some(&mut fault),
            ..TaskContext::default()
        },
    )
    .unwrap_err();
    easy_archive_core::transaction::inspect_transactions(root)
        .unwrap()
        .remove(0)
        .directory
}

#[test]
fn restart_json_plans_without_writes_and_finishes_the_same_task() {
    let temp = tempfile::tempdir().unwrap();
    let task = unfinished_copy(temp.path());
    let count = fs::read_dir(&task).unwrap().count();
    let old = fs::read(task.join("payload-00001")).unwrap();
    let output = run(&["restart", task.to_str().unwrap(), "--dry-run", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = json_output(&output);
    assert_eq!(value["status"], "planned");
    assert_eq!(value["verification_level"], "unverified");
    assert_eq!(value["data"]["content_read"], false);
    assert_eq!(value["data"]["writes_performed"], false);
    assert_eq!(value["data"]["previous_attempts_retained"], true);
    assert_eq!(fs::read_dir(&task).unwrap().count(), count);
    let output = run(&["restart", task.to_str().unwrap(), "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let actual = json_output(&output);
    assert_eq!(actual["status"], "success");
    assert_eq!(
        actual["data"]["transaction_id"],
        value["data"]["transaction_id"]
    );
    assert_eq!(fs::read(task.join("payload-00001")).unwrap(), old);
    let manifest = temp.path().join("sample.zip.afparts.json");
    assert!(
        run(&["verify", manifest.to_str().unwrap(), "--json"])
            .status
            .success()
    );
}

#[test]
fn restart_json_conflict_refuses_overwrite_and_does_not_create_an_attempt() {
    let temp = tempfile::tempdir().unwrap();
    let task = unfinished_copy(temp.path());
    let conflict = temp.path().join("sample.zip.001");
    fs::write(&conflict, b"KEEP").unwrap();
    let before = fs::read_dir(&task).unwrap().count();
    let output = run(&["restart", task.to_str().unwrap(), "--json"]);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json_output(&output)["error"]["code"], "OUTPUT_EXISTS");
    assert_eq!(fs::read_dir(&task).unwrap().count(), before);
    assert_eq!(fs::read(conflict).unwrap(), b"KEEP");
}

#[test]
fn balanced_options_and_exact_decimal_units_work() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.zip");
    fs::write(&path, [0; 205]).unwrap();
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--size",
        "0.1KB",
        "--policy",
        "balanced",
        "--dry-run",
        "--json",
    ]);
    assert!(output.status.success());
    assert_eq!(json_output(&output)["data"]["part_count"], 2);
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--size",
        "100",
        "--max-size",
        "100",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_output(&output)["error"]["code"], "INVALID_ARGUMENT");
}

#[test]
fn size_errors_missing_input_and_native_zip_volumes_have_stable_codes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.zip");
    create_sample(&path);
    let output = run(&[
        "split",
        path.to_str().unwrap(),
        "--size",
        "1.1B",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_output(&output)["error"]["code"], "INVALID_SIZE");
    let output = run(&[
        "inspect",
        temp.path().join("missing.zip").to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(json_output(&output)["error"]["code"], "INPUT_NOT_FOUND");
    let native = temp.path().join("sample.z01");
    create_sample(&native);
    let output = run(&["inspect", native.to_str().unwrap(), "--json"]);
    assert_eq!(
        json_output(&output)["error"]["code"],
        "UNSUPPORTED_VOLUME_FORMAT"
    );
}

#[test]
fn merge_dry_run_and_transaction_reports_write_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let part = temp.path().join("a.zip.001");
    fs::write(&part, b"abc").unwrap();
    let output = run(&["merge", part.to_str().unwrap(), "--dry-run", "--json"]);
    assert!(output.status.success());
    let plan = json_output(&output);
    assert_eq!(plan["status"], "planned");
    assert_eq!(plan["verification_level"], "unverified");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    let output = run(&["transactions", temp.path().to_str().unwrap(), "--json"]);
    assert!(output.status.success());
    assert_eq!(json_output(&output)["data"]["tasks"], json!([]));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn failed_hash_and_force_flags_never_report_success_or_write_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let part = temp.path().join("a.zip.001");
    fs::write(&part, b"abc").unwrap();
    let wrong = "0".repeat(64);
    let output = run(&[
        "merge",
        part.to_str().unwrap(),
        "--expected-sha256",
        &wrong,
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        json_output(&output)["error"]["code"],
        "SOURCE_HASH_MISMATCH"
    );
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    let input = temp.path().join("source.zip");
    create_sample(&input);
    for flag in ["--force", "--yes"] {
        let output = run(&[
            "split",
            input.to_str().unwrap(),
            "--size",
            "2B",
            flag,
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(json_output(&output)["error"]["code"], "INVALID_ARGUMENT");
    }
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn selected_part_cli_sorts_external_paths_and_preserves_json_contract() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("drive-a");
    let b = temp.path().join("drive-b");
    let out = temp.path().join("out");
    for d in [&a, &b, &out] {
        fs::create_dir(d).unwrap();
    }
    let first = a.join("sample.zip.001");
    let second = b.join("sample.zip.002");
    fs::write(&first, b"hel").unwrap();
    fs::write(&second, b"lo").unwrap();
    let selected = [
        "--part-file",
        second.to_str().unwrap(),
        "--part-file",
        first.to_str().unwrap(),
    ];
    let mut args = vec!["inspect", first.to_str().unwrap()];
    args.extend(selected);
    args.push("--json");
    let inspection = json_output(&run(&args));
    assert_eq!(inspection["data"]["group"]["part_count"], 2);
    assert_eq!(inspection["data"]["group"]["explicit_selection"], true);
    let mut args = vec!["merge", first.to_str().unwrap()];
    args.extend(selected);
    args.extend(["--output-dir", out.to_str().unwrap(), "--dry-run", "--json"]);
    let planned = run(&args);
    assert!(planned.status.success());
    assert_eq!(json_output(&planned)["status"], "planned");
    assert!(fs::read_dir(&out).unwrap().next().is_none());
    args.retain(|a| *a != "--dry-run");
    let merged = run(&args);
    assert!(merged.status.success());
    let result = json_output(&merged);
    assert_eq!(
        result["verification_level"],
        "verified_against_available_sources"
    );
    assert_eq!(fs::read(out.join("sample.zip")).unwrap(), b"hello");
    let mut args = vec!["verify", first.to_str().unwrap()];
    args.extend(selected);
    args.push("--json");
    assert!(run(&args).status.success());
    assert_eq!(fs::read(&first).unwrap(), b"hel");
    assert_eq!(fs::read(&second).unwrap(), b"lo");
}

#[test]
fn selected_part_cli_refuses_implicit_output_or_mixed_groups() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("sample.zip.001");
    let other = temp.path().join("other.zip.002");
    fs::write(&first, b"a").unwrap();
    fs::write(&other, b"b").unwrap();
    let output = run(&[
        "merge",
        first.to_str().unwrap(),
        "--part-file",
        first.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        json_output(&output)["error"]["code"],
        "OUTPUT_DIRECTORY_REQUIRED"
    );
    let output = run(&[
        "verify",
        first.to_str().unwrap(),
        "--part-file",
        first.to_str().unwrap(),
        "--part-file",
        other.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(json_output(&output)["error"]["code"], "PART_GROUP_MISMATCH");
}
