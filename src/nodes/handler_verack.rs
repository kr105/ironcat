// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};

use super::{ban_expires_at, BanReason, NodeManager, NodeState, SENDHEADERS_VERSION};
use crate::network::{message_getheaders::MessageGetHeaders, SharedTcpWriter, SharedTcpWriterExt};
use crate::types::hash::Hash256;
use crate::utils::unix_now;

/// Handles an incoming verack message from a peer
pub(super) async fn handle_verack(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
) -> Result<()> {
	let pending_ip = {
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager"))?;

		// Verack before version is a protocol violation
		if !node.version_received {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Node {} sent verack before version, banning", address);
			return Err(anyhow!("Node {address} sent verack before version"));
		}

		info!(
			"Connection ready with node {} version={}, blocks={}, user_agent={}",
			address, node.version, node.height, node.user_agent
		);

		node.state = NodeState::Connected {
			writer: Arc::clone(tcp_writer),
		};
		node.sent_getaddr = true;

		node.pending_external_ip.take()
		// RefMut dropped here
	};

	// Record external IP vote only after handshake completes
	if let Some(ip) = pending_ip {
		node_manager.record_external_ip_vote(ip);
	}

	tcp_writer
		.send_message("getaddr", &[])
		.await
		.context("failed to send getaddr")?;

	// Send sendheaders if peer supports it (BIP 130)
	let peer_version = node_manager.nodes.get(address).map_or(0, |n| n.version);
	if peer_version >= SENDHEADERS_VERSION {
		tcp_writer
			.send_message("sendheaders", &[])
			.await
			.context("failed to send sendheaders")?;
	}

	// Begin header sync
	let locator = node_manager.header_store.read().build_locator();
	let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
	tcp_writer
		.send_message("getheaders", &getheaders.to_bytes())
		.await
		.context("failed to send initial getheaders")?;

	debug!(peer = %address, "sent initial getheaders for header sync");

	Ok(())
}
