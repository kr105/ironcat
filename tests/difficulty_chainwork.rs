// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used)]

use ironcat::difficulty::U256;
use ironcat::difficulty::compact::work_from_bits;

#[test]
fn work_from_genesis_bits() {
	// Catcoin genesis nBits = 0x1e0ffff0
	let work = work_from_bits(0x1e0f_fff0);
	// work = 2^256 / (target + 1), should be a small positive number
	assert!(work > U256::zero());
}

#[test]
fn work_from_zero_bits_is_zero() {
	// Zero target means zero work (invalid but should not panic)
	let work = work_from_bits(0);
	assert_eq!(work, U256::zero());
}

#[test]
fn higher_difficulty_means_more_work() {
	// Lower target = higher difficulty = more work per block
	let work_easy = work_from_bits(0x1e0f_fff0); // genesis (easy)
	let work_hard = work_from_bits(0x1c0f_fff0); // difficulty 16 reset
	assert!(work_hard > work_easy);
}

#[test]
fn work_from_bits_negative_target_is_zero() {
	// Negative flag set -> invalid target -> zero work
	let work = work_from_bits(0x0380_0001);
	assert_eq!(work, U256::zero());
}

#[test]
fn work_from_bits_overflow_target_is_zero() {
	// Overflow target -> zero work
	let work = work_from_bits(0x2300_0001);
	assert_eq!(work, U256::zero());
}
