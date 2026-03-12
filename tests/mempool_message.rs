// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::network::message_inv::{InvItem, InvType, MessageInv};
use ironcat::types::hash::Hash256;

#[test]
fn inv_round_trip_with_tx_items() {
	let txids: Vec<Hash256> = (0..5u8)
		.map(|i| {
			let mut bytes = [0u8; 32];
			bytes[0] = i;
			Hash256::from_bytes(bytes)
		})
		.collect();

	let items: Vec<InvItem> = txids
		.iter()
		.map(|hash| InvItem {
			inv_type: InvType::Tx,
			hash: *hash,
		})
		.collect();

	let msg = MessageInv::new(items);
	let bytes = msg.to_bytes();
	let decoded = MessageInv::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.items().len(), 5);
	for (i, item) in decoded.items().iter().enumerate() {
		assert_eq!(item.inv_type, InvType::Tx);
		assert_eq!(item.hash, txids[i]);
	}
}
