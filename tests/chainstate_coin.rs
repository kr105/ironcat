// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use ironcat::chainstate::coin::Coin;
use ironcat::types::transaction::TxOut;

const fn make_coin(value: i64, script: Vec<u8>, height: u32, is_coinbase: bool) -> Coin {
	Coin {
		tx_out: TxOut {
			value,
			script_pubkey: script,
		},
		height,
		is_coinbase,
	}
}

#[test]
fn roundtrip_coinbase() {
	let coin = make_coin(50_0000_0000, vec![0x76, 0xa9, 0x14, 0x01, 0x02], 100, true);
	let bytes = coin.to_bytes();
	let decoded = Coin::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.height, 100);
	assert!(decoded.is_coinbase);
	assert_eq!(decoded.tx_out.value, 50_0000_0000);
	assert_eq!(decoded.tx_out.script_pubkey, vec![0x76, 0xa9, 0x14, 0x01, 0x02]);
}

#[test]
fn roundtrip_non_coinbase() {
	let coin = make_coin(1_0000_0000, vec![0x76, 0xa9], 42_000, false);
	let bytes = coin.to_bytes();
	let decoded = Coin::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.height, 42_000);
	assert!(!decoded.is_coinbase);
	assert_eq!(decoded.tx_out.value, 1_0000_0000);
	assert_eq!(decoded.tx_out.script_pubkey, vec![0x76, 0xa9]);
}

#[test]
fn roundtrip_empty_script() {
	let coin = make_coin(0, vec![], 0, false);
	let bytes = coin.to_bytes();
	let decoded = Coin::from_bytes(&bytes).unwrap();

	assert_eq!(decoded.height, 0);
	assert!(!decoded.is_coinbase);
	assert_eq!(decoded.tx_out.value, 0);
	assert!(decoded.tx_out.script_pubkey.is_empty());
}

#[test]
fn too_short_data_returns_error() {
	// Minimum valid coin is 17 bytes (4 + 1 + 8 + 4 + 0 script)
	let short = vec![0u8; 5];
	assert!(Coin::from_bytes(&short).is_err());
}

#[test]
fn truncated_script_returns_error() {
	// Build valid header but claim script is 100 bytes while providing none
	let mut data = Vec::new();
	data.extend_from_slice(&42u32.to_le_bytes()); // height
	data.push(0); // is_coinbase
	data.extend_from_slice(&1000i64.to_le_bytes()); // value
	data.extend_from_slice(&100u32.to_le_bytes()); // script_len = 100
												// No script bytes follow

	assert!(Coin::from_bytes(&data).is_err());
}

#[test]
fn serialized_size_matches_expected() {
	let script = vec![0xAA; 25];
	let coin = make_coin(999, script, 500, true);
	let bytes = coin.to_bytes();

	// 4 (height) + 1 (is_coinbase) + 8 (value) + 4 (script_len) + 25 (script) = 42
	assert_eq!(bytes.len(), 42);
}
