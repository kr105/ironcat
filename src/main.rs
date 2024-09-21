// SPDX-License-Identifier: Apache-2.0

mod network;
mod nodes;

use anyhow::{anyhow, Result};
use nodes::{insert_node, NodeManager};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpStream},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

use network::{
    message_version::MessageVersion, NetworkAddress, NetworkMessage, ServiceMask, MAX_MESSAGE_SIZE,
};

fn handle_tcp_stream(mut stream: TcpStream) -> Result<()> {
    println!("Successfully connected to server");

    let addr = NetworkAddress::new(IpAddr::from(Ipv4Addr::from_str("127.0.0.1").unwrap()), 1234);
    let version = MessageVersion::new(addr);

    let message = NetworkMessage::new("version", version.to_bytes())?;

    stream.write_all(&message.to_bytes())?;
    println!("Sent version message, awaiting reply...");

    // Set a read timeout
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;

    let mut buffer: VecDeque<u8> = VecDeque::new();
    let mut temp_buffer = [0u8; 1024];

    loop {
        match stream.read(&mut temp_buffer) {
            Ok(0) => {
                // Connection was closed
                return Err(anyhow!("Connection closed by peer"));
            }
            Ok(bytes_read) => {
                let _ = buffer.write(&temp_buffer[..bytes_read]);

                // Try to parse the message
                match NetworkMessage::from_bytes(buffer.make_contiguous()) {
                    Ok(message) => {
                        println!("Received message 1: {:?}", message);

                        let xxx = MessageVersion::from_bytes(&message.payload)?;

                        println!("Received message 2: {:?}", xxx);

                        // REMOVE PROCESSED????
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

#[tokio::main]
async fn main() -> Result<()> {
    let node_manager = Arc::new(Mutex::new(NodeManager::new()));

    let nm_clone = Arc::clone(&node_manager);
    insert_node(nm_clone, "127.0.0.1", 9933).await;

    // Keep the main task running
    tokio::signal::ctrl_c().await?;
    Ok(())
}
