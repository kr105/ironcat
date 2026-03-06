// SPDX-License-Identifier: Apache-2.0

//! CIP06: LWMA-1 difficulty adjustment (blocks 397000+)
//!
//! Linearly Weighted Moving Average algorithm by Zawy, a modification of
//! WT-144 by Tom Harding. This is the current mainnet algorithm.
//!
//! The algorithm averages the difficulty targets of the last N blocks, weighted
//! linearly (recent blocks get higher weight), and adjusts proportionally to
//! the weighted sum of solve times.
//!
//! Key properties:
//! - Window: 45 blocks on mainnet (configurable via `lwma_averaging_window`)
//! - Solve times are clamped to `[1, 6*T]` to prevent negative values and
//!   oscillations from long block gaps
//! - Timestamps are enforced monotonically (each >= previous + 1)
//! - Normalization constant `k = N * (N + 1) * T / 2` ensures correct average
//! - Target is divided by `N * k` per-iteration to prevent U256 overflow
//!
//! Reference: <https://github.com/zawy12/difficulty-algorithms/issues/3>

use anyhow::Result;

use super::{
	compact::{compact_to_target, target_to_compact},
	lookup_header, ChainLookup, ConsensusParams, U256,
};

/// Computes the expected `nBits` for `height` under CIP06 (LWMA) rules
#[allow(clippy::arithmetic_side_effects)] // difficulty math on bounded consensus values
pub fn get_next_work(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	let parent_height = height - 1; // height > 0 guaranteed by dispatcher

	let t = params.pow_target_spacing;
	let n = params.lwma_averaging_window;

	// Normalization constant
	let k = n * (n + 1) * t / 2;

	let pow_limit = params.pow_limit;

	// For the first N blocks, return pow_limit
	if i64::from(parent_height) < n {
		return Ok(target_to_compact(pow_limit));
	}

	// Get the timestamp of the block N positions before the parent
	#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
	// n <= 45, parent_height >= n
	let anchor_height = parent_height - n as u32;
	let (anchor_timestamp, _) = lookup_header(chain, anchor_height)?;
	let mut previous_timestamp = i64::from(anchor_timestamp);

	let mut sum_weighted_solvetimes: i64 = 0;
	let mut avg_target = U256::zero();
	let mut j: i64 = 0;

	// Loop through the N most recent blocks
	let start_height = anchor_height + 1;
	for h in start_height..=parent_height {
		let (block_timestamp, block_bits) = lookup_header(chain, h)?;
		let block_time = i64::from(block_timestamp);

		// Enforce monotonic timestamps: this_timestamp >= previous + 1
		let this_timestamp = if block_time > previous_timestamp {
			block_time
		} else {
			previous_timestamp + 1
		};

		// Cap solve time at 6*T to prevent oscillations from long gaps
		let solvetime = (this_timestamp - previous_timestamp).min(6 * t);

		previous_timestamp = this_timestamp;

		// Linear weight: oldest block in window gets weight 1, newest gets N
		j += 1;
		sum_weighted_solvetimes += solvetime * j;

		// Accumulate average target, dividing early to prevent overflow
		let (target, _, _) = compact_to_target(block_bits);
		#[allow(clippy::cast_sign_loss)] // n and k are positive
		{
			avg_target += target / U256::from(n as u64) / U256::from(k as u64);
		}
	}

	// Final target = weighted_average_target * weighted_sum_of_solvetimes
	#[allow(clippy::cast_sign_loss)] // sum is non-negative due to solvetime clamping
	let next_target = avg_target * U256::from(sum_weighted_solvetimes as u64);

	let next_target = if next_target > pow_limit {
		pow_limit
	} else {
		next_target
	};

	Ok(target_to_compact(next_target))
}
