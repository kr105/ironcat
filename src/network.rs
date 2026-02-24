// SPDX-License-Identifier: Apache-2.0

use std::{
	collections::VecDeque,
	io::{Cursor, Read},
	net::IpAddr,
	sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use bitflags::bitflags;
use byteorder::{LittleEndian, ReadBytesExt};
use sha2::{Digest, Sha256};
use tokio::{
	io::AsyncWriteExt,
	net::{tcp::OwnedWriteHalf, TcpListener},
	sync::Mutex,
};
use tracing::{debug, error, trace, warn};

use crate::{
	nodes::{node_connection_loop, ConnectionType, NodeEndpoint, NodeManager, NodeState},
	utils::ipv4_to_mapped_ipv6,
};

pub mod message_addr;
pub mod message_version;

/// Maximum allowed size for a network message
pub const MAX_MESSAGE_SIZE: usize = 5_000_000;

/// Length of the command field in the network message
const COMMAND_LENGTH: usize = 12;

/// Network magic bytes for Catcoin mainnet
const NET_MAGIC: [u8; 4] = [0xFC, 0xC1, 0xB7, 0xDC];

/// Maximum allowed length for a variable-length string
const MAX_VARSTR_LENGTH: usize = 4096;

/// Thread-safe shared TCP writer for sending messages to a peer
pub type SharedTcpWriter = Arc<Mutex<OwnedWriteHalf>>;

/// Extension trait for sending protocol messages over a shared TCP writer
pub trait SharedTcpWriterExt {
	/// Constructs and sends a protocol message with the given command and payload
	async fn send_message(&self, command: &str, payload: &[u8]) -> Result<()>;
}

impl SharedTcpWriterExt for SharedTcpWriter {
	async fn send_message(&self, command: &str, payload: &[u8]) -> Result<()> {
		let packet = Message::new(command, payload).map_err(|e| {
			warn!("Failed to construct message for command '{command}': {e}");
			e
		})?;

		if let Err(error) = self.lock().await.write_all(&packet.to_bytes()).await {
			return Err(anyhow!("Error in write_all: {error:?}"));
		}

		Ok(())
	}
}

/// Represents a network address in the Catcoin protocol
#[derive(Debug, Hash, Eq, PartialEq)]
pub struct NetworkAddress {
	/// bitfield of features to be enabled for this connection
	pub services: ServiceMask,
	/// IPv6 address. Network byte order. IPv4 address is written as a 16 byte IPv4-mapped IPv6 address
	pub address: IpAddr,
	/// port number, network byte order
	pub port: u16,
}

impl NetworkAddress {
	/// Creates a new `NetworkAddress` with the given IP address and port
	pub fn new(address: IpAddr, port: u16) -> Self {
		let mut services = ServiceMask::empty();
		services.set(ServiceMask::NODE_NETWORK_LIMITED, true);

		Self {
			services,
			address,
			port,
		}
	}

	/// Converts the Address to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(26);
		bytes.extend_from_slice(&self.services.bits().to_le_bytes());
		bytes.extend_from_slice(&self.address_to_network_bytes());
		bytes.extend_from_slice(&self.port.to_be_bytes());

		bytes
	}

	/// Decodes an Address from a slice of network bytes
	pub fn from_bytes(bytes: &[u8]) -> Result<Self, std::io::Error> {
		if bytes.len() != 26 {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"Invalid byte length for Address",
			));
		}

		// Slicing and try_into below are safe: length == 26 is checked above
		#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
		let services = ServiceMask::from_bits_truncate(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));

		#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
		let ip_bytes: [u8; 16] = bytes[8..24].try_into().unwrap();
		let address = IpAddr::from(ip_bytes).to_canonical();

		#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
		let port = u16::from_be_bytes(bytes[24..26].try_into().unwrap());

		Ok(Self {
			services,
			address,
			port,
		})
	}

	/// Converts the IP address to a 16-byte network order representation
	pub(crate) const fn address_to_network_bytes(&self) -> [u8; 16] {
		match &self.address {
			IpAddr::V4(ipv4) => ipv4_to_mapped_ipv6(*ipv4),
			IpAddr::V6(ipv6) => ipv6.octets(),
		}
	}
}

bitflags! {
	/// Represents the services offered by a node
	#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
	pub struct ServiceMask: u64 {
		/// Node can serve full blocks
		const NODE_NETWORK = 1;

		/// Node can respond to getutxo requests (see BIP64)
		const NODE_GETUTXO = 2;

		/// Node supports Bloom filtering (see BIP111)
		const NODE_BLOOM = 4;

		/// Node supports segregated witness (see BIP144)
		const NODE_WITNESS = 8;

		/// Discontinued feature, was used for Xtreme Thinblocks
		const NODE_XTHIN = 16;

		/// Node supports compact block filters (see BIP157)
		const NODE_COMPACT_FILTERS = 64;

		/// Node is a pruned client with limited block serving capability
		const NODE_NETWORK_LIMITED = 1024;
	}
}

/// Distinguishes incomplete data (need more bytes) from corrupt data (bad magic, checksum, etc)
#[derive(Debug)]
pub enum MessageParseError {
	/// Not enough bytes yet -- wait for more data
	Incomplete,
	/// Data is corrupt -- disconnect the peer
	Corrupt(String),
}

impl std::fmt::Display for MessageParseError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Incomplete => write!(f, "incomplete message data"),
			Self::Corrupt(reason) => write!(f, "corrupt message: {reason}"),
		}
	}
}

impl std::error::Error for MessageParseError {}

/// Represents a Catcoin network message
#[derive(Debug)]
pub struct Message {
	magic: [u8; 4],
	pub command: [u8; COMMAND_LENGTH],
	length: u32,
	checksum: u32,
	pub payload: Vec<u8>,
}

impl Message {
	/// Creates a new `NetworkMessage` with the given command and payload
	pub fn new(command: &str, payload: &[u8]) -> Result<Self> {
		if payload.len() > MAX_MESSAGE_SIZE {
			return Err(anyhow!(
				"payload size {} exceeds maximum {MAX_MESSAGE_SIZE}",
				payload.len()
			));
		}

		// Payload is validated <= MAX_MESSAGE_SIZE (5MB), fits in u32
		#[allow(clippy::cast_possible_truncation)]
		let mut msg = Self {
			magic: NET_MAGIC,
			command: [0; COMMAND_LENGTH],
			length: payload.len() as u32,
			checksum: 0,
			payload: payload.to_vec(),
		};

		msg.set_command(command)?;
		msg.checksum = msg.calculate_checksum();

		Ok(msg)
	}

	/// Sets the command for the message
	fn set_command(&mut self, command: &str) -> Result<()> {
		if !command.is_ascii() {
			return Err(anyhow!("Command contains non-ASCII characters"));
		}

		if command.len() >= COMMAND_LENGTH {
			return Err(anyhow!(
				"Command is too long (max {COMMAND_LENGTH} characters, plus NULL padding)"
			));
		}

		if command.is_empty() {
			return Err(anyhow!("Command is too short"));
		}

		// Copy command string into fixed-size array, leaving the last byte as 0
		// Slice is safe: command.len() < COMMAND_LENGTH is checked above
		#[allow(clippy::indexing_slicing)]
		self.command[..command.len()].copy_from_slice(command.as_bytes());

		Ok(())
	}

	/// Calculate the checksum for the message payload
	/// The checksum is the first 4 bytes of the double SHA256 hash of the payload
	fn calculate_checksum(&self) -> u32 {
		let mut hasher = Sha256::new();

		// First round of SHA256
		hasher.update(&self.payload);
		let first_hash = hasher.finalize_reset();

		// Second round of SHA256
		hasher.update(first_hash);
		let second_hash = hasher.finalize();

		// SHA256 always produces 32 bytes, so slicing [..4] is always safe
		#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
		u32::from_le_bytes(second_hash[..4].try_into().unwrap())
	}

	/// Converts the `NetworkMessage` to a byte vector for network transmission
	pub fn to_bytes(&self) -> Vec<u8> {
		// Header (24 bytes) + payload; both operands are bounded so no overflow is possible
		#[allow(clippy::arithmetic_side_effects)]
		let mut bytes = Vec::with_capacity(24 + self.payload.len());
		bytes.extend_from_slice(&self.magic);
		bytes.extend_from_slice(&self.command);
		bytes.extend_from_slice(&self.length.to_le_bytes());
		bytes.extend_from_slice(&self.checksum.to_le_bytes());
		bytes.extend_from_slice(&self.payload);
		bytes
	}

	/// Decodes a `NetworkMessage` from a byte slice, returning the message and the number of bytes consumed
	pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), MessageParseError> {
		// Header size: magic(4) + command(COMMAND_LENGTH) + length(4) + checksum(4)
		let header_size = 4 + COMMAND_LENGTH + 4 + 4;

		if bytes.len() < header_size {
			return Err(MessageParseError::Incomplete);
		}

		let mut cursor = Cursor::new(bytes);

		// Read magic
		let mut magic = [0u8; 4];
		cursor
			.read_exact(&mut magic)
			.map_err(|e| MessageParseError::Corrupt(e.to_string()))?;

		if magic != NET_MAGIC {
			return Err(MessageParseError::Corrupt("invalid network magic".to_string()));
		}

		// Read command
		let mut command = [0u8; COMMAND_LENGTH];
		cursor
			.read_exact(&mut command)
			.map_err(|e| MessageParseError::Corrupt(e.to_string()))?;

		// Read length
		let length = cursor
			.read_u32::<LittleEndian>()
			.map_err(|e| MessageParseError::Corrupt(e.to_string()))?;

		// Validate length before allocating -- prevents a peer from forcing 5MB allocation per connection
		// length is u32, MAX_MESSAGE_SIZE is usize; compare as usize on 64-bit
		#[allow(clippy::cast_possible_truncation)]
		if length as usize > MAX_MESSAGE_SIZE {
			return Err(MessageParseError::Corrupt(format!(
				"declared length {length} exceeds maximum of {MAX_MESSAGE_SIZE}"
			)));
		}

		// Read checksum
		let checksum = cursor
			.read_u32::<LittleEndian>()
			.map_err(|e| MessageParseError::Corrupt(e.to_string()))?;

		// length is u32, header_size is 24; can't overflow usize on 64-bit
		#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
		let total_size = header_size + length as usize;

		if bytes.len() < total_size {
			return Err(MessageParseError::Incomplete);
		}

		// Read payload; length <= MAX_MESSAGE_SIZE is validated above, safe to cast
		#[allow(clippy::cast_possible_truncation)]
		let mut payload = vec![0u8; length as usize];
		cursor
			.read_exact(&mut payload)
			.map_err(|e| MessageParseError::Corrupt(e.to_string()))?;

		let msg = Self {
			magic,
			command,
			length,
			checksum,
			payload,
		};

		// Verify checksum
		let calculated_checksum = msg.calculate_checksum();
		if calculated_checksum != checksum {
			return Err(MessageParseError::Corrupt("checksum mismatch".to_string()));
		}

		Ok((msg, total_size))
	}
}

/// Represents the different types of network commands
#[derive(Debug, PartialEq, Eq)]
pub enum NetworkCommand {
	Version,
	Verack,
	Ping,
	Pong,
	Alert,
	GetAddr,
	Addr,
	Unknown(String),
}

impl NetworkCommand {
	/// Parses a command string into a `NetworkCommand` variant
	pub(crate) fn from_command_str(s: &str) -> Self {
		// Protocol commands are always lowercase ASCII on the wire
		match s {
			"version" => Self::Version,
			"verack" => Self::Verack,
			"ping" => Self::Ping,
			"pong" => Self::Pong,
			"alert" => Self::Alert,
			"getaddr" => Self::GetAddr,
			"addr" => Self::Addr,
			_ => Self::Unknown(s.to_string()),
		}
	}
}

/// Writes a variable length integer (`VarInt`) directly into the given buffer
pub fn write_varint(buf: &mut Vec<u8>, n: u64) {
	// Each cast is guarded by the if/else range check above it
	if n < 0xfd {
		// Range check above guarantees n fits in u8
		#[allow(clippy::cast_possible_truncation)]
		buf.push(n as u8);
	} else if n <= 0xffff {
		buf.push(0xfd);
		#[allow(clippy::cast_possible_truncation)]
		buf.extend_from_slice(&(n as u16).to_le_bytes());
	} else if n <= 0xffff_ffff {
		buf.push(0xfe);
		#[allow(clippy::cast_possible_truncation)]
		buf.extend_from_slice(&(n as u32).to_le_bytes());
	} else {
		buf.push(0xff);
		buf.extend_from_slice(&n.to_le_bytes());
	}
}

/// Encodes a string as a variable-length string for the wire protocol
pub fn encode_varstr(s: &str) -> Vec<u8> {
	// s.len() + 9 (max varint) cannot overflow usize for any real string
	#[allow(clippy::arithmetic_side_effects)]
	let mut encoded = Vec::with_capacity(s.len() + 9);
	write_varint(&mut encoded, s.len() as u64);
	encoded.extend_from_slice(s.as_bytes());
	encoded
}

/// Decodes a variable-length integer from a cursor
pub fn decode_varint(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
	let first_byte: u8 = cursor.read_u8()?;

	match first_byte {
		0xFD => {
			let uint16 = cursor.read_u16::<LittleEndian>()?;
			Ok(u64::from(uint16))
		}

		0xFE => {
			let uint32 = cursor.read_u32::<LittleEndian>()?;
			Ok(u64::from(uint32))
		}

		0xFF => {
			let uint64 = cursor.read_u64::<LittleEndian>()?;
			Ok(uint64)
		}

		_ => Ok(u64::from(first_byte)),
	}
}

/// Decodes a string from a variable length string
pub fn decode_varstr(cursor: &mut Cursor<&[u8]>) -> Result<String> {
	let length = decode_varint(cursor)?;

	// length is validated <= MAX_VARSTR_LENGTH (4096), fits in usize
	#[allow(clippy::cast_possible_truncation)]
	if length as usize > MAX_VARSTR_LENGTH {
		return Err(anyhow!("varstr length {length} exceeds maximum of {MAX_VARSTR_LENGTH}"));
	}

	#[allow(clippy::cast_possible_truncation)]
	let mut str_bytes = vec![0u8; length as usize];
	cursor.read_exact(&mut str_bytes)?;

	String::from_utf8(str_bytes).context("varstr contains invalid UTF-8")
}

/// Manages network messages and buffers incomplete messages
pub struct NetworkQueue {
	buffer: Vec<u8>,
	messages: VecDeque<Message>,
}

impl NetworkQueue {
	/// Creates a new empty `NetworkQueue`
	pub const fn new() -> Self {
		Self {
			buffer: Vec::new(),
			messages: VecDeque::new(),
		}
	}

	/// Processes incoming data, extracting complete messages and buffering incomplete ones
	pub fn process_incoming_data(&mut self, data: &[u8]) -> Result<()> {
		self.buffer.extend_from_slice(data);

		let mut consumed = 0;
		loop {
			// consumed is always <= self.buffer.len() by construction
			#[allow(clippy::indexing_slicing)]
			match Message::from_bytes(&self.buffer[consumed..]) {
				Ok((message, len)) => {
					// consumed + len <= buffer.len() by construction, no overflow possible
					#[allow(clippy::arithmetic_side_effects)]
					{
						consumed += len;
					}
					self.messages.push_back(message);
				}
				Err(MessageParseError::Incomplete) => break,
				Err(MessageParseError::Corrupt(reason)) => {
					return Err(anyhow!("Corrupt message data: {reason}"));
				}
			}
		}

		if consumed > 0 {
			// Batch drain instead of per-message split_off
			self.buffer.drain(..consumed);
		}

		if self.buffer.len() > MAX_MESSAGE_SIZE {
			return Err(anyhow!("Buffer exceeds maximum message size"));
		}

		Ok(())
	}

	/// Retrieves the next complete message from the queue
	pub fn get_next_message(&mut self) -> Option<Message> {
		self.messages.pop_front()
	}
}

/// Starts the TCP listener for incoming peer connections
pub async fn listening_start(node_manager: Arc<NodeManager>) {
	trace!("listening_start task started");

	let tcp_listener = match TcpListener::bind("0.0.0.0:9933").await {
		Ok(listener) => listener,
		Err(error) => {
			error!("Failed to listen: {:?}", error);
			return;
		}
	};

	loop {
		let connection = match tcp_listener.accept().await {
			Ok(handle) => handle,
			Err(error) => {
				warn!("Failed to accept connection: {:?}", error);
				tokio::time::sleep(std::time::Duration::from_millis(100)).await;
				continue;
			}
		};

		let (mut tcp_stream, socket_addr) = connection;

		let node_endpoint = NodeEndpoint {
			address: socket_addr.ip(),
			port: socket_addr.port(),
		};

		let nm_clone: Arc<NodeManager> = Arc::clone(&node_manager);

		// Only proceed if the node doesn't exist already
		if nm_clone.insert(node_endpoint.address, node_endpoint.port, ConnectionType::Incoming) {
			tokio::spawn(async move {
				node_connection_loop(Arc::clone(&nm_clone), node_endpoint.clone(), tcp_stream).await;

				// Incoming connections can't be retried (we don't know their real port)
				nm_clone.set_state(&node_endpoint, NodeState::Dead);
			});
		} else {
			debug!("Dropping connection {} as node exists already", node_endpoint);

			_ = tcp_stream.shutdown().await;
		}
	}
}

#[cfg(test)]
// Tests use unwrap/indexing for brevity since panics are the intended failure mode
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
	use super::*;
	use std::net::{IpAddr, Ipv4Addr};

	#[test]
	fn network_address_port_roundtrip() {
		// Encode then decode should give the same port
		let port: u16 = 9933;
		let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);

		let bytes = addr.to_bytes();
		let decoded = NetworkAddress::from_bytes(&bytes).expect("should decode");

		assert_eq!(decoded.port, port);
	}

	#[test]
	fn decode_varstr_rejects_oversized_length() {
		// Craft a varstr with length = 0xFFFF (65535), way over any sane limit
		// but only 4 bytes of actual data after it
		let mut data = vec![0xFD, 0xFF, 0xFF]; // varint = 65535
		data.extend_from_slice(&[0x41; 4]); // only 4 bytes of "AAAA"

		let mut cursor = Cursor::new(data.as_slice());
		let result = decode_varstr(&mut cursor);

		assert!(result.is_err(), "should reject varstr with length > MAX_VARSTR_LENGTH");
	}

	#[test]
	fn decode_varstr_accepts_valid_string() {
		let encoded = encode_varstr("hello");
		let mut cursor = Cursor::new(encoded.as_slice());
		let result = decode_varstr(&mut cursor).unwrap();

		assert_eq!(result, "hello");
	}

	#[test]
	fn network_address_port_is_big_endian_on_wire() {
		// Port 0x1F90 (8080) should appear as [0x1F, 0x90] in the last 2 bytes
		let port: u16 = 8080;
		let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port);

		let bytes = addr.to_bytes();

		// The wire format is: 8 bytes services + 16 bytes IP + 2 bytes port
		let port_bytes = &bytes[24..26];
		assert_eq!(port_bytes, &port.to_be_bytes(), "port must be big-endian on the wire");
	}

	#[test]
	fn from_bytes_returns_consumed_length() {
		let msg = Message::new("ping", &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
		let bytes = msg.to_bytes();
		let mut extended = bytes.clone();
		extended.extend_from_slice(&[0xFF; 50]);

		let (parsed, consumed) = Message::from_bytes(&extended).unwrap();
		assert_eq!(consumed, bytes.len());
		assert_eq!(parsed.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
	}

	#[test]
	fn from_bytes_rejects_bad_magic_immediately() {
		let mut bytes = vec![0x00, 0x00, 0x00, 0x00];
		bytes.extend_from_slice(&[0u8; 20]);
		let err = Message::from_bytes(&bytes).unwrap_err();
		assert!(matches!(err, MessageParseError::Corrupt(_)));
	}

	#[test]
	fn from_bytes_returns_incomplete_for_short_buffer() {
		let bytes = vec![0xFC, 0xC1, 0xB7, 0xDC];
		let err = Message::from_bytes(&bytes).unwrap_err();
		assert!(matches!(err, MessageParseError::Incomplete));
	}

	#[test]
	fn from_bytes_rejects_oversized_declared_length() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&[0xFC, 0xC1, 0xB7, 0xDC]);
		bytes.extend_from_slice(&[0u8; 12]);
		bytes.extend_from_slice(&((MAX_MESSAGE_SIZE as u32) + 1).to_le_bytes());
		bytes.extend_from_slice(&[0u8; 4]);
		let err = Message::from_bytes(&bytes).unwrap_err();
		assert!(matches!(err, MessageParseError::Corrupt(_)));
	}

	#[test]
	fn process_incoming_data_detects_corrupt_magic() {
		let mut queue = NetworkQueue::new();
		let bad_data = vec![
			0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
		];
		let result = queue.process_incoming_data(&bad_data);
		assert!(result.is_err());
	}
}
