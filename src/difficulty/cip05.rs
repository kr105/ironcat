// SPDX-License-Identifier: Apache-2.0

//! CIP05: Time-gated `DigiShield` (blocks 46331 - 396999)
//!
//! A modified per-block `DigiShield` algorithm that only triggers when the
//! previous block's timestamp (seconds within the minute) falls in specific
//! windows: `[0, 14]` or `[30, 44]`. When the timestamp is outside these
//! windows, the algorithm falls back to CIP04's PID controller.
//!
//! When triggered, it computes a simple ratio adjustment using the single-block
//! solve time clamped to `[T - T/4, T + T/2]` = `[450, 900]` seconds.
//! This gives roughly 50% of blocks a `DigiShield` adjustment and the other
//! 50% a PID adjustment, providing a blend of stability and responsiveness

use anyhow::Result;

use super::{
	cip04,
	compact::{compact_to_target, target_to_compact},
	lookup_header, ChainLookup, ConsensusParams,
};

/// Computes the expected `nBits` for `height` under CIP05 rules
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher
	let (parent_timestamp, parent_bits) = lookup_header(chain, parent_height)?;

	// Check time gate: only adjust if seconds portion is in [0,14] or [30,44]
	let seconds = i64::from(parent_timestamp) % 60;
	let in_window = (0..=14).contains(&seconds) || (30..=44).contains(&seconds);

	if !in_window {
		// Fall back to CIP04 PID controller
		return cip04::get_next_work(height, chain, params);
	}

	// DigiShield path: single-block solve time
	let prev_height = parent_height - 1; // parent_height >= 1 at CIP05 activation
	let (prev_timestamp, _) = lookup_header(chain, prev_height)?;

	let mut actual_timespan = i64::from(parent_timestamp) - i64::from(prev_timestamp);

	// Clamp to [T - T/4, T + T/2] = [450, 900]
	let min_timespan = params.pow_target_spacing - params.pow_target_spacing / 4;
	let max_timespan = params.pow_target_spacing + params.pow_target_spacing / 2;
	actual_timespan = actual_timespan.clamp(min_timespan, max_timespan);

	// new_target = old_target * actual_timespan / target_spacing
	let (mut new_target, _, _) = compact_to_target(parent_bits);

	#[allow(clippy::cast_sign_loss)] // actual_timespan is clamped to [450, 900]
	{
		new_target = new_target * super::U256::from(actual_timespan as u64)
			/ super::U256::from(params.pow_target_spacing as u64);
	}

	if new_target > params.pow_limit {
		new_target = params.pow_limit;
	}

	let compact = target_to_compact(new_target);

	// Floor at minimum difficulty (same as CIP04)
	if compact > cip04::MIN_DIFFICULTY_COMPACT {
		return Ok(cip04::MIN_DIFFICULTY_COMPACT);
	}

	Ok(compact)
}
