// SPDX-License-Identifier: Apache-2.0

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, warn};

use super::{ban_expires_at, BanReason, NodeManager, NodeState};
use crate::network::{SharedTcpWriter, SharedTcpWriterExt};
use crate::utils::{unix_now, vec_to_u64_le};

/// Handles an incoming ping message from a peer
pub(super) async fn handle_ping(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	// Pre-BIP31 nodes send 0-byte pings; just acknowledge silently
	if payload.is_empty() {
		debug!("Received pre-BIP31 ping (no nonce) from {}", address);
		return Ok(());
	}

	if payload.len() != 8 {
		warn!("Received malformed ping command from {}", address);

		let now = unix_now();
		node_manager.set_state(
			address,
			NodeState::Banned {
				reason: BanReason::Misbehavior,
				created: now,
				expires: ban_expires_at(now),
			},
		);

		return Err(anyhow!("Received malformed ping command from {address}"));
	}

	let nonce = vec_to_u64_le(payload).context("failed to parse ping nonce")?;
	debug!("Received ping command from {} with nonce {}", address, nonce);

	tcp_writer
		.send_message("pong", &nonce.to_le_bytes())
		.await
		.context("failed to send pong")
}

/// Handles an incoming pong message from a peer
///
/// Validates that the nonce matches the last ping we sent
pub(super) fn handle_pong(node_manager: &NodeManager, address: &IpAddr, payload: &[u8]) {
	if payload.len() != 8 {
		warn!(
			"Received malformed pong from {} ({} bytes, expected 8)",
			address,
			payload.len()
		);
		return;
	}

	let Ok(nonce) = vec_to_u64_le(payload) else {
		warn!("Failed to parse pong nonce from {}", address);
		return;
	};

	if let Some(mut node) = node_manager.nodes.get_mut(address) {
		match node.last_ping_nonce.take() {
			Some(expected) if expected == nonce => {
				debug!("Valid pong from {}", address);
			}
			Some(expected) => {
				warn!(
					"Pong nonce mismatch from {}: expected {}, got {}",
					address, expected, nonce
				);
			}
			None => {
				debug!("Received unsolicited pong from {}", address);
			}
		}
	}
}
