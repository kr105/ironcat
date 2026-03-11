// SPDX-License-Identifier: Apache-2.0

//! UTXO set management and block connection/disconnection

use std::path::Path;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use rayon::prelude::*;
use redb::{Database, ReadableTable, Table, TableDefinition};
use tracing::{debug, info};

use crate::script::verify::verify_script;
use crate::types::block::Block;
use crate::types::hash::{Hash256, HASH_LEN};
use crate::types::transaction::OutPoint;
use crate::validation::subsidy::get_block_subsidy;

use self::coin::Coin;
use self::undo::{BlockUndo, TxUndo};
use self::undo_store::{UndoStore, UNDO_INDEX};

pub mod coin;
pub mod undo;
pub mod undo_store;

/// UTXO set table: outpoint (txid 32 bytes + index 4 bytes) -> serialized Coin
const UTXO_SET: TableDefinition<&[u8; 36], &[u8]> = TableDefinition::new("utxos");

/// Metadata table for chain tip and other state
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// Number of confirmations required before a coinbase output can be spent
pub const COINBASE_MATURITY: u32 = 100;

/// Key for the chain tip hash in the META table
const TIP_KEY: &str = "tip";

/// Key for the chain tip height in the META table
const TIP_HEIGHT_KEY: &str = "tip_height";

/// Manages the UTXO set and block connection/disconnection
///
/// Owns the redb Database shared between the UTXO set tables and the
/// undo index. Provides `connect_block` and `disconnect_block` for
/// applying and reverting blocks against the UTXO set
pub struct ChainState {
	db: Database,
	undo_store: UndoStore,
	/// Current chain tip hash and height, updated atomically
	tip: RwLock<(Hash256, u32)>,
	/// Blocks at or below this height skip script verification
	script_verify_height: u32,
}

/// Formats a catoshi value as a CAT string (e.g. 50.00000000)
#[allow(clippy::arithmetic_side_effects)] // modulo by a non-zero constant
fn format_catoshis(catoshis: i64) -> String {
	const CAT: i64 = 100_000_000;
	let whole = catoshis / CAT;
	let frac = (catoshis % CAT).unsigned_abs();
	format!("{whole}.{frac:08}")
}

/// Converts an outpoint to a 36-byte key for the UTXO table
fn outpoint_to_key(outpoint: &OutPoint) -> [u8; 36] {
	let mut key = [0u8; 36];
	#[allow(clippy::indexing_slicing)] // constant range on fixed-size 36-byte array
	key[..32].copy_from_slice(outpoint.txid.as_bytes());
	#[allow(clippy::indexing_slicing)] // constant range on fixed-size 36-byte array
	key[32..36].copy_from_slice(&outpoint.index.to_le_bytes());
	key
}

/// Inserts transaction outputs into the UTXO table
fn insert_outputs(
	utxo_table: &mut Table<&[u8; 36], &[u8]>,
	txid: &Hash256,
	outputs: &[crate::types::transaction::TxOut],
	height: u32,
	is_coinbase: bool,
) -> Result<()> {
	for (vout_idx, output) in outputs.iter().enumerate() {
		let outpoint = OutPoint {
			txid: *txid,
			#[allow(clippy::cast_possible_truncation)] // vout index fits u32
			index: vout_idx as u32,
		};
		let coin = Coin {
			tx_out: output.clone(),
			height,
			is_coinbase,
		};
		let key = outpoint_to_key(&outpoint);
		let coin_bytes = coin.to_bytes();
		utxo_table
			.insert(&key, coin_bytes.as_slice())
			.context("failed to insert utxo")?;
	}
	Ok(())
}

impl ChainState {
	/// Opens or creates a chain state database at the given data directory
	///
	/// Creates the redb Database at `datadir/chainstate.redb` with all
	/// required tables (`UTXO_SET`, `META`, `UNDO_INDEX`). Loads the current
	/// tip from the META table, defaulting to the zero hash
	#[allow(clippy::missing_panics_doc)] // unwrap is after a length check, can't panic
	pub fn open(datadir: &Path, script_verify_height: u32) -> Result<Self> {
		let db_path = datadir.join("chainstate.redb");
		let db = Database::create(&db_path)
			.with_context(|| format!("failed to open chainstate db at {}", db_path.display()))?;

		// Ensure all tables exist
		let txn = db
			.begin_write()
			.context("failed to begin write txn for table creation")?;
		{
			let _t = txn.open_table(UTXO_SET).context("failed to open utxos table")?;
			let _t = txn.open_table(META).context("failed to open meta table")?;
			let _t = txn.open_table(UNDO_INDEX).context("failed to open undo_index table")?;
		}
		txn.commit().context("failed to commit table creation")?;

		// Load tip and tip_height from META in a single read transaction
		let (tip, tip_height) = {
			let txn = db.begin_read().context("failed to begin read txn for tip load")?;
			let table = txn.open_table(META).context("failed to open meta table for tip load")?;

			let tip = table
				.get(TIP_KEY)
				.context("failed to read tip from meta")?
				.map_or(Hash256::ZERO, |guard| {
					let bytes = guard.value();
					if bytes.len() == HASH_LEN {
						// Length verified by the check above; try_into always succeeds
						#[allow(clippy::unwrap_used)]
						let arr: [u8; HASH_LEN] = bytes.try_into().unwrap();
						Hash256::from_bytes(arr)
					} else {
						Hash256::ZERO
					}
				});

			let tip_height = table
				.get(TIP_HEIGHT_KEY)
				.context("failed to read tip_height from meta")?
				.map_or(0u32, |guard| {
					let bytes = guard.value();
					if bytes.len() == 4 {
						// Length verified by the check above
						#[allow(clippy::unwrap_used)]
						let arr: [u8; 4] = bytes.try_into().unwrap();
						u32::from_le_bytes(arr)
					} else {
						0
					}
				});

			(tip, tip_height)
		};

		let undo_store = UndoStore::open(datadir, &db).context("failed to open undo store")?;

		info!(tip = %tip, tip_height, "ChainState opened");

		Ok(Self {
			db,
			undo_store,
			tip: RwLock::new((tip, tip_height)),
			script_verify_height,
		})
	}

	/// Returns the current chain tip hash
	pub fn tip(&self) -> Hash256 {
		self.tip.read().0
	}

	/// Returns the current chain tip height
	pub fn tip_height(&self) -> u32 {
		self.tip.read().1
	}

	/// Fsyncs the undo flat file to ensure data is durable on disk
	pub fn fsync_undo(&self) -> Result<()> {
		self.undo_store.fsync()
	}

	/// Returns a reference to the underlying database
	///
	/// Exposed for testing purposes
	pub const fn database(&self) -> &Database {
		&self.db
	}

	/// Connects a block to the chain, updating the UTXO set
	///
	/// For each transaction: spends inputs (removing from UTXO set and
	/// recording undo data), validates coinbase maturity and value bounds,
	/// and adds outputs to the UTXO set. Updates the chain tip on success
	#[allow(clippy::too_many_lines)] // sequential validation steps, splitting would hurt readability
	pub fn connect_block(&self, block: &Block, height: u32, block_hash: &Hash256) -> Result<()> {
		let current_height = self.tip.read().1;
		if height != current_height.saturating_add(1) {
			anyhow::bail!(
				"connect_block: expected height {} but got {height}",
				current_height.saturating_add(1)
			);
		}

		anyhow::ensure!(
			!block.transactions.is_empty(),
			"block has no transactions (missing coinbase)"
		);

		let mut block_undo = BlockUndo { tx_undos: Vec::new() };
		let mut total_fees: i64 = 0;
		let mut total_inputs: usize = 0;
		let mut total_outputs: usize = 0;
		let mut total_input_value: i64 = 0;
		let mut total_output_value: i64 = 0;
		let mut scripts_verified: usize = 0;

		let txn = self
			.db
			.begin_write()
			.context("failed to begin write txn for connect_block")?;

		{
			let mut utxo_table = txn
				.open_table(UTXO_SET)
				.context("failed to open utxos table for connect_block")?;

			for tx in &block.transactions {
				let txid = tx.txid();

				if tx.is_coinbase() {
					insert_outputs(&mut utxo_table, &txid, &tx.vout, height, true)?;
					total_outputs = total_outputs.saturating_add(tx.vout.len());
				} else {
					anyhow::ensure!(!tx.vin.is_empty(), "non-coinbase transaction {txid} has no inputs");

					// Process non-coinbase transaction
					let mut tx_undo = TxUndo {
						spent_outputs: Vec::with_capacity(tx.vin.len()),
					};
					let mut input_sum: i64 = 0;

					// Spend inputs
					for input in &tx.vin {
						let key = outpoint_to_key(&input.prev_output);
						let coin_guard = utxo_table.get(&key).context("failed to look up utxo")?.ok_or_else(|| {
							anyhow::anyhow!(
								"input references missing utxo: {}:{}",
								input.prev_output.txid,
								input.prev_output.index
							)
						})?;

						let coin = Coin::from_bytes(coin_guard.value()).context("failed to deserialize utxo coin")?;
						drop(coin_guard);

						// Check coinbase maturity
						if coin.is_coinbase {
							let confirmations = height.saturating_sub(coin.height);
							if confirmations < COINBASE_MATURITY {
								anyhow::bail!(
									"coinbase output not mature: {confirmations} confirmations, need {COINBASE_MATURITY}"
								);
							}
						}

						input_sum = input_sum
							.checked_add(coin.tx_out.value)
							.ok_or_else(|| anyhow::anyhow!("input value sum overflow"))?;

						tx_undo.spent_outputs.push(coin);

						// Remove from UTXO set
						utxo_table.remove(&key).context("failed to remove spent utxo")?;
					}

					// Verify scripts for all inputs in parallel
					if height > self.script_verify_height {
						let txid_display = txid;
						tx_undo
							.spent_outputs
							.par_iter()
							.enumerate()
							.try_for_each(|(input_idx, coin)| {
								// spent_outputs has exactly tx.vin.len() entries, so input_idx is always valid
								#[allow(clippy::indexing_slicing)]
								verify_script(&tx.vin[input_idx].script_sig, &coin.tx_out.script_pubkey, tx, input_idx)
									.map_err(|e| {
										anyhow::anyhow!(
										"script verification failed for input {input_idx} of tx {txid_display}: {e}"
									)
									})
							})?;
						scripts_verified = scripts_verified.saturating_add(tx.vin.len());
					}

					// Sum outputs
					let mut output_sum: i64 = 0;
					for output in &tx.vout {
						output_sum = output_sum
							.checked_add(output.value)
							.ok_or_else(|| anyhow::anyhow!("output value sum overflow"))?;
					}

					total_inputs = total_inputs.saturating_add(tx.vin.len());
					total_outputs = total_outputs.saturating_add(tx.vout.len());
					total_input_value = total_input_value.saturating_add(input_sum);
					total_output_value = total_output_value.saturating_add(output_sum);

					if output_sum > input_sum {
						anyhow::bail!("tx {txid} outputs ({output_sum}) exceed inputs ({input_sum})");
					}

					// checked_sub is safe since we verified output_sum <= input_sum
					#[allow(clippy::arithmetic_side_effects)]
					let fee = input_sum - output_sum;
					total_fees = total_fees
						.checked_add(fee)
						.ok_or_else(|| anyhow::anyhow!("total fee overflow"))?;

					insert_outputs(&mut utxo_table, &txid, &tx.vout, height, false)?;

					block_undo.tx_undos.push(tx_undo);
				}
			}
		}

		// Verify coinbase value
		if let Some(coinbase) = block.transactions.first() {
			let mut coinbase_value: i64 = 0;
			for output in &coinbase.vout {
				coinbase_value = coinbase_value
					.checked_add(output.value)
					.ok_or_else(|| anyhow::anyhow!("coinbase value sum overflow"))?;
			}

			let subsidy = get_block_subsidy(height);
			let max_coinbase = subsidy
				.checked_add(total_fees)
				.ok_or_else(|| anyhow::anyhow!("subsidy + fees overflow"))?;

			if coinbase_value > max_coinbase {
				anyhow::bail!(
					"coinbase value {coinbase_value} exceeds allowed {max_coinbase} (subsidy {subsidy} + fees {total_fees})"
				);
			}
		}

		// Store undo data and write index entry in the same redb transaction.
		// Note: the flat file write happens before the redb commit. On crash
		// between the two, orphaned bytes in the flat file are harmless --
		// verify_or_clear_index tolerates files larger than the index expects
		if !block_undo.tx_undos.is_empty() {
			let loc = self
				.undo_store
				.store_undo_to_file(block_hash, &block_undo)
				.context("failed to write block undo data to flat file")?;

			// Write undo index inside the same redb transaction for crash safety
			let mut undo_table = txn
				.open_table(UNDO_INDEX)
				.context("failed to open undo_index for connect_block")?;
			undo_table
				.insert(&loc.hash, (loc.file_number, loc.offset, loc.size))
				.context("failed to insert undo index entry")?;
		}

		// Update tip in META
		{
			let mut meta_table = txn
				.open_table(META)
				.context("failed to open meta table for tip update")?;
			meta_table
				.insert(TIP_KEY, block_hash.as_bytes().as_slice())
				.context("failed to update chain tip")?;
			meta_table
				.insert(TIP_HEIGHT_KEY, height.to_le_bytes().as_slice())
				.context("failed to update chain tip height")?;
		}

		txn.commit().context("failed to commit connect_block")?;

		*self.tip.write() = (*block_hash, height);

		debug!(
			height,
			hash = %block_hash,
			txs = block.transactions.len(),
			inputs = total_inputs,
			outputs = total_outputs,
			input_cat = %format_catoshis(total_input_value),
			output_cat = %format_catoshis(total_output_value),
			fees_cat = %format_catoshis(total_fees),
			scripts_verified,
			"Connected block"
		);

		Ok(())
	}

	/// Disconnects a block from the chain, restoring the UTXO set
	///
	/// Reverses the effects of `connect_block`: removes outputs added
	/// by the block's transactions and restores previously spent inputs
	/// using the undo data. Updates the chain tip to the previous block
	pub fn disconnect_block(&self, block: &Block, height: u32, block_hash: &Hash256) -> Result<()> {
		if height == 0 {
			anyhow::bail!("cannot disconnect the genesis block");
		}

		let (current_tip, current_height) = *self.tip.read();
		if *block_hash != current_tip || height != current_height {
			anyhow::bail!(
				"disconnect_block: expected tip {current_tip} at height {current_height}, got {block_hash} at {height}"
			);
		}

		// height > 0 is guaranteed by the guard above
		#[allow(clippy::arithmetic_side_effects)]
		let prev_height = height - 1;

		// Load undo data if needed (must happen before the write txn)
		let non_coinbase_count = block.transactions.iter().filter(|tx| !tx.is_coinbase()).count();
		let block_undo = if non_coinbase_count > 0 {
			let undo = self
				.undo_store
				.load_undo(&self.db, block_hash)
				.context("failed to load block undo data")?;
			if undo.tx_undos.len() != non_coinbase_count {
				anyhow::bail!(
					"undo data has {} entries but block has {non_coinbase_count} non-coinbase txs",
					undo.tx_undos.len()
				);
			}
			Some(undo)
		} else {
			None
		};

		let txn = self
			.db
			.begin_write()
			.context("failed to begin write txn for disconnect_block")?;

		{
			let mut utxo_table = txn
				.open_table(UTXO_SET)
				.context("failed to open utxos table for disconnect_block")?;

			// Process transactions in reverse order
			for (tx_idx, tx) in block.transactions.iter().enumerate().rev() {
				let txid = tx.txid();

				// Remove outputs added by this tx
				for (vout_idx, _output) in tx.vout.iter().enumerate() {
					let outpoint = OutPoint {
						txid,
						#[allow(clippy::cast_possible_truncation)] // vout index fits u32
						index: vout_idx as u32,
					};
					let key = outpoint_to_key(&outpoint);
					utxo_table
						.remove(&key)
						.context("failed to remove utxo during disconnect")?;
				}

				// Restore spent inputs for non-coinbase txs
				if !tx.is_coinbase() {
					if let Some(ref undo) = block_undo {
						// Non-coinbase tx at block position i maps to undo index (i-1)
						// since coinbase is at position 0
						#[allow(clippy::arithmetic_side_effects)] // tx_idx > 0 for non-coinbase
						let undo_idx = tx_idx - 1;
						let tx_undo = undo
							.tx_undos
							.get(undo_idx)
							.ok_or_else(|| anyhow::anyhow!("missing tx undo at index {undo_idx}"))?;

						for (input_idx, input) in tx.vin.iter().enumerate() {
							let coin = tx_undo
								.spent_outputs
								.get(input_idx)
								.ok_or_else(|| anyhow::anyhow!("missing spent output at input index {input_idx}"))?;
							let key = outpoint_to_key(&input.prev_output);
							let coin_bytes = coin.to_bytes();
							utxo_table
								.insert(&key, coin_bytes.as_slice())
								.context("failed to restore utxo during disconnect")?;
						}
					}
				}
			}
		}

		// Update tip to previous block
		{
			let mut meta_table = txn
				.open_table(META)
				.context("failed to open meta table for tip update during disconnect")?;
			meta_table
				.insert(TIP_KEY, block.header.prev_hash.as_bytes().as_slice())
				.context("failed to update chain tip during disconnect")?;
			meta_table
				.insert(TIP_HEIGHT_KEY, prev_height.to_le_bytes().as_slice())
				.context("failed to update chain tip height during disconnect")?;
		}

		txn.commit().context("failed to commit disconnect_block")?;

		*self.tip.write() = (block.header.prev_hash, prev_height);

		debug!(
			height,
			hash = %block_hash,
			prev_hash = %block.header.prev_hash,
			"Disconnected block"
		);

		Ok(())
	}

	/// Looks up a single UTXO by outpoint
	///
	/// Returns `None` if the outpoint is not in the UTXO set. Used by
	/// the mempool to validate transaction inputs and calculate fees
	pub fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<Coin>> {
		let key = outpoint_to_key(outpoint);
		let txn = self.db.begin_read().context("failed to begin read txn for get_utxo")?;
		let table = txn
			.open_table(UTXO_SET)
			.context("failed to open utxos table for get_utxo")?;

		match table.get(&key)? {
			Some(guard) => {
				let coin = Coin::from_bytes(guard.value()).context("failed to deserialize utxo coin")?;
				Ok(Some(coin))
			}
			None => Ok(None),
		}
	}
}
