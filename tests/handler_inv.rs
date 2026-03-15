// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_possible_truncation)]

use std::net::IpAddr;
use std::sync::Arc;

use ironcat::difficulty::ConsensusParams;
use ironcat::network::message_inv::{InvItem, InvType, MessageInv};
use ironcat::nodes::NodeManager;
use ironcat::nodes::handler_inv::{MAX_BLOCKS_PER_GETDATA, handle_getdata};
use ironcat::nodes::test_helpers::{command_name, read_all_messages, tcp_pair, test_genesis};
use ironcat::storage::block_store::BlockStore;
use ironcat::types::hash::Hash256;

#[tokio::test]
async fn getdata_serves_block_from_store() {
	let dir = tempfile::tempdir().unwrap();
	let store = Arc::new(BlockStore::open(dir.path()).unwrap());

	let hash = Hash256::from_bytes([0xAA; 32]);
	let block_data = vec![1u8, 2, 3, 4, 5];
	store.store_block(&hash, &block_data).unwrap();
	store.flush().unwrap();

	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
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

	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
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
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));

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

	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));
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
	let nm = Arc::new(NodeManager::new(test_genesis(), ConsensusParams::mainnet()));

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
