// SPDX-License-Identifier: Apache-2.0

//! Mempool transaction entry with fee metadata

use std::time::Instant;

use crate::types::transaction::Transaction;

/// A transaction in the mempool with associated fee metadata
pub struct MempoolEntry {
	/// The transaction
	pub tx: Transaction,
	/// Absolute fee in catoshis (sum of input values - sum of output values)
	pub fee: u64,
	/// Serialized transaction size in bytes
	pub size: usize,
	/// Fee rate in catoshis per byte (fee / size)
	pub fee_rate: f64,
	/// When this entry was added to the mempool
	pub added_at: Instant,
}
