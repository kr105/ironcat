// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tracing::{debug, info, warn};

use super::{BanReason, NodeManager, NodeState, SENDHEADERS_VERSION, ban_expires_at};
use crate::network::{SharedTcpWriter, SharedTcpWriterExt, message_getheaders::MessageGetHeaders};
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
			warn!(peer = %address, "peer sent verack before version, banning");
			return Err(anyhow!("Node {address} sent verack before version"));
		}

		info!(
			peer = %address,
			version = node.version,
			height = node.height,
			user_agent = %node.user_agent,
			"connection ready"
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

	// Begin header sync (skip if peer is excluded from requests)
	let excluded = node_manager.nodes.get(address).is_some_and(|n| n.strike_excluded);

	if !excluded {
		let locator = node_manager.header_store.read().build_locator();
		let getheaders = MessageGetHeaders::new(locator, Hash256::ZERO);
		tcp_writer
			.send_message("getheaders", &getheaders.to_bytes())
			.await
			.context("failed to send initial getheaders")?;

		if let Some(mut node) = node_manager.nodes.get_mut(address) {
			node.pending_getheaders = Some(tokio::time::Instant::now());
		}

		debug!(peer = %address, "sent initial getheaders for header sync");
	}

	Ok(())
}
