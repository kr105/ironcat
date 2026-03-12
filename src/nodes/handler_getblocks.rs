// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::debug;

use super::NodeManager;
use crate::network::{
	message_getheaders::MessageGetHeaders,
	message_inv::{InvItem, InvType, MessageInv},
	SharedTcpWriter, SharedTcpWriterExt,
};
use crate::types::hash::Hash256;

/// Maximum number of block inv items per getblocks response.
/// Matches the reference client limit
const MAX_BLOCKS_PER_GETBLOCKS: usize = 500;

/// Handles an incoming getblocks message from a peer
///
/// Finds the fork point using the block locator, then responds with
/// an inv message containing up to `MAX_BLOCKS_PER_GETBLOCKS` block
/// hashes starting after that point. Uses the same wire format as
/// getheaders for the request
pub(super) async fn handle_getblocks(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageGetHeaders::from_bytes(payload).context("failed to parse getblocks")?;

	let items = {
		let store = node_manager.header_store.read();

		// Find fork point: first locator hash that exists in our chain
		let mut start_height: u32 = 0;
		for hash in msg.locator_hashes() {
			if *hash == store.genesis() {
				start_height = 0;
				break;
			}
			if let Some(stored) = store.get(hash) {
				start_height = stored.height;
				break;
			}
		}

		let tip_height = store.height();
		if start_height >= tip_height {
			debug!(peer = %address, "getblocks: already at tip, no blocks to send");
			return Ok(());
		}

		// Collect block hashes from start_height+1 up to tip or hash_stop
		let hash_stop = msg.hash_stop();
		let mut items: Vec<InvItem> = Vec::new();

		// start_height < tip_height, so +1 is safe
		#[allow(clippy::arithmetic_side_effects)]
		let mut h = start_height + 1;

		while h <= tip_height && items.len() < MAX_BLOCKS_PER_GETBLOCKS {
			if let Some(hash) = store.hash_at_height(h) {
				items.push(InvItem {
					inv_type: InvType::Block,
					hash,
				});

				if hash_stop != Hash256::ZERO && hash == hash_stop {
					break;
				}
			}

			// h <= tip_height and loop cap is MAX_BLOCKS_PER_GETBLOCKS (500), can't overflow u32
			#[allow(clippy::arithmetic_side_effects)]
			{
				h += 1;
			}
		}

		drop(store);
		items
	};

	if items.is_empty() {
		debug!(peer = %address, "getblocks: no blocks to send");
		return Ok(());
	}

	debug!(peer = %address, count = items.len(), "responding to getblocks with inv");

	let inv = MessageInv::new(items);
	tcp_writer
		.send_message("inv", &inv.to_bytes())
		.await
		.context("failed to send inv response to getblocks")
}

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
	use super::*;
	use crate::difficulty::ConsensusParams;
	use crate::network::message_getheaders::MessageGetHeaders;
	use crate::nodes::test_helpers::{read_all_messages, tcp_pair, test_genesis};

	use std::net::IpAddr;

	#[tokio::test]
	async fn getblocks_at_tip_sends_nothing() {
		let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));

		// Locator with genesis hash -- we're already at genesis (tip)
		let genesis = nm.header_store.read().genesis();
		let msg = MessageGetHeaders::new(vec![genesis], Hash256::ZERO);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getblocks(&nm, &peer, &writer, &msg.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert!(msgs.is_empty(), "at tip should produce no response");
	}

	#[tokio::test]
	async fn getblocks_empty_locator_at_genesis_sends_nothing() {
		let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));

		// Empty locator with only genesis in store: start_height=0, tip=0,
		// so start_height >= tip_height returns early with no response
		let msg = MessageGetHeaders::new(vec![], Hash256::ZERO);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getblocks(&nm, &peer, &writer, &msg.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert!(msgs.is_empty());
	}
}
