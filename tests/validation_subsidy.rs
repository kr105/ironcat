// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::validation::subsidy::{COIN, HALVING_INTERVAL, INITIAL_SUBSIDY, MAX_MONEY, get_block_subsidy};

#[test]
fn constants_are_correct() {
	assert_eq!(COIN, 100_000_000);
	assert_eq!(MAX_MONEY, 2_100_000_000_000_000);
	assert_eq!(HALVING_INTERVAL, 210_000);
	assert_eq!(INITIAL_SUBSIDY, 5_000_000_000);
}

#[test]
fn subsidy_at_genesis() {
	assert_eq!(get_block_subsidy(0), 50 * COIN);
}

#[test]
fn subsidy_before_first_halving() {
	assert_eq!(get_block_subsidy(209_999), 50 * COIN);
}

#[test]
fn subsidy_at_first_halving() {
	assert_eq!(get_block_subsidy(210_000), 25 * COIN);
}

#[test]
fn subsidy_at_second_halving() {
	assert_eq!(get_block_subsidy(420_000), 1_250_000_000); // 12.5 CAT
}

#[test]
fn subsidy_at_third_halving() {
	assert_eq!(get_block_subsidy(630_000), 625_000_000); // 6.25 CAT
}

#[test]
fn subsidy_zero_after_64_halvings() {
	// 64 * 210_000 = 13_440_000
	assert_eq!(get_block_subsidy(13_440_000), 0);
	assert_eq!(get_block_subsidy(u32::MAX), 0);
}

#[test]
fn total_supply_does_not_exceed_max_money() {
	// Integer truncation during halvings means the actual mined total is
	// slightly less than MAX_MONEY (same behavior as Bitcoin)
	let mut total: i64 = 0;
	let mut height: u32 = 0;
	loop {
		let subsidy = get_block_subsidy(height);
		if subsidy == 0 {
			break;
		}
		// Each halving period has HALVING_INTERVAL blocks at this subsidy
		total += subsidy * i64::from(HALVING_INTERVAL);
		height += HALVING_INTERVAL;
	}
	// 33 halving periods produce coins before subsidy drops to 0
	assert_eq!(height, 33 * HALVING_INTERVAL);
	// Actual total is 2,099,999,997,690,000 catoshis due to truncation
	assert_eq!(total, 2_099_999_997_690_000);
	assert!(total <= MAX_MONEY);
}
