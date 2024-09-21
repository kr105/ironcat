// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{Cursor, Read},
    net::{IpAddr, Ipv4Addr},
};

use anyhow::{anyhow, Result};
use bitflags::bitflags;
use byteorder::{LittleEndian, ReadBytesExt};
use sha2::{Digest, Sha256};

pub mod message_version;

pub const MAX_MESSAGE_SIZE: usize = 5_000_000;

/// Length of the command field in the network message
const COMMAND_LENGTH: usize = 12;

/// Network magic bytes for Catcoin mainnet
const NET_MAGIC: [u8; 4] = [0xFC, 0xC1, 0xB7, 0xDC];

/// Represents a network address in the Bitcoin protocol
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
    /// Creates a new NetworkAddress with the given IP address and port
    pub fn new(address: IpAddr, port: u16) -> Self {
        let mut services = ServiceMask::empty();
        services.set(ServiceMask::NODE_NETWORK_LIMITED, true);

        NetworkAddress {
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
        bytes.extend_from_slice(&self.port.to_le_bytes());

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

        let services =
            ServiceMask::from_bits_truncate(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));

        let ip_bytes: [u8; 16] = bytes[8..24].try_into().unwrap();
        let address = IpAddr::from(ip_bytes);

        let port = u16::from_be_bytes(bytes[24..26].try_into().unwrap());

        Ok(NetworkAddress {
            services,
            address,
            port,
        })
    }

    /// Converts the IP address to a 16-byte network order representation
    fn address_to_network_bytes(&self) -> [u8; 16] {
        match &self.address {
            IpAddr::V4(ipv4) => Self::ipv4_to_mapped_ipv6(*ipv4),
            IpAddr::V6(ipv6) => ipv6.octets(),
        }
    }

    /// Converts an IPv4 address to an IPv4-mapped IPv6 address in network byte order
    fn ipv4_to_mapped_ipv6(ipv4: Ipv4Addr) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[10] = 0xff;
        bytes[11] = 0xff;
        bytes[12..].copy_from_slice(&ipv4.octets());
        bytes
    }
}

bitflags! {
    /// Represents the services offered by a node
    #[derive(Debug, Hash, Eq, PartialEq)]
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

/// Represents a Catcoin network message
#[derive(Debug)]
pub struct NetworkMessage {
    magic: [u8; 4],
    pub command: [u8; COMMAND_LENGTH],
    length: u32,
    checksum: u32,
    pub payload: Vec<u8>,
}

impl NetworkMessage {
    /// Creates a new NetworkMessage with the given command and payload
    pub fn new(command: &str, payload: Vec<u8>) -> Result<Self> {
        let mut msg = NetworkMessage {
            magic: NET_MAGIC,
            command: [0; COMMAND_LENGTH],
            length: payload.len() as u32,
            checksum: 0,
            payload,
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
                "Command is too long (max {} characters, plus NULL padding)",
                COMMAND_LENGTH
            ));
        }

        if command.is_empty() {
            return Err(anyhow!("Command is too short"));
        }

        // Copy command string into fixed-size array, leaving the last byte as 0
        self.command[..command.len()].copy_from_slice(command.as_bytes());
        // The rest of the array is already initialized to 0, so we don't need to set it explicitly

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

        // Convert the first 4 bytes of the second hash to a u32
        u32::from_le_bytes(second_hash[..4].try_into().unwrap())
    }

    /// Converts the NetworkMessage to a byte vector for network transmission
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.magic);
        bytes.extend_from_slice(&self.command);
        bytes.extend_from_slice(&self.length.to_le_bytes());
        bytes.extend_from_slice(&self.checksum.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// Decodes a NetworkMessage from a byte slice
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 + COMMAND_LENGTH + 4 + 4 {
            return Err(anyhow!(
                "Byte slice is too short for a valid NetworkMessage"
            ));
        }

        let mut cursor = Cursor::new(bytes);

        // Read magic
        let mut magic = [0u8; 4];
        cursor.read_exact(&mut magic)?;

        if magic != NET_MAGIC {
            return Err(anyhow!("Invalid network magic"));
        }

        // Read command
        let mut command = [0u8; COMMAND_LENGTH];
        cursor.read_exact(&mut command)?;

        // Read length
        let length = cursor.read_u32::<LittleEndian>()?;

        // Read checksum
        let checksum = cursor.read_u32::<LittleEndian>()?;

        if bytes.len() < 4 + COMMAND_LENGTH + 4 + 4 + length as usize {
            return Err(anyhow!(
                "Byte slice is too short for the entire NetworkMessage"
            ));
        }

        // Read payload
        let mut payload = vec![0u8; length as usize];
        cursor.read_exact(&mut payload)?;

        // Create the NetworkMessage
        let msg = NetworkMessage {
            magic,
            command,
            length,
            checksum,
            payload,
        };

        // Verify checksum
        let calculated_checksum = msg.calculate_checksum();
        if calculated_checksum != checksum {
            return Err(anyhow!("Checksum mismatch"));
        }

        Ok(msg)
    }
}

/// Encodes a u64 as a variable length integer (VarInt)
fn encode_varint(n: u64) -> Vec<u8> {
    if n < 0xfd {
        vec![n as u8]
    } else if n <= 0xffff {
        let mut v = vec![0xfd];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= 0xffffffff {
        let mut v = vec![0xfe];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![0xff];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
}

/// Encodes a string as a variable length string
pub fn encode_varstr(s: &str) -> Vec<u8> {
    let mut encoded = encode_varint(s.len() as u64);
    encoded.extend_from_slice(s.as_bytes());
    encoded
}

/// Decodes a u64 from a variable length integer (VarInt)
pub fn decode_varint(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let first_byte: u8 = cursor.read_u8()?;

    match first_byte {
        0xFD => {
            let uint16 = cursor.read_u16::<LittleEndian>()?;
            Ok(uint16 as u64)
        }

        0xFE => {
            let uint32 = cursor.read_u32::<LittleEndian>()?;
            Ok(uint32 as u64)
        }

        0xFF => {
            let uint64 = cursor.read_u64::<LittleEndian>()?;
            Ok(uint64)
        }

        _ => Ok(first_byte as u64),
    }
}

/// Decodes a string from a variable length string
pub fn decode_varstr(cursor: &mut Cursor<&[u8]>) -> Result<String> {
    let length = decode_varint(cursor)?;

    let mut str_bytes = vec![0u8; length as usize];
    cursor.read_exact(&mut str_bytes)?;

    Ok(String::from_utf8(str_bytes)?)
}

pub struct NetworkQueue {
    buffer: Vec<u8>,
    messages: Vec<NetworkMessage>,
}

impl NetworkQueue {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            messages: Vec::new(),
        }
    }

    pub fn process_incoming_data(&mut self, data: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(data);

        loop {
            match NetworkMessage::from_bytes(&self.buffer) {
                Ok(message) => {
                    let message_len = message.to_bytes().len();
                    self.messages.push(message);
                    self.buffer = self.buffer.split_off(message_len);
                }
                Err(_) => {
                    if self.buffer.len() > MAX_MESSAGE_SIZE {
                        return Err(anyhow!("Received oversized message"));
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn get_next_message(&mut self) -> Option<NetworkMessage> {
        if !self.messages.is_empty() {
            Some(self.messages.remove(0))
        } else {
            None
        }
    }
}
