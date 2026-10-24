use serde::Serialize;
use std::fmt;

pub type Result<T> = std::result::Result<T, VolumeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    InvalidSize,
    LimitExceeded,
    InputNotFound,
    InputNotRegularFile,
    UnsafePath,
    UnsupportedVolumeFormat,
    OutputExists,
    ManifestInvalid,
    ManifestRequired,
    PartMissing,
    PartDuplicate,
    PartGroupMismatch,
    OutputDirectoryRequired,
    PartHashMismatch,
    SourceHashMismatch,
    SourceIdentityMismatch,
    SourceSizeMismatch,
    MissingCoverage,
    InputChangedDuringOperation,
    Cancelled,
    InsufficientDiskSpace,
    TransactionNeedsRecovery,
    NotImplemented,
    IoError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BilingualText {
    pub en: String,
    pub zh_cn: String,
}

impl BilingualText {
    pub fn new(en: impl Into<String>, zh_cn: impl Into<String>) -> Self {
        Self {
            en: en.into(),
            zh_cn: zh_cn.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeError {
    pub code: ErrorCode,
    pub message: BilingualText,
    pub suggestion: Box<BilingualText>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_details: Option<Box<CoverageFailureDetails>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageFailureDetails {
    pub missing_ranges: Vec<crate::ByteRange>,
    pub pending_ranges: Vec<crate::ByteRange>,
}

impl VolumeError {
    pub fn new(code: ErrorCode) -> Self {
        let (en, zh, hint_en, hint_zh) = match code {
            ErrorCode::InvalidArgument => (
                "Invalid command or option.",
                "命令或选项无效。",
                "Run --help for supported options.",
                "运行 --help 查看支持的选项。",
            ),
            ErrorCode::InvalidSize => (
                "Size must resolve to a positive whole number of bytes.",
                "容量必须对应正整数字节数。",
                "Use e.g. 100MiB, 1.5GiB, 500MB, or an integer byte count.",
                "请使用 100MiB、1.5GiB、500MB 或整数字节数。",
            ),
            ErrorCode::LimitExceeded => (
                "The request exceeds a supported numeric or part-count limit.",
                "请求超出数值或分卷数上限。",
                "Use at most 10,000 parts, a u64 byte size, and tolerance between 0 and 100%.",
                "最多允许 10,000 卷，容量不超过 u64，容差为 0 至 100%。",
            ),
            ErrorCode::InputNotFound => (
                "The input file or selected directory does not exist.",
                "输入文件或所选目录不存在。",
                "Select an existing regular file and directory.",
                "请选择已存在的普通文件和目录。",
            ),
            ErrorCode::InputNotRegularFile => (
                "Input must be a regular file.",
                "输入必须是普通文件。",
                "Do not select directories, devices, or pipes.",
                "请勿选择目录、设备或管道。",
            ),
            ErrorCode::UnsafePath => (
                "A path contains an unsafe name, symbolic link, or reparse point.",
                "路径包含不安全名称、符号链接或重解析点。",
                "Use a regular local path with safe base filenames.",
                "请使用具有安全基本文件名的普通本地路径。",
            ),
            ErrorCode::UnsupportedVolumeFormat => (
                "This input format is not supported by this operation.",
                "此操作不支持该输入格式。",
                "Split a complete .zip/.7z, or merge raw .001 volumes. Native .z01 ZIP volumes are unsupported.",
                "完整 .zip/.7z 可拆分，原始 .001 分卷可合并。不支持原生 .z01 ZIP 分卷。",
            ),
            ErrorCode::OutputExists => (
                "An output already exists; existing files were not overwritten.",
                "输出文件已存在；未覆盖已有文件。",
                "Select another existing output directory or a new --basename; overwriting is unavailable.",
                "请选择其他已存在的输出目录或新的 --basename；不提供覆盖功能。",
            ),
            ErrorCode::ManifestInvalid => (
                "The manifest violates the version 1 schema or range constraints.",
                "清单不符合版本 1 结构或字节区间约束。",
                "Check version, filenames, counts, hashes, and contiguous ranges. A manifest is not a signature.",
                "请检查版本、文件名、数量、摘要及连续区间。清单不是数字签名。",
            ),
            ErrorCode::ManifestRequired => (
                "Re-sharding requires a complete manifest-backed source group.",
                "重分卷需要具有完整清单的来源组。",
                "Select an existing .afparts.json and all its volumes. No source identity is inferred from filenames.",
                "请选择现有 .afparts.json 及其全部分卷；不会根据文件名推断来源身份。",
            ),
            ErrorCode::NotImplemented => (
                "This operation is not implemented in the current milestone.",
                "当前阶段尚未实现此操作。",
                "See the implementation status for supported operations.",
                "请查看实现状态文档中的可用操作。",
            ),
            ErrorCode::PartMissing => (
                "A volume is missing or its size is wrong.",
                "存在缺失分卷或分卷大小错误。",
                "Restore the missing volume; without a manifest, a missing final volume cannot be detected.",
                "请补齐缺失卷；无清单时无法检测最后一卷缺失。",
            ),
            ErrorCode::PartDuplicate => (
                "Volume numbering is duplicated or noncanonical.",
                "分卷编号重复或命名不规范。",
                "Use one sequence: .001, .002, ... .1000; do not mix numbering styles.",
                "请统一使用 .001、.002…….1000 编号，不要混用命名格式。",
            ),
            ErrorCode::PartGroupMismatch => (
                "Selected volumes use different base filenames.",
                "所选分卷使用不同的基本文件名，不能作为同一组合并。",
                "Select one numbered group. A matching filename alone does not verify byte identity.",
                "请选择同一套编号分卷；文件名相同本身不证明字节来源一致。",
            ),
            ErrorCode::OutputDirectoryRequired => (
                "Explicitly selected volumes require an output directory.",
                "手动选择分卷时必须指定输出目录。",
                "Select an existing output folder; no source drive is chosen implicitly.",
                "请选择一个已存在的输出文件夹；不会自动写入某个来源盘。",
            ),
            ErrorCode::PartHashMismatch => (
                "A volume does not match its recorded SHA-256.",
                "分卷与记录的 SHA-256 不一致。",
                "Preserve inputs and obtain a verified replacement volume.",
                "请保留输入并获取经验证的替代分卷。",
            ),
            ErrorCode::SourceHashMismatch => (
                "The complete byte stream does not match the expected SHA-256.",
                "完整字节流与预期 SHA-256 不一致。",
                "Check the selected manifest or trusted digest; do not use the output as verified.",
                "请核对清单或可信摘要，不得将输出当作已验证数据。",
            ),
            ErrorCode::SourceIdentityMismatch => (
                "Source and target manifests identify different complete byte streams.",
                "来源与目标清单标识不同的完整字节流。",
                "Use manifests with the same original size and SHA-256; matching names are insufficient.",
                "请使用原始大小和 SHA-256 相同的清单；文件名相同不足以确认身份。",
            ),
            ErrorCode::SourceSizeMismatch => (
                "The original file size does not match the target manifest.",
                "原文件大小与目标清单不一致。",
                "Select the intact original described by the manifest; no payload hash was verified by this size check.",
                "请选择清单对应的完整原文件；本次大小检查未验证载荷摘要。",
            ),
            ErrorCode::MissingCoverage => (
                "Complete, verified source coverage is unavailable.",
                "缺少完整、经验证的来源覆盖。",
                "Select complete manifest-backed source groups. Run coverage to inspect missing and pending byte ranges; incomplete groups cannot authorize repair.",
                "请选择完整清单来源组，运行 coverage 查看缺失与待验证字节区间；缺卷组不能授权修复。",
            ),
            ErrorCode::InputChangedDuringOperation => (
                "An input's identity, length, time, or content changed during processing.",
                "处理期间输入身份、长度、时间或内容发生变化。",
                "Stop other writers and retry after reviewing any retained task.",
                "请停止其他写入程序，检查保留的任务后重试。",
            ),
            ErrorCode::Cancelled => (
                "The task was cancelled; inputs were preserved.",
                "任务已取消；输入已保留。",
                "Review retained staging and the journal. Partly published outputs are not a completed group.",
                "请检查保留的暂存目录和日志，部分发布的分卷不代表完整分组。",
            ),
            ErrorCode::InsufficientDiskSpace => (
                "The space estimate or filesystem reports insufficient space.",
                "空间预估或文件系统报告空间不足。",
                "Free space and review the retained task; inputs were not deleted.",
                "请释放空间并检查保留的任务；输入未删除。",
            ),
            ErrorCode::TransactionNeedsRecovery => (
                "The selected output directory contains an unfinished or unrecognized task.",
                "所选输出目录中存在未完成或无法识别的任务。",
                "Run transactions DIRECTORY --json. Use recover TASK_DIRECTORY --dry-run --json for complete staging, or restart TASK_DIRECTORY --dry-run --json for an interrupted copy with a current start record. Legacy or unrecognized records stay untouched.",
                "请运行 transactions 目录 --json。完整暂存使用 recover 任务目录 --dry-run --json；带当前启动记录的未完成复制使用 restart 任务目录 --dry-run --json。旧版或无法识别记录保持原状。",
            ),
            ErrorCode::IoError => (
                "A filesystem operation failed.",
                "文件系统操作失败。",
                "Check permissions and drive availability, then retry.",
                "请检查权限和磁盘连接，然后重试。",
            ),
        };
        Self {
            code,
            message: BilingualText::new(en, zh),
            suggestion: Box::new(BilingualText::new(hint_en, hint_zh)),
            retryable: matches!(code, ErrorCode::IoError),
            recovery_task_id: None,
            part_index: None,
            coverage_details: None,
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.code {
            ErrorCode::InvalidArgument
            | ErrorCode::InvalidSize
            | ErrorCode::LimitExceeded
            | ErrorCode::OutputDirectoryRequired => 2,
            ErrorCode::OutputExists => 4,
            ErrorCode::NotImplemented => 5,
            ErrorCode::TransactionNeedsRecovery => 6,
            ErrorCode::Cancelled => 130,
            _ => 3,
        }
    }

    pub fn for_part(mut self, index: u32) -> Self {
        self.part_index = Some(index);
        self
    }
}

impl fmt::Display for VolumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} / {}", self.message.zh_cn, self.message.en)
    }
}

impl std::error::Error for VolumeError {}

pub(crate) fn io_error(error: std::io::Error) -> VolumeError {
    VolumeError::new(match error.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::InputNotFound,
        std::io::ErrorKind::AlreadyExists => ErrorCode::OutputExists,
        std::io::ErrorKind::StorageFull => ErrorCode::InsufficientDiskSpace,
        _ => ErrorCode::IoError,
    })
}
