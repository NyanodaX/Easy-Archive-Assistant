use easy_archive_core::{ByteRange, ErrorCode, MAX_PARTS, SplitPolicy, plan_split};

fn check(size: u64, policy: SplitPolicy, alignment: Option<u64>) {
    let cap = policy.effective_max().unwrap();
    let count = ((u128::from(size) + u128::from(cap) - 1) / u128::from(cap)).max(1) as u64;
    let result = plan_split(size, policy, alignment);
    if count > MAX_PARTS as u64 {
        assert_eq!(result.unwrap_err().code, ErrorCode::LimitExceeded);
        return;
    }
    let plan = result.unwrap();
    assert_eq!(plan.part_count as u64, count);
    assert_eq!(plan.parts.len(), count as usize);
    assert_eq!(plan.parts[0].range.start, 0);
    assert_eq!(plan.parts.last().unwrap().range.end_exclusive, size);
    let mut end = 0;
    let mut total: u128 = 0;
    for (i, part) in plan.parts.iter().enumerate() {
        assert_eq!(part.index as usize, i + 1);
        assert_eq!(part.range.start, end);
        assert_eq!(part.range.checked_len().unwrap(), part.size);
        assert!(part.size <= cap);
        assert!(part.size > 0 || size == 0);
        if plan.alignment_applied && i + 1 < plan.parts.len() {
            assert_eq!(part.range.end_exclusive % alignment.unwrap(), 0);
        }
        end = part.range.end_exclusive;
        total += u128::from(part.size);
    }
    assert_eq!(total, u128::from(size));
    if !plan.alignment_applied {
        let min = plan.parts.iter().map(|p| p.size).min().unwrap();
        let max = plan.parts.iter().map(|p| p.size).max().unwrap();
        assert!(max - min <= 1);
    }
}

#[test]
fn empty_one_and_target_boundaries() {
    for target in [1, 2, 3, 100, 1 << 20, 1 << 32, u64::MAX] {
        for size in [0, 1, target - 1, target, target.saturating_add(1)] {
            check(size, SplitPolicy::Hard { max_bytes: target }, None);
            check(
                size,
                SplitPolicy::Balanced {
                    target_bytes: target,
                    tolerance_ppm: 50_000,
                    hard_max_bytes: None,
                },
                Some(1 << 20),
            );
        }
    }
}

#[test]
fn exhaustive_small_sizes_are_balanced_and_gapless() {
    for target in 1..=80 {
        for size in 0..=500 {
            check(size, SplitPolicy::Hard { max_bytes: target }, None);
            check(
                size,
                SplitPolicy::Balanced {
                    target_bytes: target,
                    tolerance_ppm: 50_000,
                    hard_max_bytes: Some(target + 2),
                },
                Some(8),
            );
        }
    }
}

#[test]
fn deterministic_random_u64_sizes_and_alignment_preserve_invariants() {
    let mut state = 0x1234_5678_90ab_cdef_u64;
    for i in 0..10_000 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let size = state;
        let target = (state.rotate_left(23) / 5_000).max(1);
        let policy = if i % 2 == 0 {
            SplitPolicy::Hard { max_bytes: target }
        } else {
            SplitPolicy::Balanced {
                target_bytes: target,
                tolerance_ppm: (i * 97) % 1_000_001,
                hard_max_bytes: Some(target.saturating_mul(2)),
            }
        };
        check(
            size,
            policy,
            Some(if i % 3 == 0 { u64::MAX } else { 1 << 20 }),
        );
    }
}

#[test]
fn balanced_reduces_tail_without_exceeding_tolerance() {
    let hard = plan_split(205, SplitPolicy::Hard { max_bytes: 100 }, None).unwrap();
    let balanced = plan_split(
        205,
        SplitPolicy::Balanced {
            target_bytes: 100,
            tolerance_ppm: 50_000,
            hard_max_bytes: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(
        hard.parts.iter().map(|p| p.size).collect::<Vec<_>>(),
        [69, 68, 68]
    );
    assert_eq!(
        balanced.parts.iter().map(|p| p.size).collect::<Vec<_>>(),
        [103, 102]
    );
    assert_eq!(balanced.effective_max_bytes, 105);
}

#[test]
fn explicit_hard_cap_wins_over_tolerance() {
    let policy = SplitPolicy::Balanced {
        target_bytes: 100,
        tolerance_ppm: 1_000_000,
        hard_max_bytes: Some(80),
    };
    assert_eq!(policy.effective_max().unwrap(), 80);
    check(300, policy, Some(16));
}

#[test]
fn alignment_is_best_effort_and_never_changes_coverage() {
    let yes = plan_split(100, SplitPolicy::Hard { max_bytes: 60 }, Some(10)).unwrap();
    assert!(yes.alignment_applied);
    let no = plan_split(101, SplitPolicy::Hard { max_bytes: 51 }, Some(32)).unwrap();
    assert!(!no.alignment_applied);
    assert_eq!(
        no.parts.iter().map(|p| p.size).collect::<Vec<_>>(),
        [51, 50]
    );
    check(
        u64::MAX,
        SplitPolicy::Hard {
            max_bytes: u64::MAX / 2,
        },
        Some(u64::MAX),
    );
}

#[test]
fn part_count_is_bounded_before_allocation() {
    assert_eq!(
        plan_split(u64::MAX, SplitPolicy::Hard { max_bytes: 1 }, None)
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    assert!(plan_split(MAX_PARTS as u64, SplitPolicy::Hard { max_bytes: 1 }, None).is_ok());
    assert!(
        plan_split(
            MAX_PARTS as u64 + 1,
            SplitPolicy::Hard { max_bytes: 1 },
            None
        )
        .is_err()
    );
}

#[test]
fn invalid_zero_parameters_and_inverted_ranges_are_rejected() {
    assert!(plan_split(0, SplitPolicy::Hard { max_bytes: 0 }, None).is_err());
    assert!(plan_split(1, SplitPolicy::Hard { max_bytes: 1 }, Some(0)).is_err());
    for (target, tolerance, cap) in [(0, 0, None), (1, 0, Some(0)), (1, 1_000_001, None)] {
        assert!(
            plan_split(
                1,
                SplitPolicy::Balanced {
                    target_bytes: target,
                    tolerance_ppm: tolerance,
                    hard_max_bytes: cap
                },
                None
            )
            .is_err()
        );
    }
    assert!(ByteRange::new(2, 1).is_err());
    assert!(
        ByteRange {
            start: 2,
            end_exclusive: 1
        }
        .checked_len()
        .is_err()
    );
}
