# 便捷压缩助手_V0.0.1 / Easy Archive Assistant

[简体中文](README.zh-CN.md)
## 下载与使用 / Download & Run

👉 [下载 Windows 便携版](https://github.com/NyanodaX/Easy-Archive-Assistant/releases)

下载 Assets 中的“便捷压缩助手_V0.0.1.zip”，
完整解压后运行“便捷压缩助手_V0.0.1.exe”。
需要已安装 Microsoft Edge WebView2 运行时。

Offline raw-byte splitting, merging, direct re-sharding and SHA-256 verification for existing ZIP/7z files, with exact single-part rebuilding, coverage maps, verified cross-set rescue and recoverable publication. **Chinese/English Tauri desktop and independent CLI** are implemented. The user selected the name and initial version; ArchiveFlow in historical specifications is the old working name.

Volumes can be added from **different folders or USB drives**, over multiple selections or drops. Check them first, then choose the output directory. The desktop uses a plain, traditional tabbed layout. Use the cumulative volume list to select every part; choose a manifest explicitly and keep all source drives connected.

## Run

For the portable package, unpack its whole directory and run `便捷压缩助手_V0.0.1.exe`. Keep `WebView2Loader.dll` beside it. Desktop requires an already installed WebView2 runtime; CLI `easy-archive-assistant.exe` works independently. The interface has traditional tabs, progress and cancellation.

Build with Rust 1.90+ (tested 1.99.1), Node 24 and the [Tauri platform prerequisites](https://v2.tauri.app/start/prerequisites/). The wrappers use private tools in ignored `.tools/` when present, otherwise system Cargo/npm.

```powershell
powershell -NoProfile -File scripts/npm.ps1 ci
powershell -NoProfile -File scripts/npm.ps1 run build
powershell -NoProfile -File scripts/dev.ps1 build --workspace --release
.\target\release\easy-archive-assistant.exe --lang both --help
# Generate dependency notices before packaging; see README.md.
powershell -NoProfile -File scripts/package-windows.ps1
```

Packaging creates a fresh directory under `.artifacts/releases/` with desktop, CLI, loader, bilingual documentation, dependency notices, per-file SHA-256 checksums, ZIP and ZIP checksum. This is an unsigned portable development build, without installer or public release. The publisher has confirmed the [MIT license](LICENSE); retain this notice and all third-party licenses when redistributing.

Replace the example paths with your own. Output directories must exist. Plan first, then omit `--dry-run` to execute. Original inputs are preserved.

```powershell
.\target\release\easy-archive-assistant.exe split "D:\Backups\sample.7z" --size 1.5GiB --dry-run --json
.\target\release\easy-archive-assistant.exe split "D:\Backups\sample.7z" --size 1.5GiB --json
.\target\release\easy-archive-assistant.exe verify "D:\Backups\sample.7z.afparts.json" --json
.\target\release\easy-archive-assistant.exe merge "D:\Backups\sample.7z.001" --basename sample.restored.7z --json
.\target\release\easy-archive-assistant.exe reshard "D:\Backups\sample.7z.afparts.json" --size 1.5GiB --basename sample.small.7z --dry-run --json
.\target\release\easy-archive-assistant.exe reshard "D:\Backups\sample.7z.afparts.json" --size 1.5GiB --basename sample.small.7z --json
.\target\release\easy-archive-assistant.exe rebuild-part --original "D:\Backups\sample.7z" --manifest "D:\Backups\sample.7z.afparts.json" --part 2 --basename sample.rebuilt.7z.002 --dry-run --json
.\target\release\easy-archive-assistant.exe rebuild-part --original "D:\Backups\sample.7z" --manifest "D:\Backups\sample.7z.afparts.json" --part 2 --basename sample.rebuilt.7z.002 --json
.\target\release\easy-archive-assistant.exe coverage --target-manifest "D:\Backups\sample.7z.afparts.json" --source-manifest "D:\Backups\sample.small.7z.afparts.json" --part 2 --json
.\target\release\easy-archive-assistant.exe rescue --target-manifest "D:\Backups\sample.7z.afparts.json" --source-manifest "D:\Backups\sample.small.7z.afparts.json" --part 2 --basename sample.repaired.7z.002 --dry-run --json
.\target\release\easy-archive-assistant.exe transactions "D:\Backups" --json
.\target\release\easy-archive-assistant.exe recover "D:\Backups\.eaa-TASK_ID" --dry-run --json
.\target\release\easy-archive-assistant.exe recover "D:\Backups\.eaa-TASK_ID" --json
.\target\release\easy-archive-assistant.exe restart "D:\Backups\.eaa-TASK_ID" --dry-run --json
.\target\release\easy-archive-assistant.exe restart "D:\Backups\.eaa-TASK_ID" --json
```

`--lang en|zh-CN|both` selects human output; the default is bilingual. JSON always includes both message languages. Response schema **v2** and manifest **v1** remain in use. See the CLI contract.

## Behavior

- `inspect INPUT` reads metadata. `split` uses an 8 MiB buffer and creates `.001`, `.002`, … and `.afparts.json`. Conflicts stop processing; choose a new `--basename` or directory. No overwrite flag exists.
- `--policy hard` evenly distributes bytes within `--size`. `balanced` permits 5% tolerance by default, with `--tolerance-percent` and optional `--max-size`. `--align 1MiB` requests best-effort alignment. Empty input produces one empty part.
- Sizes use exact decimal arithmetic: `1.5GiB`, `500MB`, integer bytes. Binary MiB/GiB differ from decimal MB/GB. Non-whole bytes, zero capacities and overflow are rejected.
- `merge`/`verify` accept any canonical part or its manifest. They reject gaps, duplicate numbering, extra manifest parts, malformed manifests and hash mismatches. Without a manifest or trusted digest, a missing final volume cannot be detected.
- `--expected-sha256` on merge/verify compares with an externally trusted original digest. A manifest or recovery record alone does not authenticate an original. SHA-256 proves byte equality, not archive validity, signatures, provenance or virus safety.
- `reshard` requires a complete manifest-backed group and an explicit `--basename`. It streams across existing part boundaries into new volumes, using one 8 MiB buffer and no complete intermediate archive. It still needs space for the new payload. Inputs are verified before copying and again before publication; output volumes are fully reread. Without `--expected-sha256`, success means consistency with available sources, not authenticated provenance.
- `rebuild-part` reads only the manifest-selected range from an intact-size original, checks the part hash, and writes a new file. Existing parts are never replaced. Its receipt and recovery explicitly report `verification_scope:target_range_only`; the complete original digest is not verified.
- `coverage` reports covered, missing, pending and conflicting half-open byte ranges. It is read-only and never repairs files. Only complete source groups with verified whole-stream hashes can provide covered ranges; partial groups remain pending even when their part hashes match. Volume surgery details.
- `rescue` requires complete backup groups identifying the same original size/SHA-256. It verifies every source group, copies only the target interval across old part boundaries, rereads the output and checks the target part digest. Optional `--expected-sha256` checks the actually verified complete backup against an independently retained original digest. Existing damaged parts are preserved; choose a new name/folder. The receipt reports `target_range_only`; manifest provenance remains unauthenticated. Incomplete groups cannot authorize repair.
- `.zip.001` is a raw slice; native `.z01/.z02/.zip` disk volumes are unsupported. The engine does not decompress, recompress, decrypt, inspect archive members, execute contents or upload data.

## Space, cancellation and recovery

Split/merge/re-shard plans report payload, conservative overhead and available space. A null space value means the filesystem query failed. Execution checks again and refuses a known shortfall. Estimates do not reserve space; later disk-full errors are still handled. Ctrl+C requests cancellation.

Publication requires hard links (tested on Windows NTFS), never replaces destinations, and publishes the manifest last. A multi-file group is not atomic. New tasks use `.eaa-<32-hex-id>/`; legacy `.archiveflow-*` directories are still detected. Files, immutable recovery plans and journals are flushed. Windows directory-entry durability under sudden power loss is not claimed.

`recover TASK_DIRECTORY --dry-run` fully reads and verifies recorded sources, staged payloads, assembled SHA-256, manifest consistency and destination identities without writing. Actual recovery repeats these checks, refuses a live task lock or unknown conflicting files, and publishes only missing destinations. An already published output must be the same verified file identity, not just matching bytes. Recovery reports `verified_against_available_sources`; local records are not trust anchors.

`restart TASK_DIRECTORY --dry-run` checks a current-format startup record, directory/source snapshots, planned names, other pending tasks and free space without reading payloads or writing anything. Actual restart copies from the beginning into new exclusive attempt files in the same locked task. It preserves previous attempts and all unknown files, needs space for a full new output, and never appends to partial data. If complete staging already exists, use `recover` instead. Split, merge, re-shard and exact range rebuilding, empty files, alternate names and balanced policies are supported.

On Windows, successful live split/merge/re-shard/restart operations may remove their newly created, verified staging links after the completion marker is flushed. An in-memory creation ticket and a still-open DELETE-capable handle are required; persisted records cannot authorize cleanup. Logs, previous attempts, unknown files and formal outputs remain. Permissions or cleanup errors retain links. `staged_payload_links_scheduled_for_removal` reports this invocation's count; other readers can delay deletion or space reclamation. Recovery never deletes recorded files. There is no general cleanup or deletion command.

A complete `start-draft.json` can now support restart if `start.json` was never exposed. It must pass the same bounded parsing, directory identity, source snapshot, option and conflict checks. A malformed final record never falls back to a draft. Incomplete drafts, missing owner records and legacy tasks stay untouched; inspect them and use another directory. Do not hide a failed task to present a partial group as complete. Recovery details.

Limits: 10,000 parts, 4 MiB manifests, 16 MiB recovery plans, 20,000 directory entries per scan; up to 8 coverage/rescue source groups. Unsafe names, UNC/device paths, symlinks, reparse points and non-regular files are rejected. Rust minimum 1.90 is declared but only 1.99.1 was run. Desktop IPC encodes integers as decimal strings to preserve u64 precision; CLI v2 uses JSON integers.

## Development checks

```powershell
powershell -NoProfile -File scripts/npm.ps1 test
powershell -NoProfile -File scripts/npm.ps1 run build
powershell -NoProfile -File scripts/dev.ps1 fmt --all --check
powershell -NoProfile -File scripts/dev.ps1 clippy --workspace --all-targets -- -D warnings
powershell -NoProfile -File scripts/dev.ps1 test --workspace
# Optional large acceptance / 可选大文件验收：约需 8 GiB 空间
powershell -NoProfile -File scripts/dev.ps1 test --release -p easy-archive-core --test reshard large_2gib_to_1point5gib -- --ignored --exact
powershell -NoProfile -File scripts/dev.ps1 test --release -p easy-archive-core --test rebuild large_original_offset_reads_only_target_range -- --ignored --exact
powershell -NoProfile -File scripts/dev.ps1 build --workspace --release
powershell -NoProfile -File scripts/smoke-windows.ps1
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1 -Phase draft
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1 -Phase cleanup
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1 -Phase reshard
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1 -Phase rebuild-copy
powershell -NoProfile -File scripts/smoke-interrupted-windows.ps1 -Phase rebuild-publish
```

Tests use owned temporary files and no network. Process-kill tests exercise startup-record persistence, copying, durable plan, link-before-journal and pre-marker boundaries. Controlled IO failures cover reads, writes, flushes and publication checkpoints. A scripted partial-write adapter verifies short writes, interruption, zero writes, cancellation and errors after a written prefix. Physical disk exhaustion, surprise drive removal, true power loss, actual OS short writes, Linux/macOS and a clean-machine install are not claimed tested.



M5 passed 130 Rust tests and the actual 6 GiB rescue gate. M6 adds desktop bridge tests, frontend tests, offline release-webview integration checks, restricted-path portable execution and CI definitions. Exact current counts and limitations are in the linked evidence. CI definitions have not run remotely; OS drag delivery, screen-reader speech, clean VM, Linux/macOS desktop, physical disk-full/drive-disconnect/power-loss checks are separate. The encrypted test fixture retains its [PSF attribution](tests/fixtures/README.md). [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Changes](CHANGELOG.md).

## Build/package details

The source upload intentionally omits internal development documents and agent instructions. Use the commands above with Rust, Node, MSVC C++ tools/Windows SDK and WebView2. Core/CLI work independently of the GUI.

Before packaging, generate platform-filtered notices into a fresh directory:

```powershell
cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc > metadata-windows.json
python scripts/dependency-notices.py metadata-windows.json .artifacts/notices
powershell -NoProfile -File scripts/package-windows.ps1 -NoticesDirectory .artifacts/notices
```

Local validation before this source curation: 144 Rust tests, 5 frontend tests and 38 desktop checks passed. The public snapshot changes documentation/packaging only; remote Actions must be checked separately. Physical USB removal, OS drag delivery and clean-machine certification were not tested. This pre-release is unsigned.

