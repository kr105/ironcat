// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use super::NodeManager;
use crate::headers::AcceptResult;
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
		debug!(peer = %address, "peer prefers headers announcements");
	}
}

/// Handles an incoming headers message from a peer
///
/// Validates chain continuity via batch insertion, then requests
/// more headers if we received a full batch (2000). Post-IBD, falls
/// back to individual `accept_header` for fork headers and triggers
/// reorg when a fork has more cumulative work
#[allow(clippy::too_many_lines)] // sequential handling phases, splitting would obscure the flow
pub(super) async fn handle_headers(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let msg = MessageHeaders::from_bytes(payload).context("failed to parse headers message")?;
	let count = msg.headers().len();

	// Clear pending request tracker -- response received
	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		node.pending_getheaders = None;
	}

	if count == 0 {
		return Ok(());
	}

	let headers = msg.into_headers();

	// Validate and commit under write lock (fast, in-memory only)
	// Persist to redb outside the lock to avoid blocking readers during disk I/O
	// Don't disconnect on failure -- the peer may have sent orphan or fork headers
	// that we can't connect yet
	let start = tokio::time::Instant::now();
	let (batch, locator, need_reorg) = {
		let mut store = node_manager.header_store.write();
		let result = store.validate_and_commit(&headers);
		let elapsed_ms = start.elapsed().as_millis();

		let (batch, need_reorg) = match result {
			Ok(b) => (b, false),
			Err(e) => {
				// Batch validation failed -- try accept_header individually (fork headers)
				if node_manager.is_in_ibd() {
					warn!(peer = %address, error = %e, "failed to add headers during IBD, ignoring batch");
					return Ok(());
				}

				let mut need_reorg = false;
				let mut accepted = 0u32;
				for header in &headers {
					match store.accept_header(header.clone()) {
						Ok(AcceptResult::NeedReorg) => {
							need_reorg = true;
							accepted = accepted.saturating_add(1);
						}
						Ok(AcceptResult::Accepted) => {
							accepted = accepted.saturating_add(1);
						}
						Ok(AcceptResult::AlreadyKnown) => {}
						Err(accept_err) => {
							debug!(
								peer = %address,
								error = %accept_err,
								"accept_header rejected individual header"
							);
						}
					}
				}

				if accepted > 0 {
					info!(
						peer = %address,
						accepted,
						elapsed_ms,
						"accepted fork headers individually"
					);
				} else {
					warn!(peer = %address, error = %e, "failed to add headers, ignoring batch");
				}

				(Vec::new(), need_reorg)
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

		if !batch.is_empty() {
			info!(
				received = count,
				added = batch.len(),
				tip = tip_height,
				elapsed_ms = start.elapsed().as_millis(),
				peer = %address,
				"processed headers"
			);
		}

		(batch, locator, need_reorg)
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
		let excluded = node_manager.nodes.get(address).is_some_and(|n| n.strike_excluded);

		if !excluded {
			let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
			tcp_writer
				.send_message("getheaders", &getheaders.to_bytes())
				.await
				.context("failed to send follow-up getheaders")?;

			if let Some(mut node) = node_manager.nodes.get_mut(address) {
				node.pending_getheaders = Some(tokio::time::Instant::now());
			}

			debug!(peer = %address, "requesting more headers");
		}
	}

	// Persist to redb outside the write lock -- headers are cheap to re-fetch on crash
	if !batch.is_empty() {
		let persist_result = node_manager.header_store.read().persist_batch(&batch);
		if let Err(e) = persist_result {
			error!(peer = %address, error = %e, "failed to persist headers to disk");
		}
	}

	// Trigger reorg if a fork with more work was detected
	if need_reorg {
		let nm = Arc::clone(node_manager);
		if let Err(e) = tokio::task::spawn_blocking(move || nm.try_activate_best_chain()).await {
			error!(error = %e, "reorg task panicked");
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
