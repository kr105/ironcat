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
	// The redeem script is just OP_1 which pushes true regardless
	assert!(verify_script(&script_sig, &script_pubkey, &tx, 0).is_ok());
}
