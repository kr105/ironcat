// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::{
	hash::Hash256,
	transaction::{OutPoint, Transaction, TxIn, TxOut},
};

const fn sample_outpoint() -> OutPoint {
	OutPoint {
		txid: Hash256::from_bytes([0xAA; 32]),
		index: 7,
	}
}

fn sample_txin() -> TxIn {
	TxIn {
		prev_output: sample_outpoint(),
		script_sig: vec![0x48, 0x30, 0x45, 0x02, 0x21],
		sequence: 0xFFFF_FFFF,
	}
}

fn sample_txout() -> TxOut {
	TxOut {
		value: 50_0000_0000, // 50 CAT
		script_pubkey: vec![0x76, 0xa9, 0x14, 0x01, 0x02, 0x03],
	}
}

#[test]
fn outpoint_roundtrip() {
	let op = sample_outpoint();
	let bytes = op.to_bytes();
	assert_eq!(bytes.len(), 36);

	let mut cursor = std::io::Cursor::new(bytes.as_slice());
	let decoded = OutPoint::from_cursor(&mut cursor).unwrap();
	assert_eq!(decoded.txid, op.txid);
	assert_eq!(decoded.index, op.index);
}

#[test]
fn outpoint_coinbase() {
	let cb = OutPoint::COINBASE;
	assert_eq!(cb.txid, Hash256::ZERO);
	assert_eq!(cb.index, 0xFFFF_FFFF);
}

#[test]
fn txin_roundtrip() {
	let txin = sample_txin();
	let bytes = txin.to_bytes();

	let mut cursor = std::io::Cursor::new(bytes.as_slice());
	let decoded = TxIn::from_cursor(&mut cursor).unwrap();
	assert_eq!(decoded.prev_output.txid, txin.prev_output.txid);
	assert_eq!(decoded.prev_output.index, txin.prev_output.index);
	assert_eq!(decoded.script_sig, txin.script_sig);
	assert_eq!(decoded.sequence, txin.sequence);
}

#[test]
fn txout_roundtrip() {
	let txout = sample_txout();
	let bytes = txout.to_bytes();

	let mut cursor = std::io::Cursor::new(bytes.as_slice());
	let decoded = TxOut::from_cursor(&mut cursor).unwrap();
	assert_eq!(decoded.value, txout.value);
	assert_eq!(decoded.script_pubkey, txout.script_pubkey);
}

#[test]
fn transaction_roundtrip() {
	let tx = Transaction {
		version: 1,
		vin: vec![sample_txin()],
		vout: vec![sample_txout(), sample_txout()],
		locktime: 0,
	};

	let bytes = tx.to_bytes();
	let decoded = Transaction::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.version, 1);
	assert_eq!(decoded.vin.len(), 1);
	assert_eq!(decoded.vout.len(), 2);
	assert_eq!(decoded.locktime, 0);
	assert_eq!(decoded.vin[0].script_sig, tx.vin[0].script_sig);
	assert_eq!(decoded.vout[0].value, tx.vout[0].value);
	assert_eq!(decoded.vout[1].script_pubkey, tx.vout[1].script_pubkey);
}

#[test]
fn transaction_txid_deterministic() {
	let tx = Transaction {
		version: 1,
		vin: vec![sample_txin()],
		vout: vec![sample_txout()],
		locktime: 0,
	};
	let id1 = tx.txid();
	let id2 = tx.txid();
	assert_eq!(id1, id2);
}

#[test]
fn transaction_rejects_oversized_script() {
	// Build a txin with a script > 10KB
	let mut bytes = Vec::new();
	// version
	bytes.extend_from_slice(&1i32.to_le_bytes());
	// vin count = 1
	bytes.push(1);
	// outpoint (36 bytes)
	bytes.extend_from_slice(&[0u8; 36]);
	// script length = 10001 (varint: 0xFD followed by u16 LE)
	bytes.push(0xFD);
	bytes.extend_from_slice(&10001u16.to_le_bytes());
	// script bytes (10001 zeros)
	bytes.extend_from_slice(&vec![0u8; 10001]);
	// sequence
	bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
	// vout count = 0
	bytes.push(0);
	// locktime
	bytes.extend_from_slice(&0u32.to_le_bytes());

	let result = Transaction::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn transaction_rejects_oversized_bytes() {
	// from_bytes rejects input larger than MAX_TX_SIZE (1MB)
	let bytes = vec![0u8; 1_000_001];
	let result = Transaction::from_bytes(&bytes);
	assert!(result.is_err());
}

#[test]
fn transaction_empty_inputs() {
	// Zero vin is valid at the deserialization layer (consensus validates later)
	let tx = Transaction {
		version: 1,
		vin: vec![],
		vout: vec![sample_txout()],
		locktime: 0,
	};
	let bytes = tx.to_bytes();
	let decoded = Transaction::from_bytes(&bytes).unwrap();
	assert!(decoded.vin.is_empty());
	assert_eq!(decoded.vout.len(), 1);
}
