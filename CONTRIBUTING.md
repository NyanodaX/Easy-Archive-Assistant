# Contributing / 参与开发

Read README.md and the source before contributing. Preserve user files. Keep archive payloads opaque and all offsets exact. Never add extraction, compression, shell execution, uploads, telemetry, overwrite switches, or automatic deletion of historical staging.

请先阅读以上文档。优先保护原始数据；压缩包仅作为不透明字节流，不添加解压、重压缩、任意 shell、上传、遥测、覆盖或旧任务自动删除功能。

Use Rust 1.90 or newer (this release was tested with 1.99.1), Node 24.22.1, and the lockfiles. The desktop requires the [Tauri build prerequisites](https://v2.tauri.app/start/prerequisites/); CLI/core do not require a GUI. On Windows, the normal supported developer setup is MSVC plus Windows SDK; a private GNU/LLVM fallback was used for local validation; the public Windows workflow uses MSVC.

```sh
npm ci
npm test
npm run build
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release --workspace
```

Windows PowerShell wrappers: `scripts/npm.ps1` and `scripts/dev.ps1`. For headless core/CLI checks add `--exclude easy-archive-desktop` to workspace Cargo test/build/clippy commands. Do not commit `.tools/`, `.artifacts/`, `target/`, `dist/`, `node_modules/`, binaries, credentials, or private test paths.

在无桌面依赖环境使用 `--workspace --exclude easy-archive-desktop`。不要提交构建产物、凭据或私人路径。

Include the concrete problem, resulting behavior, relevant tests actually run, and remaining limitations in each change. Tests should cover data correctness, conflicts, cancellation, and recoverability for affected write paths. Keep generic errors path-free and bilingual; explicit plans/results may show selected paths. All desktop IPC integers are decimal strings; CLI v2 retains JSON integers. Never round u64 offsets through JavaScript Number.

每次修改说明具体行为、实际验证和剩余限制。涉及写入时覆盖正确性、冲突、取消及恢复；普通错误不携带路径，计划可显示用户所选路径；桌面 IPC 整数为十进制字符串。

The publisher confirmed the MIT license. Preserve the project copyright/license notice and all third-party notices when redistributing. No contributor agreement is introduced here. See [SECURITY.md](SECURITY.md) for reporting issues without sharing private archives.
