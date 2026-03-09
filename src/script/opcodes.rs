// SPDX-License-Identifier: Apache-2.0

/// Represents a decoded Bitcoin script opcode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
	/// Push empty byte array onto stack (0x00)
	Op0,
	/// Push the next N bytes onto stack (0x01..=0x4b)
	PushBytes(u8),
	/// Next byte contains the number of bytes to push (0x4c)
	PushData1,
	/// Next two bytes contain the number of bytes to push (0x4d)
	PushData2,
	/// Next four bytes contain the number of bytes to push (0x4e)
	PushData4,
	/// Push the value -1 onto the stack (0x4f)
	Op1Negate,
	/// Push the value N (1..=16) onto the stack (0x51..=0x60)
	OpN(u8),
	/// No operation (0x61 = NOP, 0xb0 = NOP1, 0xb3..=0xb9 = NOP4..NOP10)
	OpNop(u8),
	/// If the top stack value is true, execute the following statements (0x63)
	OpIf,
	/// If the top stack value is false, execute the following statements (0x64)
	OpNotIf,
	/// Marks the else branch of an if/notif block (0x67)
	OpElse,
	/// Ends an if/else block (0x68)
	OpEndIf,
	/// Marks transaction as invalid if top stack value is false (0x69)
	OpVerify,
	/// Marks transaction as invalid and output as unspendable (0x6a)
	OpReturn,
	/// Move top stack item to alt stack (0x6b)
	OpToAltStack,
	/// Move top alt stack item to main stack (0x6c)
	OpFromAltStack,
	/// Remove top two stack items (0x6d)
	Op2Drop,
	/// Duplicate top two stack items (0x6e)
	Op2Dup,
	/// Duplicate top three stack items (0x6f)
	Op3Dup,
	/// Copy the pair of items two spaces back to the front (0x70)
	Op2Over,
	/// Move the fifth and sixth items to the top (0x71)
	Op2Rot,
	/// Swap the top two pairs of items (0x72)
	Op2Swap,
	/// If top value is not zero, duplicate it (0x73)
	OpIfDup,
	/// Push the stack size onto the stack (0x74)
	OpDepth,
	/// Remove the top stack item (0x75)
	OpDrop,
	/// Duplicate the top stack item (0x76)
	OpDup,
	/// Remove the second-to-top stack item (0x77)
	OpNip,
	/// Copy the second-to-top stack item to the top (0x78)
	OpOver,
	/// Copy item N back in stack to the top (0x79)
	OpPick,
	/// Move item N back in stack to the top (0x7a)
	OpRoll,
	/// Rotate the top three items (0x7b)
	OpRot,
	/// Swap the top two stack items (0x7c)
	OpSwap,
	/// Copy top item and insert before second-to-top (0x7d)
	OpTuck,
	/// Push the byte length of the top stack item (0x82)
	OpSize,
	/// Returns 1 if the top two items are equal, 0 otherwise (0x87)
	OpEqual,
	/// Same as `OP_EQUAL` but runs `OP_VERIFY` afterward (0x88)
	OpEqualVerify,
	/// Add 1 to the top stack item (0x8b)
	Op1Add,
	/// Subtract 1 from the top stack item (0x8c)
	Op1Sub,
	/// Negate the top stack item (0x8f)
	OpNegate,
	/// Make the top stack item positive (0x90)
	OpAbs,
	/// Boolean NOT of the top stack item (0x91)
	OpNot,
	/// Returns 0 if top is 0, 1 otherwise (0x92)
	Op0NotEqual,
	/// Add the top two stack items (0x93)
	OpAdd,
	/// Subtract the top stack item from the second (0x94)
	OpSub,
	/// Boolean AND of top two items (0x9a)
	OpBoolAnd,
	/// Boolean OR of top two items (0x9b)
	OpBoolOr,
	/// Returns 1 if the top two items are numerically equal (0x9c)
	OpNumEqual,
	/// Same as `OP_NUMEQUAL` but runs `OP_VERIFY` afterward (0x9d)
	OpNumEqualVerify,
	/// Returns 1 if the top two items are not equal (0x9e)
	OpNumNotEqual,
	/// Returns 1 if second-to-top is less than top (0x9f)
	OpLessThan,
	/// Returns 1 if second-to-top is greater than top (0xa0)
	OpGreaterThan,
	/// Returns 1 if second-to-top is less than or equal to top (0xa1)
	OpLessThanOrEqual,
	/// Returns 1 if second-to-top is greater than or equal to top (0xa2)
	OpGreaterThanOrEqual,
	/// Returns the smaller of the top two items (0xa3)
	OpMin,
	/// Returns the larger of the top two items (0xa4)
	OpMax,
	/// Returns 1 if x is within the range [min, max) (0xa5)
	OpWithin,
	/// Hash the top item with RIPEMD-160 (0xa6)
	OpRipemd160,
	/// Hash the top item with SHA-1 (0xa7)
	OpSha1,
	/// Hash the top item with SHA-256 (0xa8)
	OpSha256,
	/// Hash the top item with SHA-256 then RIPEMD-160 (0xa9)
	OpHash160,
	/// Hash the top item with double SHA-256 (0xaa)
	OpHash256,
	/// Mark the beginning of signature-checked code (0xab)
	OpCodeSeparator,
	/// Verify a signature against a public key (0xac)
	OpCheckSig,
	/// Same as `OP_CHECKSIG` but runs `OP_VERIFY` afterward (0xad)
	OpCheckSigVerify,
	/// Verify multiple signatures against multiple public keys (0xae)
	OpCheckMultiSig,
	/// Same as `OP_CHECKMULTISIG` but runs `OP_VERIFY` afterward (0xaf)
	OpCheckMultiSigVerify,
	/// Marks transaction invalid if top stack value is not >= the lock time (0xb1)
	OpCheckLockTimeVerify,
	/// Marks transaction invalid if relative lock time is not met (0xb2)
	OpCheckSequenceVerify,
	/// A disabled opcode that makes the script immediately invalid
	Disabled(u8),
	/// An invalid or unassigned opcode
	Invalid(u8),
}

impl From<u8> for Opcode {
	// Many match arms map individual bytes to opcode variants, and the
	// subtraction in range arms is guarded by the match range
	#[allow(clippy::match_same_arms, clippy::arithmetic_side_effects)]
	fn from(byte: u8) -> Self {
		match byte {
			0x00 => Self::Op0,
			0x01..=0x4b => Self::PushBytes(byte),
			0x4c => Self::PushData1,
			0x4d => Self::PushData2,
			0x4e => Self::PushData4,
			0x4f => Self::Op1Negate,
			0x50 => Self::Invalid(byte),
			0x51..=0x60 => Self::OpN(byte - 0x50),
			0x61 => Self::OpNop(0),
			0x62 => Self::Invalid(byte),
			0x63 => Self::OpIf,
			0x64 => Self::OpNotIf,
			0x65 | 0x66 => Self::Invalid(byte),
			0x67 => Self::OpElse,
			0x68 => Self::OpEndIf,
			0x69 => Self::OpVerify,
			0x6a => Self::OpReturn,
			0x6b => Self::OpToAltStack,
			0x6c => Self::OpFromAltStack,
			0x6d => Self::Op2Drop,
			0x6e => Self::Op2Dup,
			0x6f => Self::Op3Dup,
			0x70 => Self::Op2Over,
			0x71 => Self::Op2Rot,
			0x72 => Self::Op2Swap,
			0x73 => Self::OpIfDup,
			0x74 => Self::OpDepth,
			0x75 => Self::OpDrop,
			0x76 => Self::OpDup,
			0x77 => Self::OpNip,
			0x78 => Self::OpOver,
			0x79 => Self::OpPick,
			0x7a => Self::OpRoll,
			0x7b => Self::OpRot,
			0x7c => Self::OpSwap,
			0x7d => Self::OpTuck,
			0x7e..=0x81 => Self::Disabled(byte),
			0x82 => Self::OpSize,
			0x83..=0x86 => Self::Disabled(byte),
			0x87 => Self::OpEqual,
			0x88 => Self::OpEqualVerify,
			0x89 | 0x8a => Self::Invalid(byte),
			0x8b => Self::Op1Add,
			0x8c => Self::Op1Sub,
			0x8d | 0x8e => Self::Disabled(byte),
			0x8f => Self::OpNegate,
			0x90 => Self::OpAbs,
			0x91 => Self::OpNot,
			0x92 => Self::Op0NotEqual,
			0x93 => Self::OpAdd,
			0x94 => Self::OpSub,
			0x95..=0x99 => Self::Disabled(byte),
			0x9a => Self::OpBoolAnd,
			0x9b => Self::OpBoolOr,
			0x9c => Self::OpNumEqual,
			0x9d => Self::OpNumEqualVerify,
			0x9e => Self::OpNumNotEqual,
			0x9f => Self::OpLessThan,
			0xa0 => Self::OpGreaterThan,
			0xa1 => Self::OpLessThanOrEqual,
			0xa2 => Self::OpGreaterThanOrEqual,
			0xa3 => Self::OpMin,
			0xa4 => Self::OpMax,
			0xa5 => Self::OpWithin,
			0xa6 => Self::OpRipemd160,
			0xa7 => Self::OpSha1,
			0xa8 => Self::OpSha256,
			0xa9 => Self::OpHash160,
			0xaa => Self::OpHash256,
			0xab => Self::OpCodeSeparator,
			0xac => Self::OpCheckSig,
			0xad => Self::OpCheckSigVerify,
			0xae => Self::OpCheckMultiSig,
			0xaf => Self::OpCheckMultiSigVerify,
			0xb0 => Self::OpNop(1),
			0xb1 => Self::OpCheckLockTimeVerify,
			0xb2 => Self::OpCheckSequenceVerify,
			0xb3..=0xb9 => Self::OpNop(byte - 0xaf),
			0xba..=0xff => Self::Invalid(byte),
		}
	}
}

impl Opcode {
	/// Returns true if this opcode is disabled and should cause immediate script failure
	pub const fn is_disabled(&self) -> bool {
		matches!(self, Self::Disabled(_))
	}
}
