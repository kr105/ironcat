// SPDX-License-Identifier: Apache-2.0

//! CIP04: PID controller difficulty adjustment (blocks 27260 - 46330)
//!
//! Uses a Proportional-Integral-Derivative (PID) feedback controller with
//! an 8-block lookback window. The error signal is the difference between
//! the average block time and the 600-second target. Two sets of PID gains
//! are used depending on whether the error is within +/-450 seconds.
//!
//! A dead zone of +/-10 seconds prevents unnecessary adjustments when the
//! chain is running close to target. The result is applied as a delta to
//! the current target rather than a ratio multiply/divide.
//!
//! This algorithm uses floating-point arithmetic to match the reference
//! client's behavior exactly

use anyhow::Result;

use super::{
	compact::{compact_to_target, target_to_compact},
	lookup_header, ChainLookup, ConsensusParams,
};

/// Minimum difficulty floor in compact form used by CIP04 and CIP05
pub const MIN_DIFFICULTY_COMPACT: u32 = 0x1e0f_ffff;

/// Computes the expected `nBits` for `height` under CIP04 rules
#[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
// PID algorithm requires floats; values are small enough for f64 precision
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher
	let (parent_timestamp, parent_bits) = lookup_header(chain, parent_height)?;

	// Look back 8 blocks: go to pprev (parent-1), then 7 more
	let first_height = parent_height - 8; // parent_height >= 8 at CIP04 activation
	let (first_timestamp, _) = lookup_header(chain, first_height)?;

	// Average block time over the 8-block window
	let actual_timespan = i64::from(parent_timestamp) - i64::from(first_timestamp);
	let avg_timespan = actual_timespan / 8;

	// Count bits in current target (matches C++ loop that right-shifts until zero)
	let (target, _, _) = compact_to_target(parent_bits);
	let bit_len = target_bit_length(target);

	// Compute error: how far off from target spacing
	let error = avg_timespan - params.pow_target_spacing;

	// Dead zone: if error is within +/-10 seconds, no adjustment
	if error > -10 && error < 10 {
		return Ok(parent_bits);
	}

	// PID gains depend on error magnitude
	let (p_gain, i_gain, d_gain) = if (-450..=450).contains(&error) {
		(-0.005_125, -0.0225, -0.0075)
	} else {
		(-0.005_125, -0.0525, -0.0075)
	};

	let error_f = error as f64;
	let target_spacing_f = params.pow_target_spacing as f64;
	let avg_timespan_f = avg_timespan as f64;

	let p_calc = p_gain * error_f;
	let i_calc = i_gain * error_f * (target_spacing_f / avg_timespan_f);
	let d_calc = d_gain * (error_f / avg_timespan_f) * i_calc;
	let d_result = p_calc + i_calc + d_calc;

	// Scale and clamp to 23-bit mantissa
	#[allow(clippy::cast_possible_truncation)] // intentional f64->i64 truncation
	let mut result = (d_result * 65536.0) as i64;
	while result > 8_388_607 {
		result /= 2;
	}

	// Shift the result to align with the current target's magnitude
	let mut adjustment = super::U256::from(result.unsigned_abs());
	if bit_len > 24 {
		adjustment <<= bit_len - 24;
	}

	// Apply delta to current target
	// C++ uses signed CBigNum: bnNew = bnNew - bResult
	// When result >= 0 (blocks too fast): subtract -> lower target (raise difficulty)
	// When result < 0 (blocks too slow): subtract negative -> raise target (lower difficulty)
	let (current_target, _, _) = compact_to_target(parent_bits);
	let new_target = if result >= 0 {
		// Blocks too fast: decrease target (increase difficulty)
		if adjustment < current_target {
			current_target - adjustment
		} else {
			super::U256::zero()
		}
	} else {
		// Blocks too slow: increase target (decrease difficulty)
		let raised = current_target + adjustment;
		if raised > params.pow_limit {
			params.pow_limit
		} else {
			raised
		}
	};

	let compact = target_to_compact(new_target);

	// Floor at minimum difficulty
	if compact > MIN_DIFFICULTY_COMPACT {
		return Ok(MIN_DIFFICULTY_COMPACT);
	}

	Ok(compact)
}

/// Counts the bit length of a U256 by right-shifting until zero
///
/// Matches the C++ loop in `GetNextWorkRequired_CIP04` that counts iterations
/// of `bnNew >> 1` until zero. Equivalent to `ceil(log2(val + 1))`
fn target_bit_length(val: super::U256) -> usize {
	if val.is_zero() {
		return 0;
	}
	let mut count = 0_usize;
	let mut v = val;
	while v > super::U256::zero() {
		#[allow(clippy::arithmetic_side_effects)] // count bounded by 256
		{
			count += 1;
		}
		#[allow(clippy::arithmetic_side_effects)] // shifting toward zero
		{
			v >>= 1_usize;
		}
		if count > 256 {
			return 256;
		}
	}
	count
}
