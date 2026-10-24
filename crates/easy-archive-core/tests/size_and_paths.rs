use easy_archive_core::paths::{checked_path, classify_filename, plan_output_paths};
use easy_archive_core::{ErrorCode, InputKind, SourceFile, parse_size, validate_basename};
use std::fs;

#[test]
fn sizes_use_exact_binary_or_decimal_units() {
    for (text, expected) in [
        ("1", 1),
        (" 1 B ", 1),
        ("1KiB", 1024),
        ("1MiB", 1_048_576),
        ("1GiB", 1_073_741_824),
        ("1TiB", 1 << 40),
        ("1KB", 1000),
        ("500MB", 500_000_000),
        ("1.5GiB", 1_610_612_736),
        ("0.001KB", 1),
        ("18446744073709551615", u64::MAX),
        ("2 TB", 2_000_000_000_000),
    ] {
        assert_eq!(parse_size(text).unwrap(), expected, "{text}");
    }
}

#[test]
fn invalid_or_overflowing_sizes_never_round_or_wrap() {
    for value in [
        "",
        "0",
        "0B",
        "-1",
        "+1",
        "1e3",
        "1GBB",
        "1mb",
        "1.1B",
        ".5GiB",
        "1.GiB",
        "NaN",
        "inf",
        "1.2.3MB",
        "18446744073709551616",
        "18446744073709551615TiB",
        "1.0000000001GiB",
        "999999999999999999999999999999999999999999999999999999999999999999999999MB",
    ] {
        assert_eq!(
            parse_size(value).unwrap_err().code,
            ErrorCode::InvalidSize,
            "{value}"
        );
    }
}

#[test]
fn windows_portable_safe_basenames() {
    for name in [
        "原始 数据.7z",
        "archive.zip.001",
        "my file.zip",
        ".hidden.zip",
        "file..zip",
        "com10.7z",
    ] {
        assert!(validate_basename(name).is_ok(), "{name}");
    }
    for name in [
        "",
        ".",
        "..",
        "../evil",
        "a/b",
        "a\\b",
        "C:evil",
        "CON",
        "Con.txt",
        "NUL.7z",
        "LPT9.log",
        "COM¹.zip",
        "CONIN$",
        "CONOUT$",
        "x.",
        "x ",
        "a\nb",
        "a\0b",
        "a?b",
        "a*b",
        "a\"b",
        "a<b",
    ] {
        assert_eq!(
            validate_basename(name).unwrap_err().code,
            ErrorCode::UnsafePath,
            "{name:?}"
        );
    }
    assert!(validate_basename(&"a".repeat(241)).is_err());
}

#[test]
fn format_boundaries_are_explicit() {
    for name in ["A.ZIP", "a.7z"] {
        assert_eq!(classify_filename(name), InputKind::ArchiveBytes);
    }
    for name in ["a.zip.001", "a.7z.1000"] {
        assert_eq!(classify_filename(name), InputKind::RawVolume);
    }
    assert_eq!(classify_filename("a.z01"), InputKind::NativeZipVolume);
    assert_eq!(classify_filename("a.Z99"), InputKind::NativeZipVolume);
    assert_eq!(classify_filename("binary.bin"), InputKind::Other);
}

#[test]
fn source_checks_are_read_only_and_refuse_directories_or_missing_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("原始.zip");
    fs::write(&path, [0_u8, 1, 255]).unwrap();
    let source = SourceFile::inspect(&path).unwrap();
    assert_eq!(source.size, 3);
    assert!(source.require_splittable(false).is_ok());
    assert_eq!(fs::read(&path).unwrap(), [0, 1, 255]);
    assert_eq!(
        SourceFile::inspect(temp.path()).unwrap_err().code,
        ErrorCode::InputNotRegularFile
    );
    assert_eq!(
        SourceFile::inspect(&temp.path().join("missing.zip"))
            .unwrap_err()
            .code,
        ErrorCode::InputNotFound
    );
    assert!(checked_path(&temp.path().join(".."), true).is_err());
}

#[test]
fn dry_run_paths_check_conflicts_and_never_create_anything() {
    let temp = tempfile::tempdir().unwrap();
    let (parts, manifest) = plan_output_paths(temp.path(), "原始.7z", 1001).unwrap();
    assert!(parts[0].ends_with("原始.7z.001"));
    assert!(parts[1000].ends_with("原始.7z.1001"));
    assert!(manifest.ends_with("原始.7z.afparts.json"));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    fs::write(&manifest, b"keep").unwrap();
    assert_eq!(
        plan_output_paths(temp.path(), "原始.7z", 2)
            .unwrap_err()
            .code,
        ErrorCode::OutputExists
    );
    assert_eq!(fs::read(&manifest).unwrap(), b"keep");
    fs::create_dir(temp.path().join("other.zip.001")).unwrap();
    assert_eq!(
        plan_output_paths(temp.path(), "other.zip", 2)
            .unwrap_err()
            .code,
        ErrorCode::OutputExists
    );
    assert!(plan_output_paths(temp.path(), "x", 0).is_err());
    assert!(plan_output_paths(temp.path(), "../escape", 2).is_err());
}

#[test]
fn other_extensions_require_opt_in_and_volumes_are_never_splittable() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["x.bin", "x.zip.001", "x.z01"] {
        let path = temp.path().join(name);
        fs::write(&path, b"test").unwrap();
        let source = SourceFile::inspect(&path).unwrap();
        assert!(source.require_splittable(false).is_err());
        assert_eq!(source.require_splittable(true).is_ok(), name == "x.bin");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_and_linked_ancestors_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("real.zip"), b"test").unwrap();
    symlink(temp.path().join("real.zip"), temp.path().join("link.zip")).unwrap();
    assert_eq!(
        SourceFile::inspect(&temp.path().join("link.zip"))
            .unwrap_err()
            .code,
        ErrorCode::UnsafePath
    );
    symlink(temp.path(), temp.path().join("linked-dir")).unwrap();
    assert_eq!(
        SourceFile::inspect(&temp.path().join("linked-dir/real.zip"))
            .unwrap_err()
            .code,
        ErrorCode::UnsafePath
    );
}
