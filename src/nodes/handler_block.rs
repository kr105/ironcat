// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::NodeManager;
use crate::types::block::BlockHeader;

/// Handles an incoming block message from a peer
///
/// Parses only the 80-byte header to extract the block hash, then
/// forwards the full raw payload through the block channel for the
/// download manager to process. Does not parse transactions or
/// validate the merkle root
pub(super) fn handle_block(node_manager: &Arc<NodeManager>, address: &IpAddr, payload: &[u8]) -> Result<()> {
	let header = BlockHeader::from_bytes(payload).context("failed to parse block header from block message")?;
	let hash = header.block_hash();

	debug!(peer = %address, block = %hash, "received block");

	// Clone the sender (if any) while holding the lock briefly, then drop the lock
	// before calling try_send to avoid holding a sync lock across async boundaries
	let sender = node_manager.block_sender.lock().clone();

	let Some(sender) = sender else {
		warn!(peer = %address, block = %hash, "block channel not configured, dropping block");
		return Ok(());
	};

	if let Err(e) = sender.try_send((hash, payload.to_vec())) {
		warn!(peer = %address, block = %hash, error = %e, "block channel full or closed, dropping block");
	}

	Ok(())
}
