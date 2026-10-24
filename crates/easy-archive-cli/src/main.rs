use clap::{Arg, ArgAction, Command};
use easy_archive_core::recovery::{plan_recover_transaction, recover_transaction};
use easy_archive_core::restart::{plan_restart_task, restart_task};
use easy_archive_core::transaction::inspect_transactions;
use easy_archive_core::{
    BilingualText, CoverageRequest, ErrorCode, InputKind, MergeRequest, RESPONSE_SCHEMA_VERSION,
    RebuildPartRequest, Receipt, RescueRequest, ReshardRequest, SourceFile, SplitPolicy,
    SplitRequest, TaskContext, VolumeError, coverage, inspect_selected_group, merge_file,
    parse_size, plan_merge_file, plan_rebuild_part, plan_rescue, plan_reshard_file,
    plan_split_file, rebuild_part, rescue_part, reshard_file, split_file, verify_selected_group,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

#[derive(Clone, Copy)]
enum Language {
    En,
    Zh,
    Both,
}
impl Language {
    fn text(self, en: &'static str, zh: &'static str, both: &'static str) -> &'static str {
        match self {
            Self::En => en,
            Self::Zh => zh,
            Self::Both => both,
        }
    }
    fn render(self, text: &BilingualText) -> String {
        match self {
            Self::En => text.en.clone(),
            Self::Zh => text.zh_cn.clone(),
            Self::Both => format!("{} / {}", text.zh_cn, text.en),
        }
    }
}

#[derive(Serialize)]
struct Response {
    schema_version: u32,
    operation: String,
    status: &'static str,
    input: Option<Value>,
    planned_output: Vec<PathBuf>,
    verification_level: String,
    warnings: Vec<BilingualText>,
    data: Value,
    error: Option<VolumeError>,
}
impl Response {
    fn new(operation: &str, status: &'static str) -> Self {
        Self {
            schema_version: RESPONSE_SCHEMA_VERSION,
            operation: operation.to_owned(),
            status,
            input: None,
            planned_output: vec![],
            verification_level: "unverified".to_owned(),
            warnings: vec![],
            data: Value::Null,
            error: None,
        }
    }
    fn receipt(&mut self, receipt: Receipt) {
        let cleanup_count = receipt.staged_payload_links_scheduled_for_removal;
        self.status = "success";
        self.verification_level = receipt.verification_level.clone();
        self.planned_output = receipt.output_files.clone();
        if let Some(path) = &receipt.manifest_path {
            self.planned_output.push(path.clone());
        }
        self.data = serde_json::to_value(receipt).expect("receipt JSON");
        if cleanup_count > 0 {
            self.warnings.push(BilingualText::new(format!("Scheduled {cleanup_count} verified staging links from this live attempt for removal after publication. Logs, previous attempts and unknown files remain. Other open readers may delay removal or space reclamation."),format!("发布成功后，已将本次尝试的 {cleanup_count} 个已验证暂存链接安排移除。日志、旧尝试及未知文件保留；其他打开的读句柄可能延迟移除或空间释放。")));
        } else {
            self.warnings.push(BilingualText::new("Staging and journals are retained. Recovery never deletes files based on persisted records. Live cleanup is Windows-only and optional; permission failures retain links. Previous attempts may occupy additional space.","暂存与日志保留；恢复不会依据磁盘记录删除文件。本次进程的清理仅支持 Windows，权限不足时保留链接。旧尝试可能额外占用空间。"));
        }
    }
}

fn input_arg() -> Arg {
    Arg::new("input")
        .required(true)
        .value_parser(clap::value_parser!(PathBuf))
}
fn selected_parts_arg(lang: Language) -> Arg {
    Arg::new("part-file").long("part-file").action(ArgAction::Append)
        .value_parser(clap::value_parser!(PathBuf))
        .help(lang.text("Explicit volume path; repeat for all parts, including INPUT if it is a part. No adjacent-volume discovery.","明确选择的分卷路径；重复列出全部分卷（输入为分卷时也须列出）。不自动寻找相邻卷。","跨目录分卷（重复）/ Explicit volume paths (repeat)"))
}
fn selected_parts(args: &clap::ArgMatches) -> Vec<PathBuf> {
    args.get_many::<PathBuf>("part-file")
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}
fn output_args(command: Command, lang: Language) -> Command {
    command
        .arg(
            Arg::new("output-dir")
                .long("output-dir")
                .value_parser(clap::value_parser!(PathBuf))
                .help(lang.text(
                    "Existing output directory; default input directory",
                    "已存在的输出目录；默认与输入同目录",
                    "输出目录 / Existing output directory",
                )),
        )
        .arg(Arg::new("basename").long("basename").help(lang.text(
            "New output base filename; never overwrite existing files",
            "新的输出基本文件名；禁止覆盖已有文件",
            "新文件名 / New output base filename",
        )))
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help(lang.text(
                    "Plan without creating files or directories",
                    "仅规划，不创建文件或目录",
                    "仅规划，无写入 / Plan only, no writes",
                )),
        )
}
fn expected_hash_arg(lang: Language) -> Arg {
    Arg::new("expected-sha256")
        .long("expected-sha256")
        .help(lang.text(
            "Trusted external original SHA-256: 64 lowercase hex digits",
            "外部可信原始 SHA-256：64 位小写十六进制",
            "外部可信原始摘要 / Trusted external original SHA-256",
        ))
}
fn volume_args(command: Command, lang: Language) -> Command {
    let text = |en, zh, both| lang.text(en, zh, both);
    command
        .arg(Arg::new("size").long("size").required(true).help(text(
            "Size: 1.5GiB, 500MB, or integer bytes",
            "容量：1.5GiB、500MB 或整数字节",
            "容量 / Size: 1.5GiB, 500MB, bytes",
        )))
        .arg(
            Arg::new("policy")
                .long("policy")
                .default_value("hard")
                .value_parser(["hard", "balanced"])
                .help(text(
                    "hard: cap; balanced: allow tolerance",
                    "hard：不超容量；balanced：允许容差",
                    "分卷策略 / Split policy",
                )),
        )
        .arg(
            Arg::new("tolerance")
                .long("tolerance-percent")
                .value_parser(clap::value_parser!(u32).range(0..=100))
                .help(text(
                    "Balanced integer tolerance percent; default 5",
                    "仅 balanced 的整数容差百分比，默认 5",
                    "balanced 容差 / Tolerance percent (default 5)",
                )),
        )
        .arg(Arg::new("max-size").long("max-size").help(text(
            "Balanced absolute hard limit",
            "balanced 的绝对硬上限",
            "balanced 硬上限 / Hard limit",
        )))
        .arg(Arg::new("align").long("align").help(text(
            "Optional best-effort alignment, e.g. 1MiB",
            "可选尽力对齐，例如 1MiB",
            "可选切点对齐 / Optional alignment",
        )))
}
fn cli(lang: Language) -> Command {
    let text = |en, zh, both| lang.text(en, zh, both);
    let split = output_args(
        volume_args(
            Command::new("split")
                .about(text(
                    "Split archive bytes with full output reread",
                    "拆分压缩包字节，并全量重读校验输出",
                    "拆分 / Split byte volumes",
                ))
                .arg(input_arg()),
            lang,
        )
        .arg(
            Arg::new("allow-any-file")
                .long("allow-any-file")
                .action(ArgAction::SetTrue)
                .help(text(
                    "Explicitly allow other regular-file extensions",
                    "明确允许其他普通文件扩展名",
                    "允许其他普通文件 / Allow other regular files",
                )),
        ),
        lang,
    );
    let merge = output_args(
        Command::new("merge")
            .about(text(
                "Merge raw volumes without decompression",
                "合并原始分卷，不解压",
                "合并 / Merge raw volumes",
            ))
            .arg(input_arg())
            .arg(selected_parts_arg(lang))
            .arg(expected_hash_arg(lang)),
        lang,
    );
    let reshard = output_args(
        volume_args(
            Command::new("reshard")
                .about(text(
                    "Re-shard a manifest-backed group without a complete intermediate archive",
                    "对具有清单的分卷组直接重分卷，不生成完整中间文件",
                    "直接重分卷 / Re-shard without a complete intermediate archive",
                ))
                .arg(input_arg())
                .arg(selected_parts_arg(lang))
                .arg(expected_hash_arg(lang)),
            lang,
        ),
        lang,
    )
    .mut_arg("basename", |arg| arg.required(true));
    let rebuild = output_args(
        Command::new("rebuild-part")
            .about(text(
                "Rebuild one exact range from an original; never replace an existing part",
                "从原文件精确重建单卷，不替换已有分卷",
                "单卷重建 / Rebuild an exact part",
            ))
            .arg(
                Arg::new("original")
                    .long("original")
                    .required(true)
                    .value_parser(clap::value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("manifest")
                    .long("manifest")
                    .required(true)
                    .value_parser(clap::value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("part")
                    .long("part")
                    .required(true)
                    .value_parser(clap::value_parser!(u32).range(1..=10_000)),
            ),
        lang,
    );
    let coverage_cmd = Command::new("coverage")
        .about(text(
            "Read-only byte coverage diagnostics; a report never authorizes repair writes",
            "只读字节覆盖诊断；报告本身不授权修复写入",
            "覆盖地图 / Read-only byte coverage",
        ))
        .arg(
            Arg::new("target-manifest")
                .long("target-manifest")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("source-manifest")
                .long("source-manifest")
                .required(true)
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(PathBuf))
                .help(text(
                    "Repeat for up to 8 source groups",
                    "可重复指定，最多 8 组来源",
                    "最多 8 组 / Up to 8 groups",
                )),
        )
        .arg(
            Arg::new("part")
                .long("part")
                .value_parser(clap::value_parser!(u32).range(1..=10_000)),
        )
        .arg(
            Arg::new("metadata-only")
                .long("metadata-only")
                .action(ArgAction::SetTrue)
                .help(text(
                    "Skip payload hashes; present ranges remain pending",
                    "不计算载荷摘要，现有区间保持待验证",
                    "仅元数据 / Metadata only; pending",
                )),
        );
    let rescue = output_args(
        Command::new("rescue")
            .about(text(
                "Rescue one target part from complete verified backup groups",
                "从完整且经验证的备用组救援一个目标卷",
                "跨组救援 / Rescue one part from verified backup groups",
            ))
            .arg(
                Arg::new("target-manifest")
                    .long("target-manifest")
                    .required(true)
                    .value_parser(clap::value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("source-manifest")
                    .long("source-manifest")
                    .required(true)
                    .action(ArgAction::Append)
                    .value_parser(clap::value_parser!(PathBuf)),
            )
            .arg(
                Arg::new("part")
                    .long("part")
                    .required(true)
                    .value_parser(clap::value_parser!(u32).range(1..=10_000)),
            )
            .arg(expected_hash_arg(lang)),
        lang,
    );
    Command::new("easy-archive-assistant").version(env!("CARGO_PKG_VERSION"))
        .about(text("Easy Archive Assistant — Offline byte tools","便捷压缩助手_V0.0.1 — 离线字节工具","便捷压缩助手_V0.0.1 / Easy Archive Assistant — 离线 / Offline"))
        .disable_help_subcommand(true)
        .arg(Arg::new("lang").long("lang").global(true).default_value("both").value_parser(["en","zh-CN","both"]).help(text("Human-readable language","人类可读输出语言","输出语言 / Human-readable language")))
        .arg(Arg::new("json").long("json").global(true).action(ArgAction::SetTrue).help(text("One JSON result on stdout","标准输出仅含一个 JSON 结果","单个 JSON 结果 / One JSON result")))
        .subcommand(Command::new("inspect").about(text("Inspect file or group metadata; no payload hashes","检查文件或分卷组元数据，不计算载荷哈希","检查 / Inspect metadata" )).arg(input_arg()).arg(selected_parts_arg(lang)))
        .subcommand(split).subcommand(merge).subcommand(reshard).subcommand(rebuild).subcommand(coverage_cmd).subcommand(rescue)
        .subcommand(Command::new("verify").about(text("Hash all volumes and the concatenated stream","计算各卷及拼接流哈希","验证 / Verify volume hashes")).arg(input_arg()).arg(selected_parts_arg(lang)).arg(expected_hash_arg(lang)))
        .subcommand(Command::new("transactions").about(text("Report retained tasks; never resume or delete","报告保留任务，不续传或删除","任务报告 / Report retained tasks")).arg(input_arg()))
        .subcommand(Command::new("recover").about(text("Reverify and resume complete staged publication", "重新校验并恢复完整暂存的发布", "恢复发布 / Recover staged publication")).arg(input_arg()).arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue).help(text("Verify all files; perform no writes", "全量验证，不写入", "验证但不写入 / Verify, no writes"))))
        .subcommand(Command::new("restart").about(text("Restart incomplete copying; preserve previous attempts", "重新执行未完成复制，保留上次暂存", "重新复制 / Restart unfinished copying")).arg(input_arg()).arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue).help(text("Check metadata, conflicts and space without writes", "检查元数据、冲突和空间，不写入", "仅规划 / Plan only, no writes"))))
        .after_help(text(
            "Raw .zip.001 slices are not native .z01 ZIP volumes (unsupported). MiB=1048576 bytes; MB=1000000 bytes. Writes require hard-link support (e.g. Windows NTFS). Staging is retained; use recover to resume complete staging; the desktop UI is pending. See docs/CLI_CONTRACT.md.",
            ".zip.001 是原始字节切片，不是原生 .z01 ZIP 分卷（不支持）。MiB=1048576 字节；MB=1000000 字节。写入要求硬链接支持，例如 Windows NTFS。暂存保留，可恢复完整暂存；桌面界面待实现。详见 docs/CLI_CONTRACT.md。",
            ".zip.001：原始字节切片 / raw slices; .z01：不支持 / unsupported.\nMiB=1048576 字节 / bytes; MB=1000000 字节 / bytes.\n写入需硬链接支持，暂存保留 / Writes require hard-link support; staging retained.\n可恢复完整暂存；桌面界面待实现 / Explicit recovery available; desktop UI pending. docs/CLI_CONTRACT.md"))
}

fn execution_context() -> easy_archive_core::Result<TaskContext<'static>> {
    let context = TaskContext::default();
    let token = context.cancellation.clone();
    ctrlc::set_handler(move || token.cancel()).map_err(|_| VolumeError::new(ErrorCode::IoError))?;
    Ok(context)
}
fn source_warning(response: &mut Response, has_manifest: bool) {
    response.warnings.push(BilingualText::new("SHA-256 verifies bytes, not archive semantics, virus safety, signatures, or provenance. A manifest alone is not a trusted original digest.","SHA-256 校验字节，不证明压缩格式有效、无病毒、签名或来源身份。清单本身不是可信原始摘要。"));
    if !has_manifest {
        response.warnings.push(BilingualText::new("No manifest: a missing final volume or an originally damaged group cannot be detected without a trusted external digest.","没有清单：若无外部可信摘要，无法检测最后一卷缺失或输入组原本已受损。"));
    }
}
fn plan_warning(response: &mut Response) {
    response.warnings.push(BilingualText::new("Plan only; nothing was written. The space estimate is advisory and reserves nothing. A null available-space value means the query failed. Execution checks again and stops if space is insufficient; publication requires hard-link support.","仅规划，未写入。空间预估仅供参考，不预留空间；可用空间为 null 表示查询失败。执行时再次检查，空间不足则停止；发布要求文件系统支持硬链接。"));
}
fn execute(matches: &clap::ArgMatches) -> easy_archive_core::Result<Response> {
    let (operation, args) = matches
        .subcommand()
        .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))?;
    let mut response = Response::new(operation, "inspected");
    if operation == "rescue" {
        let request = RescueRequest {
            target_manifest: args
                .get_one::<PathBuf>("target-manifest")
                .expect("required target")
                .clone(),
            source_manifests: args
                .get_many::<PathBuf>("source-manifest")
                .expect("required sources")
                .cloned()
                .collect(),
            part: *args.get_one::<u32>("part").expect("required part"),
            output_dir: args.get_one::<PathBuf>("output-dir").cloned(),
            basename: args.get_one::<String>("basename").cloned(),
            expected_sha256: args.get_one::<String>("expected-sha256").cloned(),
        };
        let plan = plan_rescue(&request)?;
        source_warning(&mut response, true);
        response.warnings.push(BilingualText::new("Rescue requires complete backup groups. Execution verifies every source part and whole-stream digest, then only writes the requested target interval. Incomplete groups cannot authorize repair; local manifests do not authenticate provenance.","救援要求完整备用组。执行会验证每卷与整流摘要，仅写目标区间；缺卷组不能授权修复，本地清单不认证来源身份。"));
        if args.get_flag("dry-run") {
            response.status = "planned";
            response.planned_output = vec![plan.output_path.clone()];
            response.data = json!({"source_size":plan.target.size,"part_count":1,"plan":plan,"content_read":false,"writes_performed":false});
            plan_warning(&mut response);
        } else {
            response.receipt(rescue_part(&request, &mut execution_context()?)?);
            response.data["has_external_digest"] = json!(request.expected_sha256.is_some());
        }
        return Ok(response);
    }
    if operation == "coverage" {
        let request = CoverageRequest {
            target_manifest: args
                .get_one::<PathBuf>("target-manifest")
                .expect("required target")
                .clone(),
            source_manifests: args
                .get_many::<PathBuf>("source-manifest")
                .expect("required sources")
                .cloned()
                .collect(),
            part: args.get_one::<u32>("part").copied(),
            metadata_only: args.get_flag("metadata-only"),
        };
        response.data = serde_json::to_value(coverage(&request, &mut execution_context()?)?)
            .expect("coverage JSON");
        source_warning(&mut response, true);
        response.warnings.push(BilingualText::new("Coverage is read-only diagnostics, not a repair permit. Part-hash matches from incomplete groups remain pending whole-stream identity verification. Conflicting overlaps and integrity errors prevent complete coverage.","覆盖地图是只读诊断，不是修复许可。缺卷组的单卷摘要匹配仍须确认整流身份；重叠冲突或完整性错误阻止完整覆盖结论。"));
        return Ok(response);
    }
    if operation == "rebuild-part" {
        let request = RebuildPartRequest {
            original: args
                .get_one::<PathBuf>("original")
                .expect("required original")
                .clone(),
            manifest: args
                .get_one::<PathBuf>("manifest")
                .expect("required manifest")
                .clone(),
            part: *args.get_one::<u32>("part").expect("required part"),
            output_dir: args.get_one::<PathBuf>("output-dir").cloned(),
            basename: args.get_one::<String>("basename").cloned(),
        };
        let plan = plan_rebuild_part(&request)?;
        response.input = Some(serde_json::to_value(&plan.original).expect("original JSON"));
        source_warning(&mut response, true);
        response.warnings.push(BilingualText::new("Only the requested range is read and compared with its recorded part hash. Other original bytes and the whole original digest are not verified; the manifest does not authenticate provenance.","仅读取请求区间并核对记录的单卷摘要；不验证其他原文件字节或完整原始摘要，清单不认证来源身份。"));
        if args.get_flag("dry-run") {
            response.status = "planned";
            response.planned_output = vec![plan.output_path.clone()];
            response.data = json!({"source_size":plan.target.size,"part_count":1,"plan":plan,"content_read":false,"writes_performed":false});
            plan_warning(&mut response);
        } else {
            response.receipt(rebuild_part(&request, &mut execution_context()?)?);
            response.data["verification_scope"] = json!("target_range_only");
            response.data["target_range"] = serde_json::to_value(plan.range).expect("range JSON");
            response.data["target_part_index"] = json!(request.part);
        }
        return Ok(response);
    }
    let path = args.get_one::<PathBuf>("input").expect("required input");
    if operation == "transactions" {
        response.data = json!({"tasks":inspect_transactions(path)?,"automatic_recovery_available":false,"explicit_recovery_command":"recover TASK_DIRECTORY --dry-run --json","explicit_restart_command":"restart TASK_DIRECTORY --dry-run --json"});
        return Ok(response);
    }
    if operation == "restart" {
        let mut context = execution_context()?;
        if args.get_flag("dry-run") {
            let plan = plan_restart_task(path, &mut context)?;
            response.status = "planned";
            response.planned_output = plan.output_files.clone();
            response.data = serde_json::to_value(plan).expect("restart plan JSON");
            plan_warning(&mut response);
        } else {
            response.receipt(restart_task(path, &mut context)?);
        }
        response.warnings.push(BilingualText::new("Restart copies the recorded source ranges from their beginning into new exclusive attempt files. Previous attempts and unknown files are retained, so a full new payload allocation is needed. Changed input snapshots, other pending tasks, or existing output names stop execution. Complete staged publication uses recover instead.","重新执行会从记录的来源区间开头复制到新的排他暂存文件，保留上次尝试和未知文件，因此需要完整新载荷空间。来源快照变化、其他未完成任务或已有输出同名时会停止；完整暂存的发布请用 recover。"));
        source_warning(&mut response, true);
        return Ok(response);
    }
    if operation == "recover" {
        let mut context = execution_context()?;
        if args.get_flag("dry-run") {
            let receipt = plan_recover_transaction(path, &mut context)?;
            response.status = "planned";
            response.planned_output = receipt.output_files.clone();
            response
                .planned_output
                .extend(receipt.manifest_path.clone());
            response.data = json!({"receipt":receipt,"content_read":true,"writes_performed":false});
        } else {
            response.receipt(recover_transaction(path, &mut context)?);
        }
        source_warning(&mut response, true);
        response.warnings.push(BilingualText::new("Recovery rechecks recorded sources and staged bytes (range operations read only their recorded ranges); local records do not authenticate the original. No task files are deleted. Incomplete writing and legacy tasks cannot resume.","恢复会重新校验记录的来源和暂存字节（区间操作只读取记录的区间）；本地记录不认证原文件来源。不会删除任务文件。未写完和旧格式任务不能续传。"));
        return Ok(response);
    }
    if operation == "verify" {
        let result = verify_selected_group(
            path,
            &selected_parts(args),
            args.get_one::<String>("expected-sha256")
                .map(String::as_str),
            &mut execution_context()?,
        )?;
        response.status = "success";
        response.verification_level = result.verification_level.clone();
        source_warning(&mut response, result.has_manifest);
        response.data = serde_json::to_value(result).expect("report JSON");
        return Ok(response);
    }
    if operation == "merge" {
        let request = MergeRequest {
            part_paths: selected_parts(args),
            input: path.clone(),
            output_dir: args.get_one::<PathBuf>("output-dir").cloned(),
            basename: args.get_one::<String>("basename").cloned(),
            expected_sha256: args.get_one::<String>("expected-sha256").cloned(),
        };
        let plan = plan_merge_file(&request)?;
        response.input = Some(serde_json::to_value(&plan.group).expect("group JSON"));
        source_warning(&mut response, plan.group.has_manifest);
        if args.get_flag("dry-run") {
            response.status = "planned";
            response.planned_output = vec![plan.output_path.clone()];
            response.data = json!({"source_size":plan.group.source_size,"part_count":plan.group.part_count,"estimated_payload_bytes":plan.group.source_size,"overhead_estimate_bytes":plan.space.overhead_estimate_bytes,"available_space_bytes":plan.space.available_space_bytes,"free_space_checked":plan.space.free_space_checked,"space":plan.space,"content_read":false,"writes_performed":false,"plan":plan});
            plan_warning(&mut response);
        } else {
            response.receipt(merge_file(&request, &mut execution_context()?)?);
        }
        return Ok(response);
    }
    if operation == "reshard" {
        let request = ReshardRequest {
            part_paths: selected_parts(args),
            input: path.clone(),
            output_dir: args.get_one::<PathBuf>("output-dir").cloned(),
            basename: args
                .get_one::<String>("basename")
                .expect("required basename")
                .clone(),
            policy: parse_policy(args)?,
            alignment: args
                .get_one::<String>("align")
                .map(|v| parse_size(v))
                .transpose()?,
            expected_sha256: args.get_one::<String>("expected-sha256").cloned(),
        };
        let plan = plan_reshard_file(&request)?;
        response.input = Some(serde_json::to_value(&plan.group).expect("group JSON"));
        source_warning(&mut response, true);
        if args.get_flag("dry-run") {
            response.status = "planned";
            response.planned_output = plan.output_files.clone();
            response.planned_output.push(plan.manifest_path.clone());
            response.data = json!({"source_size":plan.group.source_size,"part_count":plan.plan.part_count,
                "estimated_payload_bytes":plan.space.estimated_payload_bytes,"space":plan.space,"plan":plan,
                "content_read":false,"writes_performed":false,"creates_complete_intermediate":false});
            plan_warning(&mut response);
        } else {
            response.receipt(reshard_file(&request, &mut execution_context()?)?);
        }
        return Ok(response);
    }
    let source = SourceFile::inspect(path)?;
    response.input = Some(serde_json::to_value(&source).expect("source JSON"));
    if operation == "inspect" {
        if source.kind == InputKind::NativeZipVolume {
            return Err(VolumeError::new(ErrorCode::UnsupportedVolumeFormat));
        }
        if source.kind == InputKind::RawVolume
            || path.to_string_lossy().ends_with(".afparts.json")
            || !selected_parts(args).is_empty()
        {
            let group = inspect_selected_group(path, &selected_parts(args))?;
            source_warning(&mut response, group.has_manifest);
            let supported = if group.has_manifest {
                vec!["merge", "verify", "reshard"]
            } else {
                vec!["merge", "verify"]
            };
            response.data =
                json!({"group":group,"supported_operations":supported,"content_read":false});
        } else {
            response.data = json!({"source_size":source.size,"identification":"filename_only","supported_operations":if source.kind==InputKind::ArchiveBytes {vec!["split"]} else {vec![]},"content_read":false});
            response.warnings.push(BilingualText::new("Identification uses the filename only. No archive contents or hashes were checked.","仅通过文件名识别；未检查压缩包内容或哈希。"));
        }
        return Ok(response);
    }
    let policy = parse_policy(args)?;
    let request = SplitRequest {
        input: path.clone(),
        output_dir: args.get_one::<PathBuf>("output-dir").cloned(),
        basename: args.get_one::<String>("basename").cloned(),
        policy,
        alignment: args
            .get_one::<String>("align")
            .map(|v| parse_size(v))
            .transpose()?,
        allow_any_file: args.get_flag("allow-any-file"),
    };
    let planned = plan_split_file(&request)?;
    if args.get_flag("dry-run") {
        response.status = "planned";
        response.planned_output = planned.output_files.clone();
        response.planned_output.push(planned.manifest_path.clone());
        if request.alignment.is_some()
            && !planned.plan.alignment_applied
            && planned.plan.part_count > 1
        {
            response.warnings.push(BilingualText::new(
                "Alignment was skipped to preserve positive lengths and the size cap.",
                "为保证正长度和容量上限，未应用对齐。",
            ));
        }
        response.data = json!({"source_size":source.size,"part_count":planned.plan.part_count,"estimated_payload_bytes":source.size,"overhead_estimate_bytes":planned.space.overhead_estimate_bytes,"available_space_bytes":planned.space.available_space_bytes,"free_space_checked":planned.space.free_space_checked,"space":planned.space,"plan":planned.plan,"manifest_path":planned.manifest_path,"content_read":false,"writes_performed":false});
        plan_warning(&mut response);
    } else {
        response.receipt(split_file(&request, &mut execution_context()?)?);
    }
    Ok(response)
}

fn parse_policy(args: &clap::ArgMatches) -> easy_archive_core::Result<SplitPolicy> {
    let size = parse_size(args.get_one::<String>("size").expect("required size"))?;
    let balanced = args
        .get_one::<String>("policy")
        .is_some_and(|v| v == "balanced");
    if !balanced && (args.contains_id("tolerance") || args.contains_id("max-size")) {
        return Err(VolumeError::new(ErrorCode::InvalidArgument));
    }
    let policy = if balanced {
        SplitPolicy::Balanced {
            target_bytes: size,
            tolerance_ppm: args.get_one::<u32>("tolerance").copied().unwrap_or(5) * 10_000,
            hard_max_bytes: args
                .get_one::<String>("max-size")
                .map(|v| parse_size(v))
                .transpose()?,
        }
    } else {
        SplitPolicy::Hard { max_bytes: size }
    };
    Ok(policy)
}

fn print_coverage(data: &Value, lang: Language) {
    if let Some(range) = data.get("requested_range") {
        println!(
            "{}: [{}, {})",
            lang.text("Requested range", "请求区间", "请求区间 / Requested range"),
            range["start"],
            range["end_exclusive"]
        );
    }
    println!(
        "{}: {}",
        lang.text("Covered bytes", "已覆盖字节", "已覆盖字节 / Covered bytes"),
        data["covered_bytes"]
    );
    let yes_no = |value: bool| {
        if value {
            lang.text("yes", "是", "是 / yes")
        } else {
            lang.text("no", "否", "否 / no")
        }
    };
    println!(
        "{}: {}",
        lang.text(
            "Complete verified coverage",
            "完整经验证覆盖",
            "完整经验证覆盖 / Complete verified coverage"
        ),
        yes_no(data["complete_coverage"].as_bool().unwrap_or(false))
    );
    println!(
        "{}: {}",
        lang.text(
            "Integrity errors",
            "完整性错误",
            "完整性错误 / Integrity errors"
        ),
        yes_no(data["has_integrity_errors"].as_bool().unwrap_or(true))
    );
    if let Some(sources) = data["sources"].as_array() {
        for (i, source) in sources.iter().enumerate() {
            println!(
                "{} {}: {}",
                lang.text("Source", "来源", "来源 / Source"),
                i + 1,
                source["manifest_path"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(segments) = data["segments"].as_array() {
        for segment in segments {
            let state = match segment["status"].as_str() {
                Some("covered") => lang.text("covered", "已覆盖", "已覆盖 / covered"),
                Some("pending") => lang.text(
                    "pending verification",
                    "待验证",
                    "待验证 / pending verification",
                ),
                Some("missing") => lang.text("missing", "缺失", "缺失 / missing"),
                _ => lang.text("conflict", "冲突", "冲突 / conflict"),
            };
            let refs = segment["sources"]
                .as_array()
                .map(|refs| {
                    refs.iter()
                        .map(|r| {
                            format!(
                                "{} {} {} {}",
                                lang.text("source", "来源", "来源 / source"),
                                r["source_index"].as_u64().unwrap_or(0) + 1,
                                lang.text("part", "卷", "卷 / part"),
                                r["part_index"]
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "[{}, {}): {state} {refs}",
                segment["range"]["start"], segment["range"]["end_exclusive"]
            );
        }
    }
}

fn print_response(response: &Response, json_mode: bool, lang: Language) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(response).expect("response JSON")
        );
    } else if let Some(error) = &response.error {
        let code = serde_json::to_value(error.code).expect("error code JSON");
        eprintln!(
            "{}: {}\n{}",
            code.as_str().expect("code string"),
            lang.render(&error.message),
            lang.render(&error.suggestion)
        );
        if let Some(id) = &error.recovery_task_id {
            eprintln!(
                "{}: {id}",
                lang.text("Retained task ID", "保留的任务 ID", "任务 ID / Task ID")
            );
        }
        if let Some(index) = error.part_index {
            eprintln!(
                "{}: {index}",
                lang.text("Part index", "分卷编号", "分卷编号 / Part index")
            );
        }
        if let Some(details) = &error.coverage_details {
            for (ranges, label) in [
                (
                    &details.missing_ranges,
                    lang.text("Missing range", "缺失区间", "缺失区间 / Missing range"),
                ),
                (
                    &details.pending_ranges,
                    lang.text("Pending range", "待验证区间", "待验证区间 / Pending range"),
                ),
            ] {
                for range in ranges {
                    eprintln!("{label}: [{}, {})", range.start, range.end_exclusive);
                }
            }
        }
    } else if let Some(help) = response.data.get("help").and_then(Value::as_str) {
        print!("{help}");
    } else if let Some(version) = response.data.get("version").and_then(Value::as_str) {
        match lang {
            Language::En => println!("{} {version}", easy_archive_core::PRODUCT_NAME_EN),
            Language::Zh => println!("{}", easy_archive_core::PRODUCT_NAME),
            Language::Both => println!(
                "{} / {} {version}",
                easy_archive_core::PRODUCT_NAME,
                easy_archive_core::PRODUCT_NAME_EN
            ),
        }
    } else {
        println!(
            "{}: {}",
            lang.text("Status", "状态", "状态 / Status"),
            response.status
        );
        println!(
            "{}: {}",
            lang.text("Verification", "验证等级", "验证等级 / Verification"),
            response.verification_level
        );
        for (key, en, zh, both) in [
            (
                "source_size",
                "Source bytes",
                "源文件字节数",
                "源文件字节数 / Source bytes",
            ),
            ("part_count", "Parts", "分卷数", "分卷数 / Parts"),
        ] {
            if let Some(value) = response.data.get(key) {
                println!("{}: {value}", lang.text(en, zh, both));
            }
        }
        for path in &response.planned_output {
            println!("  {}", path.display());
        }
        if let Some(stage) = response
            .data
            .get("staging_directory")
            .and_then(Value::as_str)
        {
            println!(
                "{}: {stage}",
                lang.text("Retained staging", "保留的暂存目录", "暂存目录 / Staging")
            );
        }
        if let Some(tasks) = response.data.get("tasks").and_then(Value::as_array) {
            for task in tasks {
                println!("{} {}", task["status"], task["directory"]);
            }
        }
        if response.operation == "coverage" {
            print_coverage(&response.data, lang);
        }
        for warning in &response.warnings {
            println!("{}", lang.render(warning));
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let json_mode = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let mut language = Language::Both;
    for (index, arg) in args.iter().enumerate().skip(1) {
        if arg == "--" {
            break;
        }
        let value = if arg == "--lang" {
            args.get(index + 1).and_then(|v| v.to_str())
        } else {
            arg.to_str().and_then(|s| s.strip_prefix("--lang="))
        };
        match value {
            Some("en") => language = Language::En,
            Some("zh-CN") => language = Language::Zh,
            Some("both") => language = Language::Both,
            _ => (),
        }
    }
    let mut operation = "request".to_owned();
    let result = match cli(language).try_get_matches_from(args) {
        Ok(matches) => {
            operation = matches.subcommand_name().unwrap_or("request").to_owned();
            execute(&matches)
        }
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let mut response = Response::new("help", "info");
            response.data = if error.kind() == clap::error::ErrorKind::DisplayVersion {
                json!({"version":env!("CARGO_PKG_VERSION"),"product_name":easy_archive_core::PRODUCT_NAME,"product_name_en":easy_archive_core::PRODUCT_NAME_EN})
            } else {
                json!({"help":error.to_string()})
            };
            Ok(response)
        }
        Err(_) => Err(VolumeError::new(ErrorCode::InvalidArgument)),
    };
    match result {
        Ok(response) => {
            print_response(&response, json_mode, language);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let code = error.exit_code();
            let mut response = Response::new(&operation, "error");
            response.error = Some(error);
            print_response(&response, json_mode, language);
            ExitCode::from(code)
        }
    }
}
