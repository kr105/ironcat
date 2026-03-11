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

/// Maximum number of blocks to serve in a single getdata response.
/// Prevents a peer from triggering unbounded disk I/O and memory usage
/// with a single message. Remaining block items are added to notfound
const MAX_BLOCKS_PER_GETDATA: usize = 16;

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
/// Serves blocks from the block store (up to `MAX_BLOCKS_PER_GETDATA`) and
/// transactions from the mempool. Items not available or exceeding the
/// per-message block cap are collected into a notfound response.
///
/// Send failures for individual items are logged and treated as notfound
/// rather than aborting the entire response, so remaining items still get
/// processed
pub(super) async fn handle_getdata(
	node_manager: &Arc<NodeManager>,
	address: &IpAddr,
	tcp_writer: &SharedTcpWriter,
	payload: &[u8],
) -> Result<()> {
	let inv = MessageInv::from_bytes(payload).context("failed to parse getdata message")?;

	debug!(peer = %address, count = inv.items().len(), "received getdata");

	let mut notfound_items: Vec<InvItem> = Vec::new();
	let mut blocks_served: usize = 0;

	// Resolve shared resources once before the loop
	let block_store = node_manager.block_store();
	let mempool = node_manager.mempool();

	for item in inv.items() {
		match item.inv_type {
			InvType::Block => {
				if blocks_served >= MAX_BLOCKS_PER_GETDATA {
					notfound_items.push(item.clone());
					continue;
				}
				if let Some(ref store) = block_store {
					let hash = item.hash;
					let store_ref = Arc::clone(store);
					match tokio::task::spawn_blocking(move || store_ref.load_block(&hash))
						.await
						.context("block load task panicked")?
					{
						Ok(block_bytes) => {
							if let Err(e) = tcp_writer.send_message("block", &block_bytes).await {
								warn!(peer = %address, hash = %item.hash, error = %e, "failed to send block");
								notfound_items.push(item.clone());
							} else {
								debug!(peer = %address, hash = %item.hash, "served block");
								blocks_served = blocks_served.saturating_add(1);
							}
						}
						Err(e) => {
							warn!(peer = %address, hash = %item.hash, error = %e, "failed to load block for getdata");
							notfound_items.push(item.clone());
						}
					}
				} else {
					notfound_items.push(item.clone());
				}
			}
			InvType::Tx => {
				if let Some(ref mempool_arc) = mempool {
					let mp = mempool_arc.read().await;
					if let Some(entry) = mp.get(&item.hash) {
						let tx_bytes = entry.tx.to_bytes();
						drop(mp);
						if let Err(e) = tcp_writer.send_message("tx", &tx_bytes).await {
							warn!(peer = %address, hash = %item.hash, error = %e, "failed to send tx");
							notfound_items.push(item.clone());
						}
					} else {
						drop(mp);
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

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
	use super::*;
	use crate::difficulty::ConsensusParams;
	use crate::network::Message;
	use crate::storage::block_store::BlockStore;
	use crate::types::block::BlockHeader;

	use std::ffi::CStr;
	use tokio::io::AsyncReadExt;
	use tokio::net::tcp::OwnedReadHalf;
	use tokio::net::TcpListener;

	const fn test_genesis() -> BlockHeader {
		BlockHeader {
			version: 1,
			prev_hash: Hash256::ZERO,
			merkle_root: Hash256::ZERO,
			timestamp: 0,
			bits: 0,
			nonce: 0,
		}
	}

	/// Creates a loopback TCP pair and returns (writer for handler, reader for assertions)
	async fn tcp_pair() -> (SharedTcpWriter, OwnedReadHalf) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let client = tokio::net::TcpStream::connect(addr).await.unwrap();
		let (server, _) = listener.accept().await.unwrap();
		let (read_half, _write_half) = server.into_split();
		let (_client_read, client_write) = client.into_split();
		let writer = Arc::new(tokio::sync::Mutex::new(client_write));
		(writer, read_half)
	}

	/// Reads all protocol messages from the stream until the writer is dropped.
	/// Call after the handler has returned so all data is already buffered
	async fn read_all_messages(reader: &mut OwnedReadHalf) -> Vec<Message> {
		let mut buf = Vec::new();
		reader.read_to_end(&mut buf).await.unwrap();
		let mut messages = Vec::new();
		let mut offset = 0;
		while offset < buf.len() {
			match Message::from_bytes(&buf[offset..]) {
				Ok((msg, consumed)) => {
					offset += consumed;
					messages.push(msg);
				}
				Err(crate::network::MessageParseError::Incomplete) => break,
				Err(e) => panic!("corrupt message at offset {offset}: {e:?}"),
			}
		}
		messages
	}

	/// Extracts the command name from a message's command field
	fn command_name(msg: &Message) -> &str {
		CStr::from_bytes_until_nul(&msg.command).unwrap().to_str().unwrap()
	}

	#[tokio::test]
	async fn getdata_serves_block_from_store() {
		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(BlockStore::open(dir.path()).unwrap());

		let hash = Hash256::from_bytes([0xAA; 32]);
		let block_data = vec![1u8, 2, 3, 4, 5];
		store.store_block(&hash, &block_data).unwrap();
		store.flush().unwrap();

		let nm = Arc::new(super::super::NodeManager::new(
			test_genesis(),
			ConsensusParams::mainnet(),
		));
		nm.set_block_store(Arc::clone(&store));

		let getdata = MessageInv::new(vec![InvItem {
			inv_type: InvType::Block,
			hash,
		}]);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getdata(&nm, &peer, &writer, &getdata.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert_eq!(msgs.len(), 1);
		assert_eq!(command_name(&msgs[0]), "block");
		assert_eq!(msgs[0].payload, block_data);
	}

	#[tokio::test]
	async fn getdata_notfound_for_missing_block() {
		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(BlockStore::open(dir.path()).unwrap());

		let nm = Arc::new(super::super::NodeManager::new(
			test_genesis(),
			ConsensusParams::mainnet(),
		));
		nm.set_block_store(Arc::clone(&store));

		let missing_hash = Hash256::from_bytes([0xBB; 32]);
		let getdata = MessageInv::new(vec![InvItem {
			inv_type: InvType::Block,
			hash: missing_hash,
		}]);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getdata(&nm, &peer, &writer, &getdata.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert_eq!(msgs.len(), 1);
		assert_eq!(command_name(&msgs[0]), "notfound");
		let inv = MessageInv::from_bytes(&msgs[0].payload).unwrap();
		assert_eq!(inv.items().len(), 1);
		assert_eq!(inv.items()[0].hash, missing_hash);
	}

	#[tokio::test]
	async fn getdata_notfound_when_no_block_store() {
		let nm = Arc::new(super::super::NodeManager::new(
			test_genesis(),
			ConsensusParams::mainnet(),
		));

		let hash = Hash256::from_bytes([0xCC; 32]);
		let getdata = MessageInv::new(vec![InvItem {
			inv_type: InvType::Block,
			hash,
		}]);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getdata(&nm, &peer, &writer, &getdata.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert_eq!(msgs.len(), 1);
		assert_eq!(command_name(&msgs[0]), "notfound");
		let inv = MessageInv::from_bytes(&msgs[0].payload).unwrap();
		assert_eq!(inv.items().len(), 1);
		assert_eq!(inv.items()[0].hash, hash);
	}

	#[tokio::test]
	async fn getdata_caps_blocks_at_limit() {
		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(BlockStore::open(dir.path()).unwrap());

		// Store more blocks than the cap
		let total = MAX_BLOCKS_PER_GETDATA + 4;
		let mut items = Vec::with_capacity(total);
		for i in 0..total {
			let mut hash_bytes = [0u8; 32];
			hash_bytes[0] = i as u8;
			hash_bytes[1] = (i >> 8) as u8;
			let hash = Hash256::from_bytes(hash_bytes);
			store.store_block(&hash, &[i as u8; 10]).unwrap();
			items.push(InvItem {
				inv_type: InvType::Block,
				hash,
			});
		}
		store.flush().unwrap();

		let nm = Arc::new(super::super::NodeManager::new(
			test_genesis(),
			ConsensusParams::mainnet(),
		));
		nm.set_block_store(Arc::clone(&store));

		let getdata = MessageInv::new(items);
		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getdata(&nm, &peer, &writer, &getdata.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		let block_count = msgs.iter().filter(|m| command_name(m) == "block").count();
		let notfound_msgs: Vec<_> = msgs.iter().filter(|m| command_name(m) == "notfound").collect();

		assert_eq!(block_count, MAX_BLOCKS_PER_GETDATA);
		assert_eq!(notfound_msgs.len(), 1);
		let inv = MessageInv::from_bytes(&notfound_msgs[0].payload).unwrap();
		assert_eq!(inv.items().len(), 4);
	}

	#[tokio::test]
	async fn getdata_unsupported_types_return_notfound() {
		let nm = Arc::new(super::super::NodeManager::new(
			test_genesis(),
			ConsensusParams::mainnet(),
		));

		let hash = Hash256::from_bytes([0xDD; 32]);
		let getdata = MessageInv::new(vec![InvItem {
			inv_type: InvType::FilteredBlock,
			hash,
		}]);

		let (writer, mut reader) = tcp_pair().await;
		let peer: IpAddr = "10.0.0.1".parse().unwrap();

		handle_getdata(&nm, &peer, &writer, &getdata.to_bytes()).await.unwrap();
		drop(writer);

		let msgs = read_all_messages(&mut reader).await;
		assert_eq!(msgs.len(), 1);
		assert_eq!(command_name(&msgs[0]), "notfound");
		let inv = MessageInv::from_bytes(&msgs[0].payload).unwrap();
		assert_eq!(inv.items().len(), 1);
		assert_eq!(inv.items()[0].inv_type, InvType::FilteredBlock);
	}
}
