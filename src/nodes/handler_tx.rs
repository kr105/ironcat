// SPDX-License-Identifier: Apache-2.0

//! Handler for incoming tx messages from peers

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::RwLock;
use tracing::debug;

use super::NodeManager;
use crate::chainstate::ChainState;
use crate::mempool::Mempool;
use crate::network::SharedTcpWriter;
use crate::types::transaction::Transaction;

/// Handles an incoming tx message from a peer
///
/// Deserializes the transaction, marks it as known in the peer's inv set,
/// and attempts to accept it into the mempool. Invalid transactions are
/// logged but do not cause a disconnect
pub(super) async fn handle_tx(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	_tcp_writer: &SharedTcpWriter,
	payload: &[u8],
	mempool: &Arc<RwLock<Mempool>>,
	chainstate: &Arc<ChainState>,
) -> Result<()> {
	let tx = Transaction::from_bytes(payload).context("failed to deserialize tx")?;
	let txid = tx.txid();

	// Mark as known so we don't re-request this tx from this peer
	node_manager.mark_peer_inv_known(address, txid);

	{
		let mut pool = mempool.write().await;
		match pool.accept_tx(tx, chainstate) {
			Ok(_) => {
				debug!(peer = %address, %txid, "accepted tx from peer into mempool");
			}
			Err(e) => {
				debug!(peer = %address, %txid, error = %e, "rejected tx from peer");
			}
		}
	}

	Ok(())
}
