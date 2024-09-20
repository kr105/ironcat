// SPDX-License-Identifier: Apache-2.0

use std::net::{IpAddr, Ipv4Addr};

use anyhow::{anyhow, Result};
use bitflags::bitflags;
use sha2::{Digest, Sha256};

pub mod message_version;

/// Length of the command field in the network message
const COMMAND_LENGTH: usize = 12;

/// Network magic bytes for Catcoin mainnet
const NET_MAGIC: [u8; 4] = [0xFC, 0xC1, 0xB7, 0xDC];

/// Represents a network address in the Bitcoin protocol
#[derive(Debug, Hash)]
pub struct Address {
    /// bitfield of features to be enabled for this connection
    pub services: ServiceMask,
    /// IPv6 address. Network byte order. IPv4 address is written as a 16 byte IPv4-mapped IPv6 address
    pub address: IpAddr,
    /// port number, network byte order
    pub port: u16,
}

impl Address {
    /// Creates a new Address with the given IP address and port
    pub fn new(address: IpAddr, port: u16) -> Self {
        let mut services = ServiceMask::empty();
        services.set(ServiceMask::NODE_NETWORK_LIMITED, true);

        Address {
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
    #[derive(Debug, Hash)]
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
    command: [u8; COMMAND_LENGTH],
    length: u32,
    checksum: u32,
    payload: Vec<u8>,
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

    /// Calculate the checksum for the message payload.
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
