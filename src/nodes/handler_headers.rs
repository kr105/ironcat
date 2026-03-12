// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use super::NodeManager;
use crate::network::{
	SharedTcpWriter, SharedTcpWriterExt,
	message_getheaders::MessageGetHeaders,
	message_headers::{MAX_HEADERS_PER_MSG, MessageHeaders},
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

	// Validate and commit under write lock (fast, in-memory only).
	// Persist to redb outside the lock to avoid blocking readers during disk I/O.
	// Don't disconnect on failure -- the peer may have sent orphan or fork headers
	// that we can't connect yet
	let start = Instant::now();
	let (batch, locator) = {
		let mut store = node_manager.header_store.write();
		let result = store.validate_and_commit(&headers);
		let elapsed_ms = start.elapsed().as_millis();
		let batch = match result {
			Ok(b) => b,
			Err(e) => {
				warn!(peer = %address, error = %e, "failed to add headers, ignoring batch");
				return Ok(());
			}
		};

		// Build locator while we still hold the lock (avoids a second acquire)
		let locator = if count == MAX_HEADERS_PER_MSG {
			Some(store.build_locator())
		} else {
			None
		};

		let tip_height = store.height();
		drop(store);

		info!(
			received = count,
			added = batch.len(),
			tip = tip_height,
			elapsed_ms = elapsed_ms,
			peer = %address,
			"processed headers"
		);

		(batch, locator)
	};

	if !batch.is_empty() {
		node_manager.refresh_chain_cache();
	}

	// Sender sent us headers, so they have at least what we have
	#[allow(clippy::cast_possible_wrap)] // chain height fits in i32 for the foreseeable chain
	let tip = node_manager.chain_height() as i32;
	if let Some(mut node) = node_manager.nodes.get_mut(address)
		&& tip > node.height
	{
		node.height = tip;
	}

	// Request more headers before persisting so the next batch arrives
	// while we're writing to disk
	if let Some(locator) = locator {
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send follow-up getheaders")?;
		debug!(peer = %address, "requesting more headers");
	}

	// Persist to redb outside the write lock -- headers are cheap to re-fetch on crash
	if !batch.is_empty() {
		let persist_result = node_manager.header_store.read().persist_batch(&batch);
		if let Err(e) = persist_result {
			error!(peer = %address, error = %e, "failed to persist headers to disk");
		}
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
