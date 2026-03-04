// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::{
	network::message_inv::{InvItem, InvType, MessageInv},
	types::hash::Hash256,
};

const fn sample_hash(byte: u8) -> Hash256 {
	Hash256::from_bytes([byte; 32])
}

#[test]
fn roundtrip_single_item() {
	let msg = MessageInv::new(vec![InvItem {
		inv_type: InvType::Tx,
		hash: sample_hash(0xAA),
	}]);

	let bytes = msg.to_bytes();
	let decoded = MessageInv::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.items().len(), 1);
	assert_eq!(decoded.items()[0].inv_type, InvType::Tx);
	assert_eq!(decoded.items()[0].hash, sample_hash(0xAA));
}

#[test]
fn roundtrip_multiple_mixed_types() {
	let items = vec![
		InvItem {
			inv_type: InvType::Tx,
			hash: sample_hash(0x01),
		},
		InvItem {
			inv_type: InvType::Block,
			hash: sample_hash(0x02),
		},
		InvItem {
			inv_type: InvType::FilteredBlock,
			hash: sample_hash(0x03),
		},
	];

	let msg = MessageInv::new(items);
	let bytes = msg.to_bytes();
	let decoded = MessageInv::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.items().len(), 3);
	assert_eq!(decoded.items()[0].inv_type, InvType::Tx);
	assert_eq!(decoded.items()[1].inv_type, InvType::Block);
	assert_eq!(decoded.items()[2].inv_type, InvType::FilteredBlock);
}

#[test]
fn roundtrip_empty() {
	let msg = MessageInv::new(vec![]);
	let bytes = msg.to_bytes();
	assert_eq!(bytes, vec![0x00]);

	let decoded = MessageInv::from_bytes(&bytes).unwrap();
	assert!(decoded.items().is_empty());
}

#[test]
fn rejects_count_over_max() {
	// Encode count as 50001 using varint (0xFE prefix + u32 LE)
	let mut bytes = vec![0xFE];
	bytes.extend_from_slice(&50_001u32.to_le_bytes());
	let result = MessageInv::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn unknown_inv_type_skipped() {
	// Build a message with 2 items: unknown type 99, then a valid Tx
	let mut bytes = Vec::new();
	bytes.push(2); // count = 2

	// Item 1: unknown type 99
	bytes.extend_from_slice(&99u32.to_le_bytes());
	bytes.extend_from_slice(&[0xBB; 32]);

	// Item 2: valid Tx
	bytes.extend_from_slice(&1u32.to_le_bytes());
	bytes.extend_from_slice(&[0xCC; 32]);

	let decoded = MessageInv::from_bytes(&bytes).unwrap();
	// Only the valid Tx item should be present
	assert_eq!(decoded.items().len(), 1);
	assert_eq!(decoded.items()[0].inv_type, InvType::Tx);
	assert_eq!(decoded.items()[0].hash, sample_hash(0xCC));
}

#[test]
fn witness_flag_stripped() {
	// Type 0x40000001 = MSG_WITNESS_FLAG | MSG_TX = should parse as Tx
	let mut bytes = Vec::new();
	bytes.push(1); // count = 1
	bytes.extend_from_slice(&0x4000_0001u32.to_le_bytes());
	bytes.extend_from_slice(&[0xDD; 32]);

	let decoded = MessageInv::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.items().len(), 1);
	assert_eq!(decoded.items()[0].inv_type, InvType::Tx);
}

#[test]
fn mweb_flag_stripped() {
	// Type 0x20000002 = MSG_MWEB_FLAG | MSG_BLOCK = should parse as Block
	let mut bytes = Vec::new();
	bytes.push(1); // count = 1
	bytes.extend_from_slice(&0x2000_0002u32.to_le_bytes());
	bytes.extend_from_slice(&[0xEE; 32]);

	let decoded = MessageInv::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.items().len(), 1);
	assert_eq!(decoded.items()[0].inv_type, InvType::Block);
}

#[test]
fn inv_item_size_is_36() {
	let item = InvItem {
		inv_type: InvType::Block,
		hash: sample_hash(0xFF),
	};
	assert_eq!(item.to_bytes().len(), 36);
}
