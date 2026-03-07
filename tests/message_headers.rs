// SPDX-License-Identifier: Apache-2.0
#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::cast_possible_truncation,
	clippy::missing_const_for_fn
)]

use ironcat::network::message_headers::{MessageHeaders, MAX_HEADERS_PER_MSG};
use ironcat::types::block::BlockHeader;
use ironcat::types::hash::Hash256;

fn make_header(nonce: u32) -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 1_387_838_302,
		bits: 0x1e0f_fff0,
		nonce,
	}
}

#[test]
fn roundtrip_single_header() {
	let headers = vec![make_header(42)];
	let msg = MessageHeaders::new(headers.clone());
	let bytes = msg.to_bytes();
	let decoded = MessageHeaders::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.headers(), headers.as_slice());
}

#[test]
fn roundtrip_multiple_headers() {
	let headers = vec![make_header(1), make_header(2), make_header(3)];
	let msg = MessageHeaders::new(headers.clone());
	let bytes = msg.to_bytes();
	let decoded = MessageHeaders::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.headers(), headers.as_slice());
}

#[test]
fn roundtrip_empty() {
	let msg = MessageHeaders::new(vec![]);
	let bytes = msg.to_bytes();
	assert_eq!(bytes, [0x00]);
	let decoded = MessageHeaders::from_bytes(&bytes).unwrap();
	assert_eq!(decoded.headers().len(), 0);
}

#[test]
fn each_header_has_zero_txcount() {
	// 1-byte count varint + 80-byte header + 1-byte tx_count = 82
	let msg = MessageHeaders::new(vec![make_header(7)]);
	let bytes = msg.to_bytes();
	assert_eq!(bytes.len(), 82);
	assert_eq!(bytes[81], 0x00);
}

#[test]
fn rejects_count_over_2000() {
	// Encode a varint of 2001 as the count, no actual headers follow
	let mut bytes: Vec<u8> = Vec::new();
	// 2001 requires 0xfd prefix (3-byte encoding)
	bytes.push(0xfd);
	let n: u16 = (MAX_HEADERS_PER_MSG + 1) as u16;
	bytes.extend_from_slice(&n.to_le_bytes());

	let result = MessageHeaders::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn consumes_headers() {
	let headers = vec![make_header(99)];
	let msg = MessageHeaders::new(headers.clone());
	let owned = msg.into_headers();
	assert_eq!(owned, headers);
}
