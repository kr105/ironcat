// SPDX-License-Identifier: Apache-2.0

//! Difficulty adjustment algorithms for the Catcoin blockchain
//!
//! Catcoin has evolved through six difficulty adjustment algorithms (CIP01-CIP06),
//! each activated at a specific block height. The [`get_next_work_required`] function
//! dispatches to the correct algorithm based on the previous block's height.
//!
//! # Algorithm timeline (mainnet)
//!
//! | Height range   | CIP   | Algorithm                        |
//! |----------------|-------|----------------------------------|
//! | 0 - 20288      | CIP01 | Original 2016-block retarget       |
//! | 20289          | --    | Hardcoded difficulty 16 reset    |
//! | 20289 - 21345  | CIP02 | 36-block retarget every block    |
//! | 21346 - 27259  | CIP03 | 36-block with tight +/-12% bounds|
//! | 27260 - 46330  | CIP04 | PID controller (8-block lookback) |
//! | 46331 - 396999 | CIP05 | Time-gated DigiShield + CIP04    |
//! | 397000+        | CIP06 | LWMA-1 (45-block weighted avg)   |

mod cip01;
mod cip02;
mod cip03;
mod cip04;
mod cip05;
mod cip06;
pub mod compact;

use anyhow::Result;
use tracing::warn;
use uint::construct_uint;

construct_uint! {
	/// 256-bit unsigned integer for difficulty target arithmetic
	pub struct U256(4);
}

/// Provides access to historical block header data needed by difficulty algorithms
///
/// Implementations must return `(timestamp, bits)` for any height in the
/// stored chain. The difficulty module never mutates state through this trait
pub trait ChainLookup {
	/// Returns `(timestamp, compact_bits)` for the block at `height`, or `None`
	/// if the height is not in the chain
	fn header_at(&self, height: u32) -> Option<(u32, u32)>;
}

/// Consensus parameters that control difficulty adjustment behavior
///
/// Parameterized to support both mainnet and future testnet configurations.
/// Use [`ConsensusParams::mainnet`] for production values
#[derive(Debug, Clone)]
pub struct ConsensusParams {
	/// Maximum allowed proof-of-work target (lowest difficulty)
	pub pow_limit: U256,
	/// Target time between blocks in seconds (600 = 10 minutes)
	pub pow_target_spacing: i64,
	/// Target timespan for CIP01 retarget window in seconds (1,209,600 = 14 days)
	pub pow_target_timespan_v1: i64,
	/// Target timespan for CIP02/CIP03 retarget window in seconds (21,600 = 6 hours)
	pub pow_target_timespan_v2: i64,
	/// Number of blocks in the LWMA averaging window (45 on mainnet)
	pub lwma_averaging_window: i64,
	/// Block height at which CIP01 ends and CIP02 begins
	pub cip01_height: u32,
	/// Block height at which CIP02 ends and CIP03 begins
	pub cip02_height: u32,
	/// Block height at which CIP03 ends and CIP04 begins
	pub cip03_height: u32,
	/// Block height at which CIP04 ends and CIP05 begins
	pub cip04_height: u32,
	/// Block height at which CIP05 ends and CIP06 begins
	pub cip05_height: u32,
	/// Whether min-difficulty blocks are allowed (false on mainnet, true on testnet)
	pub allow_min_difficulty: bool,
	/// Known-good block hashes at specific heights, sorted by height
	///
	/// Proof-of-work validation is skipped for headers at or below the last
	/// checkpoint height. At exact checkpoint heights, the block hash is
	/// verified against the checkpoint value. Adding checkpoints dramatically
	/// speeds up IBD by avoiding expensive scrypt computation for historical blocks
	pub checkpoints: &'static [(u32, [u8; 32])],
}

impl ConsensusParams {
	/// Returns the consensus parameters for Catcoin mainnet
	pub fn mainnet() -> Self {
		Self {
			pow_limit: compact::compact_to_target(0x1e0f_fff0).0,
			pow_target_spacing: 600,
			pow_target_timespan_v1: 1_209_600,
			pow_target_timespan_v2: 21_600,
			lwma_averaging_window: 45,
			cip01_height: 20_289,
			cip02_height: 21_346,
			cip03_height: 27_260,
			cip04_height: 46_331,
			cip05_height: 397_000,
			allow_min_difficulty: false,
			checkpoints: &[],
		}
	}

	/// Returns consensus parameters for tests using fake headers
	///
	/// Sets a checkpoint at `u32::MAX` so proof-of-work validation is skipped
	/// for all heights. Use this when tests build chains with synthetic headers
	/// that don't have valid scrypt hashes
	pub fn testing() -> Self {
		Self {
			checkpoints: &[(u32::MAX, [0u8; 32])],
			..Self::mainnet()
		}
	}

	/// Returns the height of the last checkpoint, or 0 if no checkpoints exist
	pub fn last_checkpoint_height(&self) -> u32 {
		self.checkpoints.last().map_or(0, |&(h, _)| h)
	}

	/// Returns the expected block hash at a checkpoint height, if one exists
	pub fn checkpoint_hash_at(&self, height: u32) -> Option<[u8; 32]> {
		self.checkpoints
			.iter()
			.find(|&&(h, _)| h == height)
			.map(|&(_, hash)| hash)
	}

	/// Number of blocks per CIP01 retarget interval
	#[allow(clippy::arithmetic_side_effects)] // constant division of known positive values
	pub const fn difficulty_adjustment_interval_v1(&self) -> i64 {
		self.pow_target_timespan_v1 / self.pow_target_spacing
	}

	/// Number of blocks per CIP02/CIP03 retarget interval
	#[allow(clippy::arithmetic_side_effects)] // constant division of known positive values
	pub const fn difficulty_adjustment_interval_v2(&self) -> i64 {
		self.pow_target_timespan_v2 / self.pow_target_spacing
	}
}

/// Converts a compact target (nBits) to a human-readable difficulty number
///
/// Matches `GetDifficulty()` in the reference client. Difficulty 1 corresponds
/// to nBits = `0x1d00ffff` (the standard minimum difficulty target)
#[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
// Floating-point arithmetic required to match reference client's GetDifficulty
pub fn compact_to_difficulty(bits: u32) -> f64 {
	let exponent = (bits >> 24) & 0xff;
	let mantissa = bits & 0x00ff_ffff;
	if mantissa == 0 {
		return 0.0;
	}

	#[allow(clippy::cast_lossless)] // u32 -> f64 is always lossless
	let mut diff = f64::from(0x0000_ffff_u32) / f64::from(mantissa);

	// Shift to normalize against exponent 29 (0x1d)
	let mut shift = exponent;
	while shift < 29 {
		diff *= 256.0;
		#[allow(clippy::arithmetic_side_effects)] // shift converges toward 29
		{
			shift += 1;
		}
	}
	while shift > 29 {
		diff /= 256.0;
		#[allow(clippy::arithmetic_side_effects)] // shift converges toward 29
		{
			shift -= 1;
		}
	}

	diff
}

/// Formats a difficulty value with SI suffix for display (e.g. "427.6 K")
#[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
// Floating-point needed for human-readable formatting
pub fn format_difficulty(diff: f64) -> String {
	if diff >= 1_000_000_000.0 {
		format!("{:.2} G", diff / 1_000_000_000.0)
	} else if diff >= 1_000_000.0 {
		format!("{:.2} M", diff / 1_000_000.0)
	} else if diff >= 1_000.0 {
		format!("{:.1} K", diff / 1_000.0)
	} else {
		format!("{diff:.2}")
	}
}

/// Returns the name of the active difficulty algorithm for a given chain height
pub const fn active_algorithm(height: u32, params: &ConsensusParams) -> &'static str {
	if height < params.cip01_height {
		"CIP01/Original"
	} else if height < params.cip02_height {
		"CIP02/36-block"
	} else if height < params.cip03_height {
		"CIP03/Tight"
	} else if height < params.cip04_height {
		"CIP04/PID"
	} else if height < params.cip05_height {
		"CIP05/DigiShield"
	} else {
		"CIP06/LWMA"
	}
}

/// Computes the expected `nBits` (compact target) for a new block
///
/// `height` is the height of the block being validated (i.e. `pindexLast->nHeight + 1`
/// in the reference client). The function looks up previous headers via `chain` and
/// selects the correct algorithm based on which CIP is active at the parent block's
/// height (`height - 1`).
///
/// Returns the expected compact target as a `u32`. The caller should compare this
/// against the block header's `bits` field to validate difficulty
pub fn get_next_work_required(height: u32, chain: &impl ChainLookup, params: &ConsensusParams) -> Result<u32> {
	if height == 0 {
		return Ok(compact::target_to_compact(params.pow_limit));
	}

	// parent_height is the height of pindexLast in C++ terms
	#[allow(clippy::arithmetic_side_effects)] // height > 0 checked above
	let parent_height = height - 1;

	if parent_height < params.cip01_height {
		return cip01::get_next_work(height, chain, params);
	}

	// Hardcoded difficulty 16 reset at exactly CIP01 activation height
	if parent_height == params.cip01_height {
		return Ok(0x1c0f_fff0);
	}

	if parent_height < params.cip02_height {
		return cip02::get_next_work(height, chain, params);
	}

	if parent_height < params.cip03_height {
		return cip03::get_next_work(height, chain, params);
	}

	if parent_height < params.cip04_height {
		return cip04::get_next_work(height, chain, params);
	}

	if parent_height < params.cip05_height {
		return cip05::get_next_work(height, chain, params);
	}

	cip06::get_next_work(height, chain, params)
}

/// Helper to retrieve header data, returning an error if the height is missing
fn lookup_header(chain: &impl ChainLookup, height: u32) -> Result<(u32, u32)> {
	chain.header_at(height).ok_or_else(|| {
		warn!(height, "missing header for difficulty calculation");
		anyhow::anyhow!("missing header at height {height}")
	})
}
