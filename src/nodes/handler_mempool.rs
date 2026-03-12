// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::debug;

use super::NodeManager;
use crate::network::{
	message_inv::{InvItem, InvType, MessageInv},
	SharedTcpWriter, SharedTcpWriterExt,
};

/// Maximum number of inv items per message (protocol limit)
const MAX_INV_ITEMS: usize = 50_000;

/// Handles an incoming mempool message from a peer
///
/// Responds with an inv message containing all transaction IDs currently
/// in the mempool, up to `MAX_INV_ITEMS`. Empty mempool produces no response
pub(super) async fn handle_mempool(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
	let Some(mempool_arc) = node_manager.mempool() else {
		debug!(peer = %address, "received mempool request but mempool not attached, ignoring");
		return Ok(());
	};

	let txids = {
		let mempool = mempool_arc.read().await;
		mempool.txids()
	};

	if txids.is_empty() {
		debug!(peer = %address, "mempool empty, no inv to send");
		return Ok(());
	}

	// Cap at protocol limit
	let count = txids.len().min(MAX_INV_ITEMS);
	let items: Vec<InvItem> = txids
		.into_iter()
		.take(count)
		.map(|hash| InvItem {
			inv_type: InvType::Tx,
			hash,
		})
		.collect();

	debug!(peer = %address, count = items.len(), "responding to mempool request with inv");

	let inv = MessageInv::new(items);
	tcp_writer
		.send_message("inv", &inv.to_bytes())
		.await
		.context("failed to send inv response to mempool request")
}

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
	use super::*;
	use crate::difficulty::ConsensusParams;
	use crate::mempool::Mempool;
	use crate::nodes::test_helpers::{read_all_messages, tcp_pair, test_genesis};
	use crate::types::hash::Hash256;

	use std::net::IpAddr;
	use tokio::sync::RwLock;

	#[tokio::test]
	async fn empty_mempool_sends_nothing() {
		let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
		let mempool = Arc::new(RwLock::new(Mempool::new()));
		nm.set_mempool(mempool);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_mempool(&nm, &peer, &writer).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert!(msgs.is_empty(), "empty mempool should produce no response");
	}

	#[tokio::test]
	async fn no_mempool_attached_sends_nothing() {
		let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_mempool(&nm, &peer, &writer).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert!(msgs.is_empty(), "no mempool should produce no response");
	}
}
