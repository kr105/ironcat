// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpStream},
    str::FromStr,
};

use network::{message_version::MessageVersion, Address, NetworkMessage};

mod network;

fn main() {
    match TcpStream::connect("localhost:9933") {
        Ok(mut stream) => {
            println!("Successfully connected to server in port 3333");

            let addr = Address::new(IpAddr::from(Ipv4Addr::from_str("127.0.0.1").unwrap()), 1234);
            let version = MessageVersion::new(addr);

            let message = NetworkMessage::new("version", version.to_bytes()).unwrap();

            stream.write_all(&message.to_bytes()).unwrap();
            println!("Sent Hello, awaiting reply...");

            let mut data = [0_u8; 1024]; // using 6 byte buffer
            match stream.read(&mut data) {
                Ok(xd) => {
                    println!("Unexpected reply: {}", xd);
                }
                Err(e) => {
                    println!("Failed to receive data: {}", e);
                }
            }
        }
        Err(e) => {
            println!("Failed to connect: {}", e);
        }
    }
    println!("Terminated.");
}
