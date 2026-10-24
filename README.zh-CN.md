# 便捷压缩助手_V0.0.1

[English](README.md) · 英文名：**Easy Archive Assistant**

支持把**不同文件夹或 U 盘**上的独立分卷分次加入同一列表：先“检查分卷”，再选择输出目录合并或重分卷。界面采用朴素的传统标签页与文件列表。手动列表非空时只使用列表中的分卷；清单须单独加入，所有来源盘须保持连接。

对已有 ZIP/7z 压缩包进行离线分卷、合并、直接重分卷与 SHA-256 字节校验，支持单卷精确重建、覆盖地图、完整来源跨组救援及可恢复提交。现已实现**中英双语 Tauri 桌面与独立 CLI**。正式名称和 0.0.1 版本由用户选定，ArchiveFlow 是历史代号。
## 下载与使用 / Download & Run

👉 [下载 Windows 便携版](https://github.com/NyanodaX/Easy-Archive-Assistant/releases)

下载 Assets 中的“便捷压缩助手_V0.0.1.zip”，
完整解压后运行“便捷压缩助手_V0.0.1.exe”。
需要已安装 Microsoft Edge WebView2 运行时。

## 运行

便携包完整解开后双击 `便捷压缩助手_V0.0.1.exe`，同目录保留 `WebView2Loader.dll`。桌面依赖已安装的 WebView2 运行时；CLI `easy-archive-assistant.exe` 独立可用。界面提供传统标签页、进度、取消及保留任务检查。

开发构建使用 Rust 1.90+（本次实测 1.99.1）、Node 24 和 [Tauri 平台依赖](https://v2.tauri.app/start/prerequisites/)。工作区脚本优先使用 `.tools/` 下的私有工具，否则使用系统 Cargo/npm。

```powershell
powershell -NoProfile -File scripts/npm.ps1 ci
powershell -NoProfile -File scripts/npm.ps1 run build
powershell -NoProfile -File scripts/dev.ps1 build --workspace --release
.\target\release\easy-archive-assistant.exe --lang both --help
powershell -NoProfile -File scripts/package-windows.ps1
```

打包脚本在 `.artifacts/releases/` 的独立目录生成桌面 EXE、独立 CLI、加载器、双语说明、第三方许可和 SHA-256 校验文件及 ZIP。打包前须生成依赖告知，见桌面文档。此版为未签名便携开发版，没有安装器或公开发布。已正式采用发布者确认的 [MIT 许可证](LICENSE)，分发时保留项目及第三方许可告知。

替换示例路径后使用。输出目录必须已存在。先用 `--dry-run` 查看规划，再去掉该参数执行；输入文件保留。

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
.\target\release\easy-archive-assistant.exe recover "D:\Backups\.eaa-任务ID" --dry-run --json
.\target\release\easy-archive-assistant.exe recover "D:\Backups\.eaa-任务ID" --json
.\target\release\easy-archive-assistant.exe restart "D:\Backups\.eaa-任务ID" --dry-run --json
.\target\release\easy-archive-assistant.exe restart "D:\Backups\.eaa-任务ID" --json
```

`--lang en|zh-CN|both` 控制人类可读输出，默认双语；JSON 消息始终含两种语言。响应保持 **v2**，清单保持 **v1**。详见 CLI 契约。

## 功能规则

- `inspect` 检查元数据；`split` 使用 8 MiB 流式缓冲，输出连续 `.001`、`.002`……及 `.afparts.json`。文件名冲突即停止，可指定新的 `--basename` 或目录；没有覆盖选项。
- `hard` 均衡分配字节且不超 `--size`；`balanced` 默认允许 5% 容差，可指定 `--tolerance-percent` 和绝对上限 `--max-size`。`--align 1MiB` 尽力对齐；空输入产生一个空卷。
- 精确支持 `1.5GiB`、`500MB` 和整数字节，区分二进制与十进制单位，拒绝非整数字节、零容量及溢出。
- 任意规范分卷或清单可用于合并、校验；缺卷、重复编号、多余清单外卷、无效清单与摘要不匹配均拒绝。没有清单或外部可信摘要时，无法判断末卷是否缺失。
- 合并/重分卷/校验可用 `--expected-sha256` 与外部可信原始摘要比对。清单和恢复记录本身不认证原文件来源。SHA-256 仅证明字节一致，不证明压缩格式有效、签名、来源身份或无病毒。
- `reshard` 要求完整清单及其分卷，并显式指定 `--basename`。使用一个 8 MiB 缓冲区跨旧卷边界直接写新卷，不生成完整中间压缩包，但仍需一份新输出的空间。复制前、发布前校验来源，逐卷完整重读输出；没有 `--expected-sha256` 时仅表示与可用来源一致，不认证来源身份。
- `rebuild-part` 从大小符合清单的原文件中只读取指定卷区间，校验单卷摘要后写新文件，不替换已有卷。回执与恢复均标明 `verification_scope:target_range_only`，不验证完整原始摘要。
- `coverage` 只读报告“已覆盖 / 缺失 / 待验证 / 冲突”的半开字节区间，不执行修复。完整整流摘要通过的来源组才可提供已覆盖区间；缺卷组即使单卷摘要匹配，仍保持待验证。详见 分卷手术台说明。
- `.zip.001` 是原始字节切片，不支持原生 `.z01/.z02/.zip` 磁盘分卷。工具不解压、不重压缩、不解密、不读取包内成员、不执行内容、不上传数据。

## 空间、取消与恢复

拆分/合并/重分卷规划给出载荷、保守开销及可用空间；可用空间为 null 表示查询失败。执行时再次检查，已知空间不足则拒绝。预估不会预留空间，运行中磁盘满仍会报错。Ctrl+C 请求取消。

发布要求硬链接支持，已在 Windows NTFS 测试；拒绝替换已有目标，清单最后发布，多文件组不具备整体原子性。新任务使用 `.eaa-32位十六进制ID/`，继续检测旧 `.archiveflow-*` 目录。数据、完整恢复计划及日志会刷盘，但不承诺 Windows 目录项在突然断电时持久。

`recover 任务目录 --dry-run` 全量读取并检查记录来源、暂存载荷、拼接流摘要、清单对应关系和现有目标身份，不写入。实际恢复重复检查，拒绝运行中任务锁及未知同名文件，只补发布缺少的目标。已有目标必须是同一已验证文件身份，仅字节相同也不算属于该任务。恢复结果为“与现有来源一致”，本地记录不能升级为可信原始摘要。

`restart 任务目录 --dry-run` 检查当前格式的启动记录、目录/来源快照、计划名称、其他未完成任务与空间，不读取载荷，也不写入。真正执行时，在同一任务锁内从头复制到新的排他暂存文件，保留上次尝试和未知文件，需要一份完整新输出的空间，不会接着写坏的半成品追加。已有完整暂存时请用 `recover`。拆分、合并、重分卷和精确区间重建、空文件、新文件名和均衡策略均已覆盖。

Windows 下，本次进程成功拆分、合并、重分卷或重新执行后，可在完成标记刷盘后移除本次新建且已验证的暂存链接。必须具有内存中的创建凭据与仍打开的可删除句柄，磁盘记录不能授权删除。日志、旧尝试、未知文件和正式输出保留；权限不足或清理失败则保留链接。`staged_payload_links_scheduled_for_removal` 报告本次安排移除的数量，其他读句柄可能延迟实际删除或空间释放。恢复不会删除记录中的文件，目前也没有通用清理/删除命令。

若 `start.json` 尚未发布，完整的 `start-draft.json` 可通过同等的大小、目录身份、来源快照、选项与冲突检查后用于重新执行。最终记录存在但无效时，绝不退回草稿绕过检查。草稿未写完、所有者记录缺失和旧格式任务仍保持现场，可检查后另选目录。不要隐藏失败任务并把部分分卷冒充完整组。详见 恢复机制。

`rescue` 接受同一原始大小/摘要的完整来源组，实际验证每组全部字节，跨原分卷边界复制目标区间，重读成果并匹配目标卷摘要。`--expected-sha256` 可核对独立保留的原文件摘要；没有它时不声称历史原文件一致。回执明确 `target_range_only`，不认证来源身份，不覆盖已有损坏卷。缺卷组不能授权修复。

限制：10,000 卷，4 MiB 清单，16 MiB 恢复计划，单次扫描 20,000 项，最多 8 个覆盖/救援来源组。拒绝不安全名称、UNC/设备路径、符号链接、重解析点及非常规文件。最低 Rust 1.90 为声明值，本机只实测 1.99.1。桌面 IPC 整数用字符串无损传输；CLI v2 仍使用 JSON 整数。

## 开发验证

```powershell
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

测试只用自有临时样例，不联网。已测试启动记录持久后、复制中、恢复计划持久后、硬链接建立但未记日志、完成标记前的进程强制终止。可控短写适配器覆盖短写、中断、零写入、取消及写入部分数据后的错误。未宣称测试物理磁盘写满、拔盘、真实断电、系统实际短写、Linux/macOS 或干净机器安装。



M5 已通过 130 项 Rust 测试和真实 6 GiB 救援验收；M6 增加桌面桥接、前端和离线发布版窗口集成测试、受限 PATH 便携运行与 CI 定义。具体数量和边界见当前证据。CI 尚未远程执行；真实 OS 拖放、屏幕阅读器朗读、干净虚拟机、其他桌面平台和物理断电/拔盘另行验证。加密样本保留 [PSF 许可与归属](tests/fixtures/README.md)。[参与开发](CONTRIBUTING.md) · [安全说明](SECURITY.md) · [更新记录](CHANGELOG.md)。
