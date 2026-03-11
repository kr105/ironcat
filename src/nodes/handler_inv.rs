// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::{NodeManager, MAX_INV_KNOWN};
use crate::network::{
	message_getheaders::MessageGetHeaders,
	message_inv::{InvItem, InvType, MessageInv},
	SharedTcpWriter, SharedTcpWriterExt,
};
use crate::types::hash::Hash256;

/// Handles an incoming inv message from a peer
///
/// Processes block and transaction announcements. Unknown blocks trigger a
/// getheaders request. Unknown transactions trigger a getdata request
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

	// Check announced blocks against our header store
	let has_new_blocks = {
		let store = node_manager.header_store.read();
		inv.items()
			.iter()
			.any(|item| item.inv_type == InvType::Block && store.get(&item.hash).is_none())
	};

	if has_new_blocks {
		// Peer announcing unknown blocks means they're at least at our tip + 1
		#[allow(clippy::cast_possible_wrap)] // chain height fits in i32 for the foreseeable chain
		let estimated = node_manager.chain_height().saturating_add(1) as i32;
		if let Some(mut node) = node_manager.nodes.get_mut(address) {
			if estimated > node.height {
				node.height = estimated;
			}
		}

		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send getheaders after block inv")?;
		debug!(peer = %address, "requesting headers after block inv");
	}

	// Check announced transactions against our mempool
	let unknown_txids: Vec<InvItem> = if let Some(mempool_arc) = node_manager.mempool() {
		let mempool = mempool_arc.read().await;
		inv.items()
			.iter()
			.filter(|item| item.inv_type == InvType::Tx && !mempool.contains(&item.hash))
			.cloned()
			.collect()
	} else {
		Vec::new()
	};

	if !unknown_txids.is_empty() {
		let getdata = MessageInv::new(unknown_txids);
		tcp_writer
			.send_message("getdata", &getdata.to_bytes())
			.await
			.context("failed to send getdata for unknown txs")?;
		debug!(peer = %address, "requesting unknown txs via getdata");
	}

	Ok(())
}

/// Handles an incoming getdata message from a peer
///
/// Serves transactions from the mempool when available. Items not found
/// in the mempool are collected into a notfound response
pub(super) async fn handle_getdata(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let inv = MessageInv::from_bytes(payload).context("failed to parse getdata message")?;

	debug!(peer = %address, count = inv.items().len(), "received getdata");

	let mut notfound_items: Vec<InvItem> = Vec::new();

	for item in inv.items() {
		match item.inv_type {
			InvType::Tx => {
				if let Some(mempool_arc) = node_manager.mempool() {
					let mempool = mempool_arc.read().await;
					if let Some(entry) = mempool.get(&item.hash) {
						let tx_bytes = entry.tx.to_bytes();
						drop(mempool);
						tcp_writer
							.send_message("tx", &tx_bytes)
							.await
							.context("failed to send tx")?;
					} else {
						drop(mempool);
						notfound_items.push(item.clone());
					}
				} else {
					notfound_items.push(item.clone());
				}
			}
			_ => {
				notfound_items.push(item.clone());
			}
		}
	}

	if !notfound_items.is_empty() {
		let notfound = MessageInv::new(notfound_items);
		tcp_writer
			.send_message("notfound", &notfound.to_bytes())
			.await
			.context("failed to send notfound")?;
	}

	Ok(())
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
