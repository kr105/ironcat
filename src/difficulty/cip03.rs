// SPDX-License-Identifier: Apache-2.0

//! CIP03: Tight-bounds retarget (blocks 21346 - 27259)
//!
//! Every-block retarget using a 36-block window like CIP02, but with much
//! tighter clamping bounds of +/-12% (ratio 112/100) instead of CIP02's
//! 0.25x-4x range. This prevents the difficulty oscillations that plagued
//! CIP02 while maintaining responsive adjustment.
//!
//! Unlike CIP01/CIP02, CIP03 retargets every block (no interval boundary check)

use anyhow::Result;

use super::{
	ChainLookup, ConsensusParams,
	compact::{compact_to_target, target_to_compact},
	lookup_header,
};

/// Computes the expected `nBits` for `height` under CIP03 rules
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher
	let (parent_timestamp, parent_bits) = lookup_header(chain, parent_height)?;

	// Always go back the full interval (36 blocks), no boundary check
	let interval = params.difficulty_adjustment_interval_v2();

	#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	let first_height = parent_height.saturating_sub(interval as u32);
	let (first_timestamp, _) = lookup_header(chain, first_height)?;

	let mut actual_timespan = i64::from(parent_timestamp) - i64::from(first_timestamp);

	// Clamp to +/-12% of target timespan (ratio 112/100)
	let numerator: i64 = 112;
	let denominator: i64 = 100;
	let low_limit = params.pow_target_timespan_v2 * denominator / numerator;
	let high_limit = params.pow_target_timespan_v2 * numerator / denominator;
	actual_timespan = actual_timespan.clamp(low_limit, high_limit);

	// Retarget with Litecoin overflow-prevention shift
	let (mut target, _, _) = compact_to_target(parent_bits);
	let pow_limit_bit_count = super::compact::bits_u256(params.pow_limit);
	let target_bit_count = super::compact::bits_u256(target);
	let needs_shift = target_bit_count > pow_limit_bit_count - 1;

	if needs_shift {
		target >>= 1_usize;
	}

	#[allow(clippy::cast_sign_loss)]
	{
		target = target * super::U256::from(actual_timespan as u64)
			/ super::U256::from(params.pow_target_timespan_v2 as u64);
	}

	if needs_shift {
		target <<= 1_usize;
	}

	if target > params.pow_limit {
		target = params.pow_limit;
	}

	Ok(target_to_compact(target))
}
