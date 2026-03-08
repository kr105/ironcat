// SPDX-License-Identifier: Apache-2.0

//! Transaction-level consensus validation
//!
//! Standalone checks that can be performed without UTXO context

use std::collections::HashSet;

use anyhow::{bail, Result};
use tracing::warn;

use crate::types::transaction::{OutPoint, Transaction};

use super::subsidy::MAX_MONEY;

/// Maximum coinbase scriptSig length in bytes
const MAX_COINBASE_SCRIPTSIG: usize = 100;

/// Minimum coinbase scriptSig length in bytes
const MIN_COINBASE_SCRIPTSIG: usize = 2;

/// Validates a transaction against context-free consensus rules
///
/// Checks structural integrity: non-empty inputs/outputs, value ranges,
/// no duplicate inputs, and coinbase-specific constraints. Does not
/// require UTXO set access
pub fn check_transaction(tx: &Transaction) -> Result<()> {
	// Must have at least one input
	if tx.vin.is_empty() {
		warn!("transaction has no inputs");
		bail!("transaction has no inputs");
	}

	// Must have at least one output
	if tx.vout.is_empty() {
		warn!("transaction has no outputs");
		bail!("transaction has no outputs");
	}

	// Check individual output values
	for output in &tx.vout {
		if output.value < 0 {
			warn!(value = output.value, "transaction output has negative value");
			bail!("transaction output has negative value: {}", output.value);
		}
		if output.value > MAX_MONEY {
			warn!(value = output.value, "transaction output exceeds MAX_MONEY");
			bail!("transaction output value {} exceeds MAX_MONEY", output.value);
		}
	}

	// Check sum of output values doesn't exceed MAX_MONEY
	let total: Option<i64> = tx
		.vout
		.iter()
		.map(|o| o.value)
		.try_fold(0i64, i64::checked_add)
		.filter(|&sum| sum <= MAX_MONEY);

	if total.is_none() {
		warn!("total output value exceeds MAX_MONEY");
		bail!("total output value exceeds MAX_MONEY");
	}

	// Check for duplicate inputs
	let mut seen = HashSet::with_capacity(tx.vin.len());
	for input in &tx.vin {
		if !seen.insert((&input.prev_output.txid, input.prev_output.index)) {
			warn!("transaction contains duplicate inputs");
			bail!("transaction contains duplicate inputs");
		}
	}

	// Coinbase-specific checks
	if tx.is_coinbase() {
		// Safe to index: is_coinbase() guarantees exactly one input
		#[allow(clippy::indexing_slicing)]
		let scriptsig_len = tx.vin[0].script_sig.len();

		if !(MIN_COINBASE_SCRIPTSIG..=MAX_COINBASE_SCRIPTSIG).contains(&scriptsig_len) {
			warn!(
				len = scriptsig_len,
				"coinbase scriptsig length out of range [{MIN_COINBASE_SCRIPTSIG}, {MAX_COINBASE_SCRIPTSIG}]"
			);
			bail!(
				"coinbase scriptsig length {scriptsig_len} out of range [{MIN_COINBASE_SCRIPTSIG}, {MAX_COINBASE_SCRIPTSIG}]"
			);
		}
	} else {
		// Non-coinbase transactions must not reference the null outpoint
		for input in &tx.vin {
			if input.prev_output == OutPoint::COINBASE {
				warn!("non-coinbase transaction has null prevout");
				bail!("non-coinbase transaction has null prevout");
			}
		}
	}

	Ok(())
}
