//! Offline byte engine. All ranges are half-open and all sizes are bytes.
pub mod coverage;
pub mod engine;
pub mod error;
pub mod group;
pub mod manifest;
mod manifest_input;
pub mod paths;
pub mod plan;
pub mod rebuild;
pub mod recovery;
pub mod rescue;
pub mod reshard;
pub mod restart;
mod secure_fs;
pub mod size;
pub mod space;
mod stream;
pub mod task;
pub mod transaction;

pub use coverage::{CoverageReport, CoverageRequest, coverage};
pub use engine::{
    MergeFilePlan, MergeRequest, Receipt, SplitFilePlan, SplitRequest, merge_file, plan_merge_file,
    plan_split_file, split_file,
};
pub use error::{BilingualText, ErrorCode, Result, VolumeError};
pub use group::{
    GroupInspection, VerificationReport, inspect_group, inspect_selected_group, verify_group,
    verify_selected_group,
};
pub use manifest::{Manifest, PartDescriptor, parse_manifest};
pub use paths::{InputKind, SourceFile, validate_basename};
pub use plan::{ByteRange, PlannedPart, SplitPlan, SplitPolicy, plan_split};
pub use rebuild::{RebuildPartPlan, RebuildPartRequest, plan_rebuild_part, rebuild_part};
pub use rescue::{
    ReadSpan, RescuePartPlan, RescueRequest, VerifiedSourceSet, plan_rescue, rescue_part,
    verify_source_set,
};
pub use reshard::{ReshardFilePlan, ReshardRequest, plan_reshard_file, reshard_file};
pub use size::parse_size;
pub use task::{CancellationToken, IoPoint, Phase, Progress, TaskContext};

pub const PRODUCT_NAME: &str = concat!("便捷压缩助手_V", env!("CARGO_PKG_VERSION"));
pub const PRODUCT_NAME_EN: &str = "Easy Archive Assistant";

pub const SCHEMA_VERSION: u32 = 1;
pub const RESPONSE_SCHEMA_VERSION: u32 = 2;
pub const MAX_PARTS: usize = 10_000;
pub const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
