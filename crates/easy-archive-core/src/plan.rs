use crate::{ErrorCode, MAX_PARTS, Result, VolumeError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    pub start: u64,
    pub end_exclusive: u64,
}

impl ByteRange {
    pub fn new(start: u64, end_exclusive: u64) -> Result<Self> {
        if start > end_exclusive {
            return Err(VolumeError::new(ErrorCode::InvalidArgument));
        }
        Ok(Self {
            start,
            end_exclusive,
        })
    }

    pub fn checked_len(self) -> Result<u64> {
        self.end_exclusive
            .checked_sub(self.start)
            .ok_or_else(|| VolumeError::new(ErrorCode::InvalidArgument))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SplitPolicy {
    Hard {
        max_bytes: u64,
    },
    Balanced {
        target_bytes: u64,
        tolerance_ppm: u32,
        hard_max_bytes: Option<u64>,
    },
}

impl SplitPolicy {
    pub fn effective_max(self) -> Result<u64> {
        let invalid = || VolumeError::new(ErrorCode::InvalidSize);
        match self {
            Self::Hard { max_bytes: 0 } => Err(invalid()),
            Self::Hard { max_bytes } => Ok(max_bytes),
            Self::Balanced {
                target_bytes,
                tolerance_ppm,
                hard_max_bytes,
            } => {
                if target_bytes == 0 || hard_max_bytes == Some(0) {
                    return Err(invalid());
                }
                if tolerance_ppm > 1_000_000 {
                    return Err(VolumeError::new(ErrorCode::LimitExceeded));
                }
                let expanded =
                    u128::from(target_bytes) * (1_000_000 + u128::from(tolerance_ppm)) / 1_000_000;
                let max = expanded.min(u128::from(hard_max_bytes.unwrap_or(u64::MAX)));
                u64::try_from(max).map_err(|_| VolumeError::new(ErrorCode::LimitExceeded))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedPart {
    pub index: u32,
    pub range: ByteRange,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SplitPlan {
    pub original_size: u64,
    pub policy: SplitPolicy,
    pub effective_max_bytes: u64,
    pub part_count: u32,
    pub requested_alignment: Option<u64>,
    pub alignment_applied: bool,
    pub parts: Vec<PlannedPart>,
}

/// Pure planner. Memory is bounded by MAX_PARTS, regardless of source length.
/// Alignment is best-effort: reject the entire aligned candidate if it breaks
/// a cap or produces an empty nonzero-source part, then use exact balancing.
pub fn plan_split(size: u64, policy: SplitPolicy, alignment: Option<u64>) -> Result<SplitPlan> {
    let cap = policy.effective_max()?;
    if alignment == Some(0) {
        return Err(VolumeError::new(ErrorCode::InvalidSize));
    }
    let count = size.div_ceil(cap).max(1);
    if count > MAX_PARTS as u64 {
        return Err(VolumeError::new(ErrorCode::LimitExceeded));
    }
    let mut cuts = Vec::with_capacity(count as usize + 1);
    cuts.push(0);
    let quotient = size / count;
    let remainder = size % count;
    for i in 1..count {
        cuts.push(quotient * i + i.min(remainder));
    }
    cuts.push(size);
    let mut alignment_applied = false;
    if let Some(alignment) = alignment.filter(|_| count > 1) {
        let aligned: Option<Vec<u64>> = cuts
            .iter()
            .enumerate()
            .map(|(i, &cut)| {
                if i == 0 || i == count as usize {
                    Some(cut)
                } else {
                    let a = u128::from(alignment);
                    u64::try_from((u128::from(cut) + a / 2) / a * a).ok()
                }
            })
            .collect();
        if let Some(aligned) = aligned.filter(|candidate| {
            candidate
                .windows(2)
                .all(|w| w[1] > w[0] && w[1] - w[0] <= cap)
        }) {
            cuts = aligned;
            alignment_applied = true;
        }
    }
    let parts = cuts
        .windows(2)
        .enumerate()
        .map(|(i, w)| PlannedPart {
            index: i as u32 + 1,
            range: ByteRange {
                start: w[0],
                end_exclusive: w[1],
            },
            size: w[1] - w[0],
        })
        .collect();
    Ok(SplitPlan {
        original_size: size,
        policy,
        effective_max_bytes: cap,
        part_count: count as u32,
        requested_alignment: alignment,
        alignment_applied,
        parts,
    })
}
