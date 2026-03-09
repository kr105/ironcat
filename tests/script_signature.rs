// SPDX-License-Identifier: Apache-2.0

#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]

use ironcat::script::signature::{
	check_low_s, check_pubkey_encoding, check_signature_encoding, find_and_delete, signature_hash, verify_ecdsa,
	SigHashType,
};

const SIGHASH_ALL: u8 = 0x01;
const SIGHASH_NONE: u8 = 0x02;
const SIGHASH_SINGLE: u8 = 0x03;
const SIGHASH_ALL_ANYONECANPAY: u8 = 0x81;
use ironcat::script::ScriptError;
use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};

fn make_test_tx() -> Transaction {
	Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint {
				txid: Hash256::from_bytes([0xaa; 32]),
				index: 0,
			},
			script_sig: vec![],
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![TxOut {
			value: 50_0000_0000,
			script_pubkey: vec![0x76, 0xa9],
		}],
		locktime: 0,
	}
}

#[test]
fn sighash_all_deterministic() {
	let tx = make_test_tx();
	let script_code = vec![0x76, 0xa9, 0x14];
	let hash1 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	let hash2 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	assert_eq!(hash1, hash2);
	assert_ne!(hash1, Hash256::ZERO);
}

#[test]
fn sighash_none_ignores_outputs() {
	let mut tx = make_test_tx();
	let script_code = vec![0x76, 0xa9, 0x14];
	let hash_none = signature_hash(&tx, 0, &script_code, SIGHASH_NONE);
	let hash_all = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	// NONE and ALL must produce different hashes for the same tx
	assert_ne!(hash_none, hash_all);

	tx.vout[0].value = 1;
	let hash_none2 = signature_hash(&tx, 0, &script_code, SIGHASH_NONE);
	// NONE ignores outputs, so changing output value doesn't change hash
	assert_eq!(hash_none, hash_none2);
}

#[test]
fn sighash_all_depends_on_outputs() {
	let mut tx = make_test_tx();
	let script_code = vec![0x76, 0xa9, 0x14];
	let hash1 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	tx.vout[0].value = 1;
	let hash2 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	assert_ne!(hash1, hash2);
}

#[test]
fn sighash_single_bug() {
	let tx = Transaction {
		version: 1,
		vin: vec![
			TxIn {
				prev_output: OutPoint {
					txid: Hash256::from_bytes([0xaa; 32]),
					index: 0,
				},
				script_sig: vec![],
				sequence: 0xFFFF_FFFF,
			},
			TxIn {
				prev_output: OutPoint {
					txid: Hash256::from_bytes([0xbb; 32]),
					index: 0,
				},
				script_sig: vec![],
				sequence: 0xFFFF_FFFF,
			},
		],
		vout: vec![TxOut {
			value: 100,
			script_pubkey: vec![],
		}],
		locktime: 0,
	};
	let script_code = vec![0x76];
	let hash = signature_hash(&tx, 1, &script_code, SIGHASH_SINGLE);
	let mut expected = [0u8; 32];
	expected[0] = 0x01;
	assert_eq!(hash, Hash256::from_bytes(expected));
}

#[test]
fn sighash_anyonecanpay_differs_from_all() {
	let tx = Transaction {
		version: 1,
		vin: vec![
			TxIn {
				prev_output: OutPoint {
					txid: Hash256::from_bytes([0xaa; 32]),
					index: 0,
				},
				script_sig: vec![],
				sequence: 0xFFFF_FFFF,
			},
			TxIn {
				prev_output: OutPoint {
					txid: Hash256::from_bytes([0xbb; 32]),
					index: 0,
				},
				script_sig: vec![],
				sequence: 0xFFFF_FFFF,
			},
		],
		vout: vec![TxOut {
			value: 100,
			script_pubkey: vec![],
		}],
		locktime: 0,
	};
	let script_code = vec![0x76];
	let hash1 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL_ANYONECANPAY);
	let hash2 = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	assert_ne!(hash1, hash2);
}

#[test]
fn sighash_all_depends_on_script_code() {
	let tx = make_test_tx();
	let hash1 = signature_hash(&tx, 0, &[0x76, 0xa9], SIGHASH_ALL);
	let hash2 = signature_hash(&tx, 0, &[0x76, 0xaa], SIGHASH_ALL);
	assert_ne!(hash1, hash2);
}

#[test]
fn sighash_single_normal() {
	let tx = make_test_tx();
	let script_code = vec![0x76];
	let hash = signature_hash(&tx, 0, &script_code, SIGHASH_SINGLE);
	assert_ne!(hash, Hash256::ZERO);
	// Single with matching output should differ from All
	let hash_all = signature_hash(&tx, 0, &script_code, SIGHASH_ALL);
	assert_ne!(hash, hash_all);
}

#[test]
fn sighash_type_from_byte() {
	assert_eq!(SigHashType::from_byte(0x01), Some(SigHashType::All));
	assert_eq!(SigHashType::from_byte(0x02), Some(SigHashType::None));
	assert_eq!(SigHashType::from_byte(0x03), Some(SigHashType::Single));
	assert_eq!(SigHashType::from_byte(0x81), Some(SigHashType::AllAnyoneCanPay));
	assert_eq!(SigHashType::from_byte(0x00), Option::None);
	assert_eq!(SigHashType::from_byte(0x04), Option::None);
}

#[test]
fn empty_signature_valid_encoding() {
	assert!(check_signature_encoding(&[]).is_ok());
}

#[test]
fn reject_too_short_signature() {
	let sig = vec![0x30, 0x01, 0x01];
	assert_eq!(
		check_signature_encoding(&sig).unwrap_err(),
		ScriptError::InvalidSignatureEncoding
	);
}

#[test]
fn reject_wrong_first_byte() {
	// Valid length but wrong marker
	let sig = vec![0x31, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01];
	assert_eq!(
		check_signature_encoding(&sig).unwrap_err(),
		ScriptError::InvalidSignatureEncoding
	);
}

#[test]
fn valid_minimal_der_signature() {
	// Minimal valid DER: 0x30 0x06 0x02 0x01 <R=1> 0x02 0x01 <S=1> <hashtype=0x01>
	let sig = vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01];
	assert!(check_signature_encoding(&sig).is_ok());
}

#[test]
fn reject_negative_r() {
	// R starts with 0x80 (negative) without 0x00 prefix
	let sig = vec![0x30, 0x06, 0x02, 0x01, 0x80, 0x02, 0x01, 0x01, 0x01];
	assert_eq!(
		check_signature_encoding(&sig).unwrap_err(),
		ScriptError::InvalidSignatureEncoding
	);
}

#[test]
fn reject_unnecessary_r_padding() {
	// R has 0x00 prefix but next byte doesn't have high bit -- unnecessary padding
	let sig = vec![0x30, 0x07, 0x02, 0x02, 0x00, 0x01, 0x02, 0x01, 0x01, 0x01];
	assert_eq!(
		check_signature_encoding(&sig).unwrap_err(),
		ScriptError::InvalidSignatureEncoding
	);
}

#[test]
fn valid_compressed_pubkey() {
	let mut pubkey = vec![0x02];
	pubkey.extend_from_slice(&[0xab; 32]);
	assert!(check_pubkey_encoding(&pubkey).is_ok());
}

#[test]
fn valid_uncompressed_pubkey() {
	let mut pubkey = vec![0x04];
	pubkey.extend_from_slice(&[0xab; 64]);
	assert!(check_pubkey_encoding(&pubkey).is_ok());
}

#[test]
fn invalid_pubkey_wrong_length() {
	let pubkey = vec![0x02; 20];
	assert_eq!(check_pubkey_encoding(&pubkey).unwrap_err(), ScriptError::InvalidPubKey);
}

#[test]
fn invalid_pubkey_empty() {
	assert_eq!(check_pubkey_encoding(&[]).unwrap_err(), ScriptError::InvalidPubKey);
}

#[test]
fn invalid_pubkey_wrong_prefix() {
	let mut pubkey = vec![0x05];
	pubkey.extend_from_slice(&[0xab; 32]);
	assert_eq!(check_pubkey_encoding(&pubkey).unwrap_err(), ScriptError::InvalidPubKey);
}

#[test]
fn verify_ecdsa_with_real_key() {
	use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};

	// Generate a key and sign a prehashed message (Bitcoin signs raw 32-byte hashes)
	let signing_key = SigningKey::from_bytes(&[0x01; 32].into()).unwrap();
	let verifying_key = signing_key.verifying_key();
	let pubkey = verifying_key.to_sec1_bytes();

	let prehash = [0xab; 32];
	let sig: k256::ecdsa::Signature = signing_key.sign_prehash(&prehash).unwrap();
	let mut sig_der = sig.to_der().to_bytes().to_vec();
	sig_der.push(0x01); // SIGHASH_ALL

	let sighash = Hash256::from_bytes(prehash);
	assert!(verify_ecdsa(&sig_der, &pubkey, &sighash));
}

#[test]
fn verify_ecdsa_wrong_message() {
	use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};

	let signing_key = SigningKey::from_bytes(&[0x01; 32].into()).unwrap();
	let verifying_key = signing_key.verifying_key();
	let pubkey = verifying_key.to_sec1_bytes();

	let prehash = [0xab; 32];
	let sig: k256::ecdsa::Signature = signing_key.sign_prehash(&prehash).unwrap();
	let mut sig_der = sig.to_der().to_bytes().to_vec();
	sig_der.push(0x01);

	let wrong_hash = Hash256::from_bytes([0xcd; 32]);
	assert!(!verify_ecdsa(&sig_der, &pubkey, &wrong_hash));
}

#[test]
fn verify_ecdsa_wrong_key() {
	use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};

	let signing_key = SigningKey::from_bytes(&[0x01; 32].into()).unwrap();
	let wrong_key = SigningKey::from_bytes(&[0x02; 32].into()).unwrap();
	let wrong_pub = wrong_key.verifying_key().to_sec1_bytes();

	let prehash = [0xab; 32];
	let sig: k256::ecdsa::Signature = signing_key.sign_prehash(&prehash).unwrap();
	let mut sig_der = sig.to_der().to_bytes().to_vec();
	sig_der.push(0x01);

	let sighash = Hash256::from_bytes(prehash);
	assert!(!verify_ecdsa(&sig_der, &wrong_pub, &sighash));
}

#[test]
fn verify_ecdsa_empty_sig_returns_false() {
	let pubkey = vec![0x02; 33];
	let sighash = Hash256::from_bytes([0xab; 32]);
	assert!(!verify_ecdsa(&[], &pubkey, &sighash));
}

#[test]
fn check_low_s_empty_is_ok() {
	assert!(check_low_s(&[]).is_ok());
}

#[test]
fn check_low_s_valid_small_s() {
	// Minimal valid DER with S=1, which is well below half order
	let sig = vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01, 0x01];
	assert!(check_low_s(&sig).is_ok());
}

#[test]
fn check_low_s_rejects_high_s() {
	// Construct a DER sig where S equals the full order (which is > half order)
	// S = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141
	let s_bytes: Vec<u8> = vec![
		0x00, // leading zero to keep it positive in DER
		0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, 0xBA, 0xAE,
		0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x41,
	];
	let r_bytes: Vec<u8> = vec![0x01];
	let total_len = 2 + r_bytes.len() + 2 + s_bytes.len();
	let mut sig = vec![0x30, total_len as u8];
	sig.push(0x02);
	sig.push(r_bytes.len() as u8);
	sig.extend_from_slice(&r_bytes);
	sig.push(0x02);
	sig.push(s_bytes.len() as u8);
	sig.extend_from_slice(&s_bytes);
	sig.push(0x01); // hashtype
	assert_eq!(check_low_s(&sig).unwrap_err(), ScriptError::InvalidSignatureEncoding);
}

#[test]
fn find_and_delete_removes_direct_push() {
	let data = vec![0xaa, 0xbb];
	// Script contains: [0x02 0xaa 0xbb] (push 2 bytes) then OP_DUP (0x76)
	let script = vec![0x02, 0xaa, 0xbb, 0x76];
	let result = find_and_delete(&script, &data);
	assert_eq!(result, vec![0x76]);
}

#[test]
fn find_and_delete_noop_for_empty_data() {
	let script = vec![0x02, 0xaa, 0xbb, 0x76];
	let result = find_and_delete(&script, &[]);
	assert_eq!(result, script);
}

#[test]
fn find_and_delete_noop_above_75_bytes() {
	// Data > 75 bytes should be returned unchanged (only direct push handled)
	let data = vec![0xaa; 76];
	let mut script = vec![0x4c, 76]; // PUSHDATA1 + length
	script.extend_from_slice(&data);
	let result = find_and_delete(&script, &data);
	assert_eq!(result, script);
}

#[test]
fn find_and_delete_at_75_byte_boundary() {
	// Exactly 75 bytes should be handled (direct push limit)
	let data = vec![0xcc; 75];
	let mut script = vec![75]; // direct push opcode
	script.extend_from_slice(&data);
	script.push(0x76); // OP_DUP after
	let result = find_and_delete(&script, &data);
	assert_eq!(result, vec![0x76]);
}
