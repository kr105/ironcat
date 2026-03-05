// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::NodeManager;
use crate::network::{
	message_getheaders::MessageGetHeaders,
	message_headers::{MessageHeaders, MAX_HEADERS_PER_MSG},
	SharedTcpWriter, SharedTcpWriterExt,
};
use crate::types::hash::Hash256;

/// Handles an incoming sendheaders message (BIP 130)
///
/// Sets the peer's preference flag so we send future block
/// announcements via headers instead of inv
pub(super) fn handle_sendheaders(node_manager: &NodeManager, address: &IpAddr) {
	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		node.prefer_headers = true;
		debug!("Peer {} prefers headers announcements", address);
	}
}

/// Handles an incoming headers message from a peer
///
/// Validates chain continuity via batch insertion, then requests
/// more headers if we received a full batch (2000)
pub(super) async fn handle_headers(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageHeaders::from_bytes(payload).context("failed to parse headers message")?;
	let count = msg.headers().len();

	if count == 0 {
		return Ok(());
	}

	let headers = msg.into_headers();

	// Batch insert under a single write lock. Don't disconnect on failure --
	// the peer may have sent orphan or fork headers that we can't connect yet.
	// Log the error and move on instead of killing the connection
	let insert_result = node_manager.header_store.write().add_headers(&headers);
	let added = match insert_result {
		Ok(n) => n,
		Err(e) => {
			warn!(peer = %address, error = %e, "failed to add headers, ignoring batch");
			return Ok(());
		}
	};

	info!(received = count, added, peer = %address, "processed headers");

	// If we got a full batch, request more
	if count == MAX_HEADERS_PER_MSG {
		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send follow-up getheaders")?;
		debug!(peer = %address, "requesting more headers");
	}

	Ok(())
}

/// Handles an incoming getheaders request from a peer
///
/// Finds the fork point using the locator hashes, then sends
/// up to 2000 headers from our chain via the height index
pub(super) async fn handle_getheaders(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageGetHeaders::from_bytes(payload).context("failed to parse getheaders")?;

	// Scope the read lock so it drops before any await point
	let response_headers = {
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

		store.get_headers_after(start_height, MAX_HEADERS_PER_MSG, msg.hash_stop())
	};

	if response_headers.is_empty() {
		debug!(peer = %address, "no headers to send for getheaders");
		return Ok(());
	}

	debug!(count = response_headers.len(), peer = %address, "sending headers response");

	let response = MessageHeaders::new(response_headers);
	tcp_writer
		.send_message("headers", &response.to_bytes())
		.await
		.context("failed to send headers response")
}
