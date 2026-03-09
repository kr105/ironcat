// SPDX-License-Identifier: Apache-2.0

pub mod engine;
pub mod opcodes;
pub mod signature;
pub mod verify;

use std::fmt;

/// Errors that can occur during script execution
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
	/// Encountered an invalid opcode byte
	InvalidOpcode(u8),
	/// Encountered a disabled opcode byte
	DisabledOpcode(u8),
	/// Attempted to pop from an empty or insufficient stack
	StackUnderflow,
	/// Stack exceeded the maximum allowed size
	StackOverflow,
	/// Script exceeds the maximum allowed byte length
	ScriptTooLarge,
	/// Script exceeded the maximum number of operations
	TooManyOps,
	/// Push data size was invalid or out of range
	InvalidPushSize,
	/// `OP_EQUALVERIFY` failed because the top two stack items differ
	EqualVerifyFailed,
	/// `OP_CHECKSIG` failed
	CheckSigFailed,
	/// `OP_CHECKMULTISIG` failed
	CheckMultiSigFailed,
	/// Signature encoding does not follow DER or other required format
	InvalidSignatureEncoding,
	/// Public key is malformed or has an invalid prefix
	InvalidPubKey,
	/// Locktime value is negative
	NegativeLocktime,
	/// Locktime constraint was not satisfied
	UnsatisfiedLocktime,
	/// IF/ELSE/ENDIF blocks are not properly balanced
	UnbalancedConditional,
	/// Script completed with a false value on the stack
	FalseReturned,
	/// `OP_RETURN` was encountered, marking the output as unspendable
	OpReturnEncountered,
	/// The redeem script does not match the expected hash
	InvalidRedeemScript,
	/// A numeric operation overflowed the allowed range
	NumericOverflow,
	/// Multisig key count is out of range
	MultisigKeyCount,
	/// Multisig signature count is out of range
	MultisigSigCount,
	/// Stack index is out of bounds
	InvalidStackIndex,
}

impl fmt::Display for ScriptError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidOpcode(b) => write!(f, "invalid opcode: 0x{b:02x}"),
			Self::DisabledOpcode(b) => write!(f, "disabled opcode: 0x{b:02x}"),
			Self::StackUnderflow => write!(f, "stack underflow"),
			Self::StackOverflow => write!(f, "stack overflow"),
			Self::ScriptTooLarge => write!(f, "script too large"),
			Self::TooManyOps => write!(f, "too many operations"),
			Self::InvalidPushSize => write!(f, "invalid push size"),
			Self::EqualVerifyFailed => write!(f, "OP_EQUALVERIFY failed"),
			Self::CheckSigFailed => write!(f, "OP_CHECKSIG failed"),
			Self::CheckMultiSigFailed => write!(f, "OP_CHECKMULTISIG failed"),
			Self::InvalidSignatureEncoding => write!(f, "invalid signature encoding"),
			Self::InvalidPubKey => write!(f, "invalid public key"),
			Self::NegativeLocktime => write!(f, "negative locktime"),
			Self::UnsatisfiedLocktime => write!(f, "unsatisfied locktime"),
			Self::UnbalancedConditional => write!(f, "unbalanced conditional"),
			Self::FalseReturned => write!(f, "script returned false"),
			Self::OpReturnEncountered => write!(f, "OP_RETURN encountered"),
			Self::InvalidRedeemScript => write!(f, "invalid redeem script"),
			Self::NumericOverflow => write!(f, "numeric overflow"),
			Self::MultisigKeyCount => write!(f, "multisig key count out of range"),
			Self::MultisigSigCount => write!(f, "multisig signature count out of range"),
			Self::InvalidStackIndex => write!(f, "invalid stack index"),
		}
	}
}

impl std::error::Error for ScriptError {}
