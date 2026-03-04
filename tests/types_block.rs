// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::{
	block::{BlockHeader, HEADER_SIZE},
	hash::Hash256,
};

/// Builds the Catcoin mainnet genesis block header from known parameters
const fn catcoin_genesis_header() -> BlockHeader {
	// Mainnet genesis: nTime=1387838302, nNonce=588050, nBits=0x1e0ffff0, nVersion=1
	// merkle root: 4007a33db5d9cdf2aab117335eb8431c8d13fb86e0214031fdaebe69a0f29cf7

	// merkle root in wire order (reverse of display hex)
	let merkle_bytes: [u8; 32] = [
		0xf7, 0x9c, 0xf2, 0xa0, 0x69, 0xbe, 0xae, 0xfd, 0x31, 0x40, 0x21, 0xe0, 0x86, 0xfb, 0x13, 0x8d, 0x1c, 0x43,
		0xb8, 0x5e, 0x33, 0x17, 0xb1, 0xaa, 0xf2, 0xcd, 0xd9, 0xb5, 0x3d, 0xa3, 0x07, 0x40,
	];

	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::from_bytes(merkle_bytes),
		timestamp: 1_387_838_302,
		bits: 0x1e0f_fff0,
		nonce: 588_050,
	}
}

#[test]
fn block_header_roundtrip() {
	let header = catcoin_genesis_header();
	let bytes = header.to_bytes();
	let decoded = BlockHeader::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.version, header.version);
	assert_eq!(decoded.prev_hash, header.prev_hash);
	assert_eq!(decoded.merkle_root, header.merkle_root);
	assert_eq!(decoded.timestamp, header.timestamp);
	assert_eq!(decoded.bits, header.bits);
	assert_eq!(decoded.nonce, header.nonce);
}

#[test]
fn block_header_size_is_80() {
	let header = catcoin_genesis_header();
	assert_eq!(header.to_bytes().len(), HEADER_SIZE);
}

#[test]
fn block_header_rejects_short_input() {
	let bytes = [0u8; 79];
	assert!(BlockHeader::from_bytes(&bytes).is_err());
}

#[test]
fn block_header_hash_deterministic() {
	let header = catcoin_genesis_header();
	let h1 = header.block_hash();
	let h2 = header.block_hash();
	assert_eq!(h1, h2);
}

#[test]
fn block_header_catcoin_genesis() {
	let header = catcoin_genesis_header();
	let hash = header.block_hash();
	// Known mainnet genesis hash (display order, reversed from wire)
	assert_eq!(
		hash.to_string(),
		"bc3b4ec43c4ebb2fef49e6240812549e61ffa623d9418608aa90eaad26c96296"
	);
}
