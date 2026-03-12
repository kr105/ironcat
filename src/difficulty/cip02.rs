// SPDX-License-Identifier: Apache-2.0

//! CIP02: Every-block retarget with 36-block window (blocks 20289 - 21345)
//!
//! Same formula as CIP01 but with a much shorter window (36 blocks / 6 hours
//! instead of 2016 blocks / 14 days). Retargets every 36 blocks with the same
//! 0.25x-4x clamping bounds. This was the first attempt at faster difficulty
//! response after CIP01's slow 2-week adjustments proved inadequate

use anyhow::Result;

use super::{
	ChainLookup, ConsensusParams,
	compact::{compact_to_target, target_to_compact},
	lookup_header,
};

/// Computes the expected `nBits` for `height` under CIP02 rules
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher
	let (_, parent_bits) = lookup_header(chain, parent_height)?;
	let interval = params.difficulty_adjustment_interval_v2();

	// Only retarget at interval boundaries
	if i64::from(height) % interval != 0 {
		return Ok(parent_bits);
	}

	// Art Forz fix: go back full period unless first retarget
	let blocks_to_go_back = if i64::from(height) == interval {
		interval - 1
	} else {
		interval
	};

	#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	let first_height = parent_height - blocks_to_go_back as u32;
	let (first_timestamp, _) = lookup_header(chain, first_height)?;
	let (parent_timestamp, _) = lookup_header(chain, parent_height)?;

	let mut actual_timespan = i64::from(parent_timestamp) - i64::from(first_timestamp);
	let min_timespan = params.pow_target_timespan_v2 / 4;
	let max_timespan = params.pow_target_timespan_v2 * 4;
	actual_timespan = actual_timespan.clamp(min_timespan, max_timespan);

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
