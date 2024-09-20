// SPDX-License-Identifier: Apache-2.0

mod network;

use anyhow::{anyhow, Result};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpStream},
    str::FromStr,
    time::Duration,
};

use network::{message_version::MessageVersion, Address, NetworkMessage, MAX_MESSAGE_SIZE};

fn handle_tcp_stream(mut stream: TcpStream) -> Result<()> {
    println!("Successfully connected to server");

    let addr = Address::new(IpAddr::from(Ipv4Addr::from_str("127.0.0.1").unwrap()), 1234);
    let version = MessageVersion::new(addr);

    let message = NetworkMessage::new("version", version.to_bytes())?;

    stream.write_all(&message.to_bytes())?;
    println!("Sent version message, awaiting reply...");

    // Set a read timeout
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    let mut buffer = Vec::new();
    let mut temp_buffer = [0u8; 1024];

    loop {
        match stream.read(&mut temp_buffer) {
            Ok(0) => {
                // Connection was closed
                return Err(anyhow!("Connection closed by peer"));
            }
            Ok(bytes_read) => {
                buffer.extend_from_slice(&temp_buffer[..bytes_read]);

                // Try to parse the message
                match NetworkMessage::from_bytes(&buffer) {
                    Ok(message) => {
                        println!("Received message: {:?}", message);

                        let xxx = MessageVersion::from_bytes(&message.payload)?;

                        println!("Received message: {:?}", xxx);

                        // Clear the buffer for the next message
                        buffer.clear();
                        break;
                    }
                    Err(_e) => {
                        if buffer.len() > MAX_MESSAGE_SIZE {
                            return Err(anyhow!("Received oversized message"));
                        }

                        // NetworkMessage rejected the data, keep reading ...
                        continue;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Timeout occurred
                return Err(anyhow!("Timeout while waiting for data"));
            }
            Err(e) => {
                return Err(anyhow!("Failed to receive data: {}", e));
            }
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    match TcpStream::connect("localhost:9933") {
        Ok(stream) => handle_tcp_stream(stream)?,
        Err(e) => println!("Failed to connect: {}", e),
    }

    Ok(())
}
