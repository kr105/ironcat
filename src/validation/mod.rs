// SPDX-License-Identifier: Apache-2.0

//! Block and transaction validation rules
//!
//! Pure functions that validate blocks and transactions against consensus
//! rules. No I/O, no state mutation. Designed to be called from the block
//! download pipeline before persisting blocks to disk

pub mod block;
pub mod subsidy;
pub mod transaction;

use anyhow::{Result, bail};
use tracing::warn;

use crate::difficulty::ChainLookup;
use crate::types::block::Block;

use subsidy::{MAX_MONEY, get_block_subsidy};

/// Block height at which BIP34 (height in coinbase) is enforced
const BIP34_HEIGHT: u32 = 111;

/// Maximum seconds a block timestamp may exceed the network-adjusted time
const MAX_FUTURE_BLOCK_TIME: u32 = 150;

/// Number of blocks used to compute median time past
const MEDIAN_TIME_SPAN: usize = 11;

/// Computes the median-time-past for a given chain height
///
/// Takes the timestamps of the block at `height` and up to 10 preceding
/// blocks, sorts them, and returns the middle value. For chains shorter
/// than 11 blocks, uses all available timestamps.
///
/// Returns `None` if the block at `height` is missing from the chain
pub fn compute_median_time_past(chain: &impl ChainLookup, height: u32) -> Option<u32> {
	// MEDIAN_TIME_SPAN is 11, so (MEDIAN_TIME_SPAN - 1) is 10
	#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
	// MEDIAN_TIME_SPAN is a small constant (11), truncation is impossible
	let start = height.saturating_sub((MEDIAN_TIME_SPAN - 1) as u32);
	let mut timestamps = Vec::with_capacity(MEDIAN_TIME_SPAN);

	for h in start..=height {
		let (ts, _bits) = chain.header_at(h)?;
		timestamps.push(ts);
	}

	timestamps.sort_unstable();

	// timestamps is non-empty (at least the block at `height`)
	#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
	Some(timestamps[timestamps.len() / 2])
}

/// Validates a block against chain-dependent consensus rules
///
/// `height` is the height of the block being validated.
/// `median_time_past` is the MTP of the previous block (height - 1).
/// `adjusted_time` is the current network-adjusted time as Unix seconds.
///
/// Checks timestamp bounds, version requirements (BIP34), coinbase height
/// encoding, and a sanity cap on coinbase output value. The actual fee
/// check is deferred until UTXO validation is implemented
pub fn check_block_context(block: &Block, height: u32, median_time_past: u32, adjusted_time: u32) -> Result<()> {
	let timestamp = block.header.timestamp;

	// Timestamp must be strictly greater than median-time-past
	if timestamp <= median_time_past {
		warn!(
			timestamp,
			mtp = median_time_past,
			"block timestamp not ahead of median-time-past"
		);
		bail!("time-too-old: block timestamp {timestamp} <= MTP {median_time_past}");
	}

	// Timestamp must not be too far in the future
	#[allow(clippy::arithmetic_side_effects)] // MAX_FUTURE_BLOCK_TIME is 150, no overflow risk
	let max_time = adjusted_time + MAX_FUTURE_BLOCK_TIME;
	if timestamp > max_time {
		warn!(timestamp, max_time, "block timestamp too far in the future");
		bail!("time-too-new: block timestamp {timestamp} > adjusted_time + {MAX_FUTURE_BLOCK_TIME} ({max_time})");
	}

	// BIP34: version >= 2 after activation height
	if height >= BIP34_HEIGHT && block.header.version < 2 {
		warn!(
			height,
			version = block.header.version,
			"block version too low for BIP34"
		);
		bail!(
			"block version {} too low at height {height} (BIP34 requires >= 2)",
			block.header.version
		);
	}

	// BIP34: coinbase scriptSig must start with serialized block height
	if height >= BIP34_HEIGHT {
		// transactions[0] is coinbase (validated by check_block before this call)
		#[allow(clippy::indexing_slicing)]
		let coinbase = &block.transactions[0];
		check_bip34_height(coinbase, height)?;
	}

	// Sanity cap on coinbase value (real fee check needs UTXO)
	#[allow(clippy::indexing_slicing)] // transactions is non-empty (validated by check_block)
	let coinbase = &block.transactions[0];
	let coinbase_total: i64 = coinbase
		.vout
		.iter()
		.try_fold(0i64, |acc, out| acc.checked_add(out.value))
		.unwrap_or(i64::MAX);

	let subsidy = get_block_subsidy(height);
	// subsidy + MAX_MONEY is the theoretical maximum (subsidy + all possible fees)
	let sanity_cap = subsidy.saturating_add(MAX_MONEY);
	if coinbase_total > sanity_cap {
		warn!(
			coinbase_total,
			subsidy, sanity_cap, "coinbase output exceeds sanity cap"
		);
		bail!("coinbase total {coinbase_total} exceeds sanity cap {sanity_cap}");
	}

	Ok(())
}

/// Validates BIP34 coinbase height encoding
///
/// Builds the expected `CScript` serialization of the block height and checks
/// that the coinbase scriptSig starts with those exact bytes. This matches the
/// reference client's approach (`CScript() << nHeight` + prefix comparison),
/// which implicitly enforces minimal encoding and correct sign-bit handling
fn check_bip34_height(coinbase: &crate::types::transaction::Transaction, expected_height: u32) -> Result<()> {
	#[allow(clippy::indexing_slicing)] // coinbase has exactly 1 input (validated by is_coinbase)
	let script = &coinbase.vin[0].script_sig;

	// Build the expected CScript encoding of the height, matching CScript::push_int64
	let expected = encode_script_height(expected_height);

	if script.len() < expected.len() {
		bail!("BIP34: coinbase scriptSig too short for height encoding");
	}

	#[allow(clippy::indexing_slicing)] // length checked above
	if script[..expected.len()] != expected[..] {
		warn!(expected = expected_height, "BIP34 coinbase height mismatch");
		bail!("BIP34: coinbase does not start with expected height encoding for {expected_height}");
	}

	Ok(())
}

/// Encodes a block height as a `CScript` push, matching `CScript() << nHeight`
///
/// - Heights 0: `[OP_0]` (0x00)
/// - Heights 1-16: `[OP_N]` (0x51-0x60)
/// - Heights > 16: `[push_len, ...CScriptNum minimal LE bytes]`
#[allow(clippy::arithmetic_side_effects)] // height values are bounded, shifts are safe
fn encode_script_height(height: u32) -> Vec<u8> {
	if height == 0 {
		return vec![0x00]; // OP_0
	}

	if height <= 16 {
		// OP_1 through OP_16 (0x51 = OP_1 - 1 + 1)
		#[allow(clippy::cast_possible_truncation)] // height <= 16
		return vec![0x50 + height as u8];
	}

	// CScriptNum::serialize: minimal signed little-endian encoding
	let mut data = Vec::with_capacity(5);
	let mut val = height;
	while val > 0 {
		#[allow(clippy::cast_possible_truncation)] // masking to u8
		data.push((val & 0xff) as u8);
		val >>= 8;
	}

	// If the MSB of the last byte has the sign bit set, append 0x00 so the
	// value is interpreted as positive (CScriptNum sign-bit convention)
	if let Some(&last) = data.last()
		&& last & 0x80 != 0
	{
		data.push(0x00);
	}

	// Prepend the push length byte
	#[allow(clippy::cast_possible_truncation)] // data.len() <= 5
	let len_byte = data.len() as u8;
	let mut result = Vec::with_capacity(data.len() + 1);
	result.push(len_byte);
	result.extend_from_slice(&data);
	result
}
