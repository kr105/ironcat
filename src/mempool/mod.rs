// SPDX-License-Identifier: Apache-2.0

//! Transaction mempool for unconfirmed transactions
//!
//! Stores validated unconfirmed transactions and provides relay
//! notification. No time-based expiration -- transactions remain as
//! long as they are valid and space permits. This is an explicit
//! design choice: valid transactions with unspent inputs can still
//! be mined regardless of age

pub mod entry;

use std::collections::HashMap;

use anyhow::{bail, Result};
use tokio::sync::broadcast;
use tracing::{debug, info};

pub use entry::MempoolEntry;

use crate::chainstate::ChainState;
use crate::script::verify::verify_script;
use crate::types::hash::Hash256;
use crate::types::transaction::{OutPoint, Transaction};
use crate::validation::transaction::check_transaction;

/// Maximum mempool size in bytes (50 MB)
const MAX_MEMPOOL_SIZE: usize = 50 * 1024 * 1024;

/// Maximum individual transaction size in bytes (100 KB, policy limit)
const MAX_TX_SIZE: usize = 100 * 1024;

/// Minimum relay fee rate in catoshis per byte
const MIN_RELAY_FEE_RATE: f64 = 1.0;

/// Broadcast channel capacity for new tx notifications
const NOTIFY_CAPACITY: usize = 256;

/// Statistics snapshot for TUI display
#[derive(Debug, Clone, Default)]
pub struct MempoolStats {
	/// Number of transactions in the mempool
	pub count: usize,
	/// Total size in bytes
	pub total_size: usize,
	/// Minimum fee rate (catoshis/byte), None if empty
	pub min_fee_rate: Option<f64>,
	/// Median fee rate (catoshis/byte), None if empty
	pub median_fee_rate: Option<f64>,
	/// Maximum fee rate (catoshis/byte), None if empty
	pub max_fee_rate: Option<f64>,
	/// Total fees in catoshis
	pub total_fees: u64,
}

/// Transaction mempool
///
/// Accepts, validates, and stores unconfirmed transactions. Provides
/// notifications for relay and statistics for display
pub struct Mempool {
	/// Transactions indexed by txid
	txs: HashMap<Hash256, MempoolEntry>,
	/// Spending index: outpoint -> txid of the mempool tx that spends it
	spends: HashMap<OutPoint, Hash256>,
	/// Total serialized size of all transactions in bytes
	total_size: usize,
	/// Maximum total size in bytes before eviction kicks in
	max_size: usize,
	/// Channel to notify relay task of newly accepted transactions
	tx_notify: broadcast::Sender<Hash256>,
}

impl Default for Mempool {
	fn default() -> Self {
		Self::new()
	}
}

impl Mempool {
	/// Creates a new empty mempool with the default 50 MB size limit
	pub fn new() -> Self {
		Self::with_max_size(MAX_MEMPOOL_SIZE)
	}

	/// Creates a new empty mempool with a custom size limit (for testing)
	pub fn with_max_size(max_size: usize) -> Self {
		let (tx_notify, _) = broadcast::channel(NOTIFY_CAPACITY);
		Self {
			txs: HashMap::new(),
			spends: HashMap::new(),
			total_size: 0,
			max_size,
			tx_notify,
		}
	}

	/// Returns a new broadcast receiver for tx acceptance notifications
	pub fn subscribe(&self) -> broadcast::Receiver<Hash256> {
		self.tx_notify.subscribe()
	}

	/// Returns true if the mempool contains a transaction with this txid
	pub fn contains(&self, txid: &Hash256) -> bool {
		self.txs.contains_key(txid)
	}

	/// Returns a reference to a transaction by txid
	pub fn get(&self, txid: &Hash256) -> Option<&MempoolEntry> {
		self.txs.get(txid)
	}

	/// Returns the number of transactions in the mempool
	pub fn len(&self) -> usize {
		self.txs.len()
	}

	/// Returns true if the mempool is empty
	pub fn is_empty(&self) -> bool {
		self.txs.is_empty()
	}

	/// Returns all transaction IDs in the mempool
	pub fn txids(&self) -> Vec<Hash256> {
		self.txs.keys().copied().collect()
	}

	/// Validates and accepts a transaction into the mempool
	///
	/// Validates against consensus rules, checks inputs exist in the
	/// chainstate UTXO set or as outputs of other mempool transactions,
	/// checks for double-spends, verifies scripts, verifies fee rate,
	/// and inserts on success. Notifies the relay task via broadcast channel
	pub fn accept_tx(&mut self, tx: Transaction, chainstate: &ChainState) -> Result<Hash256> {
		// Context-free validation
		check_transaction(&tx)?;

		// Reject coinbase
		if tx.is_coinbase() {
			bail!("coinbase transactions cannot enter the mempool");
		}

		let tx_bytes = tx.to_bytes();
		let size = tx_bytes.len();

		// Policy: max tx size
		if size > MAX_TX_SIZE {
			bail!("transaction size {size} exceeds policy limit {MAX_TX_SIZE}");
		}

		let txid = tx.txid();

		// Reject duplicates
		if self.txs.contains_key(&txid) {
			bail!("transaction {txid} already in mempool");
		}

		// Resolve inputs and calculate fee
		let mut input_sum: i64 = 0;

		for input in &tx.vin {
			// Check for double-spend within mempool
			if self.spends.contains_key(&input.prev_output) {
				bail!(
					"input {}:{} already spent by mempool transaction",
					input.prev_output.txid,
					input.prev_output.index
				);
			}

			// Resolve input value: first try chainstate, then mempool parents
			let value = self.resolve_input_value(&input.prev_output, chainstate)?;
			input_sum = input_sum
				.checked_add(value)
				.ok_or_else(|| anyhow::anyhow!("input value overflow"))?;
		}

		let output_sum: i64 = tx.vout.iter().map(|o| o.value).sum();

		if input_sum < output_sum {
			bail!("transaction spends more than its inputs: inputs={input_sum}, outputs={output_sum}");
		}

		#[allow(clippy::cast_sign_loss, clippy::arithmetic_side_effects)]
		// fee is guaranteed non-negative by the input >= output check above
		let fee = (input_sum - output_sum) as u64;

		#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // fee and size fit comfortably in f64
		let fee_rate = fee as f64 / size as f64;

		if fee_rate < MIN_RELAY_FEE_RATE {
			bail!("fee rate {fee_rate:.2} catoshis/B below minimum relay fee {MIN_RELAY_FEE_RATE}");
		}

		// Script verification: execute input scripts against the UTXOs they spend
		for (i, input) in tx.vin.iter().enumerate() {
			let script_pubkey = if let Some(coin) = chainstate.get_utxo(&input.prev_output)? {
				coin.tx_out.script_pubkey.clone()
			} else if let Some(parent) = self.txs.get(&input.prev_output.txid) {
				let idx = input.prev_output.index as usize;
				let output = parent
					.tx
					.vout
					.get(idx)
					.ok_or_else(|| anyhow::anyhow!("mempool parent has no output at index {idx}"))?;
				output.script_pubkey.clone()
			} else {
				bail!("input {i} script_pubkey not found (shouldn't happen after resolution)");
			};

			verify_script(&input.script_sig, &script_pubkey, &tx, i)
				.map_err(|e| anyhow::anyhow!("script verification failed for input {i}: {e:?}"))?;
		}

		// All checks passed -- insert
		let entry = MempoolEntry {
			tx,
			fee,
			size,
			fee_rate,
			added_at: std::time::Instant::now(),
		};

		// Update spends index
		for input in &entry.tx.vin {
			self.spends.insert(input.prev_output.clone(), txid);
		}

		self.total_size = self.total_size.saturating_add(size);
		self.txs.insert(txid, entry);

		// Evict if over size limit
		self.evict_if_needed();

		// Notify relay (ignore error if no receivers)
		let _ = self.tx_notify.send(txid);

		debug!(%txid, fee, fee_rate = format!("{fee_rate:.1}"), "accepted tx to mempool");

		Ok(txid)
	}

	/// Resolves the value of a transaction input
	///
	/// First checks the chainstate UTXO set, then falls back to
	/// outputs of transactions already in the mempool (tx chains)
	fn resolve_input_value(&self, outpoint: &OutPoint, chainstate: &ChainState) -> Result<i64> {
		// Try chainstate first
		if let Some(coin) = chainstate.get_utxo(outpoint)? {
			return Ok(coin.tx_out.value);
		}

		// Try mempool parent
		if let Some(parent) = self.txs.get(&outpoint.txid) {
			if let Some(output) = parent.tx.vout.get(outpoint.index as usize) {
				return Ok(output.value);
			}
			bail!(
				"mempool parent {} has no output at index {}",
				outpoint.txid,
				outpoint.index
			);
		}

		bail!(
			"input {}:{} not found in UTXO set or mempool",
			outpoint.txid,
			outpoint.index
		);
	}

	/// Evicts lowest fee-rate transactions until under the size limit
	///
	/// When evicting a transaction, all its descendants are also evicted
	/// since their inputs would become invalid
	fn evict_if_needed(&mut self) {
		while self.total_size > self.max_size {
			// Find the tx with the lowest fee rate
			let worst_txid = self
				.txs
				.iter()
				.min_by(|a, b| {
					a.1.fee_rate
						.partial_cmp(&b.1.fee_rate)
						.unwrap_or(std::cmp::Ordering::Equal)
				})
				.map(|(txid, _)| *txid);

			let Some(txid) = worst_txid else {
				break;
			};

			info!(%txid, "evicting tx from mempool (size limit)");
			self.remove_with_descendants(txid);
		}
	}

	/// Removes a transaction and all its descendants from the mempool
	pub fn remove_with_descendants(&mut self, txid: Hash256) {
		let mut to_remove = vec![txid];

		while let Some(current) = to_remove.pop() {
			if let Some(entry) = self.txs.remove(&current) {
				self.total_size = self.total_size.saturating_sub(entry.size);

				// Clean up spends index
				for input in &entry.tx.vin {
					self.spends.remove(&input.prev_output);
				}

				// Find children: any mempool tx spending an output of this tx
				for (idx, _) in entry.tx.vout.iter().enumerate() {
					#[allow(clippy::cast_possible_truncation)] // vout index fits in u32
					let child_outpoint = OutPoint {
						txid: current,
						index: idx as u32,
					};
					if let Some(child_txid) = self.spends.get(&child_outpoint) {
						to_remove.push(*child_txid);
					}
				}
			}
		}
	}

	/// Removes confirmed transactions and conflicting transactions after
	/// a block connects
	///
	/// 1. Remove txs confirmed in the block. Their mempool children are
	///    fine since their inputs now exist in the confirmed UTXO set.
	/// 2. Remove txs whose inputs conflict with block txs (double-spent
	///    by confirmation), along with all their descendants
	pub fn remove_block_txs(&mut self, block: &crate::types::block::Block) {
		let mut conflicting: Vec<Hash256> = Vec::new();

		for tx in &block.transactions {
			let txid = tx.txid();

			// Step 1: Remove confirmed tx from mempool
			if let Some(entry) = self.txs.remove(&txid) {
				self.total_size = self.total_size.saturating_sub(entry.size);
				for input in &entry.tx.vin {
					self.spends.remove(&input.prev_output);
				}
			}

			// Step 2: Find mempool txs spending the same inputs (conflicts)
			if !tx.is_coinbase() {
				for input in &tx.vin {
					if let Some(&conflicting_txid) = self.spends.get(&input.prev_output) {
						if conflicting_txid != txid {
							conflicting.push(conflicting_txid);
						}
					}
				}
			}
		}

		// Remove conflicting txs and their descendants
		for txid in conflicting {
			if self.txs.contains_key(&txid) {
				info!(%txid, "removing conflicting tx after block confirmation");
				self.remove_with_descendants(txid);
			}
		}
	}

	/// Returns a statistics snapshot for TUI display
	pub fn get_stats(&self) -> MempoolStats {
		if self.txs.is_empty() {
			return MempoolStats::default();
		}

		let mut fee_rates: Vec<f64> = self.txs.values().map(|e| e.fee_rate).collect();
		fee_rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

		let total_fees: u64 = self.txs.values().map(|e| e.fee).sum();

		let median_fee_rate = if fee_rates.len().is_multiple_of(2) {
			let mid = fee_rates.len() / 2;
			// mid >= 1 and mid < len because len >= 2 (non-empty by early return) and even
			#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
			Some(f64::midpoint(fee_rates[mid - 1], fee_rates[mid]))
		} else {
			fee_rates.get(fee_rates.len() / 2).copied()
		};

		MempoolStats {
			count: self.txs.len(),
			total_size: self.total_size,
			min_fee_rate: fee_rates.first().copied(),
			median_fee_rate,
			max_fee_rate: fee_rates.last().copied(),
			total_fees,
		}
	}

	/// Returns a snapshot of recent mempool entries for TUI display
	///
	/// Returns up to `limit` entries sorted by most recently added first
	pub fn recent_entries(&self, limit: usize) -> Vec<RecentTxEntry> {
		let mut entries: Vec<_> = self
			.txs
			.iter()
			.map(|(txid, e)| RecentTxEntry {
				txid: *txid,
				size: e.size,
				fee_rate: e.fee_rate,
				num_inputs: e.tx.vin.len(),
				num_outputs: e.tx.vout.len(),
				age: e.added_at.elapsed(),
			})
			.collect();
		entries.sort_by(|a, b| a.age.cmp(&b.age));
		entries.truncate(limit);
		entries
	}
}

/// A snapshot of a mempool entry for TUI display
pub struct RecentTxEntry {
	/// Transaction ID
	pub txid: Hash256,
	/// Serialized size in bytes
	pub size: usize,
	/// Fee rate in catoshis per byte
	pub fee_rate: f64,
	/// Number of inputs
	pub num_inputs: usize,
	/// Number of outputs
	pub num_outputs: usize,
	/// Time since added to mempool
	pub age: std::time::Duration,
}
