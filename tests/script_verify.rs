// SPDX-License-Identifier: Apache-2.0

#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]

mod common;

use common::{gen_keypair, make_tx, pubkey_hash, sign_tx};
use ironcat::script::verify::verify_script;
use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};
use k256::ecdsa::SigningKey;
use sha2::Digest as Sha2Digest;

#[test]
fn verify_valid_p2pkh() {
	let (sk, pk) = gen_keypair(&[0x42; 32]);
	let pkh = pubkey_hash(&pk);

	let mut script_pubkey = vec![0x76, 0xa9, 0x14]; // OP_DUP OP_HASH160 PUSH(20)
	script_pubkey.extend_from_slice(&pkh);
	script_pubkey.push(0x88); // OP_EQUALVERIFY
	script_pubkey.push(0xac); // OP_CHECKSIG

	let tx = make_tx();
	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);
	script_sig.push(pk.len() as u8);
	script_sig.extend_from_slice(&pk);

	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}

#[test]
fn verify_invalid_p2pkh_bad_sig() {
	let (_sk, pk) = gen_keypair(&[0x42; 32]);
	let pkh = pubkey_hash(&pk);

	let mut script_pubkey = vec![0x76, 0xa9, 0x14];
	script_pubkey.extend_from_slice(&pkh);
	script_pubkey.push(0x88);
	script_pubkey.push(0xac);

	let tx = make_tx();
	// Sign with wrong key
	let wrong_sk = SigningKey::from_bytes(&[0x99; 32].into()).unwrap();
	let sig = sign_tx(&wrong_sk, &tx, 0, &script_pubkey);

	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);
	script_sig.push(pk.len() as u8); // correct pubkey but wrong sig
	script_sig.extend_from_slice(&pk);

	// Should fail: CHECKSIG returns false, then script ends with false on stack
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_err());
}

#[test]
fn verify_valid_p2pk() {
	let (sk, pk) = gen_keypair(&[0x42; 32]);

	let mut script_pubkey = vec![pk.len() as u8];
	script_pubkey.extend_from_slice(&pk);
	script_pubkey.push(0xac); // OP_CHECKSIG

	let tx = make_tx();
	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);

	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}

#[test]
fn verify_op_return_unspendable() {
	let tx = make_tx();
	let script_sig = vec![];
	let script_pubkey = vec![0x6a, 0x04, 0xde, 0xad, 0xbe, 0xef];
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_err());
}

#[test]
fn verify_p2sh_simple() {
	// P2SH wrapping OP_1 (always true)
	let redeem_script = vec![0x51]; // OP_1
	let redeem_hash = {
		let sha = sha2::Sha256::digest(&redeem_script);
		ripemd::Ripemd160::digest(sha)
	};

	let mut script_pubkey = vec![0xa9, 0x14]; // OP_HASH160 PUSH(20)
	script_pubkey.extend_from_slice(&redeem_hash);
	script_pubkey.push(0x87); // OP_EQUAL

	let mut script_sig = vec![redeem_script.len() as u8];
	script_sig.extend_from_slice(&redeem_script);

	let tx = make_tx();
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}

#[test]
fn verify_p2sh_with_p2pkh_redeem() {
	// P2SH wrapping a P2PKH script
	let (sk, pk) = gen_keypair(&[0x42; 32]);
	let pkh = pubkey_hash(&pk);

	// Redeem script = P2PKH
	let mut redeem_script = vec![0x76, 0xa9, 0x14];
	redeem_script.extend_from_slice(&pkh);
	redeem_script.push(0x88);
	redeem_script.push(0xac);

	let redeem_hash = {
		let sha = sha2::Sha256::digest(&redeem_script);
		ripemd::Ripemd160::digest(sha)
	};

	let mut script_pubkey = vec![0xa9, 0x14];
	script_pubkey.extend_from_slice(&redeem_hash);
	script_pubkey.push(0x87);

	let tx = make_tx();
	// For P2SH, the sighash uses the redeem script as script_code
	let sig = sign_tx(&sk, &tx, 0, &redeem_script);

	// scriptSig: <sig> <pubkey> <redeem_script>
	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);
	script_sig.push(pk.len() as u8);
	script_sig.extend_from_slice(&pk);
	// Push redeem script using appropriate push opcode
	if redeem_script.len() >= 76 {
		script_sig.push(0x4c); // PUSHDATA1
	}
	script_sig.push(redeem_script.len() as u8);
	script_sig.extend_from_slice(&redeem_script);

	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}

#[test]
fn verify_p2sh_wrong_hash_fails() {
	let redeem_script = vec![0x51]; // OP_1
	// Use wrong hash
	let wrong_hash = [0xab; 20];

	let mut script_pubkey = vec![0xa9, 0x14];
	script_pubkey.extend_from_slice(&wrong_hash);
	script_pubkey.push(0x87);

	let mut script_sig = vec![redeem_script.len() as u8];
	script_sig.extend_from_slice(&redeem_script);

	let tx = make_tx();
	// OP_HASH160 won't match, OP_EQUAL returns false
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_err());
}

#[test]
fn verify_p2sh_redeem_fails() {
	// P2SH where hash matches but redeem script evaluates to false
	let redeem_script = vec![0x00]; // OP_0 (pushes false)
	let redeem_hash = {
		let sha = sha2::Sha256::digest(&redeem_script);
		ripemd::Ripemd160::digest(sha)
	};

	let mut script_pubkey = vec![0xa9, 0x14];
	script_pubkey.extend_from_slice(&redeem_hash);
	script_pubkey.push(0x87);

	let mut script_sig = vec![redeem_script.len() as u8];
	script_sig.extend_from_slice(&redeem_script);

	let tx = make_tx();
	// Hash matches (OP_EQUAL passes) but redeem script pushes false
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_err());
}

#[test]
fn verify_empty_scripts() {
	let tx = make_tx();
	// Empty scriptSig + empty scriptPubKey = empty stack = FalseReturned
	assert!(verify_script(&[], &[], &tx, 0).is_err());
}

#[test]
fn p2sh_rejects_non_push_only_scriptsig() {
	// Redeem script that always succeeds
	let redeem_script = vec![0x51]; // OP_1
	let redeem_hash = {
		let sha = sha2::Sha256::digest(&redeem_script);
		ripemd::Ripemd160::digest(sha)
	};

	let mut script_pubkey = vec![0xa9, 0x14]; // OP_HASH160 PUSH(20)
	script_pubkey.extend_from_slice(&redeem_hash);
	script_pubkey.push(0x87); // OP_EQUAL

	// scriptSig with OP_DUP (non-push opcode) before the redeem script push
	let mut script_sig = vec![0x76]; // OP_DUP (not a push op!)
	script_sig.push(redeem_script.len() as u8);
	script_sig.extend_from_slice(&redeem_script);

	let tx = make_tx();
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_err());
}

#[test]
fn block_2547_tx_verifies() {
	// Real transaction from Catcoin block 2547 that was failing verification
	// TX: 44438840c2fa450e8e6ce4a283378440fb72d9da1abb1c98c22a2910cb352601
	// Spends output 0 of: a5545461a8a868ea1da38b49603b49c2d8b1d6cc5f886bd6e3a9c2c941bebcf1

	// Previous output's scriptPubKey (P2PKH)
	let script_pubkey = [
		0x76, 0xa9, 0x14, // OP_DUP OP_HASH160 PUSH(20)
		0x8a, 0x40, 0x71, 0x37, 0xf1, 0xe7, 0x89, 0x22, 0x7e, 0x61, 0xb2, 0xbc, 0xd3, 0x40, 0xea, 0x27, 0x78, 0xf0,
		0xe3, 0xaa, // pubkey hash
		0x88, 0xac, // OP_EQUALVERIFY OP_CHECKSIG
	];

	// scriptSig from the spending input
	let script_sig: Vec<u8> = vec![
		0x49, // push 73 bytes (DER sig + hashtype)
		0x30, 0x46, 0x02, 0x21, 0x00, 0xda, 0x20, 0xbb, 0xa9, 0x74, 0xbd, 0x19, 0xcc, 0x74, 0x61, 0x14, 0x83, 0x89,
		0xfc, 0xa5, 0xf0, 0xfb, 0x34, 0x38, 0x55, 0x93, 0x87, 0x21, 0xc4, 0x56, 0x49, 0xc3, 0x6e, 0x7a, 0xb8, 0x62,
		0x38, 0x02, 0x21, 0x00, 0xc0, 0xd8, 0x79, 0x87, 0x08, 0xa0, 0xfa, 0xc8, 0xef, 0x06, 0x70, 0xcf, 0x1a, 0x52,
		0x9b, 0x45, 0xfd, 0x29, 0x86, 0x24, 0xfb, 0xdc, 0x6e, 0x95, 0xd0, 0x7b, 0xe9, 0xac, 0xa5, 0xf8, 0x6e, 0x82,
		0x01, // SIGHASH_ALL
		0x21, // push 33 bytes (compressed pubkey)
		0x03, 0xfb, 0xab, 0xe9, 0x73, 0x20, 0x46, 0x48, 0xdb, 0x6e, 0xf5, 0xc7, 0xdd, 0xc3, 0x9f, 0x1a, 0x96, 0x9a,
		0x79, 0xb6, 0x99, 0x9c, 0x05, 0x2f, 0xd7, 0x02, 0x83, 0x82, 0x03, 0x3a, 0x63, 0xb5, 0xd3,
	];

	// Construct the spending transaction with exact values
	// txid a5545461...bcf1 in display (big-endian) -> reverse for wire order
	let mut prev_txid = [0u8; 32];
	let prev_txid_hex = "a5545461a8a868ea1da38b49603b49c2d8b1d6cc5f886bd6e3a9c2c941bebcf1";
	for (i, chunk) in prev_txid_hex.as_bytes().chunks(2).enumerate() {
		prev_txid[31 - i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
	}

	let tx = Transaction {
		version: 1,
		vin: vec![TxIn {
			prev_output: OutPoint {
				txid: Hash256::from_bytes(prev_txid),
				index: 0,
			},
			script_sig: script_sig.clone(),
			sequence: 0xFFFF_FFFF,
		}],
		vout: vec![
			TxOut {
				value: 907_593_428, // 9.07593428 CAT
				script_pubkey: vec![
					0x76, 0xa9, 0x14, 0xa0, 0x4c, 0x72, 0x2c, 0xd8, 0xdb, 0x37, 0xe3, 0xcd, 0x40, 0xfd, 0x34, 0xeb,
					0xdc, 0x64, 0xf8, 0x9c, 0x6d, 0xa7, 0xff, 0x88, 0xac,
				],
			},
			TxOut {
				value: 4_092_406_572, // 40.92406572 CAT
				script_pubkey: vec![
					0x76, 0xa9, 0x14, 0x3d, 0xf3, 0x1f, 0x90, 0x68, 0xc9, 0x6d, 0xf0, 0xc2, 0xfc, 0xc3, 0x64, 0xa6,
					0xd3, 0xe7, 0xbe, 0xea, 0x76, 0xf4, 0xa9, 0x88, 0xac,
				],
			},
		],
		locktime: 0,
	};

	// Verify txid matches to confirm correct tx construction
	assert_eq!(
		format!("{}", tx.txid()),
		"44438840c2fa450e8e6ce4a283378440fb72d9da1abb1c98c22a2910cb352601"
	);

	let result = verify_script(&script_sig, &script_pubkey, &tx, 0);
	assert!(result.is_ok(), "block 2547 tx verification failed: {result:?}");
}

#[test]
fn p2sh_push_only_allows_op_n() {
	// P2SH scriptSig using OP_0 (valid push) should work
	// redeem script = OP_1 (always true)
	let redeem_script = vec![0x51]; // OP_1
	let redeem_hash = {
		let sha = sha2::Sha256::digest(&redeem_script);
		ripemd::Ripemd160::digest(sha)
	};
	let mut script_pubkey = vec![0xa9, 0x14];
	script_pubkey.extend_from_slice(&redeem_hash);
	script_pubkey.push(0x87);

	// scriptSig: OP_0 OP_PUSH(1) <redeem>
	// OP_0 is push-only, raw push is push-only
	let mut script_sig = vec![0x00]; // OP_0 (push empty)
	script_sig.push(redeem_script.len() as u8);
	script_sig.extend_from_slice(&redeem_script);

	let tx = make_tx();
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}
