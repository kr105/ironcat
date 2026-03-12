// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};

#[test]
fn tx_roundtrip_serialization() {
	let tx = Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint {
				txid: Hash256::from_bytes([0xAA; 32]),
				index: 0,
			},
			script_sig: vec![0x01, 0x01],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_000,
			script_pubkey: vec![0x76, 0xa9, 0x14], // P2PKH prefix
		}],
		locktime: 0,
	};

	let bytes = tx.to_bytes();
	let decoded = Transaction::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.version, tx.version);
	assert_eq!(decoded.vin.len(), 1);
	assert_eq!(decoded.vout.len(), 1);
	assert_eq!(decoded.vout[0].value, 50_000);
	assert_eq!(decoded.locktime, 0);
	assert_eq!(decoded.txid(), tx.txid());
}
