use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

const COMMAND_LENGTH: usize = 12;
const NET_MAGIC: [u8; 4] = [0xFB, 0xC0, 0xB6, 0xDB];

#[derive(Debug)]
pub struct NetworkMessage {
    magic: [u8; 4],
    command: [u8; COMMAND_LENGTH],
    length: u32,
    checksum: u32,
    payload: Vec<u8>,
}

impl NetworkMessage {
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

    fn calculate_checksum(&self) -> u32 {
        let mut hasher = Sha256::new();
        hasher.update(&self.payload);
        let result = hasher.finalize();
        let mut hasher = Sha256::new();
        hasher.update(result);
        let result = hasher.finalize();
        u32::from_le_bytes(result[..4].try_into().unwrap())
    }

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
