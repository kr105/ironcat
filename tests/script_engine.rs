// SPDX-License-Identifier: Apache-2.0

#![allow(
	clippy::unwrap_used,
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]

mod common;

use common::{gen_keypair, make_tx, p2pkh_script_pubkey, sign_tx};
use ironcat::script::ScriptError;
use ironcat::script::engine::{Engine, SignatureContext};
use sha2::Digest;

#[test]
fn push_bytes_onto_stack() {
	let script = vec![0x03, 0xaa, 0xbb, 0xcc];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], vec![0xaa, 0xbb, 0xcc]);
}

#[test]
fn op_0_pushes_empty() {
	let script = vec![0x00];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn op_1_through_16() {
	for n in 1u8..=16 {
		let script = vec![0x50 + n];
		let mut engine = Engine::new(&script);
		let stack = engine.execute().unwrap();
		assert_eq!(stack.len(), 1);
		assert_eq!(stack[0], vec![n]);
	}
}

#[test]
fn op_1negate() {
	let script = vec![0x4f];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x81]);
}

#[test]
fn pushdata1() {
	// PUSHDATA1 with 2 bytes
	let script = vec![0x4c, 0x02, 0xaa, 0xbb];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0xaa, 0xbb]);
}

#[test]
fn pushdata1_non_minimal_accepted() {
	// PUSHDATA1 with 2 bytes is non-minimal but accepted without MINIMALDATA flag
	let script = vec![0x4c, 0x02, 0xaa, 0xbb];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0xaa, 0xbb]);
}

#[test]
fn pushdata2() {
	// PUSHDATA2 with 2 bytes
	let script = vec![0x4d, 0x02, 0x00, 0xaa, 0xbb];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0xaa, 0xbb]);
}

#[test]
fn pushdata2_non_minimal_accepted() {
	// PUSHDATA2 with 2 bytes is non-minimal but accepted without MINIMALDATA flag
	let script = vec![0x4d, 0x02, 0x00, 0xaa, 0xbb];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0xaa, 0xbb]);
}

#[test]
fn push_beyond_script_fails() {
	let script = vec![0x05, 0xaa, 0xbb];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::InvalidPushSize);
}

#[test]
fn op_dup() {
	let script = vec![0x01, 0xaa, 0x76];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
	assert_eq!(stack[0], vec![0xaa]);
	assert_eq!(stack[1], vec![0xaa]);
}

#[test]
fn op_drop() {
	let script = vec![0x51, 0x52, 0x75];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_swap() {
	let script = vec![0x51, 0x52, 0x7c];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
	assert_eq!(stack[1], vec![0x01]);
}

#[test]
fn op_equal_true() {
	let script = vec![0x51, 0x51, 0x87];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_equal_false() {
	let script = vec![0x51, 0x52, 0x87];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn stack_underflow() {
	let script = vec![0x76]; // OP_DUP on empty stack
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::StackUnderflow);
}

#[test]
fn disabled_opcode_errors() {
	let script = vec![0x51, 0x51, 0x7e]; // OP_CAT
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::DisabledOpcode(0x7e));
}

#[test]
fn too_many_ops() {
	let script = vec![0x61; 202]; // 202 OP_NOPs
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::TooManyOps);
}

#[test]
fn script_too_large() {
	let script = vec![0x61; 10_001];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::ScriptTooLarge);
}

#[test]
fn op_return_errors() {
	let script = vec![0x6a];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::OpReturnEncountered);
}

#[test]
fn op_over() {
	// OP_1 OP_2 OP_OVER -> [1, 2, 1]
	let script = vec![0x51, 0x52, 0x78];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 3);
	assert_eq!(stack[0], vec![0x01]);
	assert_eq!(stack[1], vec![0x02]);
	assert_eq!(stack[2], vec![0x01]);
}

#[test]
fn op_rot() {
	// OP_1 OP_2 OP_3 OP_ROT -> [2, 3, 1]
	let script = vec![0x51, 0x52, 0x53, 0x7b];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
	assert_eq!(stack[1], vec![0x03]);
	assert_eq!(stack[2], vec![0x01]);
}

#[test]
fn op_2dup() {
	// OP_1 OP_2 OP_2DUP -> [1, 2, 1, 2]
	let script = vec![0x51, 0x52, 0x6e];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 4);
	assert_eq!(stack[0], vec![0x01]);
	assert_eq!(stack[1], vec![0x02]);
	assert_eq!(stack[2], vec![0x01]);
	assert_eq!(stack[3], vec![0x02]);
}

#[test]
fn op_depth() {
	// OP_1 OP_2 OP_3 OP_DEPTH -> [1, 2, 3, 3]
	let script = vec![0x51, 0x52, 0x53, 0x74];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 4);
	assert_eq!(stack[3], vec![0x03]);
}

#[test]
fn op_size() {
	// OP_PUSH(3) 0xaa 0xbb 0xcc OP_SIZE -> [[aa,bb,cc], [3]]
	let script = vec![0x03, 0xaa, 0xbb, 0xcc, 0x82];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
	assert_eq!(stack[0], vec![0xaa, 0xbb, 0xcc]);
	assert_eq!(stack[1], vec![0x03]);
}

#[test]
fn op_toaltstack_fromaltstack() {
	// OP_1 OP_TOALTSTACK OP_2 OP_FROMALTSTACK -> [2, 1]
	let script = vec![0x51, 0x6b, 0x52, 0x6c];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
	assert_eq!(stack[0], vec![0x02]);
	assert_eq!(stack[1], vec![0x01]);
}

#[test]
fn op_verify_true() {
	// OP_1 OP_VERIFY -> empty stack, no error
	let script = vec![0x51, 0x69];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert!(stack.is_empty());
}

#[test]
fn op_verify_false() {
	// OP_0 OP_VERIFY -> FalseReturned
	let script = vec![0x00, 0x69];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::FalseReturned);
}

#[test]
fn op_equalverify_success() {
	// OP_1 OP_1 OP_EQUALVERIFY -> empty stack
	let script = vec![0x51, 0x51, 0x88];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert!(stack.is_empty());
}

#[test]
fn op_equalverify_failure() {
	// OP_1 OP_2 OP_EQUALVERIFY -> EqualVerifyFailed
	let script = vec![0x51, 0x52, 0x88];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::EqualVerifyFailed);
}

#[test]
fn op_nip() {
	// OP_1 OP_2 OP_NIP -> [2]
	let script = vec![0x51, 0x52, 0x77];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], vec![0x02]);
}

#[test]
fn op_tuck() {
	// OP_1 OP_2 OP_TUCK -> [2, 1, 2]
	// Stack bottom-to-top: [1, 2] -> [2, 1, 2]
	let script = vec![0x51, 0x52, 0x7d];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 3);
	assert_eq!(stack[0], vec![0x02]);
	assert_eq!(stack[1], vec![0x01]);
	assert_eq!(stack[2], vec![0x02]);
}

#[test]
fn op_pick() {
	// OP_1 OP_2 OP_3 OP_2 OP_PICK -> [1, 2, 3, 1] (pick index 2 = third from top)
	let script = vec![0x51, 0x52, 0x53, 0x52, 0x79];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 4);
	assert_eq!(stack[3], vec![0x01]);
}

#[test]
fn op_roll() {
	// OP_1 OP_2 OP_3 OP_2 OP_ROLL -> [2, 3, 1] (roll index 2 = move third from top to top)
	let script = vec![0x51, 0x52, 0x53, 0x52, 0x7a];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 3);
	assert_eq!(stack[0], vec![0x02]);
	assert_eq!(stack[1], vec![0x03]);
	assert_eq!(stack[2], vec![0x01]);
}

#[test]
fn op_ifdup_nonzero() {
	// OP_1 OP_IFDUP -> [1, 1]
	let script = vec![0x51, 0x73];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
}

#[test]
fn op_ifdup_zero() {
	// OP_0 OP_IFDUP -> [empty]
	let script = vec![0x00, 0x73];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
}

#[test]
fn with_stack_preloaded() {
	// Start with [0xaa] on stack, then OP_DUP
	let script = vec![0x76]; // OP_DUP
	let mut engine = Engine::with_stack(&script, vec![vec![0xaa]]);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
	assert_eq!(stack[0], vec![0xaa]);
	assert_eq!(stack[1], vec![0xaa]);
}

// --- Control flow tests ---

#[test]
fn op_if_true_branch() {
	// OP_1 OP_IF OP_2 OP_ELSE OP_3 OP_ENDIF
	let script = vec![0x51, 0x63, 0x52, 0x67, 0x53, 0x68];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
}

#[test]
fn op_if_false_branch() {
	// OP_0 OP_IF OP_2 OP_ELSE OP_3 OP_ENDIF
	let script = vec![0x00, 0x63, 0x52, 0x67, 0x53, 0x68];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x03]);
}

#[test]
fn op_notif() {
	// OP_0 OP_NOTIF OP_2 OP_ENDIF
	let script = vec![0x00, 0x64, 0x52, 0x68];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
}

#[test]
fn nested_if() {
	// OP_1 OP_IF OP_1 OP_IF OP_5 OP_ENDIF OP_ENDIF
	let script = vec![0x51, 0x63, 0x51, 0x63, 0x55, 0x68, 0x68];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x05]);
}

#[test]
fn unbalanced_if_fails() {
	let script = vec![0x51, 0x63]; // OP_1 OP_IF (no ENDIF)
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::UnbalancedConditional);
}

#[test]
fn unbalanced_else_fails() {
	let script = vec![0x67]; // OP_ELSE without IF
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::UnbalancedConditional);
}

#[test]
fn unbalanced_endif_fails() {
	let script = vec![0x68]; // OP_ENDIF without IF
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::UnbalancedConditional);
}

#[test]
fn op_if_skips_false_branch() {
	// OP_1 OP_IF OP_2 OP_ELSE OP_RETURN OP_ENDIF
	// Should NOT hit OP_RETURN because it's in the false branch
	let script = vec![0x51, 0x63, 0x52, 0x67, 0x6a, 0x68];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
}

// --- Arithmetic tests ---

#[test]
fn op_add() {
	// OP_2 OP_3 OP_ADD -> [5]
	let script = vec![0x52, 0x53, 0x93];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x05]);
}

#[test]
fn op_sub() {
	// OP_5 OP_3 OP_SUB -> [2]
	let script = vec![0x55, 0x53, 0x94];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x02]);
}

#[test]
fn op_sub_negative_result() {
	// OP_3 OP_5 OP_SUB -> [-2]
	let script = vec![0x53, 0x55, 0x94];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x82]); // -2 in script number: 0x02 | 0x80
}

#[test]
fn op_not_zero() {
	// OP_0 OP_NOT -> [1]
	let script = vec![0x00, 0x91];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_not_nonzero() {
	// OP_5 OP_NOT -> [] (0 = empty)
	let script = vec![0x55, 0x91];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn op_1add() {
	// OP_5 OP_1ADD -> [6]
	let script = vec![0x55, 0x8b];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x06]);
}

#[test]
fn op_negate() {
	// OP_5 OP_NEGATE -> [-5]
	let script = vec![0x55, 0x8f];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x85]); // -5 = 0x05 | 0x80
}

#[test]
fn op_abs_negative() {
	// Push -5 (0x85), OP_ABS -> [5]
	let script = vec![0x01, 0x85, 0x90];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x05]);
}

#[test]
fn op_within() {
	// OP_5 OP_3 OP_7 OP_WITHIN -> [1] (3 <= 5 < 7)
	let script = vec![0x55, 0x53, 0x57, 0xa5];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_within_outside() {
	// OP_2 OP_3 OP_7 OP_WITHIN -> [0] (2 < 3, so outside)
	let script = vec![0x52, 0x53, 0x57, 0xa5];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn op_booland() {
	// OP_1 OP_1 OP_BOOLAND -> [1]
	let script = vec![0x51, 0x51, 0x9a];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_booland_false() {
	// OP_1 OP_0 OP_BOOLAND -> [0]
	let script = vec![0x51, 0x00, 0x9a];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn op_min() {
	// OP_3 OP_5 OP_MIN -> [3]
	let script = vec![0x53, 0x55, 0xa3];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x03]);
}

#[test]
fn op_max() {
	// OP_3 OP_5 OP_MAX -> [5]
	let script = vec![0x53, 0x55, 0xa4];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x05]);
}

#[test]
fn op_numequal() {
	// OP_3 OP_3 OP_NUMEQUAL -> [1]
	let script = vec![0x53, 0x53, 0x9c];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_lessthan() {
	// OP_3 OP_5 OP_LESSTHAN -> [1]
	let script = vec![0x53, 0x55, 0x9f];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn arithmetic_overflow_input() {
	// Push a 5-byte number, then OP_1ADD should fail with NumericOverflow
	let script = vec![0x05, 0x01, 0x02, 0x03, 0x04, 0x05, 0x8b];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::NumericOverflow);
}

#[test]
fn op_numequalverify_success() {
	// OP_3 OP_3 OP_NUMEQUALVERIFY -> empty stack
	let script = vec![0x53, 0x53, 0x9d];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert!(stack.is_empty());
}

#[test]
fn op_numequalverify_failure() {
	// OP_3 OP_5 OP_NUMEQUALVERIFY -> FalseReturned
	let script = vec![0x53, 0x55, 0x9d];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::FalseReturned);
}

#[test]
fn op_0notequal() {
	// OP_5 OP_0NOTEQUAL -> [1]
	let script = vec![0x55, 0x92];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_0notequal_zero() {
	// OP_0 OP_0NOTEQUAL -> [0]
	let script = vec![0x00, 0x92];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

// --- Crypto hash tests ---

#[test]
fn op_sha256() {
	// OP_PUSH(5) "hello" OP_SHA256
	let mut script = vec![0x05];
	script.extend_from_slice(b"hello");
	script.push(0xa8);
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0].len(), 32);
	// Verify against known SHA256("hello")
	let expected = sha2::Sha256::digest(b"hello");
	assert_eq!(stack[0], expected.as_slice());
}

#[test]
fn op_hash256() {
	// OP_PUSH(5) "hello" OP_HASH256 (double SHA256)
	let mut script = vec![0x05];
	script.extend_from_slice(b"hello");
	script.push(0xaa);
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	let first = sha2::Sha256::digest(b"hello");
	let expected = sha2::Sha256::digest(first);
	assert_eq!(stack[0], expected.as_slice());
}

#[test]
fn op_hash160() {
	// OP_PUSH(5) "hello" OP_HASH160 (SHA256 then RIPEMD160)
	let mut script = vec![0x05];
	script.extend_from_slice(b"hello");
	script.push(0xa9);
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0].len(), 20);
	let sha = sha2::Sha256::digest(b"hello");
	let expected = ripemd::Ripemd160::digest(sha);
	assert_eq!(stack[0], expected.as_slice());
}

#[test]
fn op_ripemd160() {
	let mut script = vec![0x05];
	script.extend_from_slice(b"hello");
	script.push(0xa6);
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0].len(), 20);
	let expected = ripemd::Ripemd160::digest(b"hello");
	assert_eq!(stack[0], expected.as_slice());
}

#[test]
fn op_sha1() {
	let mut script = vec![0x05];
	script.extend_from_slice(b"hello");
	script.push(0xa7);
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0].len(), 20);
	let expected = sha1::Sha1::digest(b"hello");
	assert_eq!(stack[0], expected.as_slice());
}

#[test]
fn op_codeseparator() {
	// OP_1 OP_CODESEPARATOR OP_2 -> [1, 2] (no stack effect, just advances codesep_pos)
	let script = vec![0x51, 0xab, 0x52];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 2);
	assert_eq!(stack[0], vec![0x01]);
	assert_eq!(stack[1], vec![0x02]);
}

#[test]
fn hash_empty_data() {
	// OP_0 OP_SHA256 -> SHA256 of empty
	let script = vec![0x00, 0xa8];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	let expected = sha2::Sha256::digest(b"");
	assert_eq!(stack[0], expected.as_slice());
}

// --- OP_CHECKSIG tests ---

#[test]
fn op_checksig_valid_p2pkh() {
	let (sk, pk) = gen_keypair(&[0x42; 32]);
	let script_pubkey = p2pkh_script_pubkey(&pk);
	let tx = make_tx();

	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	// Build scriptSig: <sig> <pubkey>
	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);
	script_sig.push(pk.len() as u8);
	script_sig.extend_from_slice(&pk);

	// Execute scriptSig
	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	// Execute scriptPubKey with sig_stack
	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();

	// Should succeed (top of stack is truthy)
	assert!(!final_stack.is_empty());
	let top = final_stack.last().unwrap();
	assert!(!top.is_empty() && top.iter().any(|&b| b != 0));
}

#[test]
fn op_checksig_wrong_key_pushes_false() {
	// Without NULLFAIL, OP_CHECKSIG with a non-empty sig that fails ECDSA
	// verification pushes false (not an error)
	let (sk, _pk) = gen_keypair(&[0x42; 32]);
	let (_sk2, pk2) = gen_keypair(&[0x99; 32]);

	// P2PK script
	let mut script_pubkey = vec![pk2.len() as u8];
	script_pubkey.extend_from_slice(&pk2);
	script_pubkey.push(0xac); // OP_CHECKSIG

	let tx = make_tx();
	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();
	// Wrong key -> push false (empty vec)
	let top = final_stack.last().unwrap();
	assert!(top.is_empty(), "expected false on stack, got {top:?}");
}

#[test]
fn op_checksigverify_wrong_key_errors() {
	// OP_CHECKSIGVERIFY with wrong key should return error
	let (sk, _pk) = gen_keypair(&[0x42; 32]);
	let (_sk2, pk2) = gen_keypair(&[0x99; 32]);

	// scriptPubKey: <pk2> OP_CHECKSIGVERIFY OP_1
	let mut script_pubkey = vec![pk2.len() as u8];
	script_pubkey.extend_from_slice(&pk2);
	script_pubkey.push(0xad); // OP_CHECKSIGVERIFY

	let tx = make_tx();
	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	assert_eq!(engine2.execute().unwrap_err(), ScriptError::CheckSigFailed);
}

#[test]
fn op_checksig_p2pk() {
	// P2PK: scriptPubKey = <pubkey> OP_CHECKSIG
	let (sk, pk) = gen_keypair(&[0x42; 32]);
	let mut script_pubkey = vec![pk.len() as u8];
	script_pubkey.extend_from_slice(&pk);
	script_pubkey.push(0xac); // OP_CHECKSIG

	let tx = make_tx();
	let sig = sign_tx(&sk, &tx, 0, &script_pubkey);

	// scriptSig = <sig>
	let mut script_sig = vec![sig.len() as u8];
	script_sig.extend_from_slice(&sig);

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();

	let top = final_stack.last().unwrap();
	assert_eq!(*top, vec![1u8]);
}

// --- OP_CHECKMULTISIG tests ---

#[test]
fn op_checkmultisig_2of3() {
	let (sk1, pk1) = gen_keypair(&[0x01; 32]);
	let (sk2, pk2) = gen_keypair(&[0x02; 32]);
	let (_sk3, pk3) = gen_keypair(&[0x03; 32]);

	// scriptPubKey: OP_2 <pk1> <pk2> <pk3> OP_3 OP_CHECKMULTISIG
	let mut script_pubkey = vec![0x52]; // OP_2
	script_pubkey.push(pk1.len() as u8);
	script_pubkey.extend_from_slice(&pk1);
	script_pubkey.push(pk2.len() as u8);
	script_pubkey.extend_from_slice(&pk2);
	script_pubkey.push(pk3.len() as u8);
	script_pubkey.extend_from_slice(&pk3);
	script_pubkey.push(0x53); // OP_3
	script_pubkey.push(0xae); // OP_CHECKMULTISIG

	let tx = make_tx();

	// Sign with sk1 and sk2
	let sig1 = sign_tx(&sk1, &tx, 0, &script_pubkey);
	let sig2 = sign_tx(&sk2, &tx, 0, &script_pubkey);

	// scriptSig: OP_0 <sig1> <sig2> (OP_0 is the dummy byte)
	let mut script_sig = vec![0x00]; // dummy
	script_sig.push(sig1.len() as u8);
	script_sig.extend_from_slice(&sig1);
	script_sig.push(sig2.len() as u8);
	script_sig.extend_from_slice(&sig2);

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();

	let top = final_stack.last().unwrap();
	assert_eq!(*top, vec![1u8]);
}

#[test]
fn op_checkmultisig_non_null_dummy_accepted() {
	// Without NULLDUMMY (BIP147), non-empty dummy is accepted
	let (sk1, pk1) = gen_keypair(&[0x01; 32]);

	let mut script_pubkey = vec![0x51]; // OP_1
	script_pubkey.push(pk1.len() as u8);
	script_pubkey.extend_from_slice(&pk1);
	script_pubkey.push(0x51); // OP_1
	script_pubkey.push(0xae); // OP_CHECKMULTISIG

	let tx = make_tx();
	let sig1 = sign_tx(&sk1, &tx, 0, &script_pubkey);

	// scriptSig with NON-empty dummy: OP_1 <sig1> (OP_1 pushes [0x01], not empty)
	let mut script_sig = vec![0x51]; // OP_1 as dummy (non-empty!)
	script_sig.push(sig1.len() as u8);
	script_sig.extend_from_slice(&sig1);

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();
	let top = final_stack.last().unwrap();
	assert_eq!(*top, vec![1u8]);
}

#[test]
fn op_checksig_no_context_fails() {
	// OP_CHECKSIG without SignatureContext
	let script = vec![0x01, 0x00, 0x01, 0x00, 0xac]; // push dummy sig, push dummy pubkey, CHECKSIG
	let mut engine = Engine::new(&script);
	assert!(engine.execute().is_err());
}

// --- CLTV/CSV as NOP tests ---

#[test]
fn op_checklocktimeverify_as_nop() {
	// CLTV should act as NOP (not activated in Catcoin)
	// OP_1 OP_CHECKLOCKTIMEVERIFY -> [1] (stack unchanged)
	let script = vec![0x51, 0xb1];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], vec![0x01]);
}

#[test]
fn op_checksequenceverify_as_nop() {
	let script = vec![0x51, 0xb2];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 1);
	assert_eq!(stack[0], vec![0x01]);
}

// --- Additional stack op tests ---

#[test]
fn op_2rot() {
	// OP_1 OP_2 OP_3 OP_4 OP_5 OP_6 OP_2ROT -> [3, 4, 5, 6, 1, 2]
	let script = vec![0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x71];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 6);
	assert_eq!(stack[0], vec![0x03]);
	assert_eq!(stack[1], vec![0x04]);
	assert_eq!(stack[2], vec![0x05]);
	assert_eq!(stack[3], vec![0x06]);
	assert_eq!(stack[4], vec![0x01]);
	assert_eq!(stack[5], vec![0x02]);
}

#[test]
fn op_2swap() {
	// OP_1 OP_2 OP_3 OP_4 OP_2SWAP -> [3, 4, 1, 2]
	let script = vec![0x51, 0x52, 0x53, 0x54, 0x72];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], vec![0x03]);
	assert_eq!(stack[1], vec![0x04]);
	assert_eq!(stack[2], vec![0x01]);
	assert_eq!(stack[3], vec![0x02]);
}

#[test]
fn op_3dup() {
	// OP_1 OP_2 OP_3 OP_3DUP -> [1, 2, 3, 1, 2, 3]
	let script = vec![0x51, 0x52, 0x53, 0x6f];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 6);
	assert_eq!(stack[0], vec![0x01]);
	assert_eq!(stack[3], vec![0x01]);
}

#[test]
fn op_2over() {
	// OP_1 OP_2 OP_3 OP_4 OP_2OVER -> [1, 2, 3, 4, 1, 2]
	let script = vec![0x51, 0x52, 0x53, 0x54, 0x70];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack.len(), 6);
	assert_eq!(stack[4], vec![0x01]);
	assert_eq!(stack[5], vec![0x02]);
}

// --- Disabled opcode and op counting in false branches ---

#[test]
fn disabled_opcode_in_false_branch() {
	// OP_0 OP_IF OP_CAT OP_ENDIF
	let script = vec![0x00, 0x63, 0x7e, 0x68];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::DisabledOpcode(0x7e));
}

#[test]
fn op_count_in_false_branch() {
	// OP_0 OP_IF [200 OP_NOPs] OP_ENDIF
	// Total non-push ops: 1 (IF) + 200 (NOP) + 1 (ENDIF) = 202 > 201
	let mut script = vec![0x00, 0x63]; // OP_0 OP_IF
	script.extend(std::iter::repeat_n(0x61, 200)); // 200 OP_NOPs
	script.push(0x68); // OP_ENDIF
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::TooManyOps);
}

// --- NULLFAIL tests ---

#[test]
fn op_checksig_empty_sig_pushes_false() {
	// Empty signature should push false, not error (NULLFAIL only applies to non-empty)
	let (_sk, pk) = gen_keypair(&[0x42; 32]);

	// P2PK script so we avoid EQUALVERIFY hash check
	let mut script_pubkey_p2pk = vec![pk.len() as u8];
	script_pubkey_p2pk.extend_from_slice(&pk);
	script_pubkey_p2pk.push(0xac); // OP_CHECKSIG

	// scriptSig: OP_0 (empty sig)
	let script_sig = vec![0x00];
	let tx = make_tx();

	let sig_ctx = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine = Engine::with_sig_context(&script_sig, sig_ctx);
	let sig_stack = engine.execute().unwrap();

	let sig_ctx2 = SignatureContext { tx: &tx, input_idx: 0 };
	let mut engine2 = Engine::with_stack_and_sig_context(&script_pubkey_p2pk, sig_stack, sig_ctx2);
	let final_stack = engine2.execute().unwrap();
	// Empty sig -> push canonical false (empty vec)
	let top = final_stack.last().unwrap();
	assert!(top.is_empty(), "expected canonical empty vec for false, got {top:?}");
}

#[test]
fn op_within_exclusive_upper_bound() {
	// OP_5 OP_3 OP_5 OP_WITHIN -> [0] (5 is NOT within [3,5) -- max is exclusive)
	let script = vec![0x55, 0x53, 0x55, 0xa5];
	let mut engine = Engine::new(&script);
	let stack = engine.execute().unwrap();
	assert_eq!(stack[0], Vec::<u8>::new());
}

#[test]
fn op_pick_negative_index() {
	// OP_1 OP_1NEGATE OP_PICK -> error (negative index)
	let script = vec![0x51, 0x4f, 0x79];
	let mut engine = Engine::new(&script);
	assert!(engine.execute().is_err());
}

#[test]
fn op_roll_negative_index() {
	// OP_1 OP_1NEGATE OP_ROLL -> error
	let script = vec![0x51, 0x4f, 0x7a];
	let mut engine = Engine::new(&script);
	assert!(engine.execute().is_err());
}

#[test]
fn pushdata1_truncated() {
	// PUSHDATA1 says 5 bytes but only 1 follows
	let script = vec![0x4c, 0x05, 0xaa];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::InvalidPushSize);
}

#[test]
fn pushdata2_truncated() {
	// PUSHDATA2 says 5 bytes but only 1 follows
	let script = vec![0x4d, 0x05, 0x00, 0xaa];
	let mut engine = Engine::new(&script);
	assert_eq!(engine.execute().unwrap_err(), ScriptError::InvalidPushSize);
}
