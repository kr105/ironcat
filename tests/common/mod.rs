// SPDX-License-Identifier: Apache-2.0

#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation,
	dead_code
)]

use ironcat::script::signature::signature_hash;
use ironcat::types::hash::Hash256;
use ironcat::types::transaction::{OutPoint, Transaction, TxIn, TxOut};
use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use sha2::Digest;

/// Generate a keypair and return (signing key, compressed pubkey bytes)
pub fn gen_keypair(seed: &[u8; 32]) -> (SigningKey, Vec<u8>) {
	let sk = SigningKey::from_bytes(seed.into()).unwrap();
	let pk = sk.verifying_key().to_sec1_bytes().to_vec();
	(sk, pk)
}

/// Sign a sighash and return DER sig + hashtype byte
pub fn sign_tx(sk: &SigningKey, tx: &Transaction, input_idx: usize, script_code: &[u8]) -> Vec<u8> {
	let sighash = signature_hash(tx, input_idx, script_code, 0x01); // SIGHASH_ALL
	let sig: k256::ecdsa::Signature = sk.sign_prehash(sighash.as_bytes()).unwrap();
	// Explicitly normalize to low-S in case k256 defaults ever change
	let sig = sig.normalize_s().unwrap_or(sig);
	let mut der = sig.to_der().to_bytes().to_vec();
	der.push(0x01); // SIGHASH_ALL
	der
}

/// Compute HASH160(pubkey) = RIPEMD160(SHA256(pubkey))
pub fn pubkey_hash(pubkey: &[u8]) -> Vec<u8> {
	let sha = sha2::Sha256::digest(pubkey);
	ripemd::Ripemd160::digest(sha).to_vec()
}

/// Build a simple spending transaction with one input and one output
pub fn make_tx() -> Transaction {
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
			value: 49_0000_0000,
			script_pubkey: vec![],
		}],
		locktime: 0,
	}
}

/// Build a P2PKH scriptPubKey for a given public key
pub fn p2pkh_script_pubkey(pubkey: &[u8]) -> Vec<u8> {
	let pkh = pubkey_hash(pubkey);
	let mut s = vec![0x76, 0xa9, 0x14]; // OP_DUP OP_HASH160 PUSH(20)
	s.extend_from_slice(&pkh);
	s.push(0x88); // OP_EQUALVERIFY
	s.push(0xac); // OP_CHECKSIG
	s
}
