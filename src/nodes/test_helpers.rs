// SPDX-License-Identifier: Apache-2.0

// Test utilities: unwrap/panic are intentional here since panics are the correct failure mode.
// missing_panics_doc is suppressed because these are test helpers -- callers expect panics on failure
#![allow(clippy::unwrap_used, clippy::panic, clippy::missing_panics_doc)]

//! Shared test utilities for handler tests
//!
//! Provides a loopback TCP pair, message reader, and dummy genesis header
//! used across handler test modules

use std::ffi::CStr;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedReadHalf;

use crate::network::{Message, MessageParseError, SharedTcpWriter};
use crate::types::block::BlockHeader;
use crate::types::hash::Hash256;

/// Dummy genesis header for tests that don't care about header data
pub const fn test_genesis() -> BlockHeader {
	BlockHeader {
		version: 1,
		prev_hash: Hash256::ZERO,
		merkle_root: Hash256::ZERO,
		timestamp: 0,
		bits: 0,
		nonce: 0,
	}
}

/// Creates a loopback TCP pair and returns (writer for handler, reader for assertions)
pub async fn tcp_pair() -> (SharedTcpWriter, OwnedReadHalf) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let client = tokio::net::TcpStream::connect(addr).await.unwrap();
	let (server, _) = listener.accept().await.unwrap();
	let (read_half, _write_half) = server.into_split();
	let (_client_read, client_write) = client.into_split();
	let writer = Arc::new(tokio::sync::Mutex::new(client_write));
	(writer, read_half)
}

/// Reads all protocol messages from the stream until the writer is dropped.
/// Call after the handler has returned so all data is already buffered
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]
pub async fn read_all_messages(reader: &mut OwnedReadHalf) -> Vec<Message> {
	let mut buf = Vec::new();
	reader.read_to_end(&mut buf).await.unwrap();
	let mut messages = Vec::new();
	let mut offset = 0;
	while offset < buf.len() {
		match Message::from_bytes(&buf[offset..]) {
			Ok((msg, consumed)) => {
				offset += consumed;
				messages.push(msg);
			}
			Err(MessageParseError::Incomplete) => break,
			Err(e) => panic!("corrupt message at offset {offset}: {e:?}"),
		}
	}
	messages
}

/// Extracts the command name from a message's command field
pub fn command_name(msg: &Message) -> &str {
	CStr::from_bytes_until_nul(&msg.command).unwrap().to_str().unwrap()
}
