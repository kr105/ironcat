// SPDX-License-Identifier: Apache-2.0

use std::io::Cursor;

use anyhow::{anyhow, Context, Result};

use super::{decode_varint, write_varint};
use crate::types::block::{BlockHeader, HEADER_SIZE};

/// Maximum number of headers allowed in a single headers message
pub const MAX_HEADERS_PER_MSG: usize = 2000;

/// A headers message payload, carrying up to 2000 block headers
///
/// Each header is followed by a `tx_count` varint that is always 0 on the wire.
/// This is a quirk of the protocol: headers messages reuse block serialization
/// but strip the transactions, leaving a zero `tx_count` in their place
pub struct MessageHeaders {
	headers: Vec<BlockHeader>,
}

impl MessageHeaders {
	/// Creates a new headers message from a list of block headers
	pub const fn new(headers: Vec<BlockHeader>) -> Self {
		Self { headers }
	}

	/// Returns the headers in this message
	pub fn headers(&self) -> &[BlockHeader] {
		&self.headers
	}

	/// Consumes the message and returns the headers
	pub fn into_headers(self) -> Vec<BlockHeader> {
		self.headers
	}

	/// Serializes the headers message to wire bytes
	///
	/// Each header is serialized as 80 bytes followed by a 0x00 `tx_count` varint
	pub fn to_bytes(&self) -> Vec<u8> {
		// varint (max 9) + (80 header + 1 tx_count) per header
		// headers.len() is bounded by MAX_HEADERS_PER_MSG (2000), multiplication can't overflow
		#[allow(clippy::arithmetic_side_effects)]
		let capacity = 9 + self.headers.len() * (HEADER_SIZE + 1);
		let mut bytes = Vec::with_capacity(capacity);

		// headers.len() bounded by MAX_HEADERS_PER_MSG, fits in u64
		#[allow(clippy::cast_possible_truncation)]
		write_varint(&mut bytes, self.headers.len() as u64);

		for header in &self.headers {
			bytes.extend_from_slice(&header.to_bytes());
			// tx_count is always 0 in headers messages
			write_varint(&mut bytes, 0);
		}

		bytes
	}

	/// Decodes a headers message from wire bytes
	///
	/// Rejects messages with more than `MAX_HEADERS_PER_MSG` headers.
	/// The `tx_count` field after each header must be 0
	pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);
		let count = decode_varint(&mut cursor).context("failed to decode headers count")?;

		if count > MAX_HEADERS_PER_MSG as u64 {
			return Err(anyhow!(
				"headers message claims {count} headers, max is {MAX_HEADERS_PER_MSG}"
			));
		}

		// count validated <= MAX_HEADERS_PER_MSG, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut headers = Vec::with_capacity(count as usize);

		for i in 0..count {
			let header = BlockHeader::from_cursor(&mut cursor).with_context(|| format!("failed to read header {i}"))?;

			let tx_count =
				decode_varint(&mut cursor).with_context(|| format!("failed to read tx_count for header {i}"))?;

			if tx_count != 0 {
				return Err(anyhow!("header {i} has non-zero tx_count {tx_count}, expected 0"));
			}

			headers.push(header);
		}

		Ok(Self { headers })
	}
}
