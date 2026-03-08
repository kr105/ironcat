// SPDX-License-Identifier: Apache-2.0

//! Block-level consensus validation
//!
//! Standalone checks that can be performed without chain context (no UTXO
//! set, no previous headers). Covers structural rules like tx count,
//! coinbase position, and block weight

use anyhow::{bail, Context, Result};
use tracing::warn;

use crate::types::block::{Block, HEADER_SIZE};

use super::transaction::check_transaction;

/// Maximum block weight (4 million weight units)
const MAX_BLOCK_WEIGHT: u64 = 4_000_000;

/// Witness scale factor applied to non-witness serialized bytes
const WITNESS_SCALE_FACTOR: u64 = 4;

/// Returns the byte size of a Bitcoin-style varint encoding for `val`
const fn varint_size(val: u64) -> u64 {
	if val < 0xFD {
		1
	} else if val <= 0xFFFF {
		3
	} else if val <= 0xFFFF_FFFF {
		5
	} else {
		9
	}
}

/// Computes the serialized size of a block in bytes
///
/// This is the raw on-wire size: 80 (header) + `varint(tx_count)` + sum of
/// each transaction's serialized length
fn block_serialized_size(block: &Block) -> u64 {
	// HEADER_SIZE is 80 and tx count is bounded; this won't overflow u64
	#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
	let tx_count = block.transactions.len() as u64;

	// All values are bounded: header is 80, varint is at most 9, and
	// individual tx sizes are bounded by memory. Sum cannot overflow u64
	#[allow(clippy::arithmetic_side_effects)]
	let size = HEADER_SIZE as u64
		+ varint_size(tx_count)
		+ block
			.transactions
			.iter()
			.map(|tx| tx.to_bytes().len() as u64)
			.sum::<u64>();

	size
}

/// Validates a block against context-free consensus rules
///
/// Checks that the block has transactions, exactly one coinbase in the
/// first position, acceptable weight, and that each transaction passes
/// individual validation. Does not check proof-of-work, merkle root, or
/// any state that requires chain context
pub fn check_block(block: &Block) -> Result<()> {
	// Must have at least one transaction
	if block.transactions.is_empty() {
		warn!("block has no transactions");
		bail!("block has no transactions");
	}

	// First transaction must be coinbase
	// Safe to index: we just verified non-empty
	#[allow(clippy::indexing_slicing)]
	if !block.transactions[0].is_coinbase() {
		warn!("first transaction is not coinbase");
		bail!("first transaction is not coinbase");
	}

	// No other transaction may be coinbase
	for (i, tx) in block.transactions.iter().enumerate().skip(1) {
		if tx.is_coinbase() {
			warn!(index = i, "multiple coinbase transactions");
			bail!("multiple coinbase transactions (index {i})");
		}
	}

	// Check block weight
	let size = block_serialized_size(block);
	// Both operands are bounded: size by available memory, factor is 4
	#[allow(clippy::arithmetic_side_effects)]
	let weight = size * WITNESS_SCALE_FACTOR;
	if weight > MAX_BLOCK_WEIGHT {
		warn!(weight, max = MAX_BLOCK_WEIGHT, "block exceeds maximum weight");
		bail!("block weight {weight} exceeds maximum {MAX_BLOCK_WEIGHT}");
	}

	// Validate each transaction individually
	for (i, tx) in block.transactions.iter().enumerate() {
		check_transaction(tx).with_context(|| format!("transaction {i} failed validation"))?;
	}

	Ok(())
}
