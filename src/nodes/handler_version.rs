// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tracing::warn;

use super::{BanReason, ConnectionType, MAX_USER_AGENT_DISPLAY, NodeManager, NodeState, ban_expires_at};
use crate::network::{NetworkAddress, SharedTcpWriter, SharedTcpWriterExt, message_version::MessageVersion};
use crate::utils::unix_now;

/// Handles an incoming version message from a peer
pub(super) async fn handle_version(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let version = MessageVersion::from_bytes(payload).context("failed to parse version message")?;

	// Phase 1: Validate under lock, then release
	let (is_incoming, port) = {
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager"))?;

		// Nodes can send only one version command
		if node.version_received {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Node {} sent version command twice, banning", address);
			return Err(anyhow!("Node {address} sent version command twice"));
		}

		if version.nonce == node_manager.my_nonce {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::ProtocolViolation,
				created: now,
				expires: ban_expires_at(now),
			};
			warn!("Self-connection detected to {}, banning", address);
			return Err(anyhow!("Node {address} is myself"));
		}

		(node.connection_type == ConnectionType::Incoming, node.port)
		// RefMut dropped here
	};

	// Phase 2: Async sends without holding any lock
	if is_incoming {
		let network_address = NetworkAddress::new(*address, port);
		let tip_height = node_manager.chainstate().map_or(0, |cs| cs.tip_height());
		let version_message = MessageVersion::new(network_address, node_manager.my_nonce, tip_height);
		tcp_writer
			.send_message("version", &version_message.to_bytes())
			.await
			.context("failed to send version reply for inbound connection")?;
	}

	// Validate start_height
	if version.start_height < 0 {
		if let Some(mut node) = node_manager.nodes.get_mut(address) {
			let now = unix_now();
			node.state = NodeState::Banned {
				reason: BanReason::Misbehavior,
				created: now,
				expires: ban_expires_at(now),
			};
		}
		warn!(
			"Node {} sent negative start_height {}, banning",
			address, version.start_height
		);
		return Err(anyhow!("Node {address} sent negative start_height"));
	}

	// Warn on extreme clock skew but don't ban (clock drift is common)
	// Protocol uses i64 for timestamp; u64 seconds won't wrap for ~584 billion years
	// i64 subtraction of two timestamps can't overflow in practice (both near current epoch)
	#[allow(clippy::cast_possible_wrap, clippy::arithmetic_side_effects)]
	let time_diff = (version.timestamp - unix_now() as i64).unsigned_abs();
	if time_diff > 4200 {
		warn!("Node {} clock skew is {}s (threshold 4200s)", address, time_diff);
	}

	// Phase 3: Re-acquire lock to write fields
	// Verify state hasn't changed between phases (another task could have banned the node)
	{
		let mut node = node_manager
			.nodes
			.get_mut(address)
			.ok_or_else(|| anyhow!("node {address} disappeared from manager during version handling"))?;

		if !matches!(node.state, NodeState::Handshaking { .. }) {
			return Err(anyhow!(
				"node {address} state changed during version handling (now {})",
				node.state
			));
		}

		node.services = version.services;
		node.timestamp = version.timestamp;
		node.user_agent = sanitize_user_agent(&version.user_agent);
		node.height = version.start_height;
		node.version = version.version;
		node.version_received = true;
		node.relay = version.relay;
		node.pending_external_ip = Some(version.addr_recv.address);
	}

	// Verack sent after all locks released
	tcp_writer
		.send_message("verack", &[])
		.await
		.context("failed to send verack")
}

/// Strips non-printable and non-ASCII characters from a user agent string
///
/// Prevents terminal escape sequence injection via malicious user agents.
/// Trims leading/trailing whitespace to prevent display confusion
pub fn sanitize_user_agent(s: &str) -> String {
	s.chars()
		.filter(|c| c.is_ascii_graphic() || *c == ' ')
		.take(MAX_USER_AGENT_DISPLAY)
		.collect::<String>()
		.trim()
		.to_string()
}
