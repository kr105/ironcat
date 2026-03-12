// SPDX-License-Identifier: Apache-2.0

//! Tests for getblocks wire format (reuses getheaders format)

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::network::message_getheaders::MessageGetHeaders;
use ironcat::types::hash::Hash256;

#[test]
fn getblocks_round_trip() {
	let locator = vec![Hash256::from_bytes([0xAA; 32]), Hash256::from_bytes([0xBB; 32])];
	let stop = Hash256::from_bytes([0xCC; 32]);

	let msg = MessageGetHeaders::new(locator.clone(), stop);
	let bytes = msg.to_bytes();
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.locator_hashes().len(), 2);
	assert_eq!(decoded.locator_hashes()[0], locator[0]);
	assert_eq!(decoded.locator_hashes()[1], locator[1]);
	assert_eq!(decoded.hash_stop(), stop);
}

#[test]
fn getblocks_empty_locator() {
	let msg = MessageGetHeaders::new(vec![], Hash256::ZERO);
	let bytes = msg.to_bytes();
	let decoded = MessageGetHeaders::from_bytes(&bytes).unwrap();

	assert!(decoded.locator_hashes().is_empty());
	assert_eq!(decoded.hash_stop(), Hash256::ZERO);
}
