# Security and data safety / 安全与数据保护

This is a local byte-copying tool. It does not inspect archive members, execute payloads, upload files, collect telemetry, request administrator rights, or expose a shell. Desktop capabilities permit only path selection, plan creation, execution of a single-use plan, cancellation, and local progress events. There are no remote capabilities or shell/fs/http/updater plugins. See DESKTOP.md.

这是本地字节工具，不读取压缩包成员、不运行载荷、不上传、不收集遥测、不要求管理员权限。桌面仅开放受限计划、执行、取消、选择路径及本地事件。

SHA-256 equality establishes the equality of the bytes checked. A manifest does not authenticate its publisher, establish historical provenance, validate archive semantics, or detect malware. A range-only result does not verify unrelated original bytes. Incomplete source groups do not authorize rescue writes.

摘要相同仅证明所检查字节一致；清单不是签名，不证明来源身份、压缩语义或无病毒。区间验证不覆盖其他字节；缺卷来源组不能授权救援写入。

Do not delete retained staging while investigating a failure. Multi-file publication is recoverable, not one atomic filesystem operation. Only the current invocation's live creation tickets can authorize narrow staging-link cleanup; recovery records never authorize deletion. Prefer read-only recovery planning before recovery or restart. Existing outputs, including corrupt ones, are never replaced.

失败后保留暂存现场。多文件发布不是一个原子操作；磁盘记录不授权删除。先只读预览再恢复或重新执行；已有损坏卷也不会被覆盖。

## Reporting / 报告

Repository: https://github.com/NyanodaX/Easy-Archive-Assistant. Do not disclose vulnerabilities or private data in public issues. If private vulnerability reporting is enabled, use the repository Security tab to report privately; otherwise request a private contact route without posting sensitive details. Do not post private archives, full local paths, manifests, hashes of sensitive material, screenshots with personal information, or credentials in public issues.

仓库为 https://github.com/NyanodaX/Easy-Archive-Assistant。若 Security 页面已启用私密漏洞报告，请通过该入口提交；否则先联系维护者确认私密渠道，不在公开 Issue 中披露漏洞细节或私人文件。

Include version, OS/filesystem, operation, stable error code, a small synthetic byte fixture, expected behavior, and actual behavior. Report path escape, overwrite/delete, unsafe cleanup, false verification, resource exhaustion, or IPC boundary issues. State whether the issue needs another process to modify inputs. Do not claim a vulnerability solely from a malicious manifest without a reproducible impact.

请提供版本、系统/文件系统、命令、错误码、无隐私合成样本及预期/实际结果；注明是否依赖其他进程修改输入。

Only v0.0.1 is currently developed. No signed installer, automatic update service, clean-system certification, or hardware power-loss guarantee is claimed. Dependency checks are point-in-time evidence, not a guarantee that every vulnerability is known.
