// SPDX-License-Identifier: Apache-2.0

use ripemd::Ripemd160;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::ScriptError;
use super::opcodes::Opcode;
use super::signature::{find_and_delete, signature_hash, verify_ecdsa};
use crate::types::transaction::Transaction;

/// Maximum allowed script size in bytes
const MAX_SCRIPT_SIZE: usize = 10_000;

/// Maximum allowed number of non-push operations
const MAX_OPS: usize = 201;

/// Maximum combined stack + alt stack size
const MAX_STACK_SIZE: usize = 1000;

/// Maximum size of a single push element in bytes
const MAX_PUSH_SIZE: usize = 520;

/// Maximum number of public keys allowed in a single `OP_CHECKMULTISIG`
const MAX_MULTISIG_KEYS: i64 = 20;

/// Transaction context needed for signature verification opcodes
pub struct SignatureContext<'a> {
	/// The transaction being validated
	pub tx: &'a Transaction,
	/// The index of the input being verified
	pub input_idx: usize,
}

/// Script execution engine that processes Bitcoin script bytecode
pub struct Engine<'a> {
	stack: Vec<Vec<u8>>,
	alt_stack: Vec<Vec<u8>>,
	script: &'a [u8],
	pc: usize,
	op_count: usize,
	/// Tracks nested IF/ELSE conditions for control flow
	conditions: Vec<bool>,
	/// Position of the last `OP_CODESEPARATOR`
	codesep_pos: usize,
	/// Transaction context for signature verification, if available
	sig_ctx: Option<SignatureContext<'a>>,
}

impl<'a> Engine<'a> {
	/// Creates a new engine with empty stacks
	pub const fn new(script: &'a [u8]) -> Self {
		Self {
			stack: Vec::new(),
			alt_stack: Vec::new(),
			script,
			pc: 0,
			op_count: 0,
			conditions: Vec::new(),
			codesep_pos: 0,
			sig_ctx: None,
		}
	}

	/// Creates a new engine with a pre-loaded stack (for scriptPubKey execution after scriptSig)
	pub const fn with_stack(script: &'a [u8], stack: Vec<Vec<u8>>) -> Self {
		Self {
			stack,
			alt_stack: Vec::new(),
			script,
			pc: 0,
			op_count: 0,
			conditions: Vec::new(),
			codesep_pos: 0,
			sig_ctx: None,
		}
	}

	/// Creates a new engine with a signature context for signature verification opcodes
	pub const fn with_sig_context(script: &'a [u8], sig_ctx: SignatureContext<'a>) -> Self {
		Self {
			stack: Vec::new(),
			alt_stack: Vec::new(),
			script,
			pc: 0,
			op_count: 0,
			conditions: Vec::new(),
			codesep_pos: 0,
			sig_ctx: Some(sig_ctx),
		}
	}

	/// Creates a new engine with both a pre-loaded stack and a signature context
	pub const fn with_stack_and_sig_context(
		script: &'a [u8],
		stack: Vec<Vec<u8>>,
		sig_ctx: SignatureContext<'a>,
	) -> Self {
		Self {
			stack,
			alt_stack: Vec::new(),
			script,
			pc: 0,
			op_count: 0,
			conditions: Vec::new(),
			codesep_pos: 0,
			sig_ctx: Some(sig_ctx),
		}
	}

	/// Runs the script and returns the final stack
	pub fn execute(&mut self) -> Result<Vec<Vec<u8>>, ScriptError> {
		if self.script.len() > MAX_SCRIPT_SIZE {
			return Err(ScriptError::ScriptTooLarge);
		}

		while self.pc < self.script.len() {
			let byte = self.read_byte()?;
			let opcode = Opcode::from(byte);

			// Disabled opcodes always fail, even inside non-executing branches
			if opcode.is_disabled() {
				return Err(ScriptError::DisabledOpcode(byte));
			}

			// Op counting happens for ALL non-push opcodes, even in false branches
			// Push ops don't count toward the 201 limit
			let is_push = matches!(
				opcode,
				Opcode::Op0
					| Opcode::PushBytes(_)
					| Opcode::PushData1
					| Opcode::PushData2
					| Opcode::PushData4
					| Opcode::Op1Negate
					| Opcode::OpN(_)
			);
			if !is_push {
				self.op_count = self.op_count.checked_add(1).ok_or(ScriptError::TooManyOps)?;
				if self.op_count > MAX_OPS {
					return Err(ScriptError::TooManyOps);
				}
			}

			let executing = self.is_executing();

			if !executing {
				// When not executing, only process control flow opcodes and
				// advance past push data. Everything else is skipped
				match opcode {
					Opcode::OpIf | Opcode::OpNotIf => {
						// Increase nesting depth without evaluating
						self.conditions.push(false);
					}
					Opcode::OpElse => {
						self.op_else()?;
					}
					Opcode::OpEndIf => {
						self.op_endif()?;
					}
					// Push ops must advance pc past their data even when skipping
					Opcode::PushBytes(n) => {
						self.skip_bytes(usize::from(n))?;
					}
					Opcode::PushData1 => {
						let len = usize::from(self.read_byte()?);
						self.skip_bytes(len)?;
					}
					Opcode::PushData2 => {
						let b0 = self.read_byte()?;
						let b1 = self.read_byte()?;
						let len = usize::from(u16::from_le_bytes([b0, b1]));
						self.skip_bytes(len)?;
					}
					Opcode::PushData4 => {
						let b0 = self.read_byte()?;
						let b1 = self.read_byte()?;
						let b2 = self.read_byte()?;
						let b3 = self.read_byte()?;
						#[allow(clippy::cast_possible_truncation)] // script size is bounded to 10000
						let len = u32::from_le_bytes([b0, b1, b2, b3]) as usize;
						self.skip_bytes(len)?;
					}
					_ => {}
				}
				continue;
			}

			self.execute_opcode(opcode, byte)?;

			let total_stack = self
				.stack
				.len()
				.checked_add(self.alt_stack.len())
				.ok_or(ScriptError::StackOverflow)?;
			if total_stack > MAX_STACK_SIZE {
				return Err(ScriptError::StackOverflow);
			}
		}

		if !self.conditions.is_empty() {
			return Err(ScriptError::UnbalancedConditional);
		}

		Ok(self.stack.clone())
	}

	/// Executes a single opcode in an executing branch
	///
	/// Op counting and disabled-opcode checks happen in the main loop before
	/// this is called. This only handles the actual opcode semantics.
	///
	/// Stack manipulation code uses dense indexing and arithmetic on lengths that
	/// are always guarded by `require_stack`, so underflow/out-of-bounds is impossible
	#[allow(clippy::too_many_lines, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
	fn execute_opcode(&mut self, opcode: Opcode, _byte: u8) -> Result<(), ScriptError> {
		match opcode {
			// Push operations
			Opcode::Op0 => {
				self.stack.push(Vec::new());
				Ok(())
			}
			Opcode::PushBytes(n) => {
				self.push_data(usize::from(n))?;
				Ok(())
			}
			Opcode::PushData1 => {
				let len = usize::from(self.read_byte()?);
				self.push_data(len)?;
				Ok(())
			}
			Opcode::PushData2 => {
				let b0 = self.read_byte()?;
				let b1 = self.read_byte()?;
				let len = usize::from(u16::from_le_bytes([b0, b1]));
				self.push_data(len)?;
				Ok(())
			}
			Opcode::PushData4 => {
				let b0 = self.read_byte()?;
				let b1 = self.read_byte()?;
				let b2 = self.read_byte()?;
				let b3 = self.read_byte()?;
				#[allow(clippy::cast_possible_truncation)] // script size is bounded to 10000
				let len = u32::from_le_bytes([b0, b1, b2, b3]) as usize;
				self.push_data(len)?;
				Ok(())
			}
			Opcode::Op1Negate => {
				self.stack.push(vec![0x81]);
				Ok(())
			}
			Opcode::OpN(n) => {
				self.stack.push(vec![n]);
				Ok(())
			}

			// Stack operations
			Opcode::OpDup => {
				let top = self.peek()?.clone();
				self.stack.push(top);
				Ok(())
			}
			Opcode::OpDrop => {
				self.pop()?;
				Ok(())
			}
			Opcode::OpSwap => {
				self.require_stack(2)?;
				let len = self.stack.len();
				self.stack.swap(len - 1, len - 2);
				Ok(())
			}
			Opcode::OpOver => {
				self.require_stack(2)?;
				let item = self.stack[self.stack.len() - 2].clone();
				self.stack.push(item);
				Ok(())
			}
			Opcode::OpRot => {
				self.require_stack(3)?;
				let len = self.stack.len();
				// a b c -> b c a: remove item at len-3, push to top
				let item = self.stack.remove(len - 3);
				self.stack.push(item);
				Ok(())
			}
			Opcode::OpNip => {
				self.require_stack(2)?;
				let len = self.stack.len();
				self.stack.remove(len - 2);
				Ok(())
			}
			Opcode::OpTuck => {
				// a b -> b a b: insert copy of top before second-to-top
				self.require_stack(2)?;
				let top = self.peek()?.clone();
				let len = self.stack.len();
				self.stack.insert(len - 2, top);
				Ok(())
			}
			Opcode::OpPick => {
				let n = self.pop_index()?;
				self.require_stack(n.checked_add(1).ok_or(ScriptError::InvalidStackIndex)?)?;
				let len = self.stack.len();
				let item = self.stack[len - 1 - n].clone();
				self.stack.push(item);
				Ok(())
			}
			Opcode::OpRoll => {
				let n = self.pop_index()?;
				self.require_stack(n.checked_add(1).ok_or(ScriptError::InvalidStackIndex)?)?;
				let len = self.stack.len();
				let item = self.stack.remove(len - 1 - n);
				self.stack.push(item);
				Ok(())
			}
			Opcode::Op2Dup => {
				self.require_stack(2)?;
				let len = self.stack.len();
				let a = self.stack[len - 2].clone();
				let b = self.stack[len - 1].clone();
				self.stack.push(a);
				self.stack.push(b);
				Ok(())
			}
			Opcode::Op2Drop => {
				self.require_stack(2)?;
				self.stack.pop();
				self.stack.pop();
				Ok(())
			}
			Opcode::Op3Dup => {
				self.require_stack(3)?;
				let len = self.stack.len();
				let a = self.stack[len - 3].clone();
				let b = self.stack[len - 2].clone();
				let c = self.stack[len - 1].clone();
				self.stack.push(a);
				self.stack.push(b);
				self.stack.push(c);
				Ok(())
			}
			Opcode::Op2Over => {
				self.require_stack(4)?;
				let len = self.stack.len();
				let a = self.stack[len - 4].clone();
				let b = self.stack[len - 3].clone();
				self.stack.push(a);
				self.stack.push(b);
				Ok(())
			}
			Opcode::Op2Rot => {
				self.require_stack(6)?;
				let len = self.stack.len();
				let a = self.stack.remove(len - 6);
				// After removing one element, the next target is at the same index
				let b = self.stack.remove(len - 6);
				self.stack.push(a);
				self.stack.push(b);
				Ok(())
			}
			Opcode::Op2Swap => {
				self.require_stack(4)?;
				let len = self.stack.len();
				self.stack.swap(len - 4, len - 2);
				self.stack.swap(len - 3, len - 1);
				Ok(())
			}
			Opcode::OpIfDup => {
				let top = self.peek()?.clone();
				if is_truthy(&top) {
					self.stack.push(top);
				}
				Ok(())
			}
			Opcode::OpDepth => {
				let depth = self.stack.len();
				#[allow(clippy::cast_possible_wrap)] // stack size is bounded to 1000
				self.stack.push(encode_script_number(depth as i64));
				Ok(())
			}
			Opcode::OpSize => {
				let size = self.peek()?.len();
				#[allow(clippy::cast_possible_wrap)] // push elements are bounded to 520
				self.stack.push(encode_script_number(size as i64));
				Ok(())
			}
			Opcode::OpToAltStack => {
				let item = self.pop()?;
				self.alt_stack.push(item);
				Ok(())
			}
			Opcode::OpFromAltStack => {
				let item = self.alt_stack.pop().ok_or(ScriptError::StackUnderflow)?;
				self.stack.push(item);
				Ok(())
			}

			// Comparison
			Opcode::OpEqual => {
				self.require_stack(2)?;
				let b = self.pop()?;
				let a = self.pop()?;
				if a == b {
					self.stack.push(vec![0x01]);
				} else {
					self.stack.push(Vec::new());
				}
				Ok(())
			}
			Opcode::OpEqualVerify => {
				self.require_stack(2)?;
				let b = self.pop()?;
				let a = self.pop()?;
				if a != b {
					return Err(ScriptError::EqualVerifyFailed);
				}
				Ok(())
			}

			// Control flow
			Opcode::OpIf => {
				let top = self.pop()?;
				self.conditions.push(is_truthy(&top));
				Ok(())
			}
			Opcode::OpNotIf => {
				let top = self.pop()?;
				self.conditions.push(!is_truthy(&top));
				Ok(())
			}
			Opcode::OpElse => {
				self.op_else()?;
				Ok(())
			}
			Opcode::OpEndIf => {
				self.op_endif()?;
				Ok(())
			}

			// Control
			// CLTV/CSV treated as NOP -- Catcoin has not activated BIP65/BIP112
			#[allow(clippy::match_same_arms)] // CLTV/CSV are semantically different from NOP, kept separate for clarity
			Opcode::OpNop(_) | Opcode::OpCheckLockTimeVerify | Opcode::OpCheckSequenceVerify => Ok(()),
			Opcode::OpVerify => {
				let top = self.pop()?;
				if !is_truthy(&top) {
					return Err(ScriptError::FalseReturned);
				}
				Ok(())
			}
			Opcode::OpReturn => Err(ScriptError::OpReturnEncountered),

			// Arithmetic - unary
			Opcode::Op1Add => {
				let n = self.pop_number()?;
				self.push_number(n.checked_add(1).ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::Op1Sub => {
				let n = self.pop_number()?;
				self.push_number(n.checked_sub(1).ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::OpNegate => {
				let n = self.pop_number()?;
				self.push_number(n.checked_neg().ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::OpAbs => {
				let n = self.pop_number()?;
				self.push_number(n.checked_abs().ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::OpNot => {
				let n = self.pop_number()?;
				self.push_number(i64::from(n == 0));
				Ok(())
			}
			Opcode::Op0NotEqual => {
				let n = self.pop_number()?;
				self.push_number(i64::from(n != 0));
				Ok(())
			}

			// Arithmetic - binary
			Opcode::OpAdd => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(a.checked_add(b).ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::OpSub => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(a.checked_sub(b).ok_or(ScriptError::NumericOverflow)?);
				Ok(())
			}
			Opcode::OpBoolAnd => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a != 0 && b != 0));
				Ok(())
			}
			Opcode::OpBoolOr => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a != 0 || b != 0));
				Ok(())
			}
			Opcode::OpNumEqual => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a == b));
				Ok(())
			}
			Opcode::OpNumEqualVerify => {
				let (a, b) = self.pop_two_numbers()?;
				if a != b {
					return Err(ScriptError::FalseReturned);
				}
				Ok(())
			}
			Opcode::OpNumNotEqual => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a != b));
				Ok(())
			}
			Opcode::OpLessThan => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a < b));
				Ok(())
			}
			Opcode::OpGreaterThan => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a > b));
				Ok(())
			}
			Opcode::OpLessThanOrEqual => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a <= b));
				Ok(())
			}
			Opcode::OpGreaterThanOrEqual => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(i64::from(a >= b));
				Ok(())
			}
			Opcode::OpMin => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(a.min(b));
				Ok(())
			}
			Opcode::OpMax => {
				let (a, b) = self.pop_two_numbers()?;
				self.push_number(a.max(b));
				Ok(())
			}
			Opcode::OpWithin => {
				let max = self.pop_number()?;
				let min = self.pop_number()?;
				let x = self.pop_number()?;
				self.push_number(i64::from(x >= min && x < max));
				Ok(())
			}

			// Crypto hash operations
			Opcode::OpSha256 => {
				let data = self.pop()?;
				self.stack.push(Sha256::digest(&data).to_vec());
				Ok(())
			}
			Opcode::OpHash256 => {
				let data = self.pop()?;
				let first = Sha256::digest(&data);
				self.stack.push(Sha256::digest(first).to_vec());
				Ok(())
			}
			Opcode::OpHash160 => {
				let data = self.pop()?;
				let sha = Sha256::digest(&data);
				self.stack.push(Ripemd160::digest(sha).to_vec());
				Ok(())
			}
			Opcode::OpRipemd160 => {
				let data = self.pop()?;
				self.stack.push(Ripemd160::digest(&data).to_vec());
				Ok(())
			}
			Opcode::OpSha1 => {
				let data = self.pop()?;
				self.stack.push(Sha1::digest(&data).to_vec());
				Ok(())
			}
			Opcode::OpCodeSeparator => {
				self.codesep_pos = self.pc;
				Ok(())
			}

			// Signature verification
			Opcode::OpCheckSig => {
				self.op_checksig(false)?;
				Ok(())
			}
			Opcode::OpCheckSigVerify => {
				self.op_checksig(true)?;
				Ok(())
			}
			Opcode::OpCheckMultiSig => {
				self.op_checkmultisig(false)?;
				Ok(())
			}
			Opcode::OpCheckMultiSigVerify => {
				self.op_checkmultisig(true)?;
				Ok(())
			}

			// Error cases
			Opcode::Disabled(b) => Err(ScriptError::DisabledOpcode(b)),
			Opcode::Invalid(b) => Err(ScriptError::InvalidOpcode(b)),
		}
	}

	/// Reads a single byte from the script at the current pc, advancing pc
	fn read_byte(&mut self) -> Result<u8, ScriptError> {
		let b = *self.script.get(self.pc).ok_or(ScriptError::InvalidPushSize)?;
		self.pc = self.pc.checked_add(1).ok_or(ScriptError::ScriptTooLarge)?;
		Ok(b)
	}

	/// Reads `len` bytes from the script and pushes them onto the stack
	fn push_data(&mut self, len: usize) -> Result<(), ScriptError> {
		if len > MAX_PUSH_SIZE {
			return Err(ScriptError::InvalidPushSize);
		}
		let end = self.pc.checked_add(len).ok_or(ScriptError::InvalidPushSize)?;
		let data = self
			.script
			.get(self.pc..end)
			.ok_or(ScriptError::InvalidPushSize)?
			.to_vec();
		self.pc = end;
		self.stack.push(data);
		Ok(())
	}

	/// Returns a reference to the top stack element without removing it
	fn peek(&self) -> Result<&Vec<u8>, ScriptError> {
		self.stack.last().ok_or(ScriptError::StackUnderflow)
	}

	/// Pops the top element from the stack
	fn pop(&mut self) -> Result<Vec<u8>, ScriptError> {
		self.stack.pop().ok_or(ScriptError::StackUnderflow)
	}

	/// Checks that the stack has at least `n` elements
	const fn require_stack(&self, n: usize) -> Result<(), ScriptError> {
		if self.stack.len() < n {
			return Err(ScriptError::StackUnderflow);
		}
		Ok(())
	}

	/// Pops the top element and interprets it as a stack index
	fn pop_index(&mut self) -> Result<usize, ScriptError> {
		let val = self.pop()?;
		let n = decode_script_number(&val).map_err(|_| ScriptError::InvalidStackIndex)?;
		if n < 0 {
			return Err(ScriptError::InvalidStackIndex);
		}
		usize::try_from(n).map_err(|_| ScriptError::InvalidStackIndex)
	}

	/// Returns true if we are currently in an executing branch
	fn is_executing(&self) -> bool {
		self.conditions.iter().all(|&c| c)
	}

	/// Handles `OP_ELSE`: flips the last condition entry
	fn op_else(&mut self) -> Result<(), ScriptError> {
		let last = self.conditions.last_mut().ok_or(ScriptError::UnbalancedConditional)?;
		*last = !*last;
		Ok(())
	}

	/// Handles `OP_ENDIF`: pops the last condition entry
	fn op_endif(&mut self) -> Result<(), ScriptError> {
		self.conditions.pop().ok_or(ScriptError::UnbalancedConditional)?;
		Ok(())
	}

	/// Advances the program counter by `n` bytes without reading data
	fn skip_bytes(&mut self, n: usize) -> Result<(), ScriptError> {
		self.pc = self.pc.checked_add(n).ok_or(ScriptError::InvalidPushSize)?;
		if self.pc > self.script.len() {
			return Err(ScriptError::InvalidPushSize);
		}
		Ok(())
	}

	/// Pops the top element and decodes it as a script number
	fn pop_number(&mut self) -> Result<i64, ScriptError> {
		let val = self.pop()?;
		decode_script_number(&val)
	}

	/// Pops two elements: second-to-top as `a`, top as `b`
	fn pop_two_numbers(&mut self) -> Result<(i64, i64), ScriptError> {
		let b = self.pop_number()?;
		let a = self.pop_number()?;
		Ok((a, b))
	}

	/// Pushes an i64 as a script number onto the stack
	fn push_number(&mut self, n: i64) {
		self.stack.push(encode_script_number(n));
	}

	/// Implements `OP_CHECKSIG` and `OP_CHECKSIGVERIFY`
	///
	/// Pops pubkey and signature, verifies the ECDSA signature against the
	/// transaction sighash. If `verify` is true, returns an error on failure
	/// instead of pushing false
	#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // sig length checked before indexing
	fn op_checksig(&mut self, verify: bool) -> Result<(), ScriptError> {
		let pubkey = self.pop()?;
		let sig = self.pop()?;

		// Empty sig means "no signature provided", push false
		if sig.is_empty() {
			if verify {
				return Err(ScriptError::CheckSigFailed);
			}
			self.stack.push(Vec::new());
			return Ok(());
		}

		let sig_ctx = self.sig_ctx.as_ref().ok_or(ScriptError::CheckSigFailed)?;

		let hash_type = sig[sig.len() - 1];

		let script_code = &self.script[self.codesep_pos..];
		// FindAndDelete: remove the signature push from script_code before hashing
		let script_code = find_and_delete(script_code, &sig);
		let sighash = signature_hash(sig_ctx.tx, sig_ctx.input_idx, &script_code, hash_type);

		let valid = verify_ecdsa(&sig, &pubkey, &sighash);

		if valid {
			if !verify {
				self.stack.push(vec![1]);
			}
			Ok(())
		} else if verify {
			Err(ScriptError::CheckSigFailed)
		} else {
			self.stack.push(Vec::new());
			Ok(())
		}
	}

	/// Implements `OP_CHECKMULTISIG` and `OP_CHECKMULTISIGVERIFY`
	///
	/// Verifies M-of-N multisig. Pops key count, keys, sig count, signatures,
	/// and the dummy element. Signatures must match keys in order. If `verify`
	/// is true, returns an error on failure instead of pushing false
	#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // indices guarded by length checks
	fn op_checkmultisig(&mut self, verify: bool) -> Result<(), ScriptError> {
		let n_keys = self.pop_number()?;
		if !(0..=MAX_MULTISIG_KEYS).contains(&n_keys) {
			return Err(ScriptError::MultisigKeyCount);
		}

		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // validated 0..=20
		let n_keys = n_keys as usize;

		self.require_stack(n_keys)?;
		let mut keys = Vec::with_capacity(n_keys);
		for _ in 0..n_keys {
			keys.push(self.pop()?);
		}

		// n_keys counts toward the 201 op limit
		self.op_count = self.op_count.checked_add(n_keys).ok_or(ScriptError::TooManyOps)?;
		if self.op_count > MAX_OPS {
			return Err(ScriptError::TooManyOps);
		}

		let n_sigs = self.pop_number()?;
		#[allow(clippy::cast_possible_wrap)] // n_keys <= 20, fits in i64
		if n_sigs < 0 || n_sigs > n_keys as i64 {
			return Err(ScriptError::MultisigSigCount);
		}

		#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // validated 0..=n_keys
		let n_sigs = n_sigs as usize;

		self.require_stack(n_sigs)?;
		let mut sigs = Vec::with_capacity(n_sigs);
		for _ in 0..n_sigs {
			sigs.push(self.pop()?);
		}

		// Pop the dummy element (Bitcoin's off-by-one bug)
		let _dummy = self.pop()?;

		let sig_ctx = self.sig_ctx.as_ref().ok_or(ScriptError::CheckMultiSigFailed)?;
		let script_code = &self.script[self.codesep_pos..];

		// FindAndDelete: remove each signature push from script_code
		let mut script_code = script_code.to_vec();
		for sig in &sigs {
			if !sig.is_empty() {
				script_code = find_and_delete(&script_code, sig);
			}
		}

		let mut key_idx = 0;
		let mut success = true;

		for sig in &sigs {
			// Empty sig always fails verification -- no key will match
			if sig.is_empty() {
				success = false;
				break;
			}

			let hash_type = sig[sig.len() - 1];
			let sighash = signature_hash(sig_ctx.tx, sig_ctx.input_idx, &script_code, hash_type);

			let mut matched = false;
			while key_idx < n_keys {
				let valid = verify_ecdsa(sig, &keys[key_idx], &sighash);
				key_idx += 1;
				if valid {
					matched = true;
					break;
				}
			}

			if !matched {
				success = false;
				break;
			}
		}

		if success {
			if !verify {
				self.stack.push(vec![1]);
			}
			Ok(())
		} else if verify {
			Err(ScriptError::CheckMultiSigFailed)
		} else {
			self.stack.push(Vec::new());
			Ok(())
		}
	}
}

/// Returns true if a stack element is considered truthy in Bitcoin script.
/// An element is falsy if it's empty, all zeros, or negative zero (0x80)
pub(crate) fn is_truthy(data: &[u8]) -> bool {
	if data.is_empty() {
		return false;
	}
	// Check for all zeros or negative zero
	// Negative zero: last byte is 0x80, all other bytes are 0x00
	for (i, &b) in data.iter().enumerate() {
		#[allow(clippy::arithmetic_side_effects)] // i < data.len(), so data.len() - 1 won't underflow
		if i == data.len() - 1 {
			// Last byte: 0x00 is zero, 0x80 is negative zero
			if b != 0x00 && b != 0x80 {
				return true;
			}
		} else if b != 0x00 {
			return true;
		}
	}
	false
}

/// Encodes an i64 as a Bitcoin script number
#[allow(clippy::cast_sign_loss)] // intentional: we handle sign separately
fn encode_script_number(value: i64) -> Vec<u8> {
	if value == 0 {
		return Vec::new();
	}

	let negative = value < 0;
	let mut abs_value = if negative {
		// Handles i64::MIN correctly by casting to u64 first
		value.cast_unsigned().wrapping_neg()
	} else {
		value.cast_unsigned()
	};

	let mut result = Vec::new();
	while abs_value > 0 {
		#[allow(clippy::cast_possible_truncation)] // intentional truncation to get low byte
		result.push((abs_value & 0xff) as u8);
		abs_value >>= 8;
	}

	// If the most significant byte has the high bit set, we need an extra byte
	// for the sign
	if let Some(last) = result.last_mut() {
		if *last & 0x80 != 0 {
			result.push(if negative { 0x80 } else { 0x00 });
		} else if negative {
			*last |= 0x80;
		}
	}

	result
}

/// Decodes a Bitcoin script number from bytes
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)] // length is checked <= 4
fn decode_script_number(data: &[u8]) -> Result<i64, ScriptError> {
	if data.is_empty() {
		return Ok(0);
	}
	if data.len() > 4 {
		return Err(ScriptError::NumericOverflow);
	}

	let mut result: i64 = 0;
	for (i, &b) in data.iter().enumerate() {
		result |= i64::from(b) << (i * 8);
	}

	// Check sign bit of the last byte
	if data[data.len() - 1] & 0x80 != 0 {
		// Remove sign bit and negate
		let shift = (data.len() - 1) * 8;
		result &= !(0x80i64 << shift);
		result = -result;
	}

	Ok(result)
}
