// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::unwrap_used)]

use ironcat::script::opcodes::Opcode;

#[test]
fn parse_known_opcodes() {
	assert_eq!(Opcode::from(0x00), Opcode::Op0);
	assert_eq!(Opcode::from(0x76), Opcode::OpDup);
	assert_eq!(Opcode::from(0xa9), Opcode::OpHash160);
	assert_eq!(Opcode::from(0x88), Opcode::OpEqualVerify);
	assert_eq!(Opcode::from(0xac), Opcode::OpCheckSig);
	assert_eq!(Opcode::from(0x6a), Opcode::OpReturn);
}

#[test]
fn parse_push_data_bytes() {
	assert_eq!(Opcode::from(0x01), Opcode::PushBytes(1));
	assert_eq!(Opcode::from(0x4b), Opcode::PushBytes(75));
}

#[test]
fn parse_op_n_values() {
	assert_eq!(Opcode::from(0x51), Opcode::OpN(1));
	assert_eq!(Opcode::from(0x60), Opcode::OpN(16));
}

#[test]
fn disabled_opcodes_identified() {
	assert!(Opcode::from(0x7e).is_disabled()); // OP_CAT
	assert!(Opcode::from(0x95).is_disabled()); // OP_MUL
	assert!(!Opcode::from(0x76).is_disabled()); // OP_DUP should not be disabled
}
