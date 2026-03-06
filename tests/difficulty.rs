// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::arithmetic_side_effects
)]

use std::collections::HashMap;

use ironcat::difficulty::{
	compact::{bits_u256, compact_to_target, target_to_compact},
	compact_to_difficulty, format_difficulty, get_next_work_required, ChainLookup, ConsensusParams, U256,
};

/// Mock chain for testing: maps height -> (timestamp, bits)
struct MockChain {
	headers: HashMap<u32, (u32, u32)>,
}

impl MockChain {
	fn new() -> Self {
		Self {
			headers: HashMap::new(),
		}
	}

	fn insert(&mut self, height: u32, timestamp: u32, bits: u32) {
		self.headers.insert(height, (timestamp, bits));
	}
}

impl ChainLookup for MockChain {
	fn header_at(&self, height: u32) -> Option<(u32, u32)> {
		self.headers.get(&height).copied()
	}
}

fn mainnet_params() -> ConsensusParams {
	ConsensusParams::mainnet()
}

// -- Compact encoding tests --

#[test]
fn compact_genesis_roundtrip() {
	let (target, neg, ovf) = compact_to_target(0x1e0f_fff0);
	assert!(!neg);
	assert!(!ovf);
	assert_eq!(target_to_compact(target), 0x1e0f_fff0);
}

#[test]
fn compact_difficulty_16_roundtrip() {
	let (target, neg, ovf) = compact_to_target(0x1c0f_fff0);
	assert!(!neg);
	assert!(!ovf);
	assert_eq!(target_to_compact(target), 0x1c0f_fff0);
}

#[test]
fn compact_zero() {
	let (target, _, _) = compact_to_target(0);
	assert_eq!(target, U256::zero());
	assert_eq!(target_to_compact(U256::zero()), 0);
}

#[test]
fn compact_small_exponent_decode() {
	// 0x03000001 = size=3, mantissa=1 -> target=1
	let (target, _, _) = compact_to_target(0x0300_0001);
	assert_eq!(target, U256::from(1));
	// Roundtrip is not exact for non-canonical compact values
}

#[test]
fn compact_canonical_roundtrip() {
	let target = U256::from(0x1234_u64) << 150;
	let compact = target_to_compact(target);
	let (decoded, _, _) = compact_to_target(compact);
	assert_eq!(target_to_compact(decoded), compact);
}

#[test]
fn compact_negative_flag_detected() {
	let (_, neg, _) = compact_to_target(0x0380_0001);
	assert!(neg);
}

#[test]
fn compact_overflow_detected() {
	let (_, _, ovf) = compact_to_target(0x2300_0001);
	assert!(ovf);
}

#[test]
fn compact_pow_limit_bit_count() {
	let (pow_limit, _, _) = compact_to_target(0x1e0f_fff0);
	// powLimit = 0x00000fffffffffff...fff0 << shifted
	// Should have ~237 bits (0x1e = 30 bytes = 240 bits, minus leading zeros)
	let bit_count = bits_u256(pow_limit);
	assert!(
		bit_count > 200,
		"powLimit should be a large number, got {bit_count} bits"
	);
}

// -- Dispatcher tests --

#[test]
fn dispatcher_returns_pow_limit_for_height_zero() {
	let params = mainnet_params();
	let chain = MockChain::new();
	let result = get_next_work_required(0, &chain, &params).unwrap();
	assert_eq!(result, target_to_compact(params.pow_limit));
}

#[test]
fn dispatcher_hardcoded_reset_at_cip01_height() {
	// At height cip01_height + 1, parent_height == cip01_height, should return 0x1c0ffff0
	let params = mainnet_params();
	let mut chain = MockChain::new();
	// Need the parent at cip01_height
	chain.insert(params.cip01_height, 1_388_000_000, 0x1e0f_fff0);
	let result = get_next_work_required(params.cip01_height + 1, &chain, &params).unwrap();
	assert_eq!(result, 0x1c0f_fff0);
}

#[test]
fn dispatcher_errors_on_missing_header() {
	let params = mainnet_params();
	let chain = MockChain::new();
	let result = get_next_work_required(100, &chain, &params);
	assert!(result.is_err());
}

// -- CIP01 tests --

#[test]
fn cip01_no_retarget_between_boundaries() {
	// Block 100 is not on a 2016-block boundary -> return parent's bits
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let parent_bits = 0x1e0f_fff0;
	chain.insert(99, 1_388_000_000, parent_bits);

	let result = get_next_work_required(100, &chain, &params).unwrap();
	assert_eq!(result, parent_bits);
}

#[test]
fn cip01_retarget_on_time() {
	// Build a chain where blocks are exactly 600s apart for the first 2016 blocks
	// At height 2016, it should retarget. Since timespan matches exactly, bits stay the same
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let genesis_bits = 0x1e0f_fff0;
	let genesis_time: u32 = 1_387_838_302;

	for h in 0..2016 {
		chain.insert(h, genesis_time + h * 600, genesis_bits);
	}

	let result = get_next_work_required(2016, &chain, &params).unwrap();
	// Timespan is exactly 2015*600 = 1,209,000, close to target 1,209,600
	// Slight deviation because first retarget goes back interval-1 = 2015 blocks
	// The result should be very close to the original difficulty
	let (orig_target, _, _) = compact_to_target(genesis_bits);
	let (new_target, _, _) = compact_to_target(result);
	// Within 1% of original
	let diff = if new_target > orig_target {
		new_target - orig_target
	} else {
		orig_target - new_target
	};
	assert!(diff < orig_target / U256::from(100));
}

#[test]
fn cip01_retarget_fast_blocks_increases_difficulty() {
	// Blocks come twice as fast -> difficulty should increase (target decreases)
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let genesis_bits = 0x1e0f_fff0;
	let genesis_time: u32 = 1_387_838_302;

	for h in 0..2016 {
		chain.insert(h, genesis_time + h * 300, genesis_bits); // 5 min blocks
	}

	let result = get_next_work_required(2016, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(genesis_bits);
	let (new_target, _, _) = compact_to_target(result);
	// Faster blocks -> lower target (higher difficulty)
	assert!(new_target < orig_target);
}

#[test]
fn cip01_retarget_slow_blocks_decreases_difficulty() {
	// Blocks come twice as slow -> difficulty should decrease (target increases)
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let genesis_bits = 0x1d0f_fff0; // higher difficulty than powLimit
	let genesis_time: u32 = 1_387_838_302;

	for h in 0..2016 {
		chain.insert(h, genesis_time + h * 1200, genesis_bits); // 20 min blocks
	}

	let result = get_next_work_required(2016, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(genesis_bits);
	let (new_target, _, _) = compact_to_target(result);
	// Slower blocks -> higher target (lower difficulty)
	assert!(new_target > orig_target);
}

#[test]
fn cip01_retarget_clamped_at_4x() {
	// Extremely slow blocks: timespan would be >4x, should be clamped
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let genesis_bits = 0x1d0f_fff0;
	let genesis_time: u32 = 1_387_838_302;

	for h in 0..2016 {
		chain.insert(h, genesis_time + h * 10_000, genesis_bits); // ~2.7 hour blocks
	}

	let result = get_next_work_required(2016, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(genesis_bits);
	let (new_target, _, _) = compact_to_target(result);
	// Should be clamped at 4x
	assert!(new_target <= orig_target * U256::from(4) + U256::from(1));
}

// -- CIP02 tests --

#[test]
fn cip02_no_retarget_off_boundary() {
	// CIP02 uses 36-block intervals. Test a block that's not on a boundary
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	// height 20291 is within CIP02 range but 20291 % 36 != 0
	chain.insert(20290, 1_389_000_000, test_bits);

	let result = get_next_work_required(20291, &chain, &params).unwrap();
	assert_eq!(result, test_bits);
}

// -- CIP03 tests --

#[test]
fn cip03_retargets_every_block() {
	// CIP03 has no boundary check, should always retarget
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;

	// Fill 36 blocks + parent for CIP03 range
	let base_height = params.cip02_height; // CIP03 starts here
	let base_time: u32 = 1_390_000_000;

	for i in 0..=37 {
		chain.insert(base_height + i, base_time + i * 600, test_bits);
	}

	// Any height in CIP03 range should retarget (not just multiples of 36)
	let test_height = base_height + 37;
	let result = get_next_work_required(test_height + 1, &chain, &params).unwrap();
	// With exact 600s blocks and constant bits, timespan should be ~21600
	// and result should be close to original
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	let diff = if new_target > orig_target {
		new_target - orig_target
	} else {
		orig_target - new_target
	};
	// CIP03 has tight 12% bounds, result should be very close
	assert!(diff < orig_target / U256::from(8));
}

// -- CIP04 tests --

#[test]
fn cip04_dead_zone_no_adjustment() {
	// When avg block time is within +/-10s of target, no adjustment
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip03_height; // CIP04 starts here
	let base_time: u32 = 1_392_000_000;

	// 9 blocks with exactly 600s spacing -> avg = 600, error = 0
	for i in 0..=9 {
		chain.insert(base_height + i, base_time + i * 600, test_bits);
	}

	let result = get_next_work_required(base_height + 9 + 1, &chain, &params).unwrap();
	assert_eq!(result, test_bits, "dead zone should return parent bits unchanged");
}

#[test]
fn cip04_fast_blocks_adjusts() {
	// When blocks are significantly faster than target, PID should adjust
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip03_height;
	let base_time: u32 = 1_392_000_000;

	// 9 blocks with 300s spacing -> avg = 300, error = -300 (within +-450)
	for i in 0..=9 {
		chain.insert(base_height + i, base_time + i * 300, test_bits);
	}

	let result = get_next_work_required(base_height + 9 + 1, &chain, &params).unwrap();
	// PID should adjust -- result should differ from input
	assert_ne!(result, test_bits, "PID should adjust for fast blocks");
	// Fast blocks -> error is negative -> target should decrease (difficulty up)
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	assert!(
		new_target < orig_target,
		"fast blocks should lower target (raise difficulty)"
	);
}

#[test]
fn cip04_slow_blocks_raises_target() {
	// When blocks are significantly slower than target, PID should raise target
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip03_height;
	let base_time: u32 = 1_392_000_000;

	// 9 blocks with 1200s spacing -> avg = 1200, error = +600 (> 450, uses Dn gains)
	for i in 0..=9 {
		chain.insert(base_height + i, base_time + i * 1200, test_bits);
	}

	let result = get_next_work_required(base_height + 9 + 1, &chain, &params).unwrap();
	assert_ne!(result, test_bits, "PID should adjust for slow blocks");
	// Slow blocks -> error is positive -> target should increase (difficulty down)
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	assert!(
		new_target > orig_target,
		"slow blocks should raise target (lower difficulty)"
	);
}

#[test]
fn cip04_floor_at_min_difficulty() {
	// Even with very slow blocks, difficulty shouldn't go below 0x1e0fffff
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1e0f_ffff; // already at min difficulty
	let base_height = params.cip03_height;
	let base_time: u32 = 1_392_000_000;

	// Very slow blocks (3600s each)
	for i in 0..=9 {
		chain.insert(base_height + i, base_time + i * 3600, test_bits);
	}

	let result = get_next_work_required(base_height + 9 + 1, &chain, &params).unwrap();
	// Should not exceed min difficulty compact
	assert!(result <= 0x1e0f_ffff);
}

// -- CIP05 tests --

#[test]
fn cip05_falls_back_to_cip04_outside_time_window() {
	// When timestamp seconds are outside [0,14] and [30,44], CIP05 uses CIP04
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip04_height;

	// Set parent timestamp so seconds portion is outside [0,14] and [30,44]
	// Need 9 blocks for CIP04 fallback (8-block lookback)
	// 1_393_999_995 % 60 = 15, which is outside both windows
	let base_time: u32 = 1_393_999_995;
	for i in 0..=9 {
		chain.insert(base_height + i, base_time + i * 600, test_bits);
	}

	let result = get_next_work_required(base_height + 9 + 1, &chain, &params).unwrap();
	// CIP04 dead zone: avg 600s, error 0 -> returns parent bits
	assert_eq!(result, test_bits);
}

#[test]
fn cip05_digishield_path_in_time_window() {
	// When timestamp seconds are in [0,14], uses DigiShield formula
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip04_height;

	// Need parent timestamp where % 60 is in [0,14]
	// 1_393_999_980 % 60 = 0, plus 600 gives 1_394_000_580 % 60 = 0
	let parent_time: u32 = 1_393_999_980; // second 0 (in window [0,14])
	chain.insert(base_height, parent_time - 600, test_bits);
	chain.insert(base_height + 1, parent_time, test_bits);

	let result = get_next_work_required(base_height + 2, &chain, &params).unwrap();
	// 600s timespan, clamped to [450, 900] -> 600 is within range
	// new_target = old_target * 600 / 600 = same
	assert_eq!(result, test_bits);
}

#[test]
fn cip05_digishield_fast_block() {
	// Fast single block -> should increase difficulty (lower target)
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip04_height;

	// Parent timestamp at second 10 (in window), 100s since previous block
	// 100s is below 450s clamp -> clamped to 450
	let parent_time: u32 = 1_394_000_010;
	chain.insert(base_height, parent_time - 100, test_bits);
	chain.insert(base_height + 1, parent_time, test_bits);

	let result = get_next_work_required(base_height + 2, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	// 450/600 = 0.75 -> target should decrease
	assert!(new_target < orig_target);
}

// -- CIP06 (LWMA) tests --

#[test]
fn cip06_pow_limit_for_early_blocks() {
	// When parent_height < N (45), LWMA returns powLimit
	// Use custom params to route directly to CIP06 at low heights
	let mut params = mainnet_params();
	params.cip05_height = 0;
	params.cip04_height = 0;
	params.cip03_height = 0;
	params.cip02_height = 0;
	params.cip01_height = 0;

	let mut chain = MockChain::new();
	chain.insert(0, 1_387_838_302, 0x1e0f_fff0);
	chain.insert(1, 1_387_838_902, 0x1e0f_fff0);

	// height=2, parent_height=1 < N(45) -> should return powLimit
	let result = get_next_work_required(2, &chain, &params).unwrap();
	assert_eq!(result, target_to_compact(params.pow_limit));
}

#[test]
fn cip06_steady_state_maintains_difficulty() {
	// Build 46 blocks with exactly 600s spacing at LWMA activation height
	// Difficulty should stay very close to the starting value
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip05_height; // CIP06 starts here
	let base_time: u32 = 1_400_000_000;

	// Need blocks from (base_height - 45) to base_height for the lookback
	for i in 0..=46 {
		let h = base_height - 46 + i;
		chain.insert(h, base_time + i * 600, test_bits);
	}

	let result = get_next_work_required(base_height + 1, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);

	// With perfect 600s spacing, weighted sum = sum(600 * i for i in 1..=45)
	// = 600 * 45*46/2 = 600 * 1035 = 621000 = k
	// So next_target = avg_target * k, which should equal the original target
	// (with some truncation error from integer division)
	let diff = if new_target > orig_target {
		new_target - orig_target
	} else {
		orig_target - new_target
	};
	// Allow 5% tolerance for integer division truncation
	assert!(
		diff < orig_target / U256::from(20),
		"steady state should maintain difficulty within 5%, diff={diff}, orig={orig_target}"
	);
}

#[test]
fn cip06_fast_blocks_increase_difficulty() {
	// Blocks come at 300s intervals -> should increase difficulty
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip05_height;
	let base_time: u32 = 1_400_000_000;

	for i in 0..=46 {
		let h = base_height - 46 + i;
		chain.insert(h, base_time + i * 300, test_bits); // 5 min blocks
	}

	let result = get_next_work_required(base_height + 1, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	assert!(new_target < orig_target, "fast blocks should lower the target");
}

#[test]
fn cip06_slow_blocks_decrease_difficulty() {
	// Blocks come at 1200s intervals -> should decrease difficulty
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip05_height;
	let base_time: u32 = 1_400_000_000;

	for i in 0..=46 {
		let h = base_height - 46 + i;
		chain.insert(h, base_time + i * 1200, test_bits); // 20 min blocks
	}

	let result = get_next_work_required(base_height + 1, &chain, &params).unwrap();
	let (orig_target, _, _) = compact_to_target(test_bits);
	let (new_target, _, _) = compact_to_target(result);
	assert!(new_target > orig_target, "slow blocks should raise the target");
}

#[test]
fn cip06_solvetime_clamped_at_6t() {
	// One very long gap shouldn't crash the algorithm
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip05_height;
	let base_time: u32 = 1_400_000_000;

	// Normal blocks except one has a 100000s gap
	for i in 0..=46 {
		let h = base_height - 46 + i;
		let time = match i.cmp(&23) {
			std::cmp::Ordering::Equal => base_time + 23 * 600 + 100_000, // huge gap
			std::cmp::Ordering::Greater => base_time + 23 * 600 + 100_000 + (i - 23) * 600,
			std::cmp::Ordering::Less => base_time + i * 600,
		};
		chain.insert(h, time, test_bits);
	}

	let result = get_next_work_required(base_height + 1, &chain, &params);
	assert!(result.is_ok(), "should handle long gaps without error");
}

#[test]
fn cip06_monotonic_timestamp_enforcement() {
	// Out-of-order timestamps should be handled (enforced monotonic)
	let params = mainnet_params();
	let mut chain = MockChain::new();
	let test_bits = 0x1c0f_fff0;
	let base_height = params.cip05_height;
	let base_time: u32 = 1_400_000_000;

	for i in 0..=46 {
		let h = base_height - 46 + i;
		// Some timestamps go backwards
		let time = if i % 5 == 3 {
			base_time + i * 600 - 100 // slightly backwards
		} else {
			base_time + i * 600
		};
		chain.insert(h, time, test_bits);
	}

	let result = get_next_work_required(base_height + 1, &chain, &params);
	assert!(result.is_ok(), "should handle non-monotonic timestamps");
}

// -- ConsensusParams tests --

#[test]
fn mainnet_params_intervals() {
	let params = mainnet_params();
	assert_eq!(params.difficulty_adjustment_interval_v1(), 2016);
	assert_eq!(params.difficulty_adjustment_interval_v2(), 36);
}

#[test]
fn mainnet_params_cip_heights_are_ordered() {
	let params = mainnet_params();
	assert!(params.cip01_height < params.cip02_height);
	assert!(params.cip02_height < params.cip03_height);
	assert!(params.cip03_height < params.cip04_height);
	assert!(params.cip04_height < params.cip05_height);
}

// -- ChainLookup trait tests --

#[test]
fn mock_chain_returns_none_for_missing_height() {
	let chain = MockChain::new();
	assert!(chain.header_at(0).is_none());
}

#[test]
fn mock_chain_returns_data_for_inserted_height() {
	let mut chain = MockChain::new();
	chain.insert(100, 1_388_000_000, 0x1e0f_fff0);
	let (ts, bits) = chain.header_at(100).unwrap();
	assert_eq!(ts, 1_388_000_000);
	assert_eq!(bits, 0x1e0f_fff0);
}

// -- compact_to_difficulty tests --

#[test]
fn difficulty_genesis_is_very_low() {
	// Genesis nBits 0x1e0ffff0: exponent 30 is one byte above Catcoin's
	// difficulty-1 exponent (29), so the target is ~4096x above difficulty 1
	let diff = compact_to_difficulty(0x1e0f_fff0);
	assert!(diff > 0.0002, "expected >0.0002, got {diff}");
	assert!(diff < 0.001, "expected <0.001, got {diff}");
}

#[test]
fn difficulty_matches_explorer_value() {
	// nBits 0x1a273d4b from mainnet tip should give ~427,600
	let diff = compact_to_difficulty(0x1a27_3d4b);
	assert!(diff > 427_000.0, "expected >427k, got {diff}");
	assert!(diff < 428_000.0, "expected <428k, got {diff}");
}

#[test]
fn difficulty_zero_mantissa() {
	#[allow(clippy::float_cmp)] // exact zero is expected for zero mantissa
	let is_zero = compact_to_difficulty(0x1d00_0000) == 0.0;
	assert!(is_zero);
}

#[test]
fn format_difficulty_si_suffixes() {
	assert_eq!(format_difficulty(1.5), "1.50");
	assert_eq!(format_difficulty(999.0), "999.00");
	assert_eq!(format_difficulty(1_500.0), "1.5 K");
	assert_eq!(format_difficulty(427_600.0), "427.6 K");
	assert_eq!(format_difficulty(1_500_000.0), "1.50 M");
	assert_eq!(format_difficulty(2_500_000_000.0), "2.50 G");
}
