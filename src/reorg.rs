// SPDX-License-Identifier: Apache-2.0

//! Chain reorganization coordinator
//!
//! Orchestrates switching to a fork with more cumulative proof-of-work.
//! Disconnects blocks from the old chain, connects blocks from the new
//! chain, and re-adds orphaned transactions to the mempool

use anyhow::{Context, Result, bail};
use tracing::{error, info, warn};

use crate::chainstate::ChainState;
use crate::headers::HeaderStore;
use crate::mempool::Mempool;
use crate::storage::block_store::BlockStore;
use crate::types::hash::Hash256;
use crate::types::transaction::Transaction;

/// Maximum number of blocks that can be disconnected in a single reorg
const MAX_REORG_DEPTH: u32 = 1_000;

/// Result of attempting to activate the best known chain
#[derive(Debug)]
pub enum ActivateResult {
	/// Active chain already has the most cumulative work
	AlreadyBest,
	/// Reorganization completed successfully
	Reorganized {
		/// Previous active chain tip
		old_tip: Hash256,
		/// New active chain tip
		new_tip: Hash256,
		/// Number of blocks disconnected from the old chain
		disconnected: u32,
		/// Number of blocks connected from the new chain
		connected: u32,
	},
	/// Fork blocks are not yet available in the block store
	NeedBlocks(Vec<Hash256>),
}

/// Attempts to switch the active chain to the fork with the most cumulative work
///
/// Compares the current chainstate tip against the best known header tip.
/// If they differ, computes the fork point and orchestrates disconnecting
/// old blocks, connecting new blocks, updating the header store, and
/// re-adding orphaned transactions to the mempool.
///
/// On failure during the connect phase, rolls back to the original chain
/// and marks the failing fork tip as invalid
pub fn activate_best_chain(
	headers: &mut HeaderStore,
	chainstate: &ChainState,
	block_store: &BlockStore,
	mempool: &mut Mempool,
) -> Result<ActivateResult> {
	let (best_hash, _, _) = headers.best_tip();
	let active_hash = chainstate.tip();
	let active_height = chainstate.tip_height();

	if best_hash == active_hash {
		return Ok(ActivateResult::AlreadyBest);
	}

	let fork_point = headers.find_fork_point(active_hash, best_hash)?;
	let fork_height = headers
		.get(&fork_point)
		.context("fork point not in header store")?
		.height;
	let disconnect_count = active_height.saturating_sub(fork_height);

	if disconnect_count > MAX_REORG_DEPTH {
		bail!("reorg depth {disconnect_count} exceeds maximum {MAX_REORG_DEPTH}");
	}

	let disconnect_path = headers.get_path(fork_point, active_hash)?;
	let connect_path = headers.get_path(fork_point, best_hash)?;

	#[allow(clippy::cast_possible_truncation)] // connect_path.len() bounded by total chain height
	let connect_count = connect_path.len() as u32;
	if connect_count > MAX_REORG_DEPTH {
		bail!("reorg connect depth {connect_count} exceeds maximum {MAX_REORG_DEPTH}");
	}

	info!(
		fork_point = %fork_point,
		disconnect = disconnect_path.len(),
		connect = connect_path.len(),
		old_tip = %active_hash,
		new_tip = %best_hash,
		"chain reorganization detected"
	);

	// Check all blocks to connect are available
	let missing: Vec<Hash256> = connect_path
		.iter()
		.filter(|(hash, _)| !block_store.has_block(hash))
		.map(|(hash, _)| *hash)
		.collect();
	if !missing.is_empty() {
		return Ok(ActivateResult::NeedBlocks(missing));
	}

	// Phase 1: DISCONNECT old chain blocks (reverse order)
	let mut disconnected_txs: Vec<Transaction> = Vec::new();
	let mut disconnected_blocks: Vec<(Hash256, u32)> = Vec::new();
	for &(hash, height) in disconnect_path.iter().rev() {
		info!(height, hash = %hash, "disconnecting block");
		let block = block_store.load_and_deserialize(&hash)?;
		chainstate.disconnect_block(&block, height, &hash)?;
		// Collect non-coinbase txs for mempool re-addition
		for tx in &block.transactions {
			if !tx.is_coinbase() {
				disconnected_txs.push(tx.clone());
			}
		}
		disconnected_blocks.push((hash, height));
	}

	// Phase 2: CONNECT new chain blocks (forward order)
	// Track txs removed from mempool during connect so we can restore on rollback
	let mut mempool_removed_txs: Vec<Transaction> = Vec::new();
	let mut connected_count: u32 = 0;
	for &(hash, height) in &connect_path {
		info!(height, hash = %hash, "connecting block");
		let block = block_store.load_and_deserialize(&hash)?;
		match chainstate.connect_block(&block, height, &hash) {
			Ok(()) => {
				// Snapshot non-coinbase txs that will be removed so we can
				// restore them if a later connect_block fails
				for tx in &block.transactions {
					if !tx.is_coinbase() && mempool.contains(&tx.txid()) {
						mempool_removed_txs.push(tx.clone());
					}
				}
				mempool.remove_block_txs(&block);
				connected_count = connected_count.saturating_add(1);
			}
			Err(e) => {
				warn!(
					height,
					hash = %hash,
					error = %e,
					"reorg aborted: connect_block failed, rolling back"
				);
				// ROLLBACK: disconnect what we connected of the new chain
				#[allow(clippy::indexing_slicing)] // connected_count <= connect_path.len(), guaranteed by the loop
				let connected_slice = &connect_path[..connected_count as usize];
				rollback(chainstate, block_store, connected_slice, &disconnected_blocks)?;
				// Restore txs removed from mempool during partial connect
				let restored = mempool.readd_disconnected_txs(mempool_removed_txs, chainstate);
				if restored > 0 {
					info!(restored, "restored mempool txs after failed reorg rollback");
				}
				headers.mark_invalid(best_hash);
				bail!("reorg failed at height {height}: {e}");
			}
		}
	}

	// Phase 3: Update header store active chain
	headers.switch_active_chain(best_hash)?;

	// Phase 4: Re-add disconnected txs to mempool
	let readded = mempool.readd_disconnected_txs(disconnected_txs, chainstate);
	info!(
		disconnected = disconnect_path.len(),
		connected = connected_count,
		mempool_readded = readded,
		"reorganization complete"
	);

	#[allow(clippy::cast_possible_truncation)] // disconnect_path.len() fits u32 (capped by MAX_REORG_DEPTH)
	Ok(ActivateResult::Reorganized {
		old_tip: active_hash,
		new_tip: best_hash,
		disconnected: disconnect_path.len() as u32,
		connected: connected_count,
	})
}

/// Rolls back a failed reorg: disconnects new chain blocks, re-connects old chain
fn rollback(
	chainstate: &ChainState,
	block_store: &BlockStore,
	connected_new: &[(Hash256, u32)],
	disconnected_old: &[(Hash256, u32)],
) -> Result<()> {
	// Disconnect the blocks we connected from the new chain (reverse order)
	for &(hash, height) in connected_new.iter().rev() {
		let block = block_store.load_and_deserialize(&hash)?;
		if let Err(e) = chainstate.disconnect_block(&block, height, &hash) {
			error!(
				height,
				hash = %hash,
				error = %e,
				"CRITICAL: rollback disconnect failed, node state is inconsistent"
			);
			std::process::exit(1);
		}
	}
	// Re-connect old chain blocks (forward order -- they were stored reversed)
	for &(hash, height) in disconnected_old.iter().rev() {
		let block = block_store.load_and_deserialize(&hash)?;
		if let Err(e) = chainstate.connect_block(&block, height, &hash) {
			error!(
				height,
				hash = %hash,
				error = %e,
				"CRITICAL: rollback reconnect failed, node state is inconsistent"
			);
			std::process::exit(1);
		}
	}
	Ok(())
}
