// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::difficulty::ChainLookup;
use ironcat::types::{
	block::{Block, BlockHeader},
	hash::Hash256,
	transaction::{OutPoint, Transaction, TxIn, TxOut},
};
use ironcat::validation::{check_block_context, compute_median_time_past};

/// Mock chain for MTP tests -- stores (timestamp, bits) indexed by height
struct MockChain {
	headers: Vec<(u32, u32)>,
}

impl MockChain {
	fn new(timestamps: &[u32]) -> Self {
		Self {
			headers: timestamps.iter().map(|&ts| (ts, 0x1e0f_fff0)).collect(),
		}
	}
}

impl ChainLookup for MockChain {
	fn header_at(&self, height: u32) -> Option<(u32, u32)> {
		self.headers.get(height as usize).copied()
	}
}

/// Encodes a block height as a `CScript` number for BIP34 coinbase
fn encode_script_height(height: u32) -> Vec<u8> {
	if height == 0 {
		return vec![0x00]; // OP_0
	}
	let mut n = height;
	let mut data = Vec::new();
	while n > 0 {
		#[allow(clippy::cast_possible_truncation)]
		data.push((n & 0xff) as u8);
		n >>= 8;
	}
	// If the most significant byte has the sign bit set, append 0x00
	if data.last().unwrap() & 0x80 != 0 {
		data.push(0x00);
	}
	let mut script = Vec::new();
	#[allow(clippy::cast_possible_truncation)]
	script.push(data.len() as u8); // push opcode = length
	script.extend_from_slice(&data);
	script
}

fn coinbase_with_height(height: u32, value: i64) -> Transaction {
	let mut script_sig = encode_script_height(height);
	// Pad to at least 2 bytes for MIN_COINBASE_SCRIPTSIG
	while script_sig.len() < 2 {
		script_sig.push(0x00);
	}
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint::COINBASE,
			script_sig,
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9],
		}],
		locktime: 0,
	}
}

fn make_block(version: i32, timestamp: u32, height: u32, value: i64) -> Block {
	Block {
		header: BlockHeader {
			version,
			prev_hash: Hash256::ZERO,
			merkle_root: Hash256::ZERO,
			timestamp,
			bits: 0x1e0f_fff0,
			nonce: 0,
		},
		transactions: vec![coinbase_with_height(height, value)],
	}
}

// --- MTP tests ---

#[test]
fn mtp_with_11_blocks() {
	let timestamps: Vec<u32> = (0..11).map(|i| (i + 1) * 100).collect();
	let chain = MockChain::new(&timestamps);
	assert_eq!(compute_median_time_past(&chain, 10), Some(600));
}

#[test]
fn mtp_with_fewer_than_11_blocks() {
	// 3 blocks at heights 0,1,2 with timestamps 100, 300, 200
	// Sorted: [100, 200, 300], median (index 1) = 200
	let chain = MockChain::new(&[100, 300, 200]);
	assert_eq!(compute_median_time_past(&chain, 2), Some(200));
}

#[test]
fn mtp_at_genesis() {
	let chain = MockChain::new(&[1_387_838_302]);
	assert_eq!(compute_median_time_past(&chain, 0), Some(1_387_838_302));
}

#[test]
fn mtp_missing_header_returns_none() {
	let chain = MockChain::new(&[100]); // only height 0
	assert_eq!(compute_median_time_past(&chain, 5), None);
}

// --- Contextual validation tests ---

#[test]
fn valid_block_context() {
	let block = make_block(2, 2000, 111, 50_0000_0000);
	assert!(check_block_context(&block, 111, 1000, 2100).is_ok());
}

#[test]
fn reject_timestamp_before_mtp() {
	let block = make_block(2, 999, 111, 50_0000_0000);
	let err = check_block_context(&block, 111, 1000, 2100).unwrap_err();
	assert!(err.to_string().contains("time-too-old"), "{err}");
}

#[test]
fn reject_timestamp_equal_to_mtp() {
	let block = make_block(2, 1000, 111, 50_0000_0000);
	let err = check_block_context(&block, 111, 1000, 2100).unwrap_err();
	assert!(err.to_string().contains("time-too-old"), "{err}");
}

#[test]
fn reject_timestamp_too_far_future() {
	let block = make_block(2, 2151, 111, 50_0000_0000);
	let err = check_block_context(&block, 111, 1000, 2000).unwrap_err();
	assert!(err.to_string().contains("time-too-new"), "{err}");
}

#[test]
fn accept_timestamp_at_future_boundary() {
	let block = make_block(2, 2150, 111, 50_0000_0000);
	assert!(check_block_context(&block, 111, 1000, 2000).is_ok());
}

#[test]
fn reject_version_1_after_bip34() {
	let block = make_block(1, 2000, 111, 50_0000_0000);
	let err = check_block_context(&block, 111, 1000, 2100).unwrap_err();
	assert!(err.to_string().contains("version"), "{err}");
}

#[test]
fn accept_version_1_before_bip34() {
	let block = make_block(1, 2000, 110, 50_0000_0000);
	assert!(check_block_context(&block, 110, 1000, 2100).is_ok());
}

#[test]
fn reject_wrong_bip34_height() {
	// Block at height 200 but coinbase encodes height 111
	let block = make_block(2, 2000, 111, 50_0000_0000);
	let err = check_block_context(&block, 200, 1000, 2100).unwrap_err();
	assert!(
		err.to_string().contains("BIP34") || err.to_string().contains("height"),
		"{err}"
	);
}

#[test]
fn bip34_not_enforced_before_activation() {
	// Height 50 with wrong coinbase height encoding -- should pass
	let block = make_block(1, 2000, 999, 50_0000_0000);
	assert!(check_block_context(&block, 50, 1000, 2100).is_ok());
}

#[test]
fn reject_bip34_height_encoding_too_long() {
	// Craft a coinbase with a 5-byte height push (exceeds CScriptNum 4-byte limit)
	// This must not panic -- it should return an error
	let mut script_sig = vec![0x05, 0x6f, 0x00, 0x00, 0x00, 0x00]; // push 5 bytes
	script_sig.push(0x00); // padding to meet MIN_COINBASE_SCRIPTSIG
	let block = Block {
		header: BlockHeader {
			version: 2,
			prev_hash: Hash256::ZERO,
			merkle_root: Hash256::ZERO,
			timestamp: 2000,
			bits: 0x1e0f_fff0,
			nonce: 0,
		},
		transactions: vec![Transaction {
			version: 1,
			vin: vec![TxIn {
				prev_output: OutPoint::COINBASE,
				script_sig,
				sequence: 0xFFFF_FFFF,
			}],
			vout: vec![TxOut {
				value: 50_0000_0000,
				script_pubkey: vec![0x76, 0xa9],
			}],
			locktime: 0,
		}],
	};
	let err = check_block_context(&block, 111, 1000, 2100).unwrap_err();
	assert!(err.to_string().contains("BIP34"), "{err}");
}

#[test]
fn reject_coinbase_output_exceeds_sanity_cap() {
	let block = make_block(
		2,
		2000,
		111,
		ironcat::validation::subsidy::MAX_MONEY + ironcat::validation::subsidy::get_block_subsidy(111) + 1,
	);
	let err = check_block_context(&block, 111, 1000, 2100).unwrap_err();
	assert!(err.to_string().contains("coinbase"), "{err}");
}
