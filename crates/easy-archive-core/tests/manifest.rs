use easy_archive_core::{ErrorCode, MAX_MANIFEST_BYTES, Manifest, parse_manifest};
use serde_json::{Value, json};

fn sample() -> Value {
    json!({
        "schema_version": 1, "tool": "ArchiveFlow", "original_filename": "样本.zip",
        "original_size": 6, "original_sha256": "a".repeat(64), "split_mode": "hard", "part_count": 2,
        "parts": [
            {"index": 1, "filename": "样本.zip.001", "offset": 0, "size": 3, "sha256": "b".repeat(64)},
            {"index": 2, "filename": "样本.zip.002", "offset": 3, "size": 3, "sha256": "c".repeat(64)}
        ],
        "created_at": "2026-10-23T12:00:00+08:00", "app_version": "0.1.0"
    })
}

fn parse(value: &Value) -> easy_archive_core::Result<Manifest> {
    parse_manifest(&serde_json::to_vec(value).unwrap())
}

#[test]
fn valid_manifest_round_trip_is_structural_only() {
    let manifest = parse(&sample()).unwrap();
    assert_eq!(
        parse_manifest(&serde_json::to_vec(&manifest).unwrap()).unwrap(),
        manifest
    );
}

#[test]
fn empty_input_has_exactly_one_empty_part() {
    let mut value = sample();
    value["original_size"] = json!(0);
    value["part_count"] = json!(1);
    value["parts"].as_array_mut().unwrap().truncate(1);
    value["parts"][0]["size"] = json!(0);
    assert!(parse(&value).is_ok());
    value["original_size"] = json!(1);
    assert!(parse(&value).is_err());
}

#[test]
fn hostile_names_and_duplicate_volume_names_are_rejected() {
    for name in [
        "../evil.001",
        "C:\\evil.001",
        "/tmp/evil.001",
        "a/b.001",
        "a\\b.001",
        "CON.001",
        "a:stream.001",
        "..",
        "x.001 ",
    ] {
        let mut value = sample();
        value["parts"][0]["filename"] = json!(name);
        assert_eq!(
            parse(&value).unwrap_err().code,
            ErrorCode::ManifestInvalid,
            "{name}"
        );
    }
    let mut value = sample();
    value["parts"][1]["filename"] = value["parts"][0]["filename"].clone();
    assert!(parse(&value).is_err());
    value = sample();
    value["original_filename"] = json!("../evil.zip");
    assert!(parse(&value).is_err());
    value = sample();
    value["parts"][1]["filename"] = json!("other.zip.002");
    assert!(parse(&value).is_err());
}

#[test]
fn gap_overlap_wrong_counts_or_hashes_and_bad_timestamps_are_rejected() {
    for (pointer, replacement) in [
        ("/schema_version", json!(2)),
        ("/part_count", json!(1)),
        ("/original_size", json!(7)),
        ("/parts/0/offset", json!(1)),
        ("/parts/1/offset", json!(2)),
        ("/parts/1/offset", json!(4)),
        ("/parts/1/index", json!(1)),
        ("/parts/1/size", json!(0)),
        ("/parts/0/sha256", json!("Z".repeat(64))),
        ("/original_sha256", json!("0".repeat(63))),
        ("/split_mode", json!("magic")),
        ("/created_at", json!("yesterday")),
        ("/created_at", json!("2026-02-30T10:00:00Z")),
        ("/parts", json!([])),
        ("/tool", json!("")),
        ("/app_version", json!("")),
    ] {
        let mut value = sample();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(parse(&value).is_err(), "{pointer}");
    }
}

#[test]
fn integer_overflow_and_wrong_json_types_are_rejected() {
    for replacement in [json!(-1), json!(1.5), json!("6"), json!(true), Value::Null] {
        let mut value = sample();
        value["original_size"] = replacement;
        assert!(parse(&value).is_err());
    }
    let mut value = sample();
    value["parts"][0]["size"] = json!(u64::MAX);
    value["parts"][1]["offset"] = json!(u64::MAX);
    assert!(parse(&value).is_err());
    let text = serde_json::to_string(&sample()).unwrap().replace(
        "\"original_size\":6",
        "\"original_size\":18446744073709551616",
    );
    assert!(parse_manifest(text.as_bytes()).is_err());
}

#[test]
fn duplicate_unknown_and_oversized_json_are_rejected() {
    let text = serde_json::to_string(&sample()).unwrap();
    let repeated = text.replacen('{', "{\"schema_version\":1,", 1);
    assert!(parse_manifest(repeated.as_bytes()).is_err());
    let mut value = sample();
    value["unrecognized"] = json!(true);
    assert!(parse(&value).is_err());
    assert!(parse_manifest(&vec![b' '; MAX_MANIFEST_BYTES + 1]).is_err());
    assert!(parse_manifest(b"{}").is_err());
    assert!(parse_manifest(b"\xff").is_err());
}
