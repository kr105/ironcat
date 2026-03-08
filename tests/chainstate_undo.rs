// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::chainstate::coin::Coin;
use ironcat::chainstate::undo::{BlockUndo, TxUndo};
use ironcat::types::transaction::TxOut;

fn make_coin(value: i64, height: u32, is_coinbase: bool) -> Coin {
	Coin {
		tx_out: TxOut {
			value,
			script_pubkey: vec![0x76, 0xa9],
		},
		height,
		is_coinbase,
	}
}

#[test]
fn tx_undos_roundtrip() {
	let undo = TxUndo {
		spent_outputs: vec![make_coin(50_0000_0000, 100, true), make_coin(1_0000_0000, 200, false)],
	};

	let bytes = undo.to_bytes();
	let decoded = TxUndo::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.spent_outputs.len(), 2);
	assert_eq!(decoded.spent_outputs[0].tx_out.value, 50_0000_0000);
	assert_eq!(decoded.spent_outputs[0].height, 100);
	assert!(decoded.spent_outputs[0].is_coinbase);
	assert_eq!(decoded.spent_outputs[1].tx_out.value, 1_0000_0000);
	assert_eq!(decoded.spent_outputs[1].height, 200);
	assert!(!decoded.spent_outputs[1].is_coinbase);
}

#[test]
fn tx_undos_empty_spent_outputs() {
	let undo = TxUndo { spent_outputs: vec![] };

	let bytes = undo.to_bytes();
	let decoded = TxUndo::from_bytes(&bytes).unwrap();

	assert!(decoded.spent_outputs.is_empty());
}

#[test]
fn block_undo_roundtrip() {
	let block_undo = BlockUndo {
		tx_undos: vec![
			TxUndo {
				spent_outputs: vec![make_coin(10_0000_0000, 50, false)],
			},
			TxUndo {
				spent_outputs: vec![make_coin(5_0000_0000, 60, true), make_coin(3_0000_0000, 70, false)],
			},
			TxUndo {
				spent_outputs: vec![make_coin(1_0000_0000, 80, false)],
			},
		],
	};

	let bytes = block_undo.to_bytes();
	let decoded = BlockUndo::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.tx_undos.len(), 3);
	assert_eq!(decoded.tx_undos[0].spent_outputs.len(), 1);
	assert_eq!(decoded.tx_undos[0].spent_outputs[0].tx_out.value, 10_0000_0000);
	assert_eq!(decoded.tx_undos[1].spent_outputs.len(), 2);
	assert_eq!(decoded.tx_undos[1].spent_outputs[0].height, 60);
	assert!(decoded.tx_undos[1].spent_outputs[0].is_coinbase);
	assert_eq!(decoded.tx_undos[2].spent_outputs[0].tx_out.value, 1_0000_0000);
}

#[test]
fn block_undo_empty() {
	let block_undo = BlockUndo { tx_undos: vec![] };

	let bytes = block_undo.to_bytes();
	let decoded = BlockUndo::from_bytes(&bytes).unwrap();

	assert!(decoded.tx_undos.is_empty());
}

#[test]
fn block_undo_truncated_returns_error() {
	// Valid block undo header claiming 5 tx entries but no data
	let mut data = Vec::new();
	data.extend_from_slice(&5u32.to_le_bytes());
	assert!(BlockUndo::from_bytes(&data).is_err());
}

#[test]
fn tx_undos_truncated_returns_error() {
	// Valid tx undo header claiming 3 coins but no data
	let mut data = Vec::new();
	data.extend_from_slice(&3u32.to_le_bytes());
	assert!(TxUndo::from_bytes(&data).is_err());
}
