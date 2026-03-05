// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::{NodeManager, MAX_INV_KNOWN};
use crate::network::{
	message_getheaders::MessageGetHeaders,
	message_inv::{InvType, MessageInv},
	SharedTcpWriter, SharedTcpWriterExt,
};
use crate::types::hash::Hash256;

/// Handles an incoming inv message from a peer
pub(super) async fn handle_inv(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let inv = MessageInv::from_bytes(payload).context("failed to parse inv message")?;

	debug!(count = inv.items().len(), peer = %address, "received inv");

	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		if node.inv_known.len() >= MAX_INV_KNOWN {
			node.inv_known.clear();
		}

		for item in inv.items() {
			node.inv_known.insert(item.hash);
		}
	}

	// Request headers if any announced blocks are unknown to us
	let has_new_blocks = {
		let store = node_manager.header_store.read();
		inv.items()
			.iter()
			.any(|item| item.inv_type == InvType::Block && store.get(&item.hash).is_none())
	};

	if has_new_blocks {
		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send getheaders after block inv")?;
		debug!(peer = %address, "requesting headers after block inv");
	}

	Ok(())
}

/// Handles an incoming getdata message from a peer
///
/// Responds with notfound for all items since we have no data to serve yet.
/// Echoes the raw payload back as notfound -- the wire format is identical,
/// and this preserves items with unknown inv types that `from_bytes` would drop
pub(super) async fn handle_getdata(
	_node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	debug!(peer = %address, payload_len = payload.len(), "received getdata, responding with notfound");

	// Echo the payload verbatim: inv/getdata/notfound share the same wire format,
	// and we have nothing to serve, so every requested item is "not found"
	tcp_writer
		.send_message("notfound", payload)
		.await
		.context("failed to send notfound")
}

/// Handles an incoming notfound message from a peer
pub(super) fn handle_notfound(address: &IpAddr, payload: &[u8]) {
	match MessageInv::from_bytes(payload) {
		Ok(inv) => {
			debug!(count = inv.items().len(), peer = %address, "received notfound");
		}
		Err(e) => {
			warn!(peer = %address, error = %e, "failed to parse notfound message");
		}
	}
}
