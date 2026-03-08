// SPDX-License-Identifier: Apache-2.0

//! Block subsidy (reward) calculation and monetary constants

/// Number of catoshis per coin (10^8)
pub const COIN: i64 = 100_000_000;

/// Maximum total supply in catoshis (21 million coins)
#[allow(clippy::arithmetic_side_effects)] // constant multiplication of known values
pub const MAX_MONEY: i64 = 21_000_000 * COIN;

/// Number of blocks between subsidy halvings
pub const HALVING_INTERVAL: u32 = 210_000;

/// Initial block reward in catoshis (50 coins)
#[allow(clippy::arithmetic_side_effects)] // constant multiplication of known values
pub const INITIAL_SUBSIDY: i64 = 50 * COIN;

/// Returns the block reward in catoshis for a given chain height
///
/// The subsidy starts at 50 CAT and halves every 210,000 blocks.
/// Returns 0 when the number of halvings reaches 64 (prevents
/// shift overflow on the i64)
pub const fn get_block_subsidy(height: u32) -> i64 {
	#[allow(clippy::arithmetic_side_effects)] // height / constant, no overflow possible
	let halvings = height / HALVING_INTERVAL;

	if halvings >= 64 {
		return 0;
	}

	// halvings < 64, so right-shift is safe and won't lose the sign bit
	#[allow(clippy::arithmetic_side_effects)]
	let subsidy = INITIAL_SUBSIDY >> halvings;
	subsidy
}
