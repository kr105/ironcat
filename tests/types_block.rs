// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::{
	block::{Block, BlockHeader, HEADER_SIZE, compute_merkle_root},
	hash::{Hash256, double_sha256},
	transaction::{OutPoint, Transaction, TxIn, TxOut},
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

// --- Block and merkle root tests ---

fn coinbase_tx() -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig: vec![0x04, 0xff, 0xff, 0x00, 0x1d],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_0000_0000,
			script_pubkey: vec![0x76, 0xa9, 0x14, 0x01, 0x02, 0x03],
		}],
		locktime: 0,
	}
}

/// Serializes a block (header + varint `tx_count` + transactions) to bytes
fn serialize_block(header: &BlockHeader, txs: &[Transaction]) -> Vec<u8> {
	let mut bytes = header.to_bytes();
	// tx count as varint -- test blocks always have < 253 txs
	#[allow(clippy::cast_possible_truncation)]
	bytes.push(txs.len() as u8);
	for tx in txs {
		bytes.extend_from_slice(&tx.to_bytes());
	}
	bytes
}

#[test]
fn block_from_bytes_single_tx() {
	let cb = coinbase_tx();
	let (merkle, _) = compute_merkle_root(&[cb.txid()]);
	let header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: merkle,
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let bytes = serialize_block(&header, &[cb]);
	let block = Block::from_bytes(&bytes).unwrap();

	assert_eq!(block.header.version, 1);
	assert_eq!(block.header.merkle_root, merkle);
	assert_eq!(block.transactions.len(), 1);
	assert_eq!(block.transactions[0].version, 1);
	assert_eq!(block.transactions[0].vin[0].prev_output.txid, Hash256::ZERO);
}

#[test]
fn compute_merkle_root_single_tx() {
	let txid = Hash256::from_bytes([0xAA; 32]);
	let (root, mutated) = compute_merkle_root(&[txid]);
	assert_eq!(root, txid);
	assert!(!mutated);
}

#[test]
fn compute_merkle_root_two_txs() {
	let tx1 = Hash256::from_bytes([0x01; 32]);
	let tx2 = Hash256::from_bytes([0x02; 32]);

	let mut combined = Vec::with_capacity(64);
	combined.extend_from_slice(tx1.as_bytes());
	combined.extend_from_slice(tx2.as_bytes());
	let expected = double_sha256(&combined);

	let (root, mutated) = compute_merkle_root(&[tx1, tx2]);
	assert_eq!(root, expected);
	assert!(!mutated);
}

#[test]
fn compute_merkle_root_odd_duplicates_last() {
	let tx1 = Hash256::from_bytes([0x01; 32]);
	let tx2 = Hash256::from_bytes([0x02; 32]);
	let tx3 = Hash256::from_bytes([0x03; 32]);

	// Level 1: hash(tx1||tx2), hash(tx3||tx3)
	let mut pair_12 = Vec::with_capacity(64);
	pair_12.extend_from_slice(tx1.as_bytes());
	pair_12.extend_from_slice(tx2.as_bytes());
	let h12 = double_sha256(&pair_12);

	let mut pair_33 = Vec::with_capacity(64);
	pair_33.extend_from_slice(tx3.as_bytes());
	pair_33.extend_from_slice(tx3.as_bytes());
	let h33 = double_sha256(&pair_33);

	// Level 2: hash(h12||h33)
	let mut pair_final = Vec::with_capacity(64);
	pair_final.extend_from_slice(h12.as_bytes());
	pair_final.extend_from_slice(h33.as_bytes());
	let expected = double_sha256(&pair_final);

	let (root, mutated) = compute_merkle_root(&[tx1, tx2, tx3]);
	assert_eq!(root, expected);
	assert!(!mutated);
}

#[test]
fn block_validates_merkle_root() {
	let cb = coinbase_tx();
	let (correct_merkle, _) = compute_merkle_root(&[cb.txid()]);

	// Build block with wrong merkle root
	let bad_header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::from_bytes([0xFF; 32]),
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let bytes = serialize_block(&bad_header, std::slice::from_ref(&cb));
	let result = Block::from_bytes_and_validate(&bytes);
	assert!(result.is_err(), "should reject block with wrong merkle root");

	// Build block with correct merkle root
	let good_header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: correct_merkle,
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let bytes = serialize_block(&good_header, std::slice::from_ref(&cb));
	let block = Block::from_bytes_and_validate(&bytes).unwrap();
	assert_eq!(block.header.merkle_root, correct_merkle);
}

#[test]
fn block_from_bytes_zero_txs() {
	let header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let mut bytes = header.to_bytes();
	bytes.push(0x00); // varint tx_count = 0
	let block = Block::from_bytes(&bytes).unwrap();
	assert!(block.transactions.is_empty());
}

#[test]
fn block_rejects_excessive_tx_count() {
	let header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let mut bytes = header.to_bytes();
	// Encode varint 100_001 (0x186A1) using 5-byte varint format (0xFE prefix + u32)
	bytes.push(0xFE);
	bytes.extend_from_slice(&100_001u32.to_le_bytes());
	// No actual tx data follows -- should fail on count check
	let result = Block::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn block_rejects_duplicate_final_tx() {
	let cb = coinbase_tx();
	let txs = [cb.clone(), cb];
	// The merkle root of [tx, tx] is hash(tx||tx) which is a valid merkle tree,
	// but CVE-2012-2459 means we must reject duplicate final txids
	let txids: Vec<Hash256> = txs.iter().map(Transaction::txid).collect();
	let (merkle, _) = compute_merkle_root(&txids);

	let header = BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: merkle,
		timestamp: 1_000_000,
		bits: 0x1e0f_fff0,
		nonce: 42,
	};
	let bytes = serialize_block(&header, &txs);
	let result = Block::from_bytes_and_validate(&bytes);
	assert!(result.is_err());
	let err_msg = result.unwrap_err().to_string();
	assert!(
		err_msg.contains("CVE-2012-2459"),
		"error should mention CVE-2012-2459, got: {err_msg}"
	);
}

#[test]
#[allow(clippy::many_single_char_names)] // single-letter names match the mathematical notation in the comment
fn merkle_mutation_detected_at_inner_level() {
	// CVE-2012-2459: duplicate subtrees at an inner tree level must be detected.
	// [A, B, C, D, E, F] and [A, B, C, D, E, F, E, F] produce the same root
	// because level 1 gets [H(AB), H(CD), H(EF), H(EF)] in both cases.
	// The leaf-level check alone misses this -- the inner-level pair check catches it
	let a = Hash256::from_bytes([0x01; 32]);
	let b = Hash256::from_bytes([0x02; 32]);
	let c = Hash256::from_bytes([0x03; 32]);
	let d = Hash256::from_bytes([0x04; 32]);
	let e = Hash256::from_bytes([0x05; 32]);
	let f = Hash256::from_bytes([0x06; 32]);

	let (root_short, mutated_short) = compute_merkle_root(&[a, b, c, d, e, f]);
	assert!(!mutated_short, "original 6-tx tree should not be mutated");

	// The mutated version: duplicate the last two txs
	let (root_long, mutated_long) = compute_merkle_root(&[a, b, c, d, e, f, e, f]);
	assert!(
		mutated_long,
		"8-tx tree with duplicate trailing pair must be detected as mutated"
	);

	// Both produce the same root (the attack vector)
	assert_eq!(root_short, root_long, "mutated tree must produce same root as original");
}
