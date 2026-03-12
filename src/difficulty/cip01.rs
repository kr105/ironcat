// SPDX-License-Identifier: Apache-2.0

//! CIP01: Original difficulty retarget (blocks 0 - 20288)
//!
//! Uses the original Satoshi retarget algorithm. Difficulty is recalculated every
//! 2016 blocks (14 days at 10-minute spacing). Between retarget boundaries,
//! the previous block's `nBits` is returned unchanged.
//!
//! The actual timespan is clamped to `[target_timespan / 4, target_timespan * 4]`
//! to prevent extreme adjustments. A shift trick from Litecoin prevents
//! intermediate overflow when multiplying the target by the timespan

use anyhow::Result;

use super::{
	ChainLookup, ConsensusParams,
	compact::{compact_to_target, target_to_compact},
	lookup_header,
};

/// Computes the expected `nBits` for `height` under CIP01 rules
///
/// `height` is the height of the block being validated. The algorithm checks
/// whether this height falls on a retarget boundary (every 2016 blocks).
/// If not, the parent block's `nBits` is returned unchanged
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher
	let (_, parent_bits) = lookup_header(chain, parent_height)?;
	let interval = params.difficulty_adjustment_interval_v1();

	// Only retarget at interval boundaries
	if i64::from(height) % interval != 0 {
		return Ok(parent_bits);
	}

	// Go back the full period unless it's the first retarget after genesis
	// (Art Forz fix from Litecoin)
	let blocks_to_go_back = if i64::from(height) == interval {
		interval - 1
	} else {
		interval
	};

	#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	// blocks_to_go_back <= 2016, parent_height >= blocks_to_go_back
	let first_height = parent_height - blocks_to_go_back as u32;
	let (first_timestamp, _) = lookup_header(chain, first_height)?;
	let (parent_timestamp, _) = lookup_header(chain, parent_height)?;

	// Compute actual timespan and clamp to [timespan/4, timespan*4]
	let mut actual_timespan = i64::from(parent_timestamp) - i64::from(first_timestamp);
	let min_timespan = params.pow_target_timespan_v1 / 4;
	let max_timespan = params.pow_target_timespan_v1 * 4;
	actual_timespan = actual_timespan.clamp(min_timespan, max_timespan);

	// Retarget: new_target = old_target * actual_timespan / target_timespan
	// Shift trick prevents intermediate overflow (from Litecoin)
	let (mut target, _, _) = compact_to_target(parent_bits);
	let pow_limit_bit_count = super::compact::bits_u256(params.pow_limit);
	let target_bit_count = super::compact::bits_u256(target);
	let needs_shift = target_bit_count > pow_limit_bit_count - 1;

	if needs_shift {
		target >>= 1_usize;
	}

	#[allow(clippy::cast_sign_loss)] // actual_timespan is clamped positive
	{
		target = target * super::U256::from(actual_timespan as u64)
			/ super::U256::from(params.pow_target_timespan_v1 as u64);
	}

	if needs_shift {
		target <<= 1_usize;
	}

	if target > params.pow_limit {
		target = params.pow_limit;
	}

	Ok(target_to_compact(target))
}
