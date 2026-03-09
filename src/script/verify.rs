// SPDX-License-Identifier: Apache-2.0

use super::engine::{is_truthy, Engine, SignatureContext};
use super::ScriptError;
use crate::types::transaction::Transaction;

/// Verifies a transaction input by executing scriptSig and scriptPubKey
///
/// Runs scriptSig first, then scriptPubKey with the resulting stack.
/// If the scriptPubKey matches the P2SH pattern, also evaluates the
/// redeem script embedded in scriptSig.
///
/// Only consensus-level script flags are enforced. For Catcoin this means
/// only `SCRIPT_VERIFY_P2SH` (from height 16). Policy-level flags like
/// `DERSIG`, `LOW_S`, `STRICTENC`, `NULLFAIL`, `NULLDUMMY`, `MINIMALDATA`,
/// and `CLEANSTACK` are not enforced here
pub fn verify_script(
	script_sig: &[u8],
	script_pubkey: &[u8],
	tx: &Transaction,
	input_idx: usize,
) -> Result<(), ScriptError> {
	// Phase 1: Execute scriptSig
	let sig_ctx = SignatureContext { tx, input_idx };
	let mut engine = Engine::with_sig_context(script_sig, sig_ctx);
	let sig_stack = engine.execute()?;

	// Save a copy for P2SH evaluation before passing to scriptPubKey
	let p2sh_stack = sig_stack.clone();

	// Phase 2: Execute scriptPubKey with the resulting stack
	let sig_ctx = SignatureContext { tx, input_idx };
	let mut engine = Engine::with_stack_and_sig_context(script_pubkey, sig_stack, sig_ctx);
	let final_stack = engine.execute()?;

	require_truthy_top(&final_stack)?;

	// Phase 3: P2SH evaluation
	if is_p2sh(script_pubkey) {
		// P2SH requires scriptSig to be push-only
		if !is_push_only(script_sig) {
			return Err(ScriptError::InvalidRedeemScript);
		}

		// The redeem script is the last element pushed by scriptSig
		let mut redeem_stack = p2sh_stack;
		let redeem_script = redeem_stack.pop().ok_or(ScriptError::InvalidRedeemScript)?;

		// Execute the redeem script with the remaining stack elements
		let sig_ctx = SignatureContext { tx, input_idx };
		let mut engine = Engine::with_stack_and_sig_context(&redeem_script, redeem_stack, sig_ctx);
		let redeem_result = engine.execute()?;

		require_truthy_top(&redeem_result)?;
	}

	Ok(())
}

/// Checks that the stack is non-empty and the top element is truthy
fn require_truthy_top(stack: &[Vec<u8>]) -> Result<(), ScriptError> {
	match stack.last() {
		Some(top) if is_truthy(top) => Ok(()),
		_ => Err(ScriptError::FalseReturned),
	}
}

/// Returns true if the script contains only push operations
///
/// Required for P2SH: the scriptSig must be push-only to prevent
/// stack manipulation attacks before redeem script execution
// Byte-level script parsing with indices guarded by bounds checks
#[allow(
	clippy::indexing_slicing,
	clippy::arithmetic_side_effects,
	clippy::cast_possible_truncation
)]
fn is_push_only(script: &[u8]) -> bool {
	let mut i = 0;
	while i < script.len() {
		let op = script[i];
		match op {
			// OP_0, OP_1NEGATE, OP_1 through OP_16: single-byte push opcodes
			#[allow(clippy::match_same_arms)] // logically distinct opcodes kept separate for clarity
			0x00 | 0x4f | 0x51..=0x60 => i += 1,
			// OP_PUSHBYTES_1 through OP_PUSHBYTES_75
			0x01..=0x4b => {
				i += 1 + op as usize;
			}
			// OP_PUSHDATA1
			0x4c => {
				if i + 1 >= script.len() {
					return false;
				}
				let len = script[i + 1] as usize;
				i += 2 + len;
			}
			// OP_PUSHDATA2
			0x4d => {
				if i + 3 > script.len() {
					return false;
				}
				let len = u16::from_le_bytes([script[i + 1], script[i + 2]]) as usize;
				i += 3 + len;
			}
			// OP_PUSHDATA4
			0x4e => {
				if i + 5 > script.len() {
					return false;
				}
				let len = u32::from_le_bytes([script[i + 1], script[i + 2], script[i + 3], script[i + 4]]) as usize;
				i += 5 + len;
			}
			// Anything else is not a push
			_ => return false,
		}
	}
	true
}

/// Detects the P2SH output pattern: `OP_HASH160` PUSH(20) <20 bytes> `OP_EQUAL`
#[allow(clippy::indexing_slicing)] // length is checked to be exactly 23 before indexing
fn is_p2sh(script: &[u8]) -> bool {
	script.len() == 23 && script[0] == 0xa9 && script[1] == 0x14 && script[22] == 0x87
}
